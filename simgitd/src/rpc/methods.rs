//! Dispatch table: one async fn per JSON-RPC method.

use std::path::PathBuf;
use std::sync::Arc;
use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::{
    RpcError, SessionStatus,
    ERR_BORROW_CONFLICT, ERR_MERGE_CONFLICT, ERR_QUOTA_EXCEEDED, ERR_SESSION_NOT_FOUND,
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
        "lock.acquire"   => lock_acquire(state, params).await,
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
    let manifest = state.deltas.load_manifest(session_id).map_err(internal)?;

    // Pre-commit conflict check against other active sessions.
    let this_changed = changed_paths_set(&manifest);
    let mut conflicts = Vec::new();
    for peer in state.sessions.list(Some(SessionStatus::Active)) {
        if peer.session_id == session_id {
            continue;
        }
        let peer_manifest = match state.deltas.load_manifest(peer.session_id) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let peer_changed = changed_paths_set(&peer_manifest);
        let overlap = overlap_paths(&this_changed, &peer_changed);
        if !overlap.is_empty() {
            conflicts.push(serde_json::json!({
                "session_id": peer.session_id,
                "task_id": peer.task_id,
                "paths": overlap,
            }));
        }
    }
    if !conflicts.is_empty() {
        return Err(RpcError {
            code: ERR_MERGE_CONFLICT,
            message: "pre-commit conflict: overlapping active session paths".to_owned(),
            data: Some(serde_json::json!({
                "session_id": session_id,
                "conflicts": conflicts,
            })),
        });
    }

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

fn overlap_paths(a: &BTreeSet<PathBuf>, b: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    a.intersection(b).cloned().collect()
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
        .output()?;

    let _ = std::fs::remove_dir_all(&tmp);

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
            return Ok(serde_json::json!({
                "acquired": true,
                "waited_ms": start.elapsed().as_millis() as u64,
            }));
        }
        if tokio::time::Instant::now() >= deadline {
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
    use super::{changed_paths_set, diff_bytes_for_path, overlap_paths};
    use super::session_commit;
    use crate::borrow::BorrowRegistry;
    use crate::config::{Config, VfsBackend};
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
            vfs_backend: VfsBackend::NfsLoopback,
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
        ));

        let state = Arc::new(AppState {
            config: cfg,
            sessions,
            borrows,
            deltas,
            events,
            vfs,
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
            .write_blob(s1.session_id, Path::new("README.md"), b"change-1\n")
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
            .write_blob(s2.session_id, Path::new("README.md"), b"change-2\n")
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
            .write_blob(s1.session_id, Path::new("README.md"), b"change-a\n")
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
            .write_blob(s2.session_id, Path::new("src.txt"), b"change-b\n")
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

        let _ = std::fs::remove_dir_all(&root);
    }
}
