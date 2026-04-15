//! Public types for the simgit SDK.
//!
//! These types are shared between the daemon and clients (CLI, agents).
//! All types are serializable for transmission over JSON-RPC.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

// ── Session ──────────────────────────────────────────────────────────────────

/// Lifecycle state of a session.
///
/// ## Transitions
/// - `ACTIVE` → `COMMITTED` (via `session.commit()`)
/// - `ACTIVE` → `ABORTED` (via `session.abort()`)
/// - `ACTIVE` → `STALE` (daemon timeout or crash recovery)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionStatus {
    /// Session is actively being written to by an agent.
    Active,
    /// Session was committed; delta flattened to a git branch.
    Committed,
    /// Session was aborted; all changes discarded.
    Aborted,
    /// Session exceeded TTL or daemon crashed and didn't recover it.
    Stale,
}

/// Metadata for a single session.
///
/// A session is a lightweight context for one agent's work on the codebase.
/// It carries its own delta layer (CoW writes) and borrow locks.
///
/// ## Example
/// ```
/// use uuid::Uuid;
/// use std::path::PathBuf;
/// use chrono::Utc;
/// use simgit_sdk::{SessionInfo, SessionStatus};
///
/// let session = SessionInfo {
///     session_id: Uuid::now_v7(),
///     task_id: "implement-feature-x".into(),
///     agent_label: Some("copilot-agent-5".into()),
///     base_commit: "abc123def456".into(),
///     created_at: Utc::now(),
///     status: SessionStatus::Active,
///     mount_path: PathBuf::from("/vdev/session-id"),
///     branch_name: None,
///     peers_enabled: false,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique identifier (UUIDv7) for this session.
    pub session_id: Uuid,
    /// Human-readable task identifier (e.g., "implement-feature-x").
    pub task_id: String,
    /// Optional agent label (e.g., "copilot-agent-5", "ci-bot-3").
    pub agent_label: Option<String>,
    /// Git commit SHA that this session started from.
    pub base_commit: String,
    /// Timestamp when the session was created.
    pub created_at: DateTime<Utc>,
    /// Current lifecycle state.
    pub status: SessionStatus,
    /// Mount path where the session's delta layer is FUSE/NFS mounted.
    pub mount_path: PathBuf,
    /// If committed, the git branch that was created or updated.
    pub branch_name: Option<String>,
    /// Whether this session opted into peer visibility (`--peers` flag).
    /// When true, agents can see in-flight writes from sibling sessions.
    pub peers_enabled: bool,
}

/// Per-commit telemetry emitted by `session.commit`.
///
/// All durations are in milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitTelemetry {
    pub total_duration_ms: f64,
    pub capture_self_queue_wait_ms: f64,
    pub capture_self_ms: f64,
    pub capture_peers_queue_wait_ms: f64,
    pub capture_peers_execution_ms: f64,
    pub capture_peers_ms: f64,
    pub scheduler_queue_wait_ms: f64,
    pub conflict_scan_ms: f64,
    pub flatten_queue_wait_ms: f64,
    pub flatten_ms: f64,
    pub flatten_write_tree_ms: f64,
    pub flatten_apply_manifest_ms: f64,
    pub flatten_ref_update_ms: f64,
    pub flatten_commit_object_ms: f64,
    pub retry_count: u32,
}

/// Return shape for `session.commit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCommitResult {
    #[serde(flatten)]
    pub session: SessionInfo,
    #[serde(default)]
    pub telemetry: CommitTelemetry,
}

// ── Merge conflicts ──────────────────────────────────────────────────────────

/// Detailed operation overlap for a single conflicting path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergePathConflict {
    pub path: PathBuf,
    pub ours_ops: Vec<String>,
    pub peer_ops: Vec<String>,
}

/// Conflict details for one active peer session that blocked our commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeConflictPeer {
    pub session_id: Uuid,
    pub task_id: String,
    pub paths: Vec<PathBuf>,
    pub path_conflicts: Vec<MergePathConflict>,
}

/// Structured payload returned by the daemon for pre-commit overlap conflicts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeConflictDetail {
    pub session_id: Uuid,
    pub kind: String,
    pub conflicts: Vec<MergeConflictPeer>,
}

impl MergeConflictDetail {
    pub fn all_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for conflict in &self.conflicts {
            for path in &conflict.paths {
                if !paths.iter().any(|p| p == path) {
                    paths.push(path.clone());
                }
            }
        }
        paths
    }
}

impl fmt::Display for MergeConflictDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let paths = self
            .all_paths()
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let peers = self
            .conflicts
            .iter()
            .map(|conflict| format!("{} ({})", conflict.session_id, conflict.task_id))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "merge conflict for session {} on paths [{}] against active session(s): {}",
            self.session_id,
            paths,
            peers
        )
    }
}

// ── Locks ─────────────────────────────────────────────────────────────────────

/// Information about a lock on a filesystem path.
///
/// Each path can have at most one writer (exclusive) and zero or more readers.
/// This mirrors Rust's borrow checker semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    /// The filesystem path being locked.
    pub path: PathBuf,
    /// Session ID of the exclusive writer (`None` if no writer holds it).
    pub writer_session: Option<Uuid>,
    /// List of sessions that hold shared read locks on this path.
    pub reader_sessions: Vec<Uuid>,
    /// Timestamp when the lock was acquired.
    pub acquired_at: DateTime<Utc>,
    /// TTL in seconds after which the lock is automatically released.
    /// `None` means the lock persists until the session ends.
    pub ttl_seconds: Option<u64>,
}

/// Event emitted by the daemon event broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEvent {
    /// Session that emitted the event.
    pub source_session: Uuid,
    /// Event kind (`lock_conflict`, `peer_commit`, etc.).
    pub kind: String,
    /// Event-specific payload.
    pub payload: serde_json::Value,
    /// Event timestamp in UTC.
    pub emitted_at: DateTime<Utc>,
}

// ── Borrow error ──────────────────────────────────────────────────────────────

/// Error returned when a session cannot acquire write access to a path.
///
/// This occurs when another session already holds the exclusive write lock.
/// The error includes the holder's session metadata so the client can wait
/// or take alternative action.
///
/// ## Example
/// ```ignore
/// match acquire_write(session_id, "/src/main.rs") {
///     Ok(()) => println!("Lock acquired"),
///     Err(e) => {
///         eprintln!("Cannot write: {}", e);
///         eprintln!("Held by: {} (task: {})", e.holder.session_id, e.holder.task_id);
///         // Optionally wait or try a different path
///     }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BorrowError {
    /// The path that cannot be written.
    pub path: PathBuf,
    /// Metadata about the session holding the write lock.
    pub holder: SessionInfo,
    /// When the lock was acquired.
    pub acquired_at: DateTime<Utc>,
    /// How long until the lock is automatically released (if any).
    pub ttl: Option<std::time::Duration>,
}

impl std::fmt::Display for BorrowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BorrowError: '{}' is exclusively held by session {} (task: {}) since {}",
            self.path.display(),
            self.holder.session_id,
            self.holder.task_id,
            self.acquired_at.format("%Y-%m-%dT%H:%M:%SZ"),
        )
    }
}

impl std::error::Error for BorrowError {}

// ── Delta / diff ──────────────────────────────────────────────────────────────

/// Result of diffing a session's delta layer against HEAD.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResult {
    /// Session that was diffed.
    pub session_id: Uuid,
    /// Unified diff format (suitable for display or patching).
    pub unified_diff: String,
    /// List of paths that were changed (created, modified, or deleted).
    pub changed_paths: Vec<PathBuf>,
}

// ── RPC request / response envelopes ─────────────────────────────────────────

/// JSON-RPC 2.0 request envelope.
///
/// Used for all daemon→client communication over the Unix socket.
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    /// Always "2.0" for JSON-RPC 2.0 compliance.
    pub jsonrpc: String,
    pub id:      u64,
    pub method:  String,
    pub params:  serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id:      u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result:  Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error:   Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code:    i32,
    pub message: String,
    pub data:    Option<serde_json::Value>,
}

// ── RPC error codes ───────────────────────────────────────────────────────────

pub const ERR_BORROW_CONFLICT:   i32 = -32001;
pub const ERR_SESSION_NOT_FOUND: i32 = -32002;
pub const ERR_MERGE_CONFLICT:    i32 = -32003;
pub const ERR_QUOTA_EXCEEDED:    i32 = -32004;
pub const ERR_INVALID_PATH:      i32 = -32005;
