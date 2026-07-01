//! `event.list` and `event.subscribe`.

use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::RpcError;

use crate::daemon::AppState;

use super::support::internal;

// ── event.list ───────────────────────────────────────────────────────────────

pub(super) async fn event_list(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let session_id = p
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| {
            Uuid::parse_str(s).map_err(|e| RpcError {
                code: -32602,
                message: format!("invalid session_id: {e}"),
                data: None,
            })
        })
        .transpose()?;

    let limit = p
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(1, 500) as usize)
        .unwrap_or(50);

    let events = state.events.recent(session_id, limit);
    serde_json::to_value(events).map_err(internal)
}

pub(super) async fn event_subscribe(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let session_id = p
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| {
            Uuid::parse_str(s).map_err(|e| RpcError {
                code: -32602,
                message: format!("invalid session_id: {e}"),
                data: None,
            })
        })
        .transpose()?;

    let timeout_ms = p
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(30_000)
        .clamp(1, 300_000);

    let mut rx = match session_id {
        Some(sid) => state.events.subscribe_session(sid),
        None => state.events.subscribe_global(),
    };

    let recv = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), rx.recv()).await;
    match recv {
        Ok(Ok(event)) => Ok(serde_json::json!({ "event": event, "timeout": false })),
        Ok(Err(_)) => Ok(serde_json::json!({ "event": serde_json::Value::Null, "timeout": false })),
        Err(_) => Ok(serde_json::json!({ "event": serde_json::Value::Null, "timeout": true })),
    }
}
