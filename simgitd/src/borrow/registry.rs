//! Borrow Registry — enforces Rust-like ownership over filesystem paths.
//!
//! Rules (mirroring Rust's borrow checker):
//!   - Multiple readers of a path are always allowed.
//!   - At most ONE exclusive writer per path at any time.
//!   - While a writer holds a lock, other agents trying to write the same
//!     path receive a `BorrowError` immediately (no blocking by default).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::Utc;
use serde_json;
use tracing::warn;
use uuid::Uuid;

use simgit_sdk::{BorrowError, LockInfo, SessionInfo};

use crate::session::SessionManager;

#[derive(Debug, Default, Clone)]
struct LockEntry {
    writer:  Option<Uuid>,
    readers: HashSet<Uuid>,
    acquired_at: chrono::DateTime<Utc>,
    ttl_seconds: Option<u64>,
}

pub struct BorrowRegistry {
    sessions: Arc<SessionManager>,
    /// In-memory lock table. The authoritative copy is also persisted to SQLite
    /// so it survives daemon restarts. This in-memory map is the fast path.
    locks: Mutex<HashMap<PathBuf, LockEntry>>,
}

impl BorrowRegistry {
    pub fn new(sessions: Arc<SessionManager>) -> Self {
        Self { sessions, locks: Mutex::new(HashMap::new()) }
    }

    // ── Read acquisition ──────────────────────────────────────────────────

    /// Record that `session` is reading `path`. Always succeeds.
    /// Readers see the pre-mutation baseline even if a writer holds the path.
    pub fn acquire_read(&self, session_id: Uuid, path: &Path) {
        let (writer, readers, acquired_at, ttl_seconds) = {
            let mut locks = self.locks.lock().unwrap();
            let entry = locks
                .entry(path.to_owned())
                .or_insert_with(|| LockEntry { acquired_at: Utc::now(), ..Default::default() });
            entry.readers.insert(session_id);
            (entry.writer, entry.readers.clone(), entry.acquired_at, entry.ttl_seconds)
        };
        if let Err(e) = self.sessions.persist_lock(path, writer, &readers, acquired_at, ttl_seconds) {
            warn!(path = %path.display(), err = %e, "failed to persist read lock to SQLite");
        }
    }

    // ── Write acquisition ─────────────────────────────────────────────────

    /// Attempt to acquire exclusive write ownership of `path` for `session`.
    ///
    /// Returns `Ok(())` if granted, or `Err(BorrowError)` if another session
    /// already holds the write lock.
    pub fn acquire_write(
        &self,
        session_id: Uuid,
        path: &Path,
        ttl_seconds: Option<u64>,
    ) -> Result<(), BorrowError> {
        // Separate enum so we can drop the MutexGuard before calling into SessionManager.
        enum Outcome {
            Granted { readers: HashSet<Uuid>, acquired_at: chrono::DateTime<Utc>, ttl: Option<u64> },
            Reentrant,
            Conflict(BorrowError),
        }

        let outcome = {
            let mut locks = self.locks.lock().unwrap();
            let entry = locks
                .entry(path.to_owned())
                .or_insert_with(|| LockEntry { acquired_at: Utc::now(), ..Default::default() });

            match &entry.writer {
                None => {
                    entry.writer       = Some(session_id);
                    entry.acquired_at  = Utc::now();
                    entry.ttl_seconds  = ttl_seconds;
                    Outcome::Granted {
                        readers:     entry.readers.clone(),
                        acquired_at: entry.acquired_at,
                        ttl:         ttl_seconds,
                    }
                }
                Some(w) if *w == session_id => {
                    // Re-entrant: same session already owns it.
                    Outcome::Reentrant
                }
                Some(holder_id) => {
                    // Conflict — build a structured error.
                    let holder_id   = *holder_id;
                    let acquired_at = entry.acquired_at;
                    let ttl         = entry.ttl_seconds.map(std::time::Duration::from_secs);
                    let holder = self.sessions.get_info_blocking(holder_id).unwrap_or_else(|| {
                        SessionInfo {
                            session_id:    holder_id,
                            task_id:       "<unknown>".into(),
                            agent_label:   None,
                            base_commit:   String::new(),
                            created_at:    Utc::now(),
                            status:        simgit_sdk::SessionStatus::Active,
                            mount_path:    PathBuf::new(),
                            branch_name:   None,
                            peers_enabled: false,
                        }
                    });
                    Outcome::Conflict(BorrowError { path: path.to_owned(), holder, acquired_at, ttl })
                }
            }
        }; // MutexGuard on self.locks dropped here

        match outcome {
            Outcome::Granted { readers, acquired_at, ttl } => {
                if let Err(e) = self.sessions.persist_lock(
                    path, Some(session_id), &readers, acquired_at, ttl,
                ) {
                    warn!(path = %path.display(), err = %e, "failed to persist write lock to SQLite");
                }
                Ok(())
            }
            Outcome::Reentrant => Ok(()),
            Outcome::Conflict(err) => Err(err),
        }
    }

    // ── Release ───────────────────────────────────────────────────────────

    /// Release ALL locks (read and write) held by `session`.
    /// Called automatically at session commit or abort.
    pub fn release_session(&self, session_id: Uuid) {
        // Phase 1: update in-memory state and collect what changed.
        let (to_delete, to_update): (Vec<PathBuf>, Vec<(PathBuf, LockEntry)>) = {
            let mut locks = self.locks.lock().unwrap();

            let affected: Vec<PathBuf> = locks
                .iter()
                .filter(|(_, e)| {
                    e.writer == Some(session_id) || e.readers.contains(&session_id)
                })
                .map(|(p, _)| p.clone())
                .collect();

            locks.retain(|_, entry| {
                entry.readers.remove(&session_id);
                if entry.writer == Some(session_id) {
                    entry.writer = None;
                }
                // Drop entries that are now completely empty.
                entry.writer.is_some() || !entry.readers.is_empty()
            });

            let mut to_delete = Vec::new();
            let mut to_update = Vec::new();
            for path in affected {
                match locks.get(&path) {
                    None        => to_delete.push(path),
                    Some(entry) => to_update.push((path, entry.clone())),
                }
            }
            (to_delete, to_update)
        }; // MutexGuard on self.locks dropped here

        // Phase 2: sync changes to SQLite.
        for path in &to_delete {
            if let Err(e) = self.sessions.remove_lock(path) {
                warn!(path = %path.display(), err = %e, "failed to remove lock from SQLite");
            }
        }
        for (path, entry) in &to_update {
            if let Err(e) = self.sessions.persist_lock(
                path,
                entry.writer,
                &entry.readers,
                entry.acquired_at,
                entry.ttl_seconds,
            ) {
                warn!(path = %path.display(), err = %e, "failed to update lock in SQLite");
            }
        }
    }

    // ── Startup recovery ──────────────────────────────────────────────────

    /// Restore the in-memory lock table from SQLite.
    ///
    /// Must be called once during daemon startup, after `BorrowRegistry::new` and
    /// before any lock operations, so that write locks held at the time of a
    /// previous crash are re-enforced immediately.
    pub fn restore_locks(&self) -> Result<()> {
        let rows = self.sessions.load_all_locks()?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut locks = self.locks.lock().unwrap();
        for row in &rows {
            let path = PathBuf::from(&row.path);
            let writer = row.writer_session.as_deref().and_then(|s| s.parse::<Uuid>().ok());
            let readers: HashSet<Uuid> = match serde_json::from_str::<Vec<String>>(&row.reader_sessions_json) {
                Ok(ids) => ids.iter().filter_map(|s| s.parse::<Uuid>().ok()).collect(),
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        err = %e,
                        "malformed reader_sessions JSON in SQLite locks table — skipping readers for this path"
                    );
                    HashSet::new()
                }
            };
            if writer.is_none() && readers.is_empty() {
                continue;
            }
            let acquired_at = match chrono::DateTime::from_timestamp(row.acquired_at, 0) {
                Some(ts) => ts,
                None => {
                    warn!(
                        path = %path.display(),
                        acquired_at = row.acquired_at,
                        "invalid acquired_at timestamp in SQLite locks table — using current time"
                    );
                    Utc::now()
                }
            };
            locks.insert(
                path,
                LockEntry { writer, readers, acquired_at, ttl_seconds: row.ttl_seconds },
            );
        }
        tracing::info!("restored {} borrow lock(s) from SQLite", locks.len());
        Ok(())
    }

    // ── Observability ─────────────────────────────────────────────────────

    /// Return all current locks, optionally filtered by path prefix.
    pub fn list(&self, path_prefix: Option<&Path>) -> Vec<LockInfo> {
        let locks = self.locks.lock().unwrap();
        locks
            .iter()
            .filter(|(p, _)| {
                path_prefix.map_or(true, |prefix| p.starts_with(prefix))
            })
            .map(|(p, e)| LockInfo {
                path:            p.clone(),
                writer_session:  e.writer,
                reader_sessions: e.readers.iter().copied().collect(),
                acquired_at:     e.acquired_at,
                ttl_seconds:     e.ttl_seconds,
            })
            .collect()
    }

    /// Returns `true` if `path` is free to write (no other session holds it).
    pub fn is_write_free(&self, path: &Path, caller: Uuid) -> bool {
        let locks = self.locks.lock().unwrap();
        locks.get(path).map_or(true, |e| {
            e.writer.is_none() || e.writer == Some(caller)
        })
    }

    /// Force-release all locks held by `session` (used by TTL sweeper).
    pub fn force_release(&self, session_id: Uuid) {
        self.release_session(session_id);
    }

    /// Force-release all locks held by `session` and mark the session as STALE.
    ///
    /// Called by the TTL sweeper when a session's write lock has exceeded its TTL.
    /// Combines lock release with a status transition so the session does not remain
    /// visible as ACTIVE after its locks have been forcibly revoked.
    pub fn force_release_and_mark_stale(&self, session_id: Uuid) {
        self.release_session(session_id);
        if let Err(e) = self.sessions.mark_stale(session_id) {
            warn!(
                session = %session_id,
                err = %e,
                "TTL sweeper: failed to mark expired session as STALE"
            );
        }
    }

    /// Iterate over (path, writer_session, acquired_at, ttl_seconds) for
    /// TTL enforcement.
    pub fn stale_writers(&self) -> Vec<(PathBuf, Uuid, chrono::DateTime<Utc>, u64)> {
        let locks = self.locks.lock().unwrap();
        let now = Utc::now();
        locks
            .iter()
            .filter_map(|(path, entry)| {
                let writer = entry.writer?;
                let ttl = entry.ttl_seconds?;
                if ttl == 0 {
                    return None;
                }
                let age = (now - entry.acquired_at).num_seconds() as u64;
                if age >= ttl {
                    Some((path.clone(), writer, entry.acquired_at, ttl))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Return sessions currently marked ACTIVE.
    pub fn active_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.list_active()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;

    /// Helper to generate deterministic test UUIDs.
    fn test_uuid(idx: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[0] = idx;
        Uuid::from_bytes(bytes)
    }

    /// Create a `BorrowRegistry` backed by an in-memory SQLite database.
    fn mk_registry() -> BorrowRegistry {
        BorrowRegistry::new(SessionManager::for_testing())
    }

    // ── Core exclusive-write invariant ────────────────────────────────────

    #[test]
    fn exclusive_write_blocks_second_writer() {
        let reg = mk_registry();
        let s1 = test_uuid(1);
        let s2 = test_uuid(2);
        let path = Path::new("/src/main.rs");

        reg.acquire_write(s1, path, None).expect("s1 should acquire write lock");

        let err = reg
            .acquire_write(s2, path, None)
            .expect_err("s2 should be blocked by s1's write lock");

        assert_eq!(err.holder.session_id, s1, "error must name the actual holder");
        assert_eq!(err.path, path);
    }

    #[test]
    fn writer_reentrant_succeeds() {
        let reg = mk_registry();
        let s1 = test_uuid(1);
        let path = Path::new("/src/lib.rs");

        reg.acquire_write(s1, path, None).expect("first acquire");
        reg.acquire_write(s1, path, None).expect("re-entrant acquire must succeed");
    }

    // ── Readers ───────────────────────────────────────────────────────────

    #[test]
    fn multiple_readers_coexist_on_same_path() {
        let reg = mk_registry();
        let r1 = test_uuid(1);
        let r2 = test_uuid(2);
        let r3 = test_uuid(3);
        let path = Path::new("/docs/README.md");

        reg.acquire_read(r1, path);
        reg.acquire_read(r2, path);
        reg.acquire_read(r3, path);

        let locks = reg.list(None);
        let lock = locks.iter().find(|l| l.path == path).expect("lock entry must exist");
        assert!(lock.writer_session.is_none(), "no writer should exist");
        assert!(lock.reader_sessions.contains(&r1));
        assert!(lock.reader_sessions.contains(&r2));
        assert!(lock.reader_sessions.contains(&r3));
    }

    // ── Release ───────────────────────────────────────────────────────────

    #[test]
    fn release_session_frees_write_lock_for_next_writer() {
        let reg = mk_registry();
        let s1 = test_uuid(1);
        let s2 = test_uuid(2);
        let path = Path::new("/src/handler.rs");

        reg.acquire_write(s1, path, None).expect("s1 acquires write lock");
        assert!(!reg.is_write_free(path, s2), "path must be locked after s1 acquires");

        reg.release_session(s1);
        assert!(reg.is_write_free(path, s2), "path must be free after s1 releases");

        reg.acquire_write(s2, path, None).expect("s2 must be able to acquire after s1 releases");
    }

    #[test]
    fn release_session_removes_reader_from_lock_entry() {
        let reg = mk_registry();
        let r1 = test_uuid(1);
        let r2 = test_uuid(2);
        let path = Path::new("/api/routes.rs");

        reg.acquire_read(r1, path);
        reg.acquire_read(r2, path);
        reg.release_session(r1);

        let locks = reg.list(None);
        let lock = locks.iter().find(|l| l.path == path).expect("lock entry must still exist for r2");
        assert!(!lock.reader_sessions.contains(&r1), "r1 must be removed after release");
        assert!(lock.reader_sessions.contains(&r2), "r2 must still be present");
    }

    #[test]
    fn release_all_sessions_removes_lock_entry_entirely() {
        let reg = mk_registry();
        let s1 = test_uuid(1);
        let path = Path::new("/src/only_writer.rs");

        reg.acquire_write(s1, path, None).expect("acquire");
        reg.release_session(s1);

        let locks = reg.list(None);
        assert!(
            locks.iter().all(|l| l.path != path),
            "lock entry must be removed when no readers or writers remain"
        );
    }

    // ── TTL ───────────────────────────────────────────────────────────────

    #[test]
    fn lock_entry_default_is_empty() {
        let entry = LockEntry::default();
        assert!(entry.writer.is_none());
        assert!(entry.readers.is_empty());
        assert!(entry.ttl_seconds.is_none());
    }
}
