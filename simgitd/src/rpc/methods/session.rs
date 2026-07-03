//! `session.create`, `session.abort`, `session.list`, `session.diff` and the
//! diffing/manifest helpers they share.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::{RpcError, SessionStatus, ERR_QUOTA_EXCEEDED};

use crate::daemon::AppState;

use super::support::{internal, not_found, resolve_head, str_field, uuid_field};

// ── session.create ────────────────────────────────────────────────────────────

pub(super) async fn session_create(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let task_id: String = str_field(&p, "task_id")?;
    let agent_label: Option<String> = p["agent_label"].as_str().map(str::to_owned);
    let base_commit: Option<String> = p["base_commit"].as_str().map(str::to_owned);
    let peers: bool = p["peers"].as_bool().unwrap_or(false);
    let initial_branch: Option<String> = p["initial_branch"].as_str().map(str::to_owned);

    // Resolve base commit (default = HEAD).
    let base_commit = base_commit.unwrap_or_else(|| resolve_head(&state.config.repo_path));

    let info = state
        .sessions
        .create(
            task_id,
            agent_label,
            base_commit.clone(),
            &state.config.mnt_dir,
            peers,
            state.config.max_sessions,
            initial_branch.clone(),
        )
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("max sessions") {
                RpcError {
                    code: ERR_QUOTA_EXCEEDED,
                    message: msg,
                    data: None,
                }
            } else {
                internal(msg)
            }
        })?;

    // Initialise delta store.
    state
        .deltas
        .init_session(info.session_id, &base_commit)
        .map_err(internal)?;

    // Mount VFS.
    state.vfs.mount(&info).await.map_err(internal)?;

    serde_json::to_value(&info).map_err(internal)
}

// ── session.set-base ──────────────────────────────────────────────────────────

pub(super) async fn session_set_base(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let session_id = uuid_field(&p, "session_id")?;
    let new_base = str_field(&p, "commit")?;

    // Update the session's base commit.
    state
        .sessions
        .set_base_commit(session_id, &new_base)
        .map_err(|e| RpcError {
            code: -32603,
            message: e.to_string(),
            data: None,
        })?;

    // Notify the VFS backend that the base commit changed so it serves
    // files from the new tree.  Must happen *before* reset_session so
    // NFS reads from the new tree when deltas are cleared below.
    state.vfs.update_base_commit(session_id, &new_base);

    // Clear the delta manifest — the working tree now reflects the new base.
    // Deltas written by git checkout to carry over file differences are
    // now stale because the VFS reads from the new tree directly.
    if let Err(e) = state.deltas.reset_session(session_id, &new_base) {
        tracing::warn!(session = %session_id, err = %e, "failed to reset delta for new base");
    }

    // Refresh the session's git refs from the real repo so new branches,
    // tags, and remote refs are visible after checkout / merge / rebase.
    if let Some(info) = state.sessions.get(session_id) {
        // Compute the git data dir the same way the NFS backend does:
        // the mount path has a sibling `.git` directory (mount_path.git/.git/).
        let git_data_dir = {
            let mut p = info.mount_path.clone();
            let _ = p.set_extension("git");
            p.join(".git")
        };
        let real_git = state.config.repo_path.join(".git");

        let session_refs = git_data_dir.join("refs");
        let real_refs = real_git.join("refs");
        if real_refs.exists() {
            if session_refs.exists() || session_refs.is_symlink() {
                let _ = std::fs::remove_dir_all(&session_refs);
            }
            use crate::git_proxy::copy_dir_all;
            if let Err(e) = copy_dir_all(&real_refs, &session_refs) {
                tracing::warn!(session = %session_id, err = %e, "failed to refresh session refs in set-base");
            }
        }

        // Also refresh packed-refs.
        let real_packed = real_git.join("packed-refs");
        let session_packed = git_data_dir.join("packed-refs");
        if real_packed.exists() {
            let _ = std::fs::copy(&real_packed, &session_packed);
        }

        // Re-init index for the new base so git status is accurate.
        let index_init = std::process::Command::new("git")
            .env("GIT_DIR", &git_data_dir)
            .args(["read-tree", &new_base])
            .output();
        if let Err(ref e) = index_init {
            tracing::warn!(session = %session_id, err = %e, "git read-tree failed during set-base");
        }
    }

    Ok(serde_json::json!({ "ok": true, "base_commit": new_base }))
}

// ── session.abort ─────────────────────────────────────────────────────────────

pub(super) async fn session_abort(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let session_id = uuid_field(&p, "session_id")?;
    let _ = state
        .sessions
        .get(session_id)
        .ok_or_else(|| not_found(session_id))?;

    state.borrows.release_session(session_id);
    state.vfs.unmount_session(session_id).await;
    let _ = state.deltas.purge_session(session_id);
    state.sessions.mark_aborted(session_id).map_err(internal)?;

    Ok(serde_json::json!({ "ok": true }))
}

// ── session.list ──────────────────────────────────────────────────────────────

pub(super) async fn session_list(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let status: Option<SessionStatus> = p["status"]
        .as_str()
        .and_then(|s| serde_json::from_value(serde_json::Value::String(s.to_owned())).ok());
    let sessions = state.sessions.list(status);
    serde_json::to_value(sessions).map_err(internal)
}

// ── session.diff ──────────────────────────────────────────────────────────────

pub(super) async fn session_diff(
    state: &Arc<AppState>,
    p: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
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
    )
    .map_err(internal)?;

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

pub(super) fn read_base_blob(repo_path: &Path, base_commit: &str, path: &Path) -> Option<Vec<u8>> {
    let repo = gix::open(repo_path).ok()?;
    let spec = format!("{}:{}", base_commit, path.to_string_lossy());
    let id = repo.rev_parse_single(spec.as_str()).ok()?;
    let obj = id.object().ok()?;
    let blob = obj.try_into_blob().ok()?;
    Some(blob.data.to_vec())
}

pub(super) fn read_delta_blob(
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

pub(super) fn changed_path_ops(
    manifest: &crate::delta::store::DeltaManifest,
) -> std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>> {
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

pub(super) fn ops_for_path(
    ops: &std::collections::BTreeMap<PathBuf, BTreeSet<&'static str>>,
    path: &Path,
) -> Vec<&'static str> {
    ops.get(path)
        .map(|set| set.iter().copied().collect())
        .unwrap_or_default()
}

fn diff_bytes_for_path(path: &Path, old: Option<&[u8]>, new: Option<&[u8]>) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!(
        "simgit-diff-{}-{}",
        std::process::id(),
        Uuid::now_v7()
    ));
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

#[cfg(test)]
fn overlap_paths(a: &BTreeSet<PathBuf>, b: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    a.intersection(b).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::{
        changed_path_ops, changed_paths_set, diff_bytes_for_path, ops_for_path, overlap_paths,
    };
    use crate::delta::store::DeltaManifest;
    use std::path::Path;

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
        let patch = diff_bytes_for_path(Path::new("README.md"), Some(b"line1\nline2\n"), Some(&[]))
            .expect("diff generation should succeed");

        assert!(patch.contains("diff --git"));
        assert!(patch.contains("-line1"));
        assert!(patch.contains("-line2"));
    }

    #[test]
    fn unified_diff_for_added_file_shows_added_lines() {
        let patch = diff_bytes_for_path(Path::new("new.txt"), Some(&[]), Some(b"hello\n"))
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
}
