//! Dispatch table: one async fn per JSON-RPC method.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::{
    BorrowError, RpcError, SessionStatus,
    ERR_BORROW_CONFLICT, ERR_MERGE_CONFLICT, ERR_SESSION_NOT_FOUND, ERR_QUOTA_EXCEEDED,
};

use crate::daemon::AppState;

/// Dispatch a JSON-RPC method call. Returns `Ok(result)` or `Err(RpcError)`.
pub async fn dispatch(
    state:  &Arc<AppState>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    match method {
        "session.create" => session_create(state, params).await,
        "session.commit" => session_commit(state, params).await,
        "session.abort"  => session_abort(state, params).await,
        "session.list"   => session_list(state, params).await,
        "session.diff"   => session_diff(state, params).await,
        "lock.list"      => lock_list(state, params).await,
        "lock.wait"      => lock_wait(state, params).await,
        _                => Err(RpcError {
            code:    -32601,
            message: format!("method not found: {method}"),
            data:    None,
        }),
    }
}

// ── session.create ────────────────────────────────────────────────────────────

async fn session_create(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let task_id:     String         = str_field(&p, "task_id")?;
    let agent_label: Option<String> = p["agent_label"].as_str().map(str::to_owned);
    let base_commit: Option<String> = p["base_commit"].as_str().map(str::to_owned);
    let peers:       bool           = p["peers"].as_bool().unwrap_or(false);

    // Resolve base commit (default = HEAD).
    let base_commit = base_commit.unwrap_or_else(|| resolve_head(&state.config.repo_path));

    // Build mount path.
    let session_id = uuid::Uuid::now_v7();
    let mount_path = state.config.mnt_dir.join(session_id.to_string());

    let info = state.sessions.create(
        task_id,
        agent_label,
        base_commit.clone(),
        mount_path.clone(),
        peers,
        state.config.max_sessions,
    ).map_err(internal)?;

    // Initialise delta store.
    state.deltas.init_session(info.session_id, &base_commit).map_err(internal)?;

    // Mount VFS.
    state.vfs.mount(&info).await.map_err(internal)?;

    serde_json::to_value(&info).map_err(internal)
}

// ── session.commit ────────────────────────────────────────────────────────────

async fn session_commit(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let session_id = uuid_field(&p, "session_id")?;
    let branch_name = p["branch_name"].as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("simgit/{session_id}"));
    let message = p["message"].as_str()
        .unwrap_or("simgit: agent commit")
        .to_owned();

    let info = state.sessions.get(session_id).ok_or_else(|| not_found(session_id))?;
    let manifest = state.deltas.load_manifest(session_id).map_err(internal)?;

    // Flatten delta to git branch.
    let result = crate::delta::flatten::flatten(
        &state.config.repo_path,
        &info.base_commit,
        &manifest,
        &state.config.state_dir.join("deltas"),
        session_id,
        &branch_name,
        &message,
    ).map_err(|e| RpcError {
        code:    ERR_MERGE_CONFLICT,
        message: e.to_string(),
        data:    None,
    })?;

    // Release locks, update status.
    state.borrows.release_session(session_id);
    let updated = state.sessions.mark_committed(session_id, &result.branch_name).map_err(internal)?;
    state.vfs.unmount_session(session_id).await;

    // Emit peer_commit event.
    state.events.publish(session_id, "peer_commit", serde_json::json!({
        "session_id": session_id,
        "branch":     result.branch_name,
        "commit":     result.commit_oid,
    }));

    serde_json::to_value(&updated).map_err(internal)
}

// ── session.abort ─────────────────────────────────────────────────────────────

async fn session_abort(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let session_id = uuid_field(&p, "session_id")?;
    let _ = state.sessions.get(session_id).ok_or_else(|| not_found(session_id))?;

    state.borrows.release_session(session_id);
    state.vfs.unmount_session(session_id).await;
    let _ = state.deltas.purge_session(session_id);
    state.sessions.mark_aborted(session_id).map_err(internal)?;

    Ok(serde_json::json!({ "ok": true }))
}

// ── session.list ──────────────────────────────────────────────────────────────

async fn session_list(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let status: Option<SessionStatus> = p["status"]
        .as_str()
        .and_then(|s| serde_json::from_value(serde_json::Value::String(s.to_owned())).ok());
    let sessions = state.sessions.list(status);
    serde_json::to_value(sessions).map_err(internal)
}

// ── session.diff ──────────────────────────────────────────────────────────────

async fn session_diff(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let session_id = uuid_field(&p, "session_id")?;
    let manifest = state.deltas.load_manifest(session_id).map_err(internal)?;

    let changed_paths: Vec<PathBuf> = manifest.writes.keys()
        .chain(manifest.deletes.iter())
        .cloned()
        .collect();

    // Build a human-readable summary (full unified diff generation is Phase 5).
    let unified_diff = changed_paths.iter()
        .map(|p| format!("M {}", p.display()))
        .chain(manifest.deletes.iter().map(|p| format!("D {}", p.display())))
        .collect::<Vec<_>>()
        .join("\n");

    let result = simgit_sdk::DiffResult {
        session_id,
        unified_diff,
        changed_paths,
    };
    serde_json::to_value(result).map_err(internal)
}

// ── lock.list ─────────────────────────────────────────────────────────────────

async fn lock_list(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let path = p["path"].as_str().map(PathBuf::from);
    let locks = state.borrows.list(path.as_deref());
    serde_json::to_value(locks).map_err(internal)
}

// ── lock.wait ─────────────────────────────────────────────────────────────────

async fn lock_wait(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let path       = PathBuf::from(str_field(&p, "path")?);
    let timeout_ms = p["timeout_ms"].as_u64().unwrap_or(5000);
    let caller     = uuid::Uuid::nil(); // anonymous poll — no specific session

    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(timeout_ms);

    loop {
        if state.borrows.is_write_free(&path, caller) {
            return Ok(serde_json::json!({ "acquired": true }));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(serde_json::json!({ "acquired": false }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn str_field(p: &serde_json::Value, key: &str) -> Result<String, RpcError> {
    p[key].as_str().map(str::to_owned).ok_or_else(|| RpcError {
        code:    -32602,
        message: format!("missing required param: {key}"),
        data:    None,
    })
}

fn uuid_field(p: &serde_json::Value, key: &str) -> Result<Uuid, RpcError> {
    let s = str_field(p, key)?;
    s.parse::<Uuid>().map_err(|_| RpcError {
        code:    -32602,
        message: format!("invalid UUID for param: {key}"),
        data:    None,
    })
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError { code: -32603, message: e.to_string(), data: None }
}

fn not_found(session_id: Uuid) -> RpcError {
    RpcError {
        code:    ERR_SESSION_NOT_FOUND,
        message: format!("session not found: {session_id}"),
        data:    None,
    }
}

fn resolve_head(repo: &std::path::Path) -> String {
    gix::open(repo)
        .ok()
        .and_then(|r| r.head_id().map(|id| id.to_string()).ok())
        .unwrap_or_else(|| "HEAD".to_owned())
}
