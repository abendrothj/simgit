use thiserror::Error;
use crate::types::{BorrowError, MergeConflictDetail};

#[derive(Debug, Error)]
pub enum SdkError {
    #[error("daemon not running or socket not found at {0}")]
    DaemonNotFound(String),

    #[error("I/O error communicating with daemon: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("RPC error {code}: {message}")]
    Rpc { code: i32, message: String },

    #[error("{0}")]
    BorrowConflict(BorrowError),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("{0}")]
    MergeConflict(MergeConflictDetail),

    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
}
