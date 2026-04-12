//! Session lifecycle management — handles creation, persistence, and cleanup.
//!
//! # Overview
//!
//! A **session** is a lightweight context bound to one agent's work. It carries:
//! - A unique UUID (v7, sortable by timestamp)
//! - Task ID (human-readable identifier)
//! - Status (ACTIVE, COMMITTED, ABORTED, STALE)
//! - Delta layer (CoW writes)
//! - Borrow locks (write-exclusive)
//! - SQLite persistence (survives restarts)
//!
//! # Lifecycle
//!
//! ```text
//! CREATE       →   ACTIVE      →   COMMIT/ABORT
//!  (microsec)      (minutes)        (atomic)
//!                    ↓
//!              (VFS mounts)
//!              (agent reads/writes)
//!              (borrow locks acquired)
//!                    ↓
//!              RELEASE LOCKS
//!              FLATTEN DELTA
//!              UPDATE STATUS
//! ```
//!
//! # Storage
//!
//! Sessions are stored in SQLite:
//! ```sql
//! CREATE TABLE sessions (
//!    session_id TEXT PRIMARY KEY,
//!    task_id TEXT,
//!    status TEXT,
//!    base_commit TEXT,
//!    created_at TIMESTAMP,
//!    branch_name TEXT,
//!    peers_enabled BOOL
//! );
//! ```
//!
//! Plus a persistent lock table:
//! ```sql
//! CREATE TABLE locks (
//!    path TEXT,
//!    session_id TEXT,
//!    is_writer BOOL,
//!    acquired_at TIMESTAMP
//! );
//! ```
//!
//! # Recovery
//!
//! On daemon startup:
//! 1. Query all ACTIVE sessions from SQLite
//! 2. If session's locks have expired (TTL > 30s), mark as STALE
//! 3. If session metadata is corrupt, discard
//! 4. Resume normal operation
//!
//! # Example
//!
//! ```ignore
//! let manager = SessionManager::new(db_path, delta_root, ttl_seconds)?;
//!
//! // Create a new session
//! let session = manager.create(
//!     "implement-feature-x",
//!     Some("copilot-agent-5"),
//!     "abc123def456", // base commit
//! )?;
//! // Returns SessionInfo with session_id, mount_path, etc.
//!
//! // Later: commit or abort
//! manager.mark_committed(session.session_id, "feature-x-branch")?;
//! // Session now persists in git
//! ```

pub mod db;
pub mod manager;

pub use manager::SessionManager;
