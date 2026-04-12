//! Session lifecycle management.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

use simgit_sdk::{SessionInfo, SessionStatus};

use super::db::{Db, LockRow, SessionRow};

pub struct SessionManager {
    db:    Mutex<Db>,
    /// In-memory cache for hot-path reads (lock conflict messages, etc.)
    cache: Mutex<HashMap<Uuid, SessionInfo>>,
}

impl SessionManager {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let db = Db::open(db_path)?;
        let mgr = Self { db: Mutex::new(db), cache: Mutex::new(HashMap::new()) };
        mgr.warm_cache()?;
        Ok(mgr)
    }

    /// Create an in-memory `SessionManager` for unit tests.
    ///
    /// Backed by an in-memory SQLite database (`:memory:`), so there is no disk
    /// I/O and the database is destroyed when the instance is dropped.
    #[cfg(test)]
    pub fn for_testing() -> std::sync::Arc<Self> {
        use super::db::Db;
        let db = Db::in_memory().expect("in-memory SQLite");
        std::sync::Arc::new(Self { db: Mutex::new(db), cache: Mutex::new(HashMap::new()) })
    }

    fn warm_cache(&self) -> Result<()> {
        let rows = self.db.lock().unwrap().load_all()?;
        let mut cache = self.cache.lock().unwrap();
        for row in rows {
            cache.insert(row_to_uuid(&row)?, row_to_info(&row)?);
        }
        Ok(())
    }

    // ── Create ────────────────────────────────────────────────────────────

    pub fn create(
        &self,
        task_id:      String,
        agent_label:  Option<String>,
        base_commit:  String,
        mount_path:   PathBuf,
        peers:        bool,
        max_sessions: usize,
    ) -> Result<SessionInfo> {
        // Enforce max-sessions limit.
        let active_count = {
            let cache = self.cache.lock().unwrap();
            cache.values().filter(|s| s.status == SessionStatus::Active).count()
        };
        if active_count >= max_sessions {
            bail!("max sessions ({max_sessions}) reached; cannot create a new session");
        }

        let session_id = Uuid::now_v7();
        let created_at = Utc::now();

        let info = SessionInfo {
            session_id,
            task_id:      task_id.clone(),
            agent_label:  agent_label.clone(),
            base_commit:  base_commit.clone(),
            created_at,
            status:       SessionStatus::Active,
            mount_path:   mount_path.clone(),
            branch_name:  None,
            peers_enabled: peers,
        };

        self.db.lock().unwrap().upsert_session(
            &session_id.to_string(),
            &task_id,
            agent_label.as_deref(),
            &base_commit,
            created_at.timestamp(),
            "ACTIVE",
            &mount_path.to_string_lossy(),
            None,
            peers,
        )?;

        self.cache.lock().unwrap().insert(session_id, info.clone());
        info!(id = %session_id, task = %task_id, "session created");
        Ok(info)
    }

    // ── Status transitions ────────────────────────────────────────────────

    pub fn mark_committed(&self, session_id: Uuid, branch_name: &str) -> Result<SessionInfo> {
        self.update_status(session_id, SessionStatus::Committed, Some(branch_name))
    }

    pub fn mark_aborted(&self, session_id: Uuid) -> Result<SessionInfo> {
        self.update_status(session_id, SessionStatus::Aborted, None)
    }

    pub fn mark_stale(&self, session_id: Uuid) -> Result<SessionInfo> {
        self.update_status(session_id, SessionStatus::Stale, None)
    }

    fn update_status(
        &self,
        session_id:  Uuid,
        status:      SessionStatus,
        branch_name: Option<&str>,
    ) -> Result<SessionInfo> {
        let status_str = format!("{status:?}").to_uppercase();
        self.db.lock().unwrap().update_status(
            &session_id.to_string(),
            &status_str,
            branch_name,
        )?;
        let mut cache = self.cache.lock().unwrap();
        let info = cache
            .get_mut(&session_id)
            .with_context(|| format!("session {session_id} not in cache"))?;
        info.status = status;
        if let Some(bn) = branch_name {
            info.branch_name = Some(bn.to_owned());
        }
        Ok(info.clone())
    }

    // ── Query ─────────────────────────────────────────────────────────────

    pub fn get(&self, session_id: Uuid) -> Option<SessionInfo> {
        self.cache.lock().unwrap().get(&session_id).cloned()
    }

    /// Synchronous get — used from non-async contexts (e.g. borrow registry).
    pub fn get_info_blocking(&self, session_id: Uuid) -> Option<SessionInfo> {
        self.get(session_id)
    }

    pub fn list(&self, status: Option<SessionStatus>) -> Vec<SessionInfo> {
        let cache = self.cache.lock().unwrap();
        cache
            .values()
            .filter(|s| status.as_ref().map_or(true, |st| s.status == *st))
            .cloned()
            .collect()
    }

    pub fn list_active(&self) -> Vec<SessionInfo> {
        self.list(Some(SessionStatus::Active))
    }

    // ── Lock persistence ──────────────────────────────────────────────────

    /// Persist (upsert) a lock entry to SQLite.
    ///
    /// Called by `BorrowRegistry` after every in-memory lock acquisition so that
    /// the lock table survives daemon restarts.
    pub fn persist_lock(
        &self,
        path:        &Path,
        writer:      Option<Uuid>,
        readers:     &HashSet<Uuid>,
        acquired_at: chrono::DateTime<Utc>,
        ttl_seconds: Option<u64>,
    ) -> Result<()> {
        let reader_ids: Vec<String> = readers.iter().map(|u| u.to_string()).collect();
        let readers_json = serde_json::to_string(&reader_ids).context("serialize reader_sessions")?;
        let writer_str = writer.map(|u| u.to_string());
        self.db.lock().unwrap().upsert_lock(
            &path.to_string_lossy(),
            writer_str.as_deref(),
            &readers_json,
            acquired_at.timestamp(),
            ttl_seconds,
        )
    }

    /// Delete a lock row from SQLite (called when a path's lock entry is fully released).
    pub fn remove_lock(&self, path: &Path) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .delete_lock(&path.to_string_lossy())
    }

    /// Load all persisted lock rows from SQLite (called on startup by `BorrowRegistry`).
    pub fn load_all_locks(&self) -> Result<Vec<LockRow>> {
        self.db.lock().unwrap().load_all_locks()
    }

    // ── Crash recovery ────────────────────────────────────────────────────

    /// Re-attach VFS mounts for sessions that were ACTIVE before a crash.
    pub async fn recover_active_sessions(
        &self,
        state: &crate::daemon::AppState,
    ) -> Result<()> {
        let active = self.list_active();
        if active.is_empty() {
            return Ok(());
        }
        warn!("{} active session(s) found from previous run — recovering", active.len());
        for session in active {
            match state.vfs.mount(&session).await {
                Ok(_) => info!(id = %session.session_id, "re-mounted session"),
                Err(e) => {
                    warn!(id = %session.session_id, err = %e, "failed to re-mount; releasing locks and marking stale");
                    state.borrows.release_session(session.session_id);
                    self.mark_stale(session.session_id)?;
                }
            }
        }
        Ok(())
    }
}

fn row_to_uuid(row: &SessionRow) -> Result<Uuid> {
    row.id.parse::<Uuid>().with_context(|| format!("parse session UUID: {}", row.id))
}

fn row_to_info(row: &SessionRow) -> Result<SessionInfo> {
    Ok(SessionInfo {
        session_id:    row.id.parse()?,
        task_id:       row.task_id.clone(),
        agent_label:   row.agent_label.clone(),
        base_commit:   row.base_commit.clone(),
        created_at:    chrono::DateTime::from_timestamp(row.created_at, 0)
            .unwrap_or_else(Utc::now),
        status:        match row.status.as_str() {
            "ACTIVE"    => SessionStatus::Active,
            "COMMITTED" => SessionStatus::Committed,
            "ABORTED"   => SessionStatus::Aborted,
            _            => SessionStatus::Stale,
        },
        mount_path:    PathBuf::from(&row.mount_path),
        branch_name:   row.branch_name.clone(),
        peers_enabled: row.peers_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::SessionManager;
    use crate::borrow::BorrowRegistry;
    use crate::config::{Config, VfsBackend};
    use crate::daemon::AppState;
    use crate::delta::DeltaStore;
    use crate::events::EventBroker;
    use crate::vfs::VfsManager;
    use simgit_sdk::SessionStatus;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let seq = TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "simgit-session-test-{}-{}-{}",
            std::process::id(),
            nanos,
            seq
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("state.db")
    }

    #[tokio::test]
    async fn session_manager_persists_active_session_across_reopen() {
        let db_path = temp_db_path();

        let created_session_id = {
            let manager = SessionManager::open(&db_path).await.expect("open manager");
            let info = manager
                .create(
                    "task-a".to_owned(),
                    Some("agent-a".to_owned()),
                    "HEAD".to_owned(),
                    std::env::temp_dir().join("simgit-mount-a"),
                    false,
                    8,
                )
                .expect("create session");
            info.session_id
        };

        let reopened = SessionManager::open(&db_path).await.expect("reopen manager");
        let active = reopened.list(Some(SessionStatus::Active));
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, created_session_id);
        assert_eq!(active[0].task_id, "task-a");

        let _ = std::fs::remove_file(&db_path);
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[tokio::test]
    async fn committed_status_persists_across_reopen() {
        let db_path = temp_db_path();

        let session_id = {
            let manager = SessionManager::open(&db_path).await.expect("open manager");
            let info = manager
                .create(
                    "task-b".to_owned(),
                    None,
                    "HEAD".to_owned(),
                    std::env::temp_dir().join("simgit-mount-b"),
                    false,
                    8,
                )
                .expect("create session");
            manager
                .mark_committed(info.session_id, "feat/test")
                .expect("mark committed");
            info.session_id
        };

        let reopened = SessionManager::open(&db_path).await.expect("reopen manager");
        let info = reopened.get(session_id).expect("session exists");
        assert_eq!(info.status, SessionStatus::Committed);
        assert_eq!(info.branch_name.as_deref(), Some("feat/test"));

        let _ = std::fs::remove_file(&db_path);
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[tokio::test]
    async fn recover_active_sessions_mounts_session_paths() {
        let db_path = temp_db_path();
        let state_root = db_path
            .parent()
            .expect("db has parent")
            .join("state-root");
        let mnt_dir = state_root.join("mnt");
        let repo_path = state_root.join("repo");
        std::fs::create_dir_all(&mnt_dir).expect("create mnt dir");
        std::fs::create_dir_all(&repo_path).expect("create repo dir");

        let cfg = Arc::new(Config {
            repo_path: repo_path.clone(),
            state_dir: state_root.clone(),
            mnt_dir: mnt_dir.clone(),
            max_sessions: 16,
            max_delta_bytes: 2 * 1024 * 1024,
            lock_ttl_seconds: 3600,
            vfs_backend: VfsBackend::NfsLoopback,
        });

        let sessions = Arc::new(SessionManager::open(&db_path).await.expect("open manager"));
        let borrows = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
        let deltas = Arc::new(DeltaStore::new(state_root.join("deltas")));
        let events = Arc::new(EventBroker::new());
        let vfs = Arc::new(VfsManager::new(
            Arc::clone(&cfg),
            Arc::clone(&deltas),
            Arc::clone(&borrows),
        ));
        let state = AppState {
            config: Arc::clone(&cfg),
            sessions: Arc::clone(&sessions),
            borrows,
            deltas,
            events,
            vfs,
        };

        let info = sessions
            .create(
                "recover-task".to_owned(),
                Some("recover-agent".to_owned()),
                "HEAD".to_owned(),
                mnt_dir.join("recover-session"),
                false,
                16,
            )
            .expect("create active session");

        assert!(!info.mount_path.exists(), "mount path should not exist before recovery mount");
        sessions
            .recover_active_sessions(&state)
            .await
            .expect("recover active sessions");
        assert!(info.mount_path.exists(), "recover should mount active session path");

        state.vfs.unmount_all().await;
        let _ = std::fs::remove_file(&db_path);
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// Verify that write locks persisted to SQLite are restored into the
    /// `BorrowRegistry` in-memory table when `restore_locks` is called.
    #[tokio::test]
    async fn borrow_locks_persist_and_restore_across_reopen() {
        let db_path = temp_db_path();
        let path = std::path::Path::new("/src/main.rs");

        let session_id = {
            let manager = Arc::new(SessionManager::open(&db_path).await.expect("open manager"));
            let registry = crate::borrow::BorrowRegistry::new(Arc::clone(&manager));

            let info = manager
                .create(
                    "lock-task".to_owned(),
                    None,
                    "HEAD".to_owned(),
                    std::env::temp_dir().join("simgit-lock-mount"),
                    false,
                    8,
                )
                .expect("create session");

            registry
                .acquire_write(info.session_id, path, Some(3600))
                .expect("acquire write lock");

            // Verify the lock is visible in-memory.
            assert!(!registry.is_write_free(path, uuid::Uuid::nil()));

            info.session_id
        };

        // Re-open the manager and restore locks — simulating a daemon restart.
        let manager2 = Arc::new(SessionManager::open(&db_path).await.expect("reopen manager"));
        let registry2 = crate::borrow::BorrowRegistry::new(Arc::clone(&manager2));

        // Before restore, the in-memory table should be empty.
        assert!(
            registry2.is_write_free(path, uuid::Uuid::nil()),
            "lock should not be visible before restore"
        );

        registry2.restore_locks().expect("restore locks");

        // After restore, the write lock must be re-enforced.
        assert!(
            !registry2.is_write_free(path, uuid::Uuid::nil()),
            "lock should be visible after restore"
        );

        // The original session still owns the lock.
        assert!(
            registry2.is_write_free(path, session_id),
            "original session should still be granted re-entrant access"
        );

        // Release and verify the lock disappears from SQLite.
        registry2.release_session(session_id);
        assert!(
            registry2.is_write_free(path, uuid::Uuid::nil()),
            "lock should be gone after release"
        );

        // Verify SQLite row is also removed.
        let rows = manager2.load_all_locks().expect("load locks after release");
        assert!(rows.is_empty(), "SQLite locks table should be empty after release");

        let _ = std::fs::remove_file(&db_path);
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
