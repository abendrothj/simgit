//! `session.commit`, `commit.status`, and the flatten/auto-merge/conflict-detection
//! machinery they rely on.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use simgit_sdk::{
    CommitRequestState, RpcError, SessionStatus, ERR_COMMIT_PENDING, ERR_DEADLINE_EXCEEDED,
    ERR_MERGE_CONFLICT,
};

use crate::commit_scheduler::CommitWaitTimeout;
use crate::daemon::AppState;
use crate::delta::flatten::FlattenError;
use crate::delta::store::ByteRange;
use crate::git_proxy::copy_dir_all;

use super::session::{changed_path_ops, ops_for_path, read_base_blob, read_delta_blob};
use super::support::{internal, not_found, now_epoch_ms, optional_uuid_field, uuid_field};

// ── session.commit ────────────────────────────────────────────────────────────

pub(super) async fn session_commit(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let commit_started = std::time::Instant::now();
    let session_id = uuid_field(&p, "session_id")?;
    let request_id = optional_uuid_field(&p, "request_id")?;
    let deadline_epoch_ms = p["deadline_epoch_ms"].as_i64();
    let branch_name = p["branch_name"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("simgit/{session_id}"));
    let message = p["message"]
        .as_str()
        .unwrap_or("simgit: agent commit")
        .to_owned();

    let info = state
        .sessions
        .get(session_id)
        .ok_or_else(|| not_found(session_id))?;

    if let Some(request_id) = request_id {
        let status = state
            .sessions
            .commit_status(session_id, request_id)
            .map_err(internal)?;
        match status.state {
            CommitRequestState::Success => {
                let result = status
                    .result
                    .ok_or_else(|| internal("stored commit success missing result"))?;
                return serde_json::to_value(&result).map_err(internal);
            }
            CommitRequestState::Failed => {
                let error = status
                    .error
                    .ok_or_else(|| internal("stored commit failure missing error"))?;
                return Err(error);
            }
            CommitRequestState::Pending => {
                return Err(RpcError {
                    code: ERR_COMMIT_PENDING,
                    message: "commit request is already in progress".to_owned(),
                    data: Some(serde_json::json!({
                        "session_id": session_id,
                        "request_id": request_id,
                    })),
                });
            }
            CommitRequestState::NotFound => {
                state
                    .sessions
                    .record_commit_pending(session_id, request_id)
                    .map_err(internal)?;
            }
        }
    }

    let commit_outcome: Result<simgit_sdk::SessionCommitResult, RpcError> = async {
        let stage_started = std::time::Instant::now();
        let mut capture_self_queue_wait_secs = 0.0f64;

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
            capture_self_queue_wait_secs = wait.as_secs_f64();
            state
                .metrics
                .observe_session_commit_stage("capture_self_queue_wait", wait.as_secs_f64());
        }
        let capture_self_secs = stage_started.elapsed().as_secs_f64();
        state
            .metrics
            .observe_session_commit_stage("capture_self", capture_self_secs);

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
        let capture_peers_secs = stage_started.elapsed().as_secs_f64();

        let stage_started = std::time::Instant::now();
        let mut manifest = state.deltas.load_manifest(session_id).map_err(internal)?;
        let delta_root = state.config.state_dir.join("deltas");
        let mut auto_merged_paths = BTreeSet::new();

        if let Some(deadline) = deadline_epoch_ms {
            check_commit_deadline(session_id, deadline, "pre_lock")?;
        }

        // Acquire per-path commit slot before the conflict scan.
        // This serialises sessions that touch the same files, eliminating the
        // retry-on-conflict loop for hot paths without blocking disjoint sessions.
        let mut scheduler_queue_wait_secs = 0.0f64;
        let _commit_guard = if state.config.commit_wait_secs > 0 {
            let our_paths: Vec<PathBuf> = {
                let ops = changed_path_ops(&manifest);
                ops.into_keys().collect()
            };
            let wait = std::time::Duration::from_secs(state.config.commit_wait_secs);
            let sched_start = std::time::Instant::now();
            match state
                .commit_scheduler
                .begin_commit(&our_paths, Some(wait))
                .await
            {
                Ok(guard) => {
                    scheduler_queue_wait_secs = sched_start.elapsed().as_secs_f64();
                    state.metrics.observe_session_commit_stage(
                        "scheduler_queue_wait",
                        scheduler_queue_wait_secs,
                    );
                    Some(guard)
                }
                Err(CommitWaitTimeout) => {
                    scheduler_queue_wait_secs = sched_start.elapsed().as_secs_f64();
                    state.metrics.observe_session_commit_stage(
                        "scheduler_queue_wait",
                        scheduler_queue_wait_secs,
                    );
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
        // Even with path scheduling enabled, we still run overlap detection here to
        // preserve conflict semantics (including conservative auto-merge behavior)
        // when another active session has uncommitted changes on the same paths.
        let conflict_scan_secs;
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
                    let merged_hash =
                        write_transient_blob(&delta_root, session_id, &bytes).map_err(internal)?;
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
        conflict_scan_secs = stage_started.elapsed().as_secs_f64();
        state
            .metrics
            .observe_session_commit_stage("conflict_scan", conflict_scan_secs);
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

        let stage_started = std::time::Instant::now();
        if let Some(deadline) = deadline_epoch_ms {
            check_commit_deadline(session_id, deadline, "pre_flatten")?;
        }
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
        let mut flatten_queue_wait_secs = 0.0f64;
        if let Ok(wait) = qrx.await {
            flatten_queue_wait_secs = wait.as_secs_f64();
            state
                .metrics
                .observe_session_commit_stage("flatten_queue_wait", flatten_queue_wait_secs);
        }
        let flatten_secs = stage_started.elapsed().as_secs_f64();
        state
            .metrics
            .observe_session_commit_stage("flatten", flatten_secs);
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
        let updated = state
            .sessions
            .mark_committed(session_id, &result.branch_name)
            .map_err(internal)?;
        // Keep VFS mounted so the agent can inspect the committed result
        // and git can finalize its local commit (pre-commit hook path).
        // Unmount happens on session abort or daemon shutdown.

        // Refresh the session's git state from the real repo so the agent
        // sees the latest branches/tags including the new commit.
        {
            let real_git = state.config.repo_path.join(".git");
            let session_git = info.mount_path.join(".git");

            // Preserve the session's local stash refs before overwriting.
            let session_stash = session_git.join("refs").join("stash");
            let stash_backup = if session_stash.exists() {
                let mut data = Vec::new();
                if std::fs::File::open(&session_stash)
                    .and_then(|mut f| std::io::Read::read_to_end(&mut f, &mut data))
                    .is_ok()
                {
                    Some(data)
                } else {
                    None
                }
            } else {
                None
            };

            // Preserve session-created tags that don't yet exist in the real
            // repo, so they survive the refs refresh and can be propagated.
            let session_tags = session_git.join("refs").join("tags");
            let real_tags = real_git.join("refs").join("tags");
            let new_tags: Vec<(PathBuf, Vec<u8>)> = if session_tags.exists() {
                collect_new_tags(&session_tags, &real_tags)
            } else {
                Vec::new()
            };

            // Refresh refs/ from real repo.
            let real_refs = real_git.join("refs");
            let session_refs = session_git.join("refs");
            if real_refs.exists() {
                if session_refs.exists() || session_refs.is_symlink() {
                    let _ = std::fs::remove_dir_all(&session_refs);
                }
                if let Err(e) = copy_dir_all(&real_refs, &session_refs) {
                    tracing::warn!(session = %session_id, err = %e, "failed to refresh session refs");
                }
            }

            // Restore preserved stash refs.
            if let Some(data) = stash_backup {
                if let Some(parent) = session_stash.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&session_stash, &data);
            }

            // Propagate session-created tags to the real repo and restore
            // them in the session after the refs refresh.
            for (rel_path, data) in &new_tags {
                // Restore in session.
                let session_dest = session_git.join("refs").join("tags").join(rel_path);
                if let Some(parent) = session_dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&session_dest, data);
                // Push to real repo.
                let real_dest = real_git.join("refs").join("tags").join(rel_path);
                if !real_dest.exists() {
                    if let Some(parent) = real_dest.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&real_dest, data);
                }
            }

            // Refresh packed-refs.
            let real_packed = real_git.join("packed-refs");
            let session_packed = session_git.join("packed-refs");
            if real_packed.exists() {
                let _ = std::fs::copy(&real_packed, &session_packed);
            }

            // Re-initialize the index from the new HEAD so git status/diff
            // show correct results after the daemon's commit.
            if session_git.join("HEAD").exists() {
                let index_init = std::process::Command::new("git")
                    .env("GIT_DIR", &session_git)
                    .args(["read-tree", "HEAD"])
                    .output();
                if let Err(ref e) = index_init {
                    tracing::warn!(session = %session_id, err = %e, "git read-tree HEAD failed during post-commit refresh");
                }
            }
        }

        // Emit peer_commit event.
        state.events.publish(
            session_id,
            "peer_commit",
            serde_json::json!({
                "session_id": session_id,
                "branch":     result.branch_name,
                "commit":     result.commit_oid,
            }),
        );

        // Release per-path commit slot so the next waiter can proceed.
        drop(_commit_guard);
        // Best-effort GC: prune path entries that have no active waiters.
        state.commit_scheduler.gc().await;

        let commit_result = simgit_sdk::SessionCommitResult {
            session: updated,
            telemetry: simgit_sdk::CommitTelemetry {
                total_duration_ms: commit_started.elapsed().as_secs_f64() * 1000.0,
                capture_self_queue_wait_ms: capture_self_queue_wait_secs * 1000.0,
                capture_self_ms: capture_self_secs * 1000.0,
                capture_peers_queue_wait_ms: peers_queue_wait_sum * 1000.0,
                capture_peers_execution_ms: peers_exec_sum * 1000.0,
                capture_peers_ms: capture_peers_secs * 1000.0,
                scheduler_queue_wait_ms: scheduler_queue_wait_secs * 1000.0,
                conflict_scan_ms: conflict_scan_secs * 1000.0,
                flatten_queue_wait_ms: flatten_queue_wait_secs * 1000.0,
                flatten_ms: flatten_secs * 1000.0,
                flatten_write_tree_ms: result.git_add_secs * 1000.0,
                flatten_apply_manifest_ms: result.apply_manifest_secs * 1000.0,
                flatten_ref_update_ms: result.checkout_branch_secs * 1000.0,
                flatten_commit_object_ms: result.git_commit_secs * 1000.0,
                retry_count: 0,
            },
        };

        Ok(commit_result)
    }
    .await;

    if let Some(request_id) = request_id {
        match &commit_outcome {
            Ok(result) => state
                .sessions
                .record_commit_success(session_id, request_id, result)
                .map_err(internal)?,
            Err(error) => state
                .sessions
                .record_commit_failure(session_id, request_id, error)
                .map_err(internal)?,
        }
    }

    match commit_outcome {
        Ok(result) => serde_json::to_value(&result).map_err(internal),
        Err(error) => Err(error),
    }
}

pub(super) async fn commit_status(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let session_id = uuid_field(&p, "session_id")?;
    let request_id = uuid_field(&p, "request_id")?;
    let status = state
        .sessions
        .commit_status(session_id, request_id)
        .map_err(internal)?;
    serde_json::to_value(&status).map_err(internal)
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
    let tmp = std::env::temp_dir().join(format!(
        "simgit-merge-{}-{}",
        std::process::id(),
        Uuid::now_v7()
    ));
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
            message: format!(
                "flatten rejected: unsafe path in delta manifest: {}",
                path.display()
            ),
            data: Some(serde_json::json!({
                "kind": "path_traversal",
                "session_id": session_id,
                "branch_name": branch_name,
                "path": path,
            })),
        },
    }
}

/// Range-aware conflict detection between two sessions.
///
/// For write-write pairs on the same path: conflict only when byte ranges overlap
/// (or when either side has no range info — NFS full-file writes).
/// For delete/rename ops on the same path: always a conflict.
fn compute_conflict_paths(
    our_ops: &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    our_manifest: &crate::delta::store::DeltaManifest,
    peer_ops: &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    peer_manifest: &crate::delta::store::DeltaManifest,
) -> Vec<PathBuf> {
    let our_paths: BTreeSet<&PathBuf> = our_ops.keys().collect();
    let peer_paths: BTreeSet<&PathBuf> = peer_ops.keys().collect();
    let mut result = Vec::new();
    for path in our_paths.intersection(&peer_paths) {
        let path = *path;
        let our_ops_set = &our_ops[path];
        let peer_ops_set = &peer_ops[path];
        // If both sides only have a "write" op, check byte ranges.
        let our_write_only = our_ops_set.len() == 1 && our_ops_set.contains("write");
        let peer_write_only = peer_ops_set.len() == 1 && peer_ops_set.contains("write");
        if our_write_only && peer_write_only {
            let our_ranges = our_manifest.ranges.get(path).map(Vec::as_slice);
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
        _ => true,
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

fn check_commit_deadline(
    session_id: Uuid,
    deadline_epoch_ms: i64,
    phase: &str,
) -> Result<(), RpcError> {
    let now = now_epoch_ms();
    if now <= deadline_epoch_ms {
        return Ok(());
    }

    Err(RpcError {
        code: ERR_DEADLINE_EXCEEDED,
        message: format!("commit deadline exceeded before {phase}"),
        data: Some(serde_json::json!({
            "session_id": session_id,
            "kind": "deadline_exceeded",
            "phase": phase,
            "deadline_epoch_ms": deadline_epoch_ms,
            "now_epoch_ms": now,
        })),
    })
}

/// Collect tag refs from `session_tags_dir` that do not exist in `real_tags_dir`.
/// Returns relative paths and their content.
fn collect_new_tags(session_tags_dir: &Path, real_tags_dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(session_tags_dir) else {
        return result;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let rel = path.strip_prefix(session_tags_dir).unwrap_or(&path).to_path_buf();
        if path.is_dir() {
            result.extend(collect_new_tags(&path, &real_tags_dir.join(&rel)));
        } else {
            let real_path = real_tags_dir.join(&rel);
            if real_path.exists() {
                continue; // Already in real repo.
            }
            if let Ok(data) = std::fs::read(&path) {
                result.push((rel, data));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{commit_status, flatten_error_to_rpc, session_commit};
    use crate::delta::flatten::FlattenError;
    use crate::delta::store::ByteRange;
    use crate::rpc::methods::support::now_epoch_ms;
    use crate::rpc::methods::test_support::build_state_for_commit_tests;
    use std::path::{Path, PathBuf};
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

    #[tokio::test]
    async fn session_commit_blocks_on_overlap_with_active_peer() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-1".to_owned(),
                Some("agent-1".to_owned()),
                "HEAD".to_owned(),
                &state.config.mnt_dir,
                false,
                8,
                None,
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
                &state.config.mnt_dir,
                false,
                8,
                None,
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

        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": "feat/overlap",
                "message": "overlap",
            }),
        )
        .await;

        assert!(res.is_err(), "overlap commit should be blocked");
        let err = res.expect_err("must fail");
        assert_eq!(err.code, simgit_sdk::ERR_MERGE_CONFLICT);
        let data = err.data.expect("conflict payload");
        let conflicts = data["conflicts"].as_array().expect("conflicts array");
        assert!(!conflicts.is_empty());

        let metrics = state.metrics.render().expect("render metrics");
        assert!(metrics
            .contains("simgit_session_commit_conflicts_total{kind=\"active_session_overlap\"} 1"));
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
                &state.config.mnt_dir,
                false,
                8,
                None,
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
                &state.config.mnt_dir,
                false,
                8,
                None,
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

        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": "feat/non-overlap",
                "message": "non-overlap",
            }),
        )
        .await;

        assert!(res.is_ok(), "non-overlap commit should succeed");
        let updated = state.sessions.get(s1.session_id).expect("s1 exists");
        assert_eq!(updated.status, simgit_sdk::SessionStatus::Committed);

        let metrics = state.metrics.render().expect("render metrics");
        assert!(metrics.contains(
            "simgit_session_commit_stage_duration_seconds_count{stage=\"capture_self\"} 1"
        ));
        assert!(metrics.contains(
            "simgit_session_commit_stage_duration_seconds_count{stage=\"capture_peers\"} 1"
        ));
        assert!(metrics.contains(
            "simgit_session_commit_stage_duration_seconds_count{stage=\"conflict_scan\"} 1"
        ));
        assert!(metrics
            .contains("simgit_session_commit_stage_duration_seconds_count{stage=\"flatten\"} 1"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_succeeds_for_non_overlapping_byte_ranges() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-r1".into(),
                Some("agent-r1".into()),
                "HEAD".into(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create s1");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init s1");
        state
            .deltas
            .write_blob(
                s1.session_id,
                Path::new("hotspot.log"),
                b"aaaaaaaaaa",
                Some(ByteRange { offset: 0, len: 10 }),
            )
            .expect("write s1");

        let s2 = state
            .sessions
            .create(
                "task-r2".into(),
                Some("agent-r2".into()),
                "HEAD".into(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create s2");
        state
            .deltas
            .init_session(s2.session_id, &s2.base_commit)
            .expect("init s2");
        state
            .deltas
            .write_blob(
                s2.session_id,
                Path::new("hotspot.log"),
                b"bbbbbbbbbb",
                Some(ByteRange {
                    offset: 10,
                    len: 10,
                }),
            )
            .expect("write s2");

        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": "feat/range-noconflict",
                "message": "non-overlapping ranges",
            }),
        )
        .await;

        assert!(
            res.is_ok(),
            "adjacent byte ranges must not conflict: {:?}",
            res.err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_conflicts_for_overlapping_byte_ranges() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-o1".into(),
                Some("agent-o1".into()),
                "HEAD".into(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create s1");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init s1");
        state
            .deltas
            .write_blob(
                s1.session_id,
                Path::new("hotspot.log"),
                b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some(ByteRange { offset: 0, len: 50 }),
            )
            .expect("write s1");

        let s2 = state
            .sessions
            .create(
                "task-o2".into(),
                Some("agent-o2".into()),
                "HEAD".into(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create s2");
        state
            .deltas
            .init_session(s2.session_id, &s2.base_commit)
            .expect("init s2");
        state
            .deltas
            .write_blob(
                s2.session_id,
                Path::new("hotspot.log"),
                b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                Some(ByteRange {
                    offset: 25,
                    len: 50,
                }),
            )
            .expect("write s2");

        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": "feat/range-conflict",
                "message": "overlapping ranges",
            }),
        )
        .await;

        assert!(res.is_err(), "overlapping byte ranges must conflict");
        assert_eq!(res.unwrap_err().code, simgit_sdk::ERR_MERGE_CONFLICT);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_auto_merges_clean_overlapping_writes() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-m1".into(),
                Some("agent-m1".into()),
                "HEAD".into(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create s1");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init s1");
        state
            .deltas
            .write_blob(
                s1.session_id,
                Path::new("merge.txt"),
                b"line1-ours\nline2\nline3\n",
                None,
            )
            .expect("write s1");

        let s2 = state
            .sessions
            .create(
                "task-m2".into(),
                Some("agent-m2".into()),
                "HEAD".into(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create s2");
        state
            .deltas
            .init_session(s2.session_id, &s2.base_commit)
            .expect("init s2");
        state
            .deltas
            .write_blob(
                s2.session_id,
                Path::new("merge.txt"),
                b"line1\nline2\nline3-theirs\n",
                None,
            )
            .expect("write s2");

        let branch = "feat/auto-merge-clean-overlap";
        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": branch,
                "message": "auto-merge clean overlap",
            }),
        )
        .await;
        assert!(
            res.is_ok(),
            "clean overlapping writes should auto-merge: {:?}",
            res.err()
        );

        let merged = run_git_output(
            &state.config.repo_path,
            &["show", &format!("{}:merge.txt", branch)],
        );
        assert_eq!(merged, "line1-ours\nline2\nline3-theirs");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_deadline_exceeded_before_lock() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-timeout-lock".to_owned(),
                Some("agent-timeout-lock".to_owned()),
                "HEAD".to_owned(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create session");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init delta");
        state
            .deltas
            .write_blob(
                s1.session_id,
                Path::new("README.md"),
                b"timeout-lock\n",
                None,
            )
            .expect("write blob");

        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": "feat/deadline-pre-lock",
                "message": "deadline pre-lock",
                "deadline_epoch_ms": 1,
            }),
        )
        .await;

        assert!(res.is_err(), "deadline must fail before lock");
        let err = res.expect_err("deadline should fail");
        assert_eq!(err.code, simgit_sdk::ERR_DEADLINE_EXCEEDED);
        let data = err.data.expect("deadline payload");
        assert_eq!(data["phase"], "pre_lock");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn session_commit_deadline_exceeded_before_flatten_after_queue_wait() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-timeout-flatten".to_owned(),
                Some("agent-timeout-flatten".to_owned()),
                "HEAD".to_owned(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create session");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init delta");
        state
            .deltas
            .write_blob(
                s1.session_id,
                Path::new("README.md"),
                b"timeout-flatten\n",
                None,
            )
            .expect("write blob");

        let guard = state
            .commit_scheduler
            .begin_commit(
                &[PathBuf::from("README.md")],
                Some(std::time::Duration::from_secs(30)),
            )
            .await
            .expect("acquire contested path");

        let releaser = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            drop(guard);
        });

        let deadline = now_epoch_ms() + 50;
        let res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": "feat/deadline-pre-flatten",
                "message": "deadline pre-flatten",
                "deadline_epoch_ms": deadline,
            }),
        )
        .await;

        let _ = releaser.await;

        assert!(res.is_err(), "deadline must fail before flatten");
        let err = res.expect_err("deadline should fail");
        assert_eq!(err.code, simgit_sdk::ERR_DEADLINE_EXCEEDED);
        let data = err.data.expect("deadline payload");
        assert_eq!(data["phase"], "pre_flatten");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn commit_status_returns_not_found_for_unknown_request() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-status-miss".to_owned(),
                Some("agent-status-miss".to_owned()),
                "HEAD".to_owned(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create session");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init delta");

        let request_id = Uuid::new_v4();
        let res = commit_status(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "request_id": request_id,
            }),
        )
        .await
        .expect("status response");

        assert_eq!(res["state"], "NOT_FOUND");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn commit_status_returns_success_for_completed_request() {
        let (state, root) = build_state_for_commit_tests().await;

        let s1 = state
            .sessions
            .create(
                "task-status-hit".to_owned(),
                Some("agent-status-hit".to_owned()),
                "HEAD".to_owned(),
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create session");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init delta");
        state
            .deltas
            .write_blob(s1.session_id, Path::new("README.md"), b"status-hit\n", None)
            .expect("write blob");

        let request_id = Uuid::new_v4();
        let commit_res = session_commit(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "branch_name": "feat/status-hit",
                "message": "status hit",
                "request_id": request_id,
            }),
        )
        .await;
        assert!(commit_res.is_ok(), "commit should succeed");

        let status_res = commit_status(
            &state,
            serde_json::json!({
                "session_id": s1.session_id,
                "request_id": request_id,
            }),
        )
        .await
        .expect("status response");

        assert_eq!(status_res["state"], "SUCCESS");
        assert_eq!(status_res["result"]["branch_name"], "feat/status-hit");

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
                &state.config.mnt_dir,
                false,
                8,
                None,
            )
            .expect("create session");
        state
            .deltas
            .init_session(s1.session_id, &s1.base_commit)
            .expect("init delta");

        state
            .deltas
            .write_blob(
                s1.session_id,
                Path::new("README.md"),
                b"updated-readme\n",
                None,
            )
            .expect("write README");
        state
            .deltas
            .mark_deleted(s1.session_id, Path::new("src.txt"))
            .expect("delete src.txt");
        state
            .deltas
            .record_rename(
                s1.session_id,
                Path::new("merge.txt"),
                Path::new("merge_renamed.txt"),
            )
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
        assert!(
            res.is_ok(),
            "flatten e2e commit should succeed: {:?}",
            res.err()
        );

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
        assert_ne!(
            old_path_status, 0,
            "merge.txt should be absent after rename"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
