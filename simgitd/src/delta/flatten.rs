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
use tracing::info;

use crate::delta::store::DeltaManifest;

pub struct FlattenResult {
    pub commit_oid:  String,
    pub branch_name: String,
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
    // ── 1. Create an isolated worktree for this flatten operation ─────────
    let wt_path = delta_store_root.join(format!("{session_id}-wt"));
    std::fs::create_dir_all(&wt_path).map_err(|source| FlattenError::Io {
        step: "create_worktree_dir",
        source,
    })?;

    // Add a Git worktree for the base commit.
    run_git(repo_path, &[
        "worktree", "add", "--detach",
        &wt_path.to_string_lossy(),
        base_commit,
    ], "worktree_add")?;

    // ── 2. Apply the delta manifest ───────────────────────────────────────
    let objects_dir = delta_store_root.join(session_id.to_string()).join("objects");

    // Deletes.
    for del in &manifest.deletes {
        let target = wt_path.join(del);
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
        let src = wt_path.join(from);
        let dst = wt_path.join(to);
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
        let dest = wt_path.join(rel_path);
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

    // ── 3. Stage and commit ───────────────────────────────────────────────
    run_git(&wt_path, &["add", "-A"], "stage_changes")?;
    run_git(&wt_path, &["checkout", "-B", branch_name], "checkout_branch")?;
    run_git(&wt_path, &["commit", "--allow-empty", "-m", message], "git_commit")?;

    // Extract commit OID.
    let commit_oid = run_git_output(&wt_path, &["rev-parse", "HEAD"], "resolve_head")?
        .trim()
        .to_owned();

    // ── 4. Clean up worktree ──────────────────────────────────────────────
    run_git(
        repo_path,
        &["worktree", "remove", "--force", &wt_path.to_string_lossy()],
        "worktree_remove",
    )
        .ok(); // best-effort; don't fail the commit if cleanup errors

    info!(commit = %commit_oid, branch = branch_name, "flatten complete");
    Ok(FlattenResult { commit_oid, branch_name: branch_name.to_owned() })
}

// ── helpers ───────────────────────────────────────────────────────────────────

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

