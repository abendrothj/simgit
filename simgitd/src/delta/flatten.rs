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

use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;

use crate::delta::store::DeltaManifest;

pub struct FlattenResult {
    pub commit_oid:  String,
    pub branch_name: String,
}

pub fn flatten(
    repo_path:        &Path,
    base_commit:      &str,
    manifest:         &DeltaManifest,
    delta_store_root: &Path,
    session_id:       uuid::Uuid,
    branch_name:      &str,
    message:          &str,
) -> Result<FlattenResult> {
    // ── 1. Create an isolated worktree for this flatten operation ─────────
    let wt_path = delta_store_root.join(format!("{session_id}-wt"));
    std::fs::create_dir_all(&wt_path)?;

    // Add a Git worktree for the base commit.
    run_git(repo_path, &[
        "worktree", "add", "--detach",
        &wt_path.to_string_lossy(),
        base_commit,
    ])?;

    // ── 2. Apply the delta manifest ───────────────────────────────────────
    let objects_dir = delta_store_root.join(session_id.to_string()).join("objects");

    // Deletes.
    for del in &manifest.deletes {
        let target = wt_path.join(del);
        if target.exists() {
            std::fs::remove_file(&target)
                .with_context(|| format!("delete {}", del.display()))?;
        }
    }

    // Renames.
    for (from, to) in &manifest.renames {
        let src = wt_path.join(from);
        let dst = wt_path.join(to);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if src.exists() {
            std::fs::rename(&src, &dst)?;
        }
    }

    // Writes.
    for (rel_path, hash) in &manifest.writes {
        let bucket   = &hash[..2];
        let blob_path = objects_dir.join(bucket).join(&hash[2..]);
        let content  = std::fs::read(&blob_path)
            .with_context(|| format!("read delta blob for {}", rel_path.display()))?;
        let dest = wt_path.join(rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &content)?;
    }

    // ── 3. Stage and commit ───────────────────────────────────────────────
    run_git(&wt_path, &["add", "-A"])?;
    run_git(&wt_path, &["checkout", "-B", branch_name])?;
    let commit_out = run_git_output(&wt_path, &[
        "commit", "--allow-empty", "-m", message,
    ])?;

    // Extract commit OID.
    let commit_oid = run_git_output(&wt_path, &["rev-parse", "HEAD"])?
        .trim()
        .to_owned();

    // ── 4. Clean up worktree ──────────────────────────────────────────────
    run_git(repo_path, &["worktree", "remove", "--force", &wt_path.to_string_lossy()])
        .ok(); // best-effort; don't fail the commit if cleanup errors

    info!(commit = %commit_oid, branch = branch_name, "flatten complete");
    Ok(FlattenResult { commit_oid, branch_name: branch_name.to_owned() })
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .status()
        .with_context(|| format!("exec git {:?}", args))?;
    if !status.success() {
        anyhow::bail!("git {:?} failed with status {status}", args);
    }
    Ok(())
}

fn run_git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("exec git {:?}", args))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

