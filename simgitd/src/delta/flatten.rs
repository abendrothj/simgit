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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
}
