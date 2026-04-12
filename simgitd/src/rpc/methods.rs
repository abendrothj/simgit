//! Dispatch table: one async fn per JSON-RPC method.

use std::path::PathBuf;
use std::sync::Arc;
use std::{collections::BTreeSet, path::Path};

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

    let mut changed_set = BTreeSet::new();
    changed_set.extend(manifest.writes.keys().cloned());
    changed_set.extend(manifest.deletes.iter().cloned());
    for (from, to) in &manifest.renames {
        changed_set.insert(from.clone());
        changed_set.insert(to.clone());
    }
    let changed_paths: Vec<PathBuf> = changed_set.into_iter().collect();

    let unified_diff = build_session_unified_diff(
        &state.config.repo_path,
        &state.config.state_dir.join("deltas"),
        session_id,
        &manifest.base_commit,
        &manifest,
    ).map_err(internal)?;

    let result = simgit_sdk::DiffResult {
        session_id,
        unified_diff,
        changed_paths,
    };
    serde_json::to_value(result).map_err(internal)
}

fn build_session_unified_diff(
    repo_path: &Path,
    delta_root: &Path,
    session_id: Uuid,
    base_commit: &str,
    manifest: &crate::delta::store::DeltaManifest,
) -> Result<String> {
    let mut out = String::new();

    // Writes: show base -> delta diff for each changed path.
    let mut write_paths: Vec<_> = manifest.writes.keys().cloned().collect();
    write_paths.sort();
    for path in write_paths {
        let old = read_base_blob(repo_path, base_commit, &path);
        let new = read_delta_blob(delta_root, session_id, manifest, &path)?;
        let patch = diff_bytes_for_path(&path, old.as_deref(), new.as_deref())?;
        if !patch.is_empty() {
            out.push_str(&patch);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }

    // Deletes: show base -> empty diff.
    let mut delete_paths: Vec<_> = manifest.deletes.iter().cloned().collect();
    delete_paths.sort();
    for path in delete_paths {
        let old = read_base_blob(repo_path, base_commit, &path);
        let patch = diff_bytes_for_path(&path, old.as_deref(), Some(&[]))?;
        if !patch.is_empty() {
            out.push_str(&patch);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }

    // Renames: include explicit marker line for visibility.
    for (from, to) in &manifest.renames {
        out.push_str(&format!("rename {} -> {}\n", from.display(), to.display()));
    }

    Ok(out)
}

fn read_base_blob(repo_path: &Path, base_commit: &str, path: &Path) -> Option<Vec<u8>> {
    let spec = format!("{}:{}", base_commit, path.to_string_lossy());
    let out = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["show", &spec])
        .output()
        .ok()?;
    if out.status.success() {
        Some(out.stdout)
    } else {
        None
    }
}

fn read_delta_blob(
    delta_root: &Path,
    session_id: Uuid,
    manifest: &crate::delta::store::DeltaManifest,
    path: &Path,
) -> Result<Option<Vec<u8>>> {
    let Some(hash) = manifest.writes.get(path) else {
        return Ok(None);
    };
    let blob_path = delta_root
        .join(session_id.to_string())
        .join("objects")
        .join(&hash[..2])
        .join(&hash[2..]);
    let bytes = std::fs::read(blob_path)?;
    Ok(Some(bytes))
}

fn diff_bytes_for_path(path: &Path, old: Option<&[u8]>, new: Option<&[u8]>) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!("simgit-diff-{}-{}", std::process::id(), Uuid::now_v7()));
    std::fs::create_dir_all(&tmp)?;
    let old_file = tmp.join("old");
    let new_file = tmp.join("new");

    std::fs::write(&old_file, old.unwrap_or_default())?;
    std::fs::write(&new_file, new.unwrap_or_default())?;

    let label_a = format!("a/{}", path.display());
    let label_b = format!("b/{}", path.display());
    let out = std::process::Command::new("git")
        .args([
            "--no-pager",
            "diff",
            "--no-index",
            "--binary",
            "--label",
            &label_a,
            "--label",
            &label_b,
            old_file.to_string_lossy().as_ref(),
            new_file.to_string_lossy().as_ref(),
        ])
        .output()?;

    let _ = std::fs::remove_dir_all(&tmp);

    // git diff --no-index exits with:
    // 0 = no differences, 1 = differences found, >1 = actual error.
    if out.status.code().unwrap_or(2) > 1 {
        anyhow::bail!("git diff --no-index failed for {}", path.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
