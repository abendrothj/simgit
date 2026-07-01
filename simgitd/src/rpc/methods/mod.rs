//! Dispatch table: one async fn per JSON-RPC method.

use std::sync::Arc;

use anyhow::Result;

use simgit_sdk::RpcError;

use crate::daemon::AppState;

mod commit;
mod events;
mod locks;
mod session;
mod support;

#[cfg(test)]
mod test_support;

use commit::{commit_status, session_commit};
use events::{event_list, event_subscribe};
use locks::{lock_acquire, lock_contention, lock_list, lock_wait};
use session::{session_abort, session_create, session_diff, session_list, session_set_base};

/// Dispatch a JSON-RPC method call. Returns `Ok(result)` or `Err(RpcError)`.
pub async fn dispatch(
    state: &Arc<AppState>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    match method {
        "session.create" => session_create(state, params).await,
        "session.commit" => session_commit(state, params).await,
        "commit.status" => commit_status(state, params).await,
        "session.abort" => session_abort(state, params).await,
        "session.list" => session_list(state, params).await,
        "session.diff" => session_diff(state, params).await,
        "session.set-base" => session_set_base(state, params).await,
        "event.list" => event_list(state, params).await,
        "event.subscribe" => event_subscribe(state, params).await,
        "lock.acquire" => lock_acquire(state, params).await,
        "lock.list" => lock_list(state, params).await,
        "lock.contention" => lock_contention(state, params).await,
        "lock.wait" => lock_wait(state, params).await,
        _ => Err(RpcError {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }),
    }
}
