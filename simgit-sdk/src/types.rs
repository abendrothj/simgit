use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

// ── Session ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionStatus {
    Active,
    Committed,
    Aborted,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id:  Uuid,
    pub task_id:     String,
    pub agent_label: Option<String>,
    pub base_commit: String,
    pub created_at:  DateTime<Utc>,
    pub status:      SessionStatus,
    pub mount_path:  PathBuf,
    pub branch_name: Option<String>,
    /// Whether this session opted in to peer visibility (--peers flag).
    pub peers_enabled: bool,
}

// ── Locks ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub path:             PathBuf,
    /// None if no exclusive writer holds this path.
    pub writer_session:   Option<Uuid>,
    pub reader_sessions:  Vec<Uuid>,
    pub acquired_at:      DateTime<Utc>,
    /// None means no TTL (held until session ends).
    pub ttl_seconds:      Option<u64>,
}

// ── Borrow error (returned when a write lock cannot be acquired) ──────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BorrowError {
    pub path:        PathBuf,
    pub holder:      SessionInfo,
    pub acquired_at: DateTime<Utc>,
    pub ttl:         Option<std::time::Duration>,
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

// ── Delta / diff ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResult {
    pub session_id:    Uuid,
    pub unified_diff:  String,
    pub changed_paths: Vec<PathBuf>,
}

// ── RPC request / response envelopes ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcRequest {
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
