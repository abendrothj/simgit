//! `lock.acquire`, `lock.list`, `lock.contention`, `lock.wait`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::{RpcError, ERR_BORROW_CONFLICT};

use crate::daemon::AppState;

use super::support::{internal, str_field, uuid_field};

// ── lock.acquire ──────────────────────────────────────────────────────────────

pub(super) async fn lock_acquire(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let path = PathBuf::from(str_field(&p, "path")?);
    let session_id = uuid_field(&p, "session_id")?;
    let ttl_seconds = p["ttl_seconds"]
        .as_u64()
        .or(Some(state.config.lock_ttl_seconds));

    match state.borrows.acquire_write(session_id, &path, ttl_seconds) {
        Ok(()) => Ok(serde_json::json!({ "acquired": true })),
        Err(e) => {
            state.metrics.record_lock_contention_path(&path);
            let payload = serde_json::to_value(&e).ok();
            Err(RpcError {
                code: ERR_BORROW_CONFLICT,
                message: e.to_string(),
                data: payload,
            })
        }
    }
}

// ── lock.list ─────────────────────────────────────────────────────────────────

pub(super) async fn lock_list(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let path = p["path"].as_str().map(PathBuf::from);
    let locks = state.borrows.list(path.as_deref());
    serde_json::to_value(locks).map_err(internal)
}

pub(super) async fn lock_contention(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let limit = p
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(1, 100) as usize)
        .unwrap_or(20);

    let top = state
        .metrics
        .top_contended_paths(limit)
        .into_iter()
        .map(|(path, conflicts)| {
            serde_json::json!({
                "path": path,
                "conflicts": conflicts,
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({ "top": top }))
}

// ── lock.wait ─────────────────────────────────────────────────────────────────

pub(super) async fn lock_wait(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let path = PathBuf::from(str_field(&p, "path")?);
    let timeout_ms = p["timeout_ms"].as_u64().unwrap_or(5000);
    let caller = p["session_id"]
        .as_str()
        .map(|s| {
            s.parse::<Uuid>().map_err(|_| RpcError {
                code: -32602,
                message: "invalid UUID for param: session_id".to_owned(),
                data: None,
            })
        })
        .transpose()?
        .unwrap_or_else(uuid::Uuid::nil);

    let start = tokio::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(timeout_ms);

    loop {
        if state.borrows.is_write_free(&path, caller) {
            state
                .metrics
                .observe_lock_wait(true, start.elapsed().as_secs_f64());
            return Ok(serde_json::json!({
                "acquired": true,
                "waited_ms": start.elapsed().as_millis() as u64,
            }));
        }
        if tokio::time::Instant::now() >= deadline {
            state
                .metrics
                .observe_lock_wait(false, start.elapsed().as_secs_f64());
            state.metrics.record_lock_contention_path(&path);
            let holder = state
                .borrows
                .list(Some(&path))
                .into_iter()
                .find(|l| l.path == path)
                .and_then(|l| l.writer_session)
                .and_then(|sid| state.sessions.get(sid))
                .map(|info| {
                    serde_json::json!({
                        "session_id": info.session_id,
                        "task_id": info.task_id,
                        "agent_label": info.agent_label,
                        "created_at": info.created_at,
                    })
                });

            return Ok(serde_json::json!({
                "acquired": false,
                "waited_ms": start.elapsed().as_millis() as u64,
                "path": path,
                "holder": holder,
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::dispatch;
    use crate::rpc::methods::test_support::build_state_for_commit_tests;
    use std::path::Path;

    #[tokio::test]
    async fn lock_contention_reports_top_paths() {
        let (state, root) = build_state_for_commit_tests().await;

        state
            .metrics
            .record_lock_contention_path(Path::new("src/a.rs"));
        state
            .metrics
            .record_lock_contention_path(Path::new("src/b.rs"));
        state
            .metrics
            .record_lock_contention_path(Path::new("src/a.rs"));

        let value = dispatch(&state, "lock.contention", serde_json::json!({ "limit": 2 }))
            .await
            .expect("lock.contention result");

        let top = value["top"].as_array().expect("top array");
        assert_eq!(top.len(), 2);
        assert_eq!(top[0]["path"], "src/a.rs");
        assert_eq!(top[0]["conflicts"], 2);
        assert_eq!(top[1]["path"], "src/b.rs");
        assert_eq!(top[1]["conflicts"], 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
