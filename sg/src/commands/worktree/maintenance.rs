use super::{
    cow, discover_repo, emit, ensure_unlocked, git_common_command, git_failure, git_output_common,
    is_ephemeral, list_worktrees, overlay, parse_duration, run_git_common,
    stale_worktree_registrations, teardown_worktree, worktree_dirty, worktree_idle, RepoContext,
    WorktreeLock,
};
use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Args, Default)]
pub struct WorktreePrune {
    /// Also delete every cached baseline, including recently used entries.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Default)]
pub struct WorktreeGc {
    /// Also allow GC to remove persistent worktrees. Without it, only
    /// ephemeral worktrees are reaped.
    #[arg(long)]
    pub include_persistent: bool,

    /// Only reap worktrees whose branch starts with this prefix.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Reap worktrees idle at least this long (e.g. 90s, 30m, 24h, 7d).
    #[arg(long, default_value = "24h")]
    pub older_than: String,

    /// Reap worktrees with uncommitted changes, discarding those changes.
    #[arg(long)]
    pub discard_dirty: bool,

    /// Delete each reaped worktree's branch.
    #[arg(long)]
    pub delete_branches: bool,

    /// Permit deleting branches that are not merged. Requires
    /// --delete-branches.
    #[arg(long)]
    pub delete_unmerged: bool,

    /// Report what would be reaped without removing anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// Keep recoverable unmounted overlays registered during native pruning.
pub(super) fn prune_git_worktrees(repo: &RepoContext) -> Result<()> {
    let mut locks = Vec::new();
    for (path, state) in overlay::registrations(repo) {
        if state.overlay_dir.is_dir() && ensure_unlocked(repo, &path).is_ok() {
            locks.push(WorktreeLock::acquire(repo, &path)?);
        }
    }
    run_git_common(repo, ["worktree", "prune"])?;
    for lock in locks {
        lock.release()?;
    }
    Ok(())
}

pub fn prune(args: WorktreePrune, json: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    let protected: HashSet<PathBuf> = overlay::registrations(&repo)
        .into_iter()
        .filter_map(|(_, state)| state.lower)
        .collect();
    // Pruning a registration is a mutation of the Git registry, and it is the
    // one `doctor` reports as stale, so it has to be reported here too:
    // "pruned 0 cached baseline(s)" reads as "nothing happened".
    let before = stale_registrations(&repo)?;
    prune_git_worktrees(&repo)?;
    let after = stale_registrations(&repo)?;
    let pruned_registrations: Vec<String> = before
        .into_iter()
        .filter(|path| !after.contains(path))
        .collect();
    let outcome = cow::prune_baselines(&repo.common_git_dir, args.all, &protected)?;

    if json {
        emit(&json!({
            "pruned": outcome.removed,
            "pruned_registrations": pruned_registrations,
            "retained": outcome.retained,
            "retained_bytes": outcome.retained_bytes,
        }));
        return Ok(());
    }
    println!(
        "pruned {} stale registration(s)",
        pruned_registrations.len()
    );
    println!(
        "pruned {} cached baseline(s); {} retained ({:.1} MiB)",
        outcome.removed.len(),
        outcome.retained.len(),
        outcome.retained_bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

/// The worktree paths Git currently reports as prunable registrations.
fn stale_registrations(repo: &RepoContext) -> Result<Vec<String>> {
    let output = git_output_common(repo, ["worktree", "list", "--porcelain", "-z"])?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    Ok(stale_worktree_registrations(&output.stdout)
        .iter()
        .filter_map(|entry| entry["worktree"].as_str().map(str::to_owned))
        .collect())
}

pub fn gc(args: WorktreeGc, json: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    let outcome = run_gc(&repo, &args)?;

    if json {
        emit(&json!({
            "dry_run": args.dry_run,
            "reaped": outcome.reaped
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            "skipped": outcome.skipped
                .iter()
                .map(|(p, why)| json!({ "worktree": p.display().to_string(), "reason": why }))
                .collect::<Vec<_>>(),
            "retained_branches": outcome.retained_branches,
            "deleted_branches": outcome.deleted_branches,
        }));
    } else {
        let verb = if args.dry_run { "would reap" } else { "reaped" };
        for path in &outcome.reaped {
            println!("{verb}: {}", path.display());
        }
        for (path, why) in &outcome.skipped {
            eprintln!("skipped ({why}): {}", path.display());
        }
        for branch in &outcome.retained_branches {
            eprintln!("retained unmerged branch: {branch}");
        }
        for branch in &outcome.deleted_branches {
            println!("deleted branch: {branch}");
        }
        println!("{verb} {} worktree(s)", outcome.reaped.len());
    }
    Ok(())
}

pub fn repair(json_output: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    let mut repaired = Vec::new();
    let mut healthy = Vec::new();
    let mut failed = Vec::new();
    for (worktree, _) in overlay::registrations(&repo) {
        match overlay::repair(&repo, &worktree) {
            Ok(true) => repaired.push(worktree),
            Ok(false) => healthy.push(worktree),
            Err(error) => failed.push((worktree, error.to_string())),
        }
    }
    if json_output {
        emit(&json!({
            "repaired": repaired.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "healthy": healthy.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "failed": failed.iter().map(|(p, error)| json!({
                "worktree": p.display().to_string(), "error": error
            })).collect::<Vec<_>>(),
        }));
    } else {
        for path in &repaired {
            println!("repaired: {}", path.display());
        }
        for path in &healthy {
            println!("healthy: {}", path.display());
        }
        for (path, error) in &failed {
            eprintln!("failed: {}: {error}", path.display());
        }
        println!("repaired {} overlay worktree(s)", repaired.len());
    }
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("{} overlay worktree(s) could not be repaired", failed.len())
    }
}

pub(super) struct GcOutcome {
    pub(super) reaped: Vec<PathBuf>,
    pub(super) skipped: Vec<(PathBuf, &'static str)>,
    pub(super) retained_branches: Vec<String>,
    pub(super) deleted_branches: Vec<String>,
}

/// Core reaping logic, separated from output for testability. Returns the
/// worktrees reaped (or that would be, under `--dry-run`) and those skipped.
pub(super) fn run_gc(repo: &RepoContext, args: &WorktreeGc) -> Result<GcOutcome> {
    // Refuse before reaping anything: on its own --delete-unmerged relaxes a
    // deletion that is never attempted.
    if args.delete_unmerged && !args.delete_branches {
        bail!("--delete-unmerged only relaxes branch deletion; pass --delete-branches too");
    }
    let older_than = parse_duration(&args.older_than)?;
    let mut reaped: Vec<PathBuf> = Vec::new();
    let mut skipped: Vec<(PathBuf, &'static str)> = Vec::new();
    let mut retained_branches = Vec::new();
    let mut deleted_branches = Vec::new();

    for entry in list_worktrees(repo)? {
        if entry.is_main {
            continue;
        }
        let branch = entry.branch.as_deref().unwrap_or("");
        let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        if let Some(prefix) = &args.prefix {
            if !short.starts_with(prefix) {
                continue;
            }
        }
        if !args.include_persistent && !is_ephemeral(repo, &entry.path) {
            skipped.push((entry.path.clone(), "persistent"));
            continue;
        }
        if ensure_unlocked(repo, &entry.path).is_err() {
            skipped.push((entry.path.clone(), "locked"));
            continue;
        }
        if worktree_idle(&entry.path) < older_than {
            // Say so: a user who just created and merged a workspace and then
            // followed the documented `gc --older-than 1h` otherwise sees no
            // mention of it at all.
            skipped.push((entry.path.clone(), "recently-active"));
            continue;
        }
        if !args.discard_dirty {
            match worktree_dirty(&entry.path) {
                Ok(true) => {
                    skipped.push((entry.path.clone(), "dirty"));
                    continue;
                }
                Err(_) => {
                    skipped.push((entry.path.clone(), "status-failed"));
                    continue;
                }
                Ok(false) => {}
            }
        }
        if args.dry_run {
            reaped.push(entry.path);
            continue;
        }
        match teardown_worktree(repo, &entry.path, args.discard_dirty) {
            Ok(()) => {
                if args.delete_branches {
                    if let Some(branch) = &entry.branch {
                        if delete_local_branch(repo, branch, args.delete_unmerged).is_err() {
                            retained_branches.push(short.to_owned());
                        } else {
                            deleted_branches.push(short.to_owned());
                        }
                    }
                }
                reaped.push(entry.path)
            }
            Err(_) => skipped.push((entry.path, "remove-failed")),
        }
    }

    if !args.dry_run {
        prune_git_worktrees(repo)?;
    }
    Ok(GcOutcome {
        reaped,
        skipped,
        retained_branches,
        deleted_branches,
    })
}

pub(super) fn delete_local_branch(repo: &RepoContext, branch_ref: &str, force: bool) -> Result<()> {
    let branch = branch_ref.strip_prefix("refs/heads/").unwrap_or(branch_ref);
    if branch == "main" || branch == "master" {
        bail!("refusing to delete primary branch '{branch}'");
    }
    let mut command = git_common_command(repo);
    command.args(["branch", if force { "-D" } else { "-d" }, branch]);
    let output = command.output().context("delete worktree branch")?;
    if output.status.success() {
        return Ok(());
    }
    // Git's own refusal names `git branch -D`, which routes the user straight
    // around simgit's safety flag, and never mentions the flag that exists for
    // exactly this. Answer in simgit's terms instead.
    if !force && String::from_utf8_lossy(&output.stderr).contains("not fully merged") {
        bail!(
            "branch '{branch}' is not fully merged: deleting it would drop commits\n\
             merge it first, or delete it with the work by adding --delete-unmerged"
        );
    }
    Err(git_failure("delete worktree branch", &output))
}
