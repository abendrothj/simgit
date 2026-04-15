//! SQLite-backed session persistence.
//!
//! Schema is created on first open. WAL mode is enabled for concurrent reads.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

// ── Schema ────────────────────────────────────────────────────────────────────

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sessions (
    id           TEXT    PRIMARY KEY,
    task_id      TEXT    NOT NULL,
    agent_label  TEXT,
    base_commit  TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,   -- Unix timestamp (seconds)
    status       TEXT    NOT NULL DEFAULT 'ACTIVE',
    mount_path   TEXT    NOT NULL,
    branch_name  TEXT,
    peers_enabled INTEGER NOT NULL DEFAULT 0,
    last_commit_request_id TEXT,
    last_commit_state      TEXT,
    last_commit_result     TEXT,
    last_commit_error      TEXT
);

CREATE TABLE IF NOT EXISTS locks (
    path           TEXT    PRIMARY KEY,
    writer_session TEXT,
    reader_sessions TEXT   NOT NULL DEFAULT '[]',
    acquired_at    INTEGER NOT NULL,
    ttl_seconds    INTEGER          -- NULL = no TTL
);

CREATE TABLE IF NOT EXISTS read_log (
    session_id  TEXT    NOT NULL,
    path        TEXT    NOT NULL,
    accessed_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS read_log_session ON read_log(session_id);
CREATE INDEX IF NOT EXISTS read_log_path    ON read_log(path);
"#;

// ── Database handle ───────────────────────────────────────────────────────────

/// A single-writer connection used by `SessionManager`.
pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open SQLite at {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("apply schema")?;
        ensure_session_commit_columns(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory SQLite database. Used by unit tests only.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory SQLite")?;
        conn.execute_batch(SCHEMA).context("apply schema")?;
        ensure_session_commit_columns(&conn)?;
        Ok(Self { conn })
    }

    /// Upsert a session row.
    pub fn upsert_session(
        &self,
        id:           &str,
        task_id:      &str,
        agent_label:  Option<&str>,
        base_commit:  &str,
        created_at:   i64,
        status:       &str,
        mount_path:   &str,
        branch_name:  Option<&str>,
        peers_enabled: bool,
    ) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO sessions
               (id, task_id, agent_label, base_commit, created_at, status, mount_path, branch_name, peers_enabled)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
               ON CONFLICT(id) DO UPDATE SET
                 status       = excluded.status,
                 mount_path   = excluded.mount_path,
                 branch_name  = excluded.branch_name,
                 peers_enabled = excluded.peers_enabled"#,
            params![
                id, task_id, agent_label, base_commit, created_at, status,
                mount_path, branch_name, peers_enabled as i64,
            ],
        )?;
        Ok(())
    }

    /// Update only the status (and optionally the branch_name) of a session.
    pub fn update_status(
        &self,
        id:          &str,
        status:      &str,
        branch_name: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status=?1, branch_name=COALESCE(?2, branch_name) WHERE id=?3",
            params![status, branch_name, id],
        )?;
        Ok(())
    }

    /// Load all sessions with a given status.
    pub fn load_by_status(&self, status: &str) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,task_id,agent_label,base_commit,created_at,status,mount_path,branch_name,peers_enabled
             FROM sessions WHERE status=?1",
        )?;
        let rows = stmt.query_map(params![status], row_to_session)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Load a single session by id.
    pub fn load(&self, id: &str) -> Result<Option<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,task_id,agent_label,base_commit,created_at,status,mount_path,branch_name,peers_enabled
             FROM sessions WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_session)?;
        Ok(rows.next().transpose()?)
    }

    /// Load all sessions (used for `session.list`).
    pub fn load_all(&self) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,task_id,agent_label,base_commit,created_at,status,mount_path,branch_name,peers_enabled
             FROM sessions ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_session)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Append a read_log entry.
    pub fn log_read(&self, session_id: &str, path: &str, accessed_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO read_log(session_id,path,accessed_at) VALUES(?1,?2,?3)",
            params![session_id, path, accessed_at],
        )?;
        Ok(())
    }

    /// Upsert a lock row (called on every `acquire_read` / `acquire_write`).
    ///
    /// `reader_sessions_json` must be a JSON array of UUID strings, e.g. `["uuid1","uuid2"]`.
    pub fn upsert_lock(
        &self,
        path: &str,
        writer_session: Option<&str>,
        reader_sessions_json: &str,
        acquired_at: i64,
        ttl_seconds: Option<u64>,
    ) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO locks (path, writer_session, reader_sessions, acquired_at, ttl_seconds)
               VALUES (?1, ?2, ?3, ?4, ?5)
               ON CONFLICT(path) DO UPDATE SET
                 writer_session  = excluded.writer_session,
                 reader_sessions = excluded.reader_sessions,
                 acquired_at     = excluded.acquired_at,
                 ttl_seconds     = excluded.ttl_seconds"#,
            params![
                path,
                writer_session,
                reader_sessions_json,
                acquired_at,
                ttl_seconds.map(|t| t as i64),
            ],
        )?;
        Ok(())
    }

    /// Delete a lock row for `path` (called when the entry becomes empty after release).
    pub fn delete_lock(&self, path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM locks WHERE path=?1", params![path])?;
        Ok(())
    }

    /// Load all persisted lock rows (called on daemon startup to restore in-memory state).
    pub fn load_all_locks(&self) -> Result<Vec<LockRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, writer_session, reader_sessions, acquired_at, ttl_seconds FROM locks",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(LockRow {
                    path:                 row.get(0)?,
                    writer_session:       row.get(1)?,
                    reader_sessions_json: row.get(2)?,
                    acquired_at:          row.get(3)?,
                    ttl_seconds:          row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_commit_request(
        &self,
        session_id: &str,
        request_id: &str,
        state: &str,
        result_json: Option<&str>,
        error_json: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions
             SET last_commit_request_id=?1,
                 last_commit_state=?2,
                 last_commit_result=?3,
                 last_commit_error=?4
             WHERE id=?5",
            params![request_id, state, result_json, error_json, session_id],
        )?;
        Ok(())
    }

    pub fn load_commit_request(&self, session_id: &str) -> Result<Option<CommitRequestRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT last_commit_request_id, last_commit_state, last_commit_result, last_commit_error
             FROM sessions WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(params![session_id], |row| {
            Ok(CommitRequestRow {
                request_id: row.get(0)?,
                state: row.get(1)?,
                result_json: row.get(2)?,
                error_json: row.get(3)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }
}

fn ensure_session_commit_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let required = [
        ("last_commit_request_id", "ALTER TABLE sessions ADD COLUMN last_commit_request_id TEXT"),
        ("last_commit_state", "ALTER TABLE sessions ADD COLUMN last_commit_state TEXT"),
        ("last_commit_result", "ALTER TABLE sessions ADD COLUMN last_commit_result TEXT"),
        ("last_commit_error", "ALTER TABLE sessions ADD COLUMN last_commit_error TEXT"),
    ];

    for (name, ddl) in required {
        if !rows.iter().any(|existing| existing == name) {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

// ── Raw DB row ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id:           String,
    pub task_id:      String,
    pub agent_label:  Option<String>,
    pub base_commit:  String,
    pub created_at:   i64,
    pub status:       String,
    pub mount_path:   String,
    pub branch_name:  Option<String>,
    pub peers_enabled: bool,
}

fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id:           row.get(0)?,
        task_id:      row.get(1)?,
        agent_label:  row.get(2)?,
        base_commit:  row.get(3)?,
        created_at:   row.get(4)?,
        status:       row.get(5)?,
        mount_path:   row.get(6)?,
        branch_name:  row.get(7)?,
        peers_enabled: row.get::<_, i64>(8)? != 0,
    })
}

/// A single row from the `locks` table.
#[derive(Debug, Clone)]
pub struct LockRow {
    pub path:                String,
    pub writer_session:      Option<String>,
    /// Raw JSON string from `reader_sessions` column (e.g. `["uuid1","uuid2"]`).
    /// Callers should parse this with serde_json and handle errors explicitly.
    pub reader_sessions_json: String,
    pub acquired_at:          i64,
    pub ttl_seconds:          Option<u64>,
}

#[derive(Debug, Clone)]
pub struct CommitRequestRow {
    pub request_id: Option<String>,
    pub state: Option<String>,
    pub result_json: Option<String>,
    pub error_json: Option<String>,
}