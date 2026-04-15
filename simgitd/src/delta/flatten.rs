//! Flatten a session's delta layer into a new git branch.
//!
//! Phase 4 implementation: calls the git CLI to create a branch from the
//! in-memory delta. The pure gitoxide tree-rewrite implementation will replace
//! this in Phase 7 once the gix object-write API stabilises.
//!
//! Algorithm:
//!   1. git checkout -B <branch> <base_commit>  (in a temp worktree)
//!   2. Apply delta writes/deletes/renames to that worktree.
//!   3. git add -A && git commit -m <message>
//!   4. Capture commit OID; clean up temp worktree.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{info, warn};

use crate::delta::store::DeltaManifest;

/// Flatten engine selector.
///
/// Runtime remains on `GitCli` today. `GixScaffold` exists so Phase 7 work can
/// incrementally migrate tree construction without changing the public flatten API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlattenEngine {
    GitCli,
    Gix,
}

/// Current active flatten engine.
pub fn active_flatten_engine() -> FlattenEngine {
    match std::env::var("SIMGIT_FLATTEN_ENGINE") {
        Ok(v) if v.eq_ignore_ascii_case("gix") => FlattenEngine::Gix,
        _ => FlattenEngine::GitCli,
    }
}

/// Phase 7 migration checklist for the future pure-gix implementation.
pub const GIX_MIGRATION_STEPS: [&str; 4] = [
    "Load base commit tree from gix object database",
    "Apply manifest operations (write/delete/rename) to in-memory tree",
    "Write new tree + commit objects via gix",
    "Update branch ref atomically and preserve flatten error taxonomy",
];

pub struct FlattenResult {
    pub commit_oid:  String,
    pub branch_name: String,
    pub worktree_add_secs: f64,
    pub apply_manifest_secs: f64,
    pub git_add_secs: f64,
    pub checkout_branch_secs: f64,
    pub git_commit_secs: f64,
    pub worktree_remove_secs: f64,
}

#[derive(Debug, Error)]
pub enum FlattenError {
    #[error("io error during {step}: {source}")]
    Io {
        step: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("missing delta blob for {path} ({hash})")]
    MissingBlob {
        path: PathBuf,
        hash: String,
    },
    #[error("git step `{step}` failed (status {status}): {stderr}")]
    GitCommand {
        step: &'static str,
        status: i32,
        stderr: String,
        args: Vec<String>,
    },
    #[error("path traversal rejected: manifest path '{path}' must be relative and must not contain '..' components")]
    PathTraversal {
        path: PathBuf,
    },
}

/// Validate that `rel` is a safe relative path that cannot escape the base dir.
///
/// Rejects:
/// - Absolute paths (e.g. `/etc/passwd`)
/// - Paths containing `..` components (e.g. `../../secrets`)
fn safe_join(base: &Path, rel: &Path) -> Result<PathBuf, FlattenError> {
    if rel.is_absolute() {
        return Err(FlattenError::PathTraversal { path: rel.to_owned() });
    }
    for component in rel.components() {
        if component == std::path::Component::ParentDir {
            return Err(FlattenError::PathTraversal { path: rel.to_owned() });
        }
    }
    Ok(base.join(rel))
}

pub fn flatten(
    repo_path:        &Path,
    base_commit:      &str,
    manifest:         &DeltaManifest,
    delta_store_root: &Path,
    session_id:       uuid::Uuid,
    branch_name:      &str,
    message:          &str,
) -> Result<FlattenResult, FlattenError> {
    match active_flatten_engine() {
        FlattenEngine::GitCli => {}
        FlattenEngine::Gix => {
            // Parallel implementation entry point for pure-gix flatten.
            // We intentionally keep GitCli as the default until gix write-path
            // parity is proven under stress.
            return flatten_gix_tree_construction(
                repo_path,
                base_commit,
                manifest,
                delta_store_root,
                session_id,
                branch_name,
                message,
            );
        }
    }

    // ── 1. Prepare persistent per-session worktree ────────────────────────
    let reuse_worktree = worktree_reuse_enabled();
    let wt_path = if reuse_worktree {
        worktree_path(delta_store_root, session_id)
    } else {
        ephemeral_worktree_path(delta_store_root, session_id)
    };
    let step_started = std::time::Instant::now();
    prepare_worktree(repo_path, &wt_path, base_commit, reuse_worktree)?;
    let worktree_add_secs = step_started.elapsed().as_secs_f64();

    // ── 2. Apply the delta manifest ───────────────────────────────────────
    let step_started = std::time::Instant::now();
    let objects_dir = delta_store_root.join(session_id.to_string()).join("objects");

    // Deletes.
    for del in &manifest.deletes {
        let target = safe_join(&wt_path, del)?;
        if target.exists() {
            std::fs::remove_file(&target)
                .map_err(|source| FlattenError::Io {
                    step: "apply_delete",
                    source,
                })?;
        }
    }

    // Renames.
    for (from, to) in &manifest.renames {
        let src = safe_join(&wt_path, from)?;
        let dst = safe_join(&wt_path, to)?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|source| FlattenError::Io {
                step: "apply_rename_create_parent",
                source,
            })?;
        }
        if src.exists() {
            std::fs::rename(&src, &dst).map_err(|source| FlattenError::Io {
                step: "apply_rename",
                source,
            })?;
        }
    }

    // Writes.
    for (rel_path, hash) in &manifest.writes {
        let bucket   = &hash[..2];
        let blob_path = objects_dir.join(bucket).join(&hash[2..]);
        let content = std::fs::read(&blob_path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                FlattenError::MissingBlob {
                    path: rel_path.clone(),
                    hash: hash.clone(),
                }
            } else {
                FlattenError::Io {
                    step: "read_delta_blob",
                    source,
                }
            }
        })?;
        let dest = safe_join(&wt_path, rel_path)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|source| FlattenError::Io {
                step: "apply_write_create_parent",
                source,
            })?;
        }
        std::fs::write(&dest, &content).map_err(|source| FlattenError::Io {
            step: "apply_write",
            source,
        })?;
    }
    let apply_manifest_secs = step_started.elapsed().as_secs_f64();

    // ── 3. Stage and commit ───────────────────────────────────────────────
    let step_started = std::time::Instant::now();
    run_git(&wt_path, &["add", "-A"], "stage_changes")?;
    let git_add_secs = step_started.elapsed().as_secs_f64();

    let step_started = std::time::Instant::now();
    run_git(&wt_path, &["checkout", "-B", branch_name], "checkout_branch")?;
    let checkout_branch_secs = step_started.elapsed().as_secs_f64();

    let step_started = std::time::Instant::now();
    run_git(&wt_path, &["commit", "--allow-empty", "-m", message], "git_commit")?;
    let git_commit_secs = step_started.elapsed().as_secs_f64();

    // Extract commit OID.
    let commit_oid = resolve_head_oid(&wt_path)?;

    // ── 4. Post-commit worktree hygiene ───────────────────────────────────
    // Keep the per-session worktree for potential retry/reuse; ensure it is
    // clean for the next attempt. Best-effort by design.
    let step_started = std::time::Instant::now();
    if reuse_worktree {
        let _ = clear_git_locks(&wt_path);
        let _ = run_git(&wt_path, &["reset", "--hard", "HEAD"], "worktree_post_commit_reset");
        let _ = run_git(&wt_path, &["clean", "-fd"], "worktree_post_commit_clean");
    } else {
        let _ = run_git(
            repo_path,
            &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
            "worktree_remove",
        );
        let _ = std::fs::remove_dir_all(&wt_path);
    }
    let worktree_remove_secs = step_started.elapsed().as_secs_f64();

    info!(commit = %commit_oid, branch = branch_name, "flatten complete");
    Ok(FlattenResult {
        commit_oid,
        branch_name: branch_name.to_owned(),
        worktree_add_secs,
        apply_manifest_secs,
        git_add_secs,
        checkout_branch_secs,
        git_commit_secs,
        worktree_remove_secs,
    })
}

fn flatten_gix_tree_construction(
    repo_path: &Path,
    base_commit: &str,
    manifest: &DeltaManifest,
    delta_store_root: &Path,
    session_id: uuid::Uuid,
    branch_name: &str,
    message: &str,
) -> Result<FlattenResult, FlattenError> {
    use gix::object::tree::EntryKind;
    use gix::refs::transaction::PreviousValue;

    // Tree-construction-first migration entry point: no worktree checkout,
    // all edits are applied to an in-memory tree editor and written directly.
    let worktree_add_secs = 0.0;
    let repo = gix::open(repo_path).map_err(|err| FlattenError::GitCommand {
        step: "gix_open_repo",
        status: -1,
        stderr: err.to_string(),
        args: vec![repo_path.display().to_string()],
    })?;

    let base_id = repo
        .rev_parse_single(base_commit)
        .map_err(|err| FlattenError::GitCommand {
            step: "gix_resolve_base_commit",
            status: -1,
            stderr: err.to_string(),
            args: vec![base_commit.to_owned()],
        })?;

    let base_commit_id = base_id.detach();
    let base_commit_obj = base_id
        .object()
        .map_err(|err| FlattenError::GitCommand {
            step: "gix_load_base_commit_object",
            status: -1,
            stderr: err.to_string(),
            args: vec![base_commit.to_owned()],
        })?
        .try_into_commit()
        .map_err(|err| FlattenError::GitCommand {
            step: "gix_base_object_to_commit",
            status: -1,
            stderr: err.to_string(),
            args: vec![base_commit.to_owned()],
        })?;

    let base_tree_id = base_commit_obj
        .tree_id()
        .map_err(|err| FlattenError::GitCommand {
            step: "gix_base_commit_tree_id",
            status: -1,
            stderr: err.to_string(),
            args: vec![base_commit.to_owned()],
        })?
        .detach();

    let mut editor = repo.edit_tree(base_tree_id).map_err(|err| FlattenError::GitCommand {
        step: "gix_edit_tree",
        status: -1,
        stderr: err.to_string(),
        args: vec![base_tree_id.to_string()],
    })?;

    let step_started = std::time::Instant::now();
    let objects_dir = delta_store_root.join(session_id.to_string()).join("objects");

    // Deletes.
    for del in &manifest.deletes {
        validate_manifest_path(del)?;
        let del_path = del.to_string_lossy().to_string();
        editor.remove(&del_path).map_err(|err| FlattenError::GitCommand {
            step: "gix_apply_delete",
            status: -1,
            stderr: err.to_string(),
            args: vec![del.display().to_string()],
        })?;
    }

    // Renames.
    for (from, to) in &manifest.renames {
        validate_manifest_path(from)?;
        validate_manifest_path(to)?;
        let from_s = from.to_string_lossy().to_string();
        let to_s = to.to_string_lossy().to_string();

        // Preserve existing object content for pure rename operations.
        // If `to` is also in writes, the later write upsert takes precedence.
        if !manifest.writes.contains_key(to) {
            if let Some(entry) = editor.get(&from_s) {
                editor
                    .upsert(&to_s, entry.kind(), entry.object_id())
                    .map_err(|err| FlattenError::GitCommand {
                        step: "gix_apply_rename_upsert",
                        status: -1,
                        stderr: err.to_string(),
                        args: vec![from.display().to_string(), to.display().to_string()],
                    })?;
            }
        }

        editor.remove(&from_s).map_err(|err| FlattenError::GitCommand {
            step: "gix_apply_rename_remove_from",
            status: -1,
            stderr: err.to_string(),
            args: vec![from.display().to_string(), to.display().to_string()],
        })?;
    }

    // Writes.
    for (rel_path, hash) in &manifest.writes {
        validate_manifest_path(rel_path)?;
        let bucket = &hash[..2];
        let blob_path = objects_dir.join(bucket).join(&hash[2..]);
        let content = std::fs::read(&blob_path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                FlattenError::MissingBlob {
                    path: rel_path.clone(),
                    hash: hash.clone(),
                }
            } else {
                FlattenError::Io {
                    step: "gix_read_delta_blob",
                    source,
                }
            }
        })?;

        let blob_id = repo.write_blob(&content).map_err(|err| FlattenError::GitCommand {
            step: "gix_write_blob",
            status: -1,
            stderr: err.to_string(),
            args: vec![rel_path.display().to_string()],
        })?;

        let rel = rel_path.to_string_lossy().to_string();
        let kind = editor
            .get(&rel)
            .map(|entry| match entry.kind() {
                EntryKind::BlobExecutable => EntryKind::BlobExecutable,
                EntryKind::Link => EntryKind::Link,
                _ => EntryKind::Blob,
            })
            .unwrap_or(EntryKind::Blob);
        editor
            .upsert(&rel, kind, blob_id.detach())
            .map_err(|err| FlattenError::GitCommand {
                step: "gix_apply_write_upsert",
                status: -1,
                stderr: err.to_string(),
                args: vec![rel_path.display().to_string()],
            })?;
    }
    let apply_manifest_secs = step_started.elapsed().as_secs_f64();

    let step_started = std::time::Instant::now();
    let tree_id = editor.write().map_err(|err| FlattenError::GitCommand {
        step: "gix_write_tree",
        status: -1,
        stderr: err.to_string(),
        args: vec![],
    })?;
    let git_add_secs = step_started.elapsed().as_secs_f64();

    let step_started = std::time::Instant::now();
    let commit_obj = repo
        .new_commit(message, tree_id.detach(), [base_commit_id])
        .map_err(|err| FlattenError::GitCommand {
            step: "gix_new_commit",
            status: -1,
            stderr: err.to_string(),
            args: vec![base_commit.to_owned()],
        })?;
    let commit_id = commit_obj.id().detach();
    let git_commit_secs = step_started.elapsed().as_secs_f64();

    let step_started = std::time::Instant::now();
    let ref_name = format!("refs/heads/{branch_name}");
    repo.reference(
        ref_name.as_str(),
        commit_id,
        PreviousValue::Any,
        format!("simgit flatten: {message}"),
    )
    .map_err(|err| FlattenError::GitCommand {
        step: "gix_update_branch_ref",
        status: -1,
        stderr: err.to_string(),
        args: vec![ref_name],
    })?;
    let checkout_branch_secs = step_started.elapsed().as_secs_f64();

    let worktree_remove_secs = 0.0;
    info!(commit = %commit_id, branch = branch_name, "flatten complete (gix)");

    Ok(FlattenResult {
        commit_oid: commit_id.to_string(),
        branch_name: branch_name.to_owned(),
        worktree_add_secs,
        apply_manifest_secs,
        git_add_secs,
        checkout_branch_secs,
        git_commit_secs,
        worktree_remove_secs,
    })
}

fn validate_manifest_path(path: &Path) -> Result<(), FlattenError> {
    // Reuse existing traversal checks to keep both engines aligned on safety.
    let _ = safe_join(Path::new("."), path)?;
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn worktree_path(delta_store_root: &Path, session_id: uuid::Uuid) -> PathBuf {
    delta_store_root.join(session_id.to_string()).join("wt")
}

fn ephemeral_worktree_path(delta_store_root: &Path, session_id: uuid::Uuid) -> PathBuf {
    delta_store_root.join(format!("{session_id}-wt-{}", uuid::Uuid::now_v7()))
}

fn worktree_reuse_enabled() -> bool {
    std::env::var("SIMGIT_FLATTEN_REUSE_WORKTREE")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

fn prepare_worktree(
    repo_path: &Path,
    wt_path: &Path,
    base_commit: &str,
    reuse_worktree: bool,
) -> Result<(), FlattenError> {
    if !reuse_worktree {
        if let Some(parent) = wt_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| FlattenError::Io {
                step: "create_worktree_parent_dir",
                source,
            })?;
        }
        run_git(
            repo_path,
            &["worktree", "add", "--detach", &wt_path.to_string_lossy(), base_commit],
            "worktree_add",
        )?;
        info!(worktree = %wt_path.display(), "flatten worktree add executed");
        return Ok(());
    }

    if wt_path.exists() && !wt_path.join(".git").exists() {
        std::fs::remove_dir_all(wt_path).map_err(|source| FlattenError::Io {
            step: "remove_stale_worktree_dir",
            source,
        })?;
    }

    if wt_path.join(".git").exists() {
        match prepare_existing_worktree(wt_path, base_commit) {
            Ok(()) => {
                info!(worktree = %wt_path.display(), "flatten worktree reuse executed");
                return Ok(());
            }
            Err(err) => {
                warn!(worktree = %wt_path.display(), error = %err, "worktree reuse prepare failed; falling back to fresh worktree");
            }
        }
        // Fallback path: if reuse preparation fails, remove poisoned worktree
        // and recreate from scratch.
        force_remove_worktree(repo_path, wt_path);
    }

    if let Some(parent) = wt_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| FlattenError::Io {
            step: "create_worktree_parent_dir",
            source,
        })?;
    }
    run_git(
        repo_path,
        &["worktree", "add", "--detach", &wt_path.to_string_lossy(), base_commit],
        "worktree_add",
    )?;
    info!(worktree = %wt_path.display(), "flatten worktree add executed");
    Ok(())
}

fn prepare_existing_worktree(wt_path: &Path, base_commit: &str) -> Result<(), FlattenError> {
    clear_git_locks(wt_path)?;

    let _ = run_git_output(wt_path, &["rev-parse", "--is-inside-work-tree"], "worktree_validate")?;
    let head = resolve_head_oid(wt_path)?;
    let status = run_git_output(wt_path, &["status", "--porcelain"], "worktree_status")?;

    if head == base_commit && status.trim().is_empty() {
        return Ok(());
    }

    run_git(
        wt_path,
        &["reset", "--hard", base_commit],
        "worktree_reuse_reset",
    )?;
    run_git(wt_path, &["clean", "-fd"], "worktree_reuse_clean")?;
    Ok(())
}

fn force_remove_worktree(repo_path: &Path, wt_path: &Path) {
    let _ = run_git(
        repo_path,
        &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        "worktree_remove_poisoned",
    );
    let _ = run_git(
        repo_path,
        &["worktree", "prune", "--expire=now"],
        "worktree_prune_poisoned",
    );
    let _ = std::fs::remove_dir_all(wt_path);
}

fn clear_git_locks(wt_path: &Path) -> Result<(), FlattenError> {
    // Worktree-local index lock.
    let index_lock = wt_path.join(".git").join("index.lock");
    if index_lock.exists() {
        std::fs::remove_file(&index_lock).map_err(|source| FlattenError::Io {
            step: "clear_index_lock",
            source,
        })?;
    }

    // Worktree metadata locks under the resolved gitdir.
    let gitdir = resolve_worktree_gitdir(wt_path)?;
    for lock in [gitdir.join("HEAD.lock"), gitdir.join("index.lock")] {
        if lock.exists() {
            std::fs::remove_file(&lock).map_err(|source| FlattenError::Io {
                step: "clear_worktree_lock",
                source,
            })?;
        }
    }

    Ok(())
}

fn resolve_worktree_gitdir(wt_path: &Path) -> Result<PathBuf, FlattenError> {
    let dot_git = wt_path.join(".git");
    if dot_git.is_dir() {
        return Ok(dot_git);
    }
    let content = std::fs::read_to_string(&dot_git).map_err(|source| FlattenError::Io {
        step: "read_dot_git",
        source,
    })?;
    let Some(raw) = content.strip_prefix("gitdir:") else {
        return Ok(dot_git);
    };
    let raw = raw.trim();
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(wt_path.join(path))
    }
}

fn run_git(cwd: &Path, args: &[&str], step: &'static str) -> Result<(), FlattenError> {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|source| FlattenError::Io { step, source })?;
    if !out.status.success() {
        return Err(FlattenError::GitCommand {
            step,
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            args: args.iter().map(|s| s.to_string()).collect(),
        });
    }
    Ok(())
}

fn run_git_output(cwd: &Path, args: &[&str], step: &'static str) -> Result<String, FlattenError> {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|source| FlattenError::Io { step, source })?;
    if !out.status.success() {
        return Err(FlattenError::GitCommand {
            step,
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            args: args.iter().map(|s| s.to_string()).collect(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn resolve_head_oid(cwd: &Path) -> Result<String, FlattenError> {
    let repo = gix::open(cwd).map_err(|err| FlattenError::GitCommand {
        step: "resolve_head",
        status: -1,
        stderr: err.to_string(),
        args: vec!["rev-parse".to_owned(), "HEAD".to_owned()],
    })?;
    let head = repo.head_id().map_err(|err| FlattenError::GitCommand {
        step: "resolve_head",
        status: -1,
        stderr: err.to_string(),
        args: vec!["rev-parse".to_owned(), "HEAD".to_owned()],
    })?;
    Ok(head.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::Path;

    #[test]
    fn flatten_engine_defaults_to_git_cli() {
        assert_eq!(active_flatten_engine(), FlattenEngine::GitCli);
        assert_eq!(GIX_MIGRATION_STEPS.len(), 4);
    }

    #[test]
    fn flatten_engine_selects_gix_from_env() {
        let key = "SIMGIT_FLATTEN_ENGINE";
        let prev = std::env::var(key).ok();

        unsafe { std::env::set_var(key, "gix"); }
        assert_eq!(active_flatten_engine(), FlattenEngine::Gix);

        match prev {
            Some(v) => unsafe { std::env::set_var(key, v); },
            None => unsafe { std::env::remove_var(key); },
        }
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        let base = Path::new("/worktree");
        let result = safe_join(base, Path::new("/etc/passwd"));
        assert!(
            matches!(result, Err(FlattenError::PathTraversal { .. })),
            "absolute paths must be rejected"
        );
    }

    #[test]
    fn safe_join_rejects_parent_dir_traversal() {
        let base = Path::new("/worktree");
        let result = safe_join(base, Path::new("../../etc/shadow"));
        assert!(
            matches!(result, Err(FlattenError::PathTraversal { .. })),
            "parent-dir traversal must be rejected"
        );
    }

    #[test]
    fn safe_join_rejects_embedded_parent_dir() {
        let base = Path::new("/worktree");
        let result = safe_join(base, Path::new("foo/../../../etc/passwd"));
        assert!(
            matches!(result, Err(FlattenError::PathTraversal { .. })),
            "embedded parent-dir components must be rejected"
        );
    }

    #[test]
    fn safe_join_accepts_normal_relative_path() {
        let base = Path::new("/worktree");
        let result = safe_join(base, Path::new("src/main.rs"));
        assert!(result.is_ok(), "normal relative paths must be accepted");
        assert_eq!(result.unwrap(), Path::new("/worktree/src/main.rs"));
    }

    #[test]
    fn safe_join_accepts_nested_relative_path() {
        let base = Path::new("/worktree");
        let result = safe_join(base, Path::new("a/b/c/d.txt"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Path::new("/worktree/a/b/c/d.txt"));
    }

    #[test]
    fn worktree_path_is_deterministic_per_session() {
        let root = Path::new("/tmp/state/deltas");
        let sid = uuid::Uuid::nil();
        let p = worktree_path(root, sid);
        assert_eq!(p, Path::new("/tmp/state/deltas/00000000-0000-0000-0000-000000000000/wt"));
    }

    #[test]
    fn ephemeral_worktree_path_is_unique_and_prefixed() {
        let root = Path::new("/tmp/state/deltas");
        let sid = uuid::Uuid::nil();
        let p1 = ephemeral_worktree_path(root, sid);
        let p2 = ephemeral_worktree_path(root, sid);
        assert_ne!(p1, p2);
        assert!(p1.to_string_lossy().contains("00000000-0000-0000-0000-000000000000-wt-"));
    }

    #[test]
    fn worktree_reuse_flag_parsing() {
        let key = "SIMGIT_FLATTEN_REUSE_WORKTREE";
        let prev = std::env::var(key).ok();

        unsafe { std::env::remove_var(key); }
        assert!(worktree_reuse_enabled());

        unsafe { std::env::set_var(key, "0"); }
        assert!(!worktree_reuse_enabled());

        unsafe { std::env::set_var(key, "false"); }
        assert!(!worktree_reuse_enabled());

        unsafe { std::env::set_var(key, "1"); }
        assert!(worktree_reuse_enabled());

        match prev {
            Some(v) => unsafe { std::env::set_var(key, v); },
            None => unsafe { std::env::remove_var(key); },
        }
    }

    #[test]
    fn resolve_worktree_gitdir_supports_file_indirection() {
        let tmp = std::env::temp_dir().join(format!("simgit-gitdir-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&tmp).expect("create tmp");
        let gitdir = tmp.join("actual-gitdir");
        fs::create_dir_all(&gitdir).expect("create gitdir");
        fs::write(tmp.join(".git"), format!("gitdir: {}\n", gitdir.display())).expect("write .git");

        let resolved = resolve_worktree_gitdir(&tmp).expect("resolve gitdir");
        assert_eq!(resolved, gitdir);

        let _ = fs::remove_dir_all(&tmp);
    }

    fn run_git_ok(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .expect("git command should execute");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn run_git_stdout(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("git command should execute");
        assert!(out.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn write_delta_blob(delta_root: &Path, session_id: uuid::Uuid, bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut hash = String::with_capacity(digest.len() * 2);
        for b in digest {
            use std::fmt::Write as _;
            let _ = write!(&mut hash, "{b:02x}");
        }
        let bucket = &hash[..2];
        let dir = delta_root.join(session_id.to_string()).join("objects").join(bucket);
        fs::create_dir_all(&dir).expect("create delta object dir");
        let path = dir.join(&hash[2..]);
        fs::write(path, bytes).expect("write delta blob");
        hash
    }

    #[test]
    fn gix_flatten_applies_write_delete_and_rename() {
        let key = "SIMGIT_FLATTEN_ENGINE";
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "gix"); }

        let root = std::env::temp_dir().join(format!("simgit-flatten-gix-{}", uuid::Uuid::now_v7()));
        let repo = root.join("repo");
        let delta_root = root.join("deltas");
        fs::create_dir_all(&repo).expect("create repo");
        fs::create_dir_all(&delta_root).expect("create deltas");

        run_git_ok(&repo, &["init"]);
        run_git_ok(&repo, &["config", "user.email", "tests@simgit.local"]);
        run_git_ok(&repo, &["config", "user.name", "simgit-tests"]);

        fs::write(repo.join("keep.txt"), b"base-keep\n").expect("write keep");
        fs::write(repo.join("delete.txt"), b"to-delete\n").expect("write delete");
        fs::write(repo.join("move.txt"), b"move-me\n").expect("write move");
        run_git_ok(&repo, &["add", "."]);
        run_git_ok(&repo, &["commit", "-m", "base"]);

        let base = run_git_stdout(&repo, &["rev-parse", "HEAD"]);
        let sid = uuid::Uuid::now_v7();
        let mut writes = HashMap::new();
        writes.insert(
            Path::new("keep.txt").to_path_buf(),
            write_delta_blob(&delta_root, sid, b"updated-keep\n"),
        );
        writes.insert(
            Path::new("new.txt").to_path_buf(),
            write_delta_blob(&delta_root, sid, b"new-file\n"),
        );

        let mut deletes = HashSet::new();
        deletes.insert(Path::new("delete.txt").to_path_buf());

        let manifest = DeltaManifest {
            base_commit: base.clone(),
            writes,
            deletes,
            renames: vec![(Path::new("move.txt").to_path_buf(), Path::new("moved.txt").to_path_buf())],
            ranges: HashMap::new(),
        };

        let result = flatten(
            &repo,
            &base,
            &manifest,
            &delta_root,
            sid,
            "simgit/test-gix",
            "gix flatten test",
        )
        .expect("gix flatten should succeed");

        let branch_oid = run_git_stdout(&repo, &["rev-parse", "refs/heads/simgit/test-gix"]);
        assert_eq!(branch_oid, result.commit_oid);
        assert_eq!(run_git_stdout(&repo, &["show", "refs/heads/simgit/test-gix:keep.txt"]), "updated-keep");
        assert_eq!(run_git_stdout(&repo, &["show", "refs/heads/simgit/test-gix:new.txt"]), "new-file");
        assert_eq!(run_git_stdout(&repo, &["show", "refs/heads/simgit/test-gix:moved.txt"]), "move-me");

        let deleted = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["cat-file", "-e", "refs/heads/simgit/test-gix:delete.txt"])
            .status()
            .expect("git cat-file should execute");
        assert!(!deleted.success(), "delete.txt should not exist in flattened branch");

        let _ = fs::remove_dir_all(&root);
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v); },
            None => unsafe { std::env::remove_var(key); },
        }
    }

    #[test]
    fn gix_flatten_preserves_executable_mode_on_write() {
        let key = "SIMGIT_FLATTEN_ENGINE";
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, "gix"); }

        let root = std::env::temp_dir().join(format!("simgit-flatten-gix-mode-{}", uuid::Uuid::now_v7()));
        let repo = root.join("repo");
        let delta_root = root.join("deltas");
        fs::create_dir_all(&repo).expect("create repo");
        fs::create_dir_all(&delta_root).expect("create deltas");

        run_git_ok(&repo, &["init"]);
        run_git_ok(&repo, &["config", "user.email", "tests@simgit.local"]);
        run_git_ok(&repo, &["config", "user.name", "simgit-tests"]);

        fs::write(repo.join("script.sh"), b"#!/bin/sh\necho one\n").expect("write script");
        run_git_ok(&repo, &["add", "script.sh"]);
        run_git_ok(&repo, &["update-index", "--chmod=+x", "script.sh"]);
        run_git_ok(&repo, &["commit", "-m", "base"]);

        let base = run_git_stdout(&repo, &["rev-parse", "HEAD"]);
        let sid = uuid::Uuid::now_v7();
        let mut writes = HashMap::new();
        writes.insert(
            Path::new("script.sh").to_path_buf(),
            write_delta_blob(&delta_root, sid, b"#!/bin/sh\necho two\n"),
        );

        let manifest = DeltaManifest {
            base_commit: base.clone(),
            writes,
            deletes: HashSet::new(),
            renames: Vec::new(),
            ranges: HashMap::new(),
        };

        let _ = flatten(
            &repo,
            &base,
            &manifest,
            &delta_root,
            sid,
            "simgit/test-gix-mode",
            "gix mode test",
        )
        .expect("gix flatten should succeed");

        let ls = run_git_stdout(&repo, &["ls-tree", "refs/heads/simgit/test-gix-mode", "script.sh"]);
        assert!(ls.starts_with("100755 "), "script.sh should remain executable, got: {ls}");

        let _ = fs::remove_dir_all(&root);
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v); },
            None => unsafe { std::env::remove_var(key); },
        }
    }
}
