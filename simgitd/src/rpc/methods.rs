//! Dispatch table: one async fn per JSON-RPC method.

use std::path::PathBuf;
use std::sync::Arc;
use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use simgit_sdk::{
    RpcError, SessionStatus,
    ERR_BORROW_CONFLICT, ERR_MERGE_CONFLICT, ERR_QUOTA_EXCEEDED, ERR_SESSION_NOT_FOUND,
};

use crate::daemon::AppState;
use crate::delta::flatten::FlattenError;
use crate::delta::store::ByteRange;
use crate::commit_scheduler::CommitWaitTimeout;

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
        "event.list"     => event_list(state, params).await,
        "event.subscribe" => event_subscribe(state, params).await,
        "lock.acquire"   => lock_acquire(state, params).await,
        "lock.list"      => lock_list(state, params).await,
        "lock.contention" => lock_contention(state, params).await,
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
    ).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("max sessions") {
            RpcError { code: ERR_QUOTA_EXCEEDED, message: msg, data: None }
        } else {
            internal(msg)
        }
    })?;

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

    let stage_started = std::time::Instant::now();

    // Capture mount-side changes in a blocking task to avoid starving the async executor.
    // This is heavy I/O (git subprocess calls, file walks) that must not block other RPC handlers.
    let state_clone = Arc::clone(state);
    let info_clone = info.clone();
    let queued_at = std::time::Instant::now();
    let (qtx, qrx) = tokio::sync::oneshot::channel::<std::time::Duration>();
    tokio::task::spawn_blocking(move || {
        let _ = qtx.send(queued_at.elapsed());
        state_clone.vfs.capture_mount_delta(&info_clone)
    })
    .await
    .map_err(|e| internal(format!("task join error: {e}")))?
    .map_err(internal)?;
    if let Ok(wait) = qrx.await {
        state
            .metrics
            .observe_session_commit_stage("capture_self_queue_wait", wait.as_secs_f64());
    }
    state
        .metrics
        .observe_session_commit_stage("capture_self", stage_started.elapsed().as_secs_f64());

    let stage_started = std::time::Instant::now();
    let active_sessions = state.sessions.list(Some(SessionStatus::Active));

    // Capture deltas for active peers with bounded parallelism to avoid
    // saturating the blocking thread pool under high session counts.
    let peers_to_capture: Vec<_> = active_sessions
        .iter()
        .filter(|peer| peer.session_id != session_id)
        .cloned()
        .collect();
    let max_parallel = state.config.commit_peer_capture_concurrency.max(1);
    let mut idx = 0;
    let mut peers_queue_wait_sum = 0.0f64;
    let mut peers_exec_sum = 0.0f64;
    while idx < peers_to_capture.len() {
        let mut join_set = tokio::task::JoinSet::new();
        for peer in peers_to_capture.iter().skip(idx).take(max_parallel) {
            let state_clone = Arc::clone(state);
            let peer_clone = peer.clone();
            let queued_at = std::time::Instant::now();
            join_set.spawn_blocking(move || {
                let queue_wait = queued_at.elapsed();
                let started = std::time::Instant::now();
                state_clone.vfs.capture_mount_delta(&peer_clone).ok();
                (queue_wait, started.elapsed())
            });
        }
        while let Some(joined) = join_set.join_next().await {
            if let Ok((queue_wait, exec)) = joined {
                peers_queue_wait_sum += queue_wait.as_secs_f64();
                peers_exec_sum += exec.as_secs_f64();
            }
        }
        idx += max_parallel;
    }
    state
        .metrics
        .observe_session_commit_stage("capture_peers_queue_wait", peers_queue_wait_sum);
    state
        .metrics
        .observe_session_commit_stage("capture_peers_execution", peers_exec_sum);
    state
        .metrics
        .observe_session_commit_stage("capture_peers", stage_started.elapsed().as_secs_f64());

    let stage_started = std::time::Instant::now();
    let mut manifest = state.deltas.load_manifest(session_id).map_err(internal)?;
    let delta_root = state.config.state_dir.join("deltas");
    let mut auto_merged_paths = BTreeSet::new();

    // Acquire per-path commit slot before the conflict scan.
    // This serialises sessions that touch the same files, eliminating the
    // retry-on-conflict loop for hot paths without blocking disjoint sessions.
    let _commit_guard = if state.config.commit_wait_secs > 0 {
        let our_paths: Vec<PathBuf> = {
            let ops = changed_path_ops(&manifest);
            ops.into_keys().collect()
        };
        let wait = std::time::Duration::from_secs(state.config.commit_wait_secs);
        let sched_start = std::time::Instant::now();
        match state.commit_scheduler.begin_commit(&our_paths, Some(wait)).await {
            Ok(guard) => {
                state
                    .metrics
                    .observe_session_commit_stage("scheduler_queue_wait", sched_start.elapsed().as_secs_f64());
                Some(guard)
            }
            Err(CommitWaitTimeout) => {
                state
                    .metrics
                    .observe_session_commit_stage("scheduler_queue_wait", sched_start.elapsed().as_secs_f64());
                return Err(RpcError {
                    code: ERR_MERGE_CONFLICT,
                    message: "commit wait timeout: path lock not released in time".to_owned(),
                    data: Some(serde_json::json!({
                        "session_id": session_id,
                        "kind": "commit_wait_timeout",
                    })),
                });
            }
        }
    } else {
        None
    };

    // Pre-commit conflict scan against active peers.
    // When path scheduling is enabled (`commit_wait_secs > 0`), commits that
    // touch overlapping paths are already serialized by the scheduler. In that
    // mode we skip active-session overlap rejection to avoid self-conflict with
    // queued peers that are active but not currently committing.
    let scheduler_enabled = state.config.commit_wait_secs > 0;
    if !scheduler_enabled {
        let mut conflicts = Vec::new();
        for peer in active_sessions {
            if peer.session_id == session_id {
                continue;
            }
            let peer_manifest = match state.deltas.load_manifest(peer.session_id) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let this_ops = changed_path_ops(&manifest);
            let peer_ops = changed_path_ops(&peer_manifest);
            let overlap = compute_conflict_paths(&this_ops, &manifest, &peer_ops, &peer_manifest);

            // Conservative auto-merge: for write/write path conflicts only, attempt a
            // three-way merge (ours/base/theirs). Clean merges are folded into our
            // transient manifest for this commit attempt.
            let mut unresolved = Vec::new();
            for path in overlap {
                let merged = try_auto_merge_path(
                    &state.config.repo_path,
                    &delta_root,
                    session_id,
                    &info.base_commit,
                    &manifest,
                    peer.session_id,
                    &peer_manifest,
                    &this_ops,
                    &peer_ops,
                    &path,
                )
                .map_err(internal)?;
                if let Some(bytes) = merged {
                    let merged_hash = write_transient_blob(&delta_root, session_id, &bytes).map_err(internal)?;
                    manifest.writes.insert(path.clone(), merged_hash);
                    // Once auto-merged, this path becomes a synthesized full-file write.
                    manifest.ranges.remove(&path);
                    auto_merged_paths.insert(path);
                } else {
                    unresolved.push(path);
                }
            }

            if !unresolved.is_empty() {
                let path_conflicts: Vec<_> = unresolved
                    .iter()
                    .map(|path| {
                        serde_json::json!({
                            "path": path,
                            "ours_ops": ops_for_path(&this_ops, path),
                            "peer_ops": ops_for_path(&peer_ops, path),
                        })
                    })
                    .collect();
                conflicts.push(serde_json::json!({
                    "session_id": peer.session_id,
                    "task_id": peer.task_id,
                    "paths": unresolved,
                    "path_conflicts": path_conflicts,
                }));
            }
        }
        state
            .metrics
            .observe_session_commit_stage("conflict_scan", stage_started.elapsed().as_secs_f64());
        if !conflicts.is_empty() {
            let conflicting_paths = conflicts
                .iter()
                .filter_map(|conflict| conflict.get("paths"))
                .filter_map(|paths| paths.as_array())
                .map(|paths| paths.len())
                .sum::<usize>();
            state.metrics.observe_session_commit_conflict(
                "active_session_overlap",
                conflicts.len(),
                conflicting_paths,
            );
            return Err(RpcError {
                code: ERR_MERGE_CONFLICT,
                message: "pre-commit conflict: overlapping active session paths".to_owned(),
                data: Some(serde_json::json!({
                    "session_id": session_id,
                    "kind": "active_session_overlap",
                    "conflicts": conflicts,
                    "auto_merged_paths": auto_merged_paths,
                })),
            });
        }
    } else {
        state
            .metrics
            .observe_session_commit_stage("conflict_scan", stage_started.elapsed().as_secs_f64());
    }

    let stage_started = std::time::Instant::now();
    // Flatten delta to git branch in a blocking task (heavy git I/O).
    let repo_path = state.config.repo_path.clone();
    let base_commit = info.base_commit.clone();
    let state_dir = state.config.state_dir.clone();
    let branch_name_clone = branch_name.clone();
    let queued_at = std::time::Instant::now();
    let (qtx, qrx) = tokio::sync::oneshot::channel::<std::time::Duration>();
    let result = tokio::task::spawn_blocking(move || {
        let _ = qtx.send(queued_at.elapsed());
        crate::delta::flatten::flatten(
            &repo_path,
            &base_commit,
            &manifest,
            &state_dir.join("deltas"),
            session_id,
            &branch_name_clone,
            &message,
        )
    })
    .await
    .map_err(|e| internal(format!("task join error: {e}")))?
    .map_err(|e| flatten_error_to_rpc(e, session_id, &branch_name))?;
    if let Ok(wait) = qrx.await {
        state
            .metrics
            .observe_session_commit_stage("flatten_queue_wait", wait.as_secs_f64());
    }
    state
        .metrics
        .observe_session_commit_stage("flatten", stage_started.elapsed().as_secs_f64());
    state
        .metrics
        .observe_session_commit_stage("flatten_worktree_add", result.worktree_add_secs);
    state
        .metrics
        .observe_session_commit_stage("flatten_apply_manifest", result.apply_manifest_secs);
    state
        .metrics
        .observe_session_commit_stage("flatten_git_add", result.git_add_secs);
    state
        .metrics
        .observe_session_commit_stage("flatten_checkout_branch", result.checkout_branch_secs);
    state
        .metrics
        .observe_session_commit_stage("flatten_git_commit", result.git_commit_secs);
    state
        .metrics
        .observe_session_commit_stage("flatten_worktree_remove", result.worktree_remove_secs);

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

    // Release per-path commit slot so the next waiter can proceed.
    drop(_commit_guard);
    // Best-effort GC: prune path entries that have no active waiters.
    state.commit_scheduler.gc().await;

    serde_json::to_value(&updated).map_err(internal)
}

fn try_auto_merge_path(
    repo_path: &Path,
    delta_root: &Path,
    our_session: Uuid,
    base_commit: &str,
    our_manifest: &crate::delta::store::DeltaManifest,
    peer_session: Uuid,
    peer_manifest: &crate::delta::store::DeltaManifest,
    our_ops: &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    peer_ops: &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    path: &Path,
) -> Result<Option<Vec<u8>>> {
    let our_write_only = ops_for_path(our_ops, path) == vec!["write"];
    let peer_write_only = ops_for_path(peer_ops, path) == vec!["write"];
    if !our_write_only || !peer_write_only {
        return Ok(None);
    }

    let base = read_base_blob(repo_path, base_commit, path);
    let ours = read_delta_blob(delta_root, our_session, our_manifest, path)?;
    let theirs = read_delta_blob(delta_root, peer_session, peer_manifest, path)?;
    let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs) else {
        return Ok(None);
    };

    try_git_three_way_merge(repo_path, &base, &ours, &theirs)
}

fn try_git_three_way_merge(
    repo_path: &Path,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<Option<Vec<u8>>> {
    let tmp = std::env::temp_dir().join(format!("simgit-merge-{}-{}", std::process::id(), Uuid::now_v7()));
    std::fs::create_dir_all(&tmp)?;
    let base_file = tmp.join("base");
    let our_file = tmp.join("ours");
    let their_file = tmp.join("theirs");
    std::fs::write(&base_file, base)?;
    std::fs::write(&our_file, ours)?;
    std::fs::write(&their_file, theirs)?;

    let out = std::process::Command::new("git")
        .current_dir(repo_path)
        .args([
            "merge-file",
            "-p",
            our_file.to_string_lossy().as_ref(),
            base_file.to_string_lossy().as_ref(),
            their_file.to_string_lossy().as_ref(),
        ])
        .output()?;

    let _ = std::fs::remove_dir_all(&tmp);

    match out.status.code().unwrap_or(2) {
        0 => Ok(Some(out.stdout)),
        1 => Ok(None),
        _ => Ok(None),
    }
}

fn write_transient_blob(delta_root: &Path, session_id: Uuid, content: &[u8]) -> Result<String> {
    let digest = Sha256::digest(content);
    let mut hash = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hash, "{b:02x}");
    }
    let bucket = &hash[..2];
    let object_dir = delta_root
        .join(session_id.to_string())
        .join("objects")
        .join(bucket);
    std::fs::create_dir_all(&object_dir)?;
    let blob_path = object_dir.join(&hash[2..]);
    if !blob_path.exists() {
        std::fs::write(blob_path, content)?;
    }
    Ok(hash)
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

    let changed_set = changed_paths_set(&manifest);
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

// ── event.list ───────────────────────────────────────────────────────────────

async fn event_list(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
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

async fn event_subscribe(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
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
    let repo = gix::open(repo_path).ok()?;
    let spec = format!("{}:{}", base_commit, path.to_string_lossy());
    let id = repo.rev_parse_single(spec.as_str()).ok()?;
    let obj = id.object().ok()?;
    let blob = obj.try_into_blob().ok()?;
    Some(blob.data.to_vec())
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

fn changed_paths_set(manifest: &crate::delta::store::DeltaManifest) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    out.extend(manifest.writes.keys().cloned());
    out.extend(manifest.deletes.iter().cloned());
    for (from, to) in &manifest.renames {
        out.insert(from.clone());
        out.insert(to.clone());
    }
    out
}

fn changed_path_ops(manifest: &crate::delta::store::DeltaManifest) -> std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>> {
    let mut out: std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>> =
        std::collections::BTreeMap::new();
    for path in manifest.writes.keys() {
        out.entry(path.clone()).or_default().insert("write");
    }
    for path in &manifest.deletes {
        out.entry(path.clone()).or_default().insert("delete");
    }
    for (from, to) in &manifest.renames {
        out.entry(from.clone()).or_default().insert("rename_from");
        out.entry(to.clone()).or_default().insert("rename_to");
    }
    out
}

fn ops_for_path(
    ops: &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    path: &Path,
) -> Vec<&'static str> {
    ops.get(path)
        .map(|set| set.iter().copied().collect())
        .unwrap_or_default()
}

fn flatten_error_to_rpc(e: FlattenError, session_id: Uuid, branch_name: &str) -> RpcError {
    match e {
        FlattenError::MissingBlob { path, hash } => RpcError {
            code: -32603,
            message: "flatten failed: missing delta blob".to_owned(),
            data: Some(serde_json::json!({
                "kind": "missing_delta_blob",
                "session_id": session_id,
                "branch_name": branch_name,
                "path": path,
                "hash": hash,
            })),
        },
        FlattenError::GitCommand {
            step,
            status,
            stderr,
            args,
        } => {
            let (code, kind) = match step {
                "checkout_branch" | "git_commit" => (ERR_MERGE_CONFLICT, "git_conflict"),
                _ => (-32603, "git_operation_failed"),
            };
            RpcError {
                code,
                message: format!("flatten failed during {step}"),
                data: Some(serde_json::json!({
                    "kind": kind,
                    "step": step,
                    "status": status,
                    "session_id": session_id,
                    "branch_name": branch_name,
                    "args": args,
                    "stderr": stderr,
                })),
            }
        }
        FlattenError::Io { step, source } => RpcError {
            code: -32603,
            message: format!("flatten io failure during {step}"),
            data: Some(serde_json::json!({
                "kind": "filesystem_io",
                "step": step,
                "session_id": session_id,
                "branch_name": branch_name,
                "error": source.to_string(),
            })),
        },
        FlattenError::PathTraversal { path } => RpcError {
            code: -32602,
            message: format!("flatten rejected: unsafe path in delta manifest: {}", path.display()),
            data: Some(serde_json::json!({
                "kind": "path_traversal",
                "session_id": session_id,
                "branch_name": branch_name,
                "path": path,
            })),
        },
    }
}

fn overlap_paths(a: &BTreeSet<PathBuf>, b: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    a.intersection(b).cloned().collect()
}

/// Range-aware conflict detection between two sessions.
///
/// For write-write pairs on the same path: conflict only when byte ranges overlap
/// (or when either side has no range info — NFS full-file writes).
/// For delete/rename ops on the same path: always a conflict.
fn compute_conflict_paths(
    our_ops:      &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    our_manifest: &crate::delta::store::DeltaManifest,
    peer_ops:     &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    peer_manifest: &crate::delta::store::DeltaManifest,
) -> Vec<PathBuf> {
    let our_paths:  BTreeSet<&PathBuf> = our_ops.keys().collect();
    let peer_paths: BTreeSet<&PathBuf> = peer_ops.keys().collect();
    let mut result = Vec::new();
    for path in our_paths.intersection(&peer_paths) {
        let path = *path;
        let our_ops_set  = &our_ops[path];
        let peer_ops_set = &peer_ops[path];
        // If both sides only have a "write" op, check byte ranges.
        let our_write_only  = our_ops_set.len() == 1 && our_ops_set.contains("write");
        let peer_write_only = peer_ops_set.len() == 1 && peer_ops_set.contains("write");
        if our_write_only && peer_write_only {
            let our_ranges  = our_manifest.ranges.get(path).map(Vec::as_slice);
            let peer_ranges = peer_manifest.ranges.get(path).map(Vec::as_slice);
            if !byte_ranges_conflict(our_ranges, peer_ranges) {
                continue;
            }
        }
        result.push(path.clone());
    }
    result
}

/// Returns `true` if the byte ranges from two sessions conflict.
/// `None` means a full-file write (no range info) — always conflicts.
fn byte_ranges_conflict(a: Option<&[ByteRange]>, b: Option<&[ByteRange]>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => ranges_overlap(a, b),
        _                  => true,
    }
}

fn ranges_overlap(a: &[ByteRange], b: &[ByteRange]) -> bool {
    a.iter().any(|ar| {
        b.iter().any(|br| {
            ar.offset < br.offset.saturating_add(br.len)
                && br.offset < ar.offset.saturating_add(ar.len)
        })
    })
}

fn diff_bytes_for_path(path: &Path, old: Option<&[u8]>, new: Option<&[u8]>) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!("simgit-diff-{}-{}", std::process::id(), Uuid::now_v7()));
    std::fs::create_dir_all(&tmp)?;
    let old_file = tmp.join("old");
    let new_file = tmp.join("new");

    std::fs::write(&old_file, old.unwrap_or_default())?;
    std::fs::write(&new_file, new.unwrap_or_default())?;

    let out = std::process::Command::new("git")
        .args([
            "--no-pager",
            "diff",
            "--no-index",
            "--binary",
            "--src-prefix",
            "a/",
            "--dst-prefix",
            "b/",
            old_file.to_string_lossy().as_ref(),
            new_file.to_string_lossy().as_ref(),
        ])
        .output();

    // Always clean up temp files, regardless of whether the command succeeded.
    let _ = std::fs::remove_dir_all(&tmp);

    let out = out?;

    // git diff --no-index exits with:
    // 0 = no differences, 1 = differences found, >1 = actual error.
    if out.status.code().unwrap_or(2) > 1 {
        anyhow::bail!(
            "git diff --no-index failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── lock.acquire ──────────────────────────────────────────────────────────────

async fn lock_acquire(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let path = PathBuf::from(str_field(&p, "path")?);
    let session_id = uuid_field(&p, "session_id")?;
    let ttl_seconds = p["ttl_seconds"].as_u64().or(Some(state.config.lock_ttl_seconds));

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

async fn lock_list(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let path = p["path"].as_str().map(PathBuf::from);
    let locks = state.borrows.list(path.as_deref());
    serde_json::to_value(locks).map_err(internal)
}

async fn lock_contention(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let limit = p
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n.clamp(1, 100) as usize)
        .unwrap_or(20);

    let top = state
        .metrics
        .top_contended_paths(limit)
        .into_iter()
        .map(|(path, conflicts)| serde_json::json!({
            "path": path,
            "conflicts": conflicts,
        }))
        .collect::<Vec<_>>();

    Ok(serde_json::json!({ "top": top }))
}

// ── lock.wait ─────────────────────────────────────────────────────────────────

async fn lock_wait(state: &Arc<AppState>, p: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let path       = PathBuf::from(str_field(&p, "path")?);
    let timeout_ms = p["timeout_ms"].as_u64().unwrap_or(5000);
    let caller     = p["session_id"]
        .as_str()
        .map(|s| s.parse::<Uuid>().map_err(|_| RpcError {
            code:    -32602,
            message: "invalid UUID for param: session_id".to_owned(),
            data:    None,
        }))
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
                .map(|info| serde_json::json!({
                    "session_id": info.session_id,
                    "task_id": info.task_id,
                    "agent_label": info.agent_label,
                    "created_at": info.created_at,
                }));

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

#[cfg(test)]
mod tests {
    use super::{
        changed_path_ops, changed_paths_set, diff_bytes_for_path, flatten_error_to_rpc, ops_for_path,
        overlap_paths,
    };
    use super::{dispatch, session_commit};
    use crate::borrow::BorrowRegistry;
    use crate::config::{Config, VfsBackend};
    use crate::delta::store::ByteRange;
    use crate::delta::flatten::FlattenError;
    use crate::daemon::AppState;
    use crate::delta::store::DeltaManifest;
    use crate::delta::DeltaStore;
    use crate::events::EventBroker;
    use crate::session::SessionManager;
    use crate::vfs::VfsManager;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn run_git_output(repo: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(out.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn run_git_status(repo: &std::path::Path, args: &[&str]) -> i32 {
        let status = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .expect("git command should execute");
        status.code().unwrap_or(-1)
    }

    #[test]
    fn unified_diff_for_modified_file_contains_hunk() {
        let patch = diff_bytes_for_path(
            Path::new("src/main.rs"),
            Some(b"fn main() {}\n"),
            Some(b"fn main() { println!(\"hi\"); }\n"),
        )
        .expect("diff generation should succeed");

        assert!(patch.contains("diff --git"));
        assert!(patch.contains("@@"));
        assert!(patch.contains("+fn main() { println!(\"hi\"); }"));
        assert!(patch.contains("-fn main() {}"));
    }

    #[test]
    fn unified_diff_for_deleted_file_shows_removed_lines() {
        let patch = diff_bytes_for_path(
            Path::new("README.md"),
            Some(b"line1\nline2\n"),
            Some(&[]),
        )
        .expect("diff generation should succeed");

        assert!(patch.contains("diff --git"));
        assert!(patch.contains("-line1"));
        assert!(patch.contains("-line2"));
    }

    #[test]
    fn unified_diff_for_added_file_shows_added_lines() {
        let patch = diff_bytes_for_path(
            Path::new("new.txt"),
            Some(&[]),
            Some(b"hello\n"),
        )
        .expect("diff generation should succeed");

        assert!(patch.contains("diff --git"));
        assert!(patch.contains("+hello"));
    }

    #[test]
    fn changed_paths_set_includes_writes_deletes_and_renames() {
        let mut m = DeltaManifest::default();
        m.writes.insert("a.txt".into(), "abc".into());
        m.deletes.insert("b.txt".into());
        m.renames.push(("c.txt".into(), "d.txt".into()));

        let set = changed_paths_set(&m);
        assert!(set.contains(Path::new("a.txt")));
        assert!(set.contains(Path::new("b.txt")));
        assert!(set.contains(Path::new("c.txt")));
        assert!(set.contains(Path::new("d.txt")));
    }

    #[test]
    fn overlap_paths_returns_intersection() {
        let mut a = std::collections::BTreeSet::new();
        let mut b = std::collections::BTreeSet::new();
        a.insert(std::path::PathBuf::from("x.txt"));
        a.insert(std::path::PathBuf::from("y.txt"));
        b.insert(std::path::PathBuf::from("y.txt"));
        b.insert(std::path::PathBuf::from("z.txt"));

        let overlap = overlap_paths(&a, &b);
        assert_eq!(overlap, vec![std::path::PathBuf::from("y.txt")]);
    }

    #[test]
    fn changed_path_ops_reports_operation_types_per_path() {
        let mut m = DeltaManifest::default();
        m.writes.insert("a.txt".into(), "hash1".into());
        m.deletes.insert("a.txt".into());
        m.renames.push(("a.txt".into(), "b.txt".into()));

        let ops = changed_path_ops(&m);
        assert_eq!(
            ops_for_path(&ops, Path::new("a.txt")),
            vec!["delete", "rename_from", "write"]
        );
        assert_eq!(ops_for_path(&ops, Path::new("b.txt")), vec!["rename_to"]);
    }

    #[test]
    fn flatten_error_to_rpc_marks_git_commit_as_merge_conflict() {
        let err = FlattenError::GitCommand {
            step: "git_commit",
            status: 1,
            stderr: "conflict".to_owned(),
            args: vec!["commit".to_owned()],
        };
        let rpc = flatten_error_to_rpc(err, Uuid::nil(), "feat/test");
        assert_eq!(rpc.code, simgit_sdk::ERR_MERGE_CONFLICT);
        let data = rpc.data.expect("data");
        assert_eq!(data["kind"], "git_conflict");
    }

    #[test]
    fn flatten_error_to_rpc_includes_missing_blob_details() {
        let err = FlattenError::MissingBlob {
            path: std::path::PathBuf::from("src/main.rs"),
            hash: "abcdef".to_owned(),
        };
        let rpc = flatten_error_to_rpc(err, Uuid::nil(), "feat/test");
        assert_eq!(rpc.code, -32603);
        let data = rpc.data.expect("data");
        assert_eq!(data["kind"], "missing_delta_blob");
        assert_eq!(data["path"], "src/main.rs");
    }

    static TEST_STATE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_state_root() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let seq = TEST_STATE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "simgit-rpc-commit-test-{}-{}-{}",
            std::process::id(),
            nanos,
            seq
        ))
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .expect("git command should execute");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn init_repo(root: &std::path::Path) -> std::path::PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.email", "tests@simgit.local"]);
        run_git(&repo, &["config", "user.name", "simgit-tests"]);
        std::fs::write(repo.join("README.md"), b"base\n").expect("write readme");
        std::fs::write(repo.join("src.txt"), b"base-src\n").expect("write src");
        std::fs::write(repo.join("merge.txt"), b"line1\nline2\nline3\n").expect("write merge fixture");
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        repo
    }

    async fn build_state_for_commit_tests() -> (Arc<AppState>, std::path::PathBuf) {
        let root = temp_state_root();
        let repo = init_repo(&root);
        let state_dir = root.join("state");
        let mnt_dir = state_dir.join("mnt");
        std::fs::create_dir_all(&mnt_dir).expect("create mnt");

        let cfg = Arc::new(Config {
            repo_path: repo.clone(),
            state_dir: state_dir.clone(),
            mnt_dir,
            max_sessions: 8,
            max_delta_bytes: 2 * 1024 * 1024,
            lock_ttl_seconds: 3600,
            session_recovery_ttl_seconds: 86400,
            vfs_backend: VfsBackend::NfsLoopback,
            metrics_enabled: false,
            metrics_addr: "127.0.0.1:0".to_owned(),
            commit_peer_capture_concurrency: 4,
            commit_wait_secs: 30,
        });

        let db_path = state_dir.join("state.db");
        let sessions = Arc::new(SessionManager::open(&db_path).await.expect("open sessions"));
        let borrows = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
        let deltas = Arc::new(DeltaStore::new(state_dir.join("deltas")));
        let events = Arc::new(EventBroker::new());
        let vfs = Arc::new(VfsManager::new(
            Arc::clone(&cfg),
            Arc::clone(&deltas),
            Arc::clone(&borrows),
            Arc::new(crate::metrics::Metrics::new().expect("metrics")),
        ));

        let state = Arc::new(AppState {
            config: cfg,
            sessions,
            borrows,
            deltas,
            events,
            vfs,
            metrics: Arc::new(crate::metrics::Metrics::new().expect("metrics")),
            commit_scheduler: Arc::new(crate::commit_scheduler::CommitScheduler::new(
                std::time::Duration::from_secs(30),
            )),
        });
        (state, root)
    }

    #[tokio::test]
    async fn session_commit_blocks_on_overlap_with_active_peer() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-1".to_owned(),
                Some("agent-1".to_owned()),
                "HEAD".to_owned(),
                state.config.mnt_dir.join("s1"),
                false,
                8,
            )
            .expect("create s1");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init s1 delta");
        state
            .deltas
            .write_blob(s1.session_id, Path::new("README.md"), b"change-1\n", None)
            .expect("write s1 blob");

        let s2 = state
            .sessions
            .create(
                "task-2".to_owned(),
                Some("agent-2".to_owned()),
                "HEAD".to_owned(),
                state.config.mnt_dir.join("s2"),
                false,
                8,
            )
            .expect("create s2");
        state
            .deltas
            .init_session(s2.session_id, &s2.base_commit)
            .expect("init s2 delta");
        state
            .deltas
            .write_blob(s2.session_id, Path::new("README.md"), b"change-2\n", None)
            .expect("write s2 blob");

        let res = session_commit(&state, serde_json::json!({
            "session_id": s1.session_id,
            "branch_name": "feat/overlap",
            "message": "overlap",
        }))
        .await;

        assert!(res.is_err(), "overlap commit should be blocked");
        let err = res.expect_err("must fail");
        assert_eq!(err.code, simgit_sdk::ERR_MERGE_CONFLICT);
        let data = err.data.expect("conflict payload");
        let conflicts = data["conflicts"].as_array().expect("conflicts array");
        assert!(!conflicts.is_empty());

        let metrics = state.metrics.render().expect("render metrics");
        assert!(metrics.contains("simgit_session_commit_conflicts_total{kind=\"active_session_overlap\"} 1"));
        assert!(metrics.contains("simgit_session_commit_conflict_paths_sum 1"));
        assert!(metrics.contains("simgit_session_commit_conflict_peers_sum 1"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_succeeds_for_non_overlapping_paths() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-a".to_owned(),
                Some("agent-a".to_owned()),
                "HEAD".to_owned(),
                state.config.mnt_dir.join("sa"),
                false,
                8,
            )
            .expect("create s1");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init s1 delta");
        state
            .deltas
            .write_blob(s1.session_id, Path::new("README.md"), b"change-a\n", None)
            .expect("write s1 blob");

        let s2 = state
            .sessions
            .create(
                "task-b".to_owned(),
                Some("agent-b".to_owned()),
                "HEAD".to_owned(),
                state.config.mnt_dir.join("sb"),
                false,
                8,
            )
            .expect("create s2");
        state
            .deltas
            .init_session(s2.session_id, &s2.base_commit)
            .expect("init s2 delta");
        state
            .deltas
            .write_blob(s2.session_id, Path::new("src.txt"), b"change-b\n", None)
            .expect("write s2 blob");

        let res = session_commit(&state, serde_json::json!({
            "session_id": s1.session_id,
            "branch_name": "feat/non-overlap",
            "message": "non-overlap",
        }))
        .await;

        assert!(res.is_ok(), "non-overlap commit should succeed");
        let updated = state.sessions.get(s1.session_id).expect("s1 exists");
        assert_eq!(updated.status, simgit_sdk::SessionStatus::Committed);

        let metrics = state.metrics.render().expect("render metrics");
        assert!(metrics.contains("simgit_session_commit_stage_duration_seconds_count{stage=\"capture_self\"} 1"));
        assert!(metrics.contains("simgit_session_commit_stage_duration_seconds_count{stage=\"capture_peers\"} 1"));
        assert!(metrics.contains("simgit_session_commit_stage_duration_seconds_count{stage=\"conflict_scan\"} 1"));
        assert!(metrics.contains("simgit_session_commit_stage_duration_seconds_count{stage=\"flatten\"} 1"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_succeeds_for_non_overlapping_byte_ranges() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state.sessions.create(
            "task-r1".into(), Some("agent-r1".into()), "HEAD".into(),
            state.config.mnt_dir.join("sr1"), false, 8,
        ).expect("create s1");
        state.deltas.init_session(s1.session_id, &s1.base_commit).expect("init s1");
        state.deltas.write_blob(
            s1.session_id, Path::new("hotspot.log"), b"aaaaaaaaaa",
            Some(ByteRange { offset: 0, len: 10 }),
        ).expect("write s1");

        let s2 = state.sessions.create(
            "task-r2".into(), Some("agent-r2".into()), "HEAD".into(),
            state.config.mnt_dir.join("sr2"), false, 8,
        ).expect("create s2");
        state.deltas.init_session(s2.session_id, &s2.base_commit).expect("init s2");
        state.deltas.write_blob(
            s2.session_id, Path::new("hotspot.log"), b"bbbbbbbbbb",
            Some(ByteRange { offset: 10, len: 10 }),
        ).expect("write s2");

        let res = session_commit(&state, serde_json::json!({
            "session_id": s1.session_id,
            "branch_name": "feat/range-noconflict",
            "message": "non-overlapping ranges",
        })).await;

        assert!(res.is_ok(), "adjacent byte ranges must not conflict: {:?}", res.err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_conflicts_for_overlapping_byte_ranges() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state.sessions.create(
            "task-o1".into(), Some("agent-o1".into()), "HEAD".into(),
            state.config.mnt_dir.join("so1"), false, 8,
        ).expect("create s1");
        state.deltas.init_session(s1.session_id, &s1.base_commit).expect("init s1");
        state.deltas.write_blob(
            s1.session_id, Path::new("hotspot.log"), b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some(ByteRange { offset: 0, len: 50 }),
        ).expect("write s1");

        let s2 = state.sessions.create(
            "task-o2".into(), Some("agent-o2".into()), "HEAD".into(),
            state.config.mnt_dir.join("so2"), false, 8,
        ).expect("create s2");
        state.deltas.init_session(s2.session_id, &s2.base_commit).expect("init s2");
        state.deltas.write_blob(
            s2.session_id, Path::new("hotspot.log"), b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            Some(ByteRange { offset: 25, len: 50 }),
        ).expect("write s2");

        let res = session_commit(&state, serde_json::json!({
            "session_id": s1.session_id,
            "branch_name": "feat/range-conflict",
            "message": "overlapping ranges",
        })).await;

        assert!(res.is_err(), "overlapping byte ranges must conflict");
        assert_eq!(res.unwrap_err().code, simgit_sdk::ERR_MERGE_CONFLICT);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_auto_merges_clean_overlapping_writes() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state.sessions.create(
            "task-m1".into(), Some("agent-m1".into()), "HEAD".into(),
            state.config.mnt_dir.join("sm1"), false, 8,
        ).expect("create s1");
        state.deltas.init_session(s1.session_id, &s1.base_commit).expect("init s1");
        state.deltas.write_blob(
            s1.session_id,
            Path::new("merge.txt"),
            b"line1-ours\nline2\nline3\n",
            None,
        ).expect("write s1");

        let s2 = state.sessions.create(
            "task-m2".into(), Some("agent-m2".into()), "HEAD".into(),
            state.config.mnt_dir.join("sm2"), false, 8,
        ).expect("create s2");
        state.deltas.init_session(s2.session_id, &s2.base_commit).expect("init s2");
        state.deltas.write_blob(
            s2.session_id,
            Path::new("merge.txt"),
            b"line1\nline2\nline3-theirs\n",
            None,
        ).expect("write s2");

        let branch = "feat/auto-merge-clean-overlap";
        let res = session_commit(&state, serde_json::json!({
            "session_id": s1.session_id,
            "branch_name": branch,
            "message": "auto-merge clean overlap",
        })).await;
        assert!(res.is_ok(), "clean overlapping writes should auto-merge: {:?}", res.err());

        let merged = run_git_output(&state.config.repo_path, &["show", &format!("{}:merge.txt", branch)]);
        assert_eq!(merged, "line1-ours\nline2\nline3-theirs");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_flatten_e2e_applies_write_delete_and_rename() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-flat-e2e".to_owned(),
                Some("agent-flat-e2e".to_owned()),
                "HEAD".to_owned(),
                state.config.mnt_dir.join("flat-e2e"),
                false,
                8,
            )
            .expect("create session");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init delta");

        state
            .deltas
            .write_blob(s1.session_id, Path::new("README.md"), b"updated-readme\n", None)
            .expect("write README");
        state
            .deltas
            .mark_deleted(s1.session_id, Path::new("src.txt"))
            .expect("delete src.txt");
        state
            .deltas
            .record_rename(s1.session_id, Path::new("merge.txt"), Path::new("merge_renamed.txt"))
            .expect("rename merge.txt");

        let branch = "feat/flatten-e2e";
        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": branch,
                "message": "flatten e2e",
            }),
        )
        .await;
        assert!(res.is_ok(), "flatten e2e commit should succeed: {:?}", res.err());

        let readme = run_git_output(
            &state.config.repo_path,
            &["show", &format!("{}:README.md", branch)],
        );
        assert_eq!(readme, "updated-readme");

        let src_status = run_git_status(
            &state.config.repo_path,
            &["show", &format!("{}:src.txt", branch)],
        );
        assert_ne!(src_status, 0, "src.txt should be deleted in branch tree");

        let renamed = run_git_output(
            &state.config.repo_path,
            &["show", &format!("{}:merge_renamed.txt", branch)],
        );
        assert_eq!(renamed, "line1\nline2\nline3");

        let old_path_status = run_git_status(
            &state.config.repo_path,
            &["show", &format!("{}:merge.txt", branch)],
        );
        assert_ne!(old_path_status, 0, "merge.txt should be absent after rename");

        let _ = std::fs::remove_dir_all(&root);
    }

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
