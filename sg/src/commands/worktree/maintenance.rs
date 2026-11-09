use super::locks::remove_dead_pending;
use super::{
    admin_head, admin_holds_uncommitted, cow, discover_repo, emit, git_common_command, git_failure,
    git_output_common, head_unreached, is_ephemeral, list_worktrees, lock_present, lock_state,
    overlay, parse_duration, registration_gone, registration_holds_uncommitted, teardown_worktree,
    worktree_dirty, worktree_idle, AdminHead, RegistryLock, RepoContext,
};
use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Args, Default)]
pub struct WorktreePrune {
    /// Also delete every cached baseline, including recently used and
    /// still-reachable entries. Fails, to be retried, while an allocation is
    /// using the cache.
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

/// What a registration prune did: the registrations it dropped, and the stale
/// ones it kept because they may hold work, each with gc's reason for it:
/// `dirty`, `unreached-commits`, or `status-failed` when it cannot tell.
pub(super) struct RegistrationPrune {
    pub(super) pruned: Vec<PathBuf>,
    pub(super) kept: Vec<(PathBuf, &'static str)>,
}

/// Drop stale registrations in one pass over Git's admin dirs, all under the
/// exclusive registry lock, so another simgit cannot change an answer in
/// between. Stale is decided from each entry's own files, as Git's prune does,
/// which also finds the corrupt ones `git worktree list` omits. Kept: a
/// directory still on disk or one that cannot be checked, a remountable
/// overlay, a locked entry, a detached HEAD no ref reaches, and staged work or
/// an operation in progress. A worktree is reported by its path, an entry
/// whose `gitdir` is corrupt by its admin dir.
pub(super) fn prune_git_worktrees(repo: &RepoContext) -> Result<RegistrationPrune> {
    let _registry = RegistryLock::exclusive(repo)?;
    let mut outcome = RegistrationPrune {
        pruned: Vec::new(),
        kept: Vec::new(),
    };
    let admins = match fs::read_dir(repo.common_git_dir.join("worktrees")) {
        Ok(admins) => admins,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(error) => return Err(error).context("read the worktree registry"),
    };
    for admin in admins {
        let admin = admin.context("read the worktree registry")?.path();
        if !admin.is_dir() {
            continue;
        }
        let worktree = match registered_worktree(&admin) {
            Ok(Some(worktree)) => Some(worktree),
            // Missing or empty: the entry names no worktree at all.
            Ok(None) => None,
            Err(()) => {
                outcome.kept.push((admin, "status-failed"));
                continue;
            }
        };
        if let Some(worktree) = &worktree {
            match (worktree.join(".git").try_exists(), worktree.try_exists()) {
                // Live, or a directory whose gitfile alone is gone: its index
                // and Git state live in the registration.
                (Ok(true), _) | (_, Ok(true)) => continue,
                (Ok(false), Ok(false)) => {}
                _ => {
                    outcome.kept.push((worktree.clone(), "status-failed"));
                    continue;
                }
            }
        }
        let label = worktree.clone().unwrap_or_else(|| admin.clone());
        let locked = |admin: &Path| lock_present(&admin.join("locked"));
        if locked(&admin)
            || overlay::read_state(&admin).is_some_and(|state| state.overlay_dir.is_dir())
        {
            continue;
        }
        match head_safe_to_drop(repo, &admin) {
            Some(true) => {}
            Some(false) => {
                outcome.kept.push((label, "unreached-commits"));
                continue;
            }
            None => {
                outcome.kept.push((label, "status-failed"));
                continue;
            }
        }
        match admin_holds_uncommitted(repo, &admin) {
            Ok(false) => {}
            Ok(true) => {
                outcome.kept.push((label, "dirty"));
                continue;
            }
            Err(_) => {
                outcome.kept.push((label, "status-failed"));
                continue;
            }
        }
        // Checked again last: a lock taken, or a directory back, meanwhile.
        let back = worktree
            .as_deref()
            .is_some_and(|worktree| !matches!(worktree.try_exists(), Ok(false)));
        if locked(&admin) || back {
            continue;
        }
        fs::remove_dir_all(&admin)
            .with_context(|| format!("remove the registration {}", admin.display()))?;
        outcome.pruned.push(label);
    }
    Ok(outcome)
}

/// The worktree an admin dir's `gitdir` file names: `Ok(None)` when the file
/// is missing or empty, `Err` when it cannot be read or names no directory to
/// check (`/`).
pub(super) fn registered_worktree(admin: &Path) -> std::result::Result<Option<PathBuf>, ()> {
    let gitdir = match fs::read_to_string(admin.join("gitdir")) {
        Ok(gitdir) => gitdir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if gitdir.trim().is_empty() {
        return Ok(None);
    }
    match overlay::resolve_gitdir(admin, &gitdir).parent() {
        Some(worktree) => Ok(Some(worktree.to_path_buf())),
        None => Err(()),
    }
}

/// Whether the HEAD in `admin` loses no commit when its registration goes: a
/// branch keeps its commits, and a detached HEAD must be reached by some ref.
/// `None` when the HEAD cannot be judged.
fn head_safe_to_drop(repo: &RepoContext, admin: &Path) -> Option<bool> {
    match admin_head(repo, admin).ok()? {
        AdminHead::Branch(_) => Some(true),
        AdminHead::Detached(head) => head_unreached(repo, Some(&head))
            .ok()
            .map(|unreached| !unreached),
    }
}

pub fn prune(args: WorktreePrune, json: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    // Pruning a registration is a mutation of the Git registry, and it is the
    // one `doctor` reports as stale, so it has to be reported here too:
    // "pruned 0 cached baseline(s)" reads as "nothing happened".
    let registrations = prune_git_worktrees(&repo)?;
    remove_dead_pending(&repo);
    cow::remove_dead_probes(&repo.common_git_dir);
    // Under --json these are part of the report: a failure below must still
    // be the only stderr line.
    if !json {
        for (path, reason) in &registrations.kept {
            let why = match *reason {
                "dirty" => "it holds uncommitted work",
                "unreached-commits" => "its detached HEAD is a commit no ref reaches",
                _ => "cannot tell whether it holds work",
            };
            eprintln!("kept stale registration {}: {why}", path.display());
        }
    }
    let pruned_registrations: Vec<String> = registrations
        .pruned
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let outcome = prune_baseline_cache(&repo, args.all, &|| overlay_lowers(&repo))?;

    if json {
        emit(&json!({
            "pruned": outcome.removed,
            "pruned_registrations": pruned_registrations,
            "kept_registrations": registrations.kept
                .iter()
                .map(|(p, why)| json!({ "worktree": p.display().to_string(), "reason": why }))
                .collect::<Vec<_>>(),
            "retained": outcome.retained,
            "retained_bytes": outcome.retained_bytes,
            "cache_busy": outcome.cache_busy,
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
    if outcome.cache_busy {
        println!("baseline cache in use by an allocation; run prune again once it finishes");
    }
    Ok(())
}

/// The baselines mounted overlays use as their lower layer.
fn overlay_lowers(repo: &RepoContext) -> HashSet<PathBuf> {
    overlay::registrations(repo)
        .into_iter()
        .filter_map(|(_, state)| state.lower)
        .collect()
}

/// Prune the baseline cache, treating a baseline as live while a ref or a
/// worktree still reaches its commit. `protected` is read under the cache
/// lock; see `cow::prune_baselines`.
pub(super) fn prune_baseline_cache(
    repo: &RepoContext,
    all: bool,
    protected: &dyn Fn() -> HashSet<PathBuf>,
) -> Result<cow::PruneOutcome> {
    // Branch worktrees are reached through their branch ref; only detached
    // HEADs need checking on their own.
    let detached_heads: Vec<String> = list_worktrees(repo)?
        .into_iter()
        .filter(|entry| entry.branch.is_none())
        .filter_map(|entry| entry.head)
        .collect();
    let reachable = |commit: &str| commit_reachable(repo, commit, &detached_heads);
    cow::prune_baselines(&repo.common_git_dir, all, protected, &reachable)
}

/// Whether a ref or a detached worktree HEAD still reaches `commit`. One that
/// nothing reaches, or that Git has already collected, can only be reused by
/// naming its object id.
fn commit_reachable(repo: &RepoContext, commit: &str, detached_heads: &[String]) -> Result<bool> {
    let spec = format!("{commit}^{{commit}}");
    let exists = git_output_common(repo, ["rev-parse", "--verify", "--quiet", &spec])?;
    match exists.status.code() {
        Some(0) => {}
        Some(1) => return Ok(false),
        _ => return Err(git_failure("git rev-parse --verify", &exists)),
    }
    let refs = git_output_common(
        repo,
        [
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "--contains",
            commit,
        ],
    )?;
    if !refs.status.success() {
        return Err(git_failure("git for-each-ref --contains", &refs));
    }
    if !refs.stdout.is_empty() {
        return Ok(true);
    }
    for head in detached_heads {
        let ancestor = git_output_common(repo, ["merge-base", "--is-ancestor", commit, head])?;
        match ancestor.status.code() {
            Some(0) => return Ok(true),
            Some(1) => {}
            _ => return Err(git_failure("git merge-base --is-ancestor", &ancestor)),
        }
    }
    Ok(false)
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
        match lock_state(repo, &entry.path) {
            Some(false) => {}
            Some(true) => {
                skipped.push((entry.path.clone(), "locked"));
                continue;
            }
            // No lock found to rule out: nothing about it can be judged.
            None => {
                skipped.push((entry.path.clone(), "status-failed"));
                continue;
            }
        }
        if worktree_idle(repo, &entry.path) < older_than {
            // Say so: a user who just created and merged a workspace and then
            // followed the documented `gc --older-than 1h` otherwise sees no
            // mention of it at all.
            skipped.push((entry.path.clone(), "recently-active"));
            continue;
        }
        // Commits only this worktree's HEAD holds would dangle; no flag allows it.
        if entry.branch.is_none() {
            match head_unreached(repo, entry.head.as_deref()) {
                Ok(true) => {
                    skipped.push((entry.path.clone(), "unreached-commits"));
                    continue;
                }
                Err(_) => {
                    skipped.push((entry.path.clone(), "status-failed"));
                    continue;
                }
                Ok(false) => {}
            }
        }
        if !args.discard_dirty {
            // A worktree deleted by hand keeps its uncommitted work in its
            // registration.
            let dirty = if registration_gone(repo, &entry.path) {
                registration_holds_uncommitted(repo, &entry.path)
            } else {
                worktree_dirty(&entry.path)
            };
            match dirty {
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

    // No closing prune: gc drops only what it reaps, and everything it skipped
    // or never selected keeps its registration.
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
    delete_branch_unguarded(repo, branch, force)
}

/// Delete a branch without the primary-branch guard. Only for a branch the
/// caller created itself, which cannot hold anyone's work yet.
pub(super) fn delete_branch_unguarded(repo: &RepoContext, branch: &str, force: bool) -> Result<()> {
    let mut command = git_common_command(repo);
    command.args(["branch", if force { "-D" } else { "-d" }, branch]);
    // Git refuses to delete a checked-out branch, so this reads every registration.
    let output = {
        let _registry = RegistryLock::shared(repo)?;
        command.output().context("delete worktree branch")?
    };
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
