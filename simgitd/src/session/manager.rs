//! Session lifecycle management.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

use simgit_sdk::{SessionInfo, SessionStatus};

use super::db::{Db, SessionRow};

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
                    warn!(id = %session.session_id, err = %e, "failed to re-mount; marking stale");
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
    use simgit_sdk::SessionStatus;
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
}
