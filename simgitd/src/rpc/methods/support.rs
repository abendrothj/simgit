//! Small helpers shared across multiple RPC method submodules.

use simgit_sdk::{RpcError, ERR_SESSION_NOT_FOUND};
use uuid::Uuid;

pub(super) fn str_field(p: &serde_json::Value, key: &str) -> Result<String, RpcError> {
    p[key].as_str().map(str::to_owned).ok_or_else(|| RpcError {
        code: -32602,
        message: format!("missing required param: {key}"),
        data: None,
    })
}

pub(super) fn uuid_field(p: &serde_json::Value, key: &str) -> Result<Uuid, RpcError> {
    let s = str_field(p, key)?;
    s.parse::<Uuid>().map_err(|_| RpcError {
        code: -32602,
        message: format!("invalid UUID for param: {key}"),
        data: None,
    })
}

pub(super) fn optional_uuid_field(
    p: &serde_json::Value,
    key: &str,
) -> Result<Option<Uuid>, RpcError> {
    match p.get(key).and_then(|value| value.as_str()) {
        Some(value) => value.parse::<Uuid>().map(Some).map_err(|_| RpcError {
            code: -32602,
            message: format!("invalid UUID for param: {key}"),
            data: None,
        }),
        None => Ok(None),
    }
}

pub(super) fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: -32603,
        message: e.to_string(),
        data: None,
    }
}

pub(super) fn not_found(session_id: Uuid) -> RpcError {
    RpcError {
        code: ERR_SESSION_NOT_FOUND,
        message: format!("session not found: {session_id}"),
        data: None,
    }
}

pub(super) fn now_epoch_ms() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(now.as_millis()).unwrap_or(i64::MAX)
}

pub(super) fn resolve_head(repo: &std::path::Path) -> String {
    gix::open(repo)
        .ok()
        .and_then(|r| r.head_id().map(|id| id.to_string()).ok())
        .unwrap_or_else(|| "HEAD".to_owned())
}
