use super::hook::remove_dead_fetch_refs;
use super::locks::remove_dead_pending;
use super::locks::{lock_present, lock_state, RegistryLock};
use super::registry::{
    admin_dirs, admin_head, admin_is_ephemeral, list_worktrees, registration_gone, worktree_dirty,
    worktree_idle, AdminHead,
};
use super::remove::{
    admin_holds_uncommitted, head_unreached, registration_holds_uncommitted, teardown_worktree,
};
use super::repo::{
    canonical_path, discover_repo, git_common_command, git_failure, git_output_common,
    output_retrying_half_written_registration,
};
use super::{cow, emit, overlay, Caller, RepoContext};
use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::json;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    remove_dead_fetch_refs(&repo);
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
    let reachable = |commit: &str| commit_reachable(repo, commit);
    cow::prune_baselines(&repo.common_git_dir, all, protected, &reachable)
}

/// Whether a ref or a detached worktree HEAD still reaches `commit`. One that
/// nothing reaches, or that Git has already collected, can only be reused by
/// naming its object id.
fn commit_reachable(repo: &RepoContext, commit: &str) -> Result<bool> {
    let spec = format!("{commit}^{{commit}}");
    let exists = git_output_common(repo, ["rev-parse", "--verify", "--quiet", &spec])?;
    match exists.status.code() {
        Some(0) => {}
        Some(1) => return Ok(false),
        _ => return Err(git_failure("git rev-parse --verify", &exists)),
    }
    // `--all` counts every ref and every worktree's HEAD, so one call answers
    // what a `merge-base` per detached worktree used to: `commit` is reached
    // exactly when nothing is left once those are subtracted. Reading every
    // HEAD meets the ones concurrent allocations are writing, so it retries.
    let mut command = git_common_command(repo);
    command.args(["rev-list", "-n1", commit, "--not", "--all"]);
    let unreached = output_retrying_half_written_registration(&mut command)?;
    if !unreached.status.success() {
        return Err(git_failure("git rev-list --not --all", &unreached));
    }
    Ok(unreached.stdout.is_empty())
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
    let mut reaped_branches = Vec::new();

    // Each check below reads a file in the worktree's admin dir; find them all
    // in one pass rather than asking git once per worktree per check.
    let admins = admin_dirs(repo);
    for entry in list_worktrees(repo)? {
        if entry.is_main {
            continue;
        }
        let admin = admins
            .get(&canonical_path(&entry.path))
            .cloned()
            .or_else(|| overlay::admin_dir(repo, &entry.path));
        let branch = entry.branch.as_deref().unwrap_or("");
        let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        if let Some(prefix) = &args.prefix {
            if !short.starts_with(prefix) {
                continue;
            }
        }
        if !args.include_persistent && !admin_is_ephemeral(admin.as_deref()) {
            skipped.push((entry.path.clone(), "persistent"));
            continue;
        }
        let locked = match &admin {
            Some(admin) => Some(lock_present(&admin.join("locked"))),
            None => lock_state(repo, &entry.path),
        };
        match locked {
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
        if worktree_idle(admin.as_deref(), &entry.path) < older_than {
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
        match teardown_worktree(repo, &entry.path, args.discard_dirty, Caller::Cli) {
            Ok(()) => {
                if args.delete_branches && entry.branch.is_some() {
                    reaped_branches.push(short.to_owned());
                }
                reaped.push(entry.path)
            }
            Err(_) => skipped.push((entry.path, "remove-failed")),
        }
    }
    if !reaped_branches.is_empty() {
        (deleted_branches, retained_branches) =
            delete_branches_together(repo, reaped_branches, args.delete_unmerged);
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

/// Delete `branches` with as few `git branch` calls as possible, each of
/// which reads every registration, and sort them into deleted and kept,
/// never by parsing Git's messages. `main` and `master` are always kept;
/// without `force`, so is any branch Git refuses as not fully merged.
///
/// It never fails: gc calls it after the worktrees are gone, and its report is
/// the only record of them. A branch it cannot confirm deleted counts as kept.
fn delete_branches_together(
    repo: &RepoContext,
    branches: Vec<String>,
    force: bool,
) -> (Vec<String>, Vec<String>) {
    let (mut retained, deletable): (Vec<String>, Vec<String>) = branches
        .into_iter()
        .partition(|branch| branch == "main" || branch == "master");
    let mut deleted = Vec::new();
    // Branches of a `git branch` call that refused some: only the refs left
    // afterwards say which.
    let mut unsure = Vec::new();
    // Chunks keep each argument list well under ARG_MAX.
    for chunk in deletable.chunks(500) {
        let mut command = git_common_command(repo);
        command
            .args(["branch", if force { "-D" } else { "-d" }])
            .args(chunk);
        let output = RegistryLock::shared(repo)
            .and_then(|_registry| command.output().context("delete worktree branches"));
        match output {
            // Git exits 0 only when it deleted every branch it was given.
            Ok(output) if output.status.success() => deleted.extend_from_slice(chunk),
            Ok(_) => unsure.extend_from_slice(chunk),
            // Git never ran, so every branch is still there.
            Err(_) => retained.extend_from_slice(chunk),
        }
    }
    if unsure.is_empty() {
        return (deleted, retained);
    }
    let left = git_output_common(repo, ["for-each-ref", "--format=%(refname)", "refs/heads/"])
        .ok()
        .filter(|output| output.status.success());
    let Some(left) = left else {
        retained.extend(unsure);
        return (deleted, retained);
    };
    let left: HashSet<String> = String::from_utf8_lossy(&left.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    // Full names: a tag or remote of the same name makes `refname:short`
    // write a branch as `heads/<name>`.
    for branch in unsure {
        if left.contains(&format!("refs/heads/{branch}")) {
            retained.push(branch);
        } else {
            deleted.push(branch);
        }
    }
    (deleted, retained)
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

/// Parse a compact duration like `90s`, `30m`, `24h`, `7d`. A bare number is
/// seconds.
fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (value, unit) = text.split_at(split);
    let count: u64 = value
        .parse()
        .with_context(|| format!("invalid duration '{text}'"))?;
    let multiplier = match unit {
        "s" | "" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        other => bail!("invalid duration unit '{other}' (use s, m, h, or d)"),
    };
    let seconds = count
        .checked_mul(multiplier)
        .with_context(|| format!("duration '{text}' is too large"))?;
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktree::add::WorktreeKind;
    use crate::commands::worktree::registry::{list_worktrees, mark_ephemeral, worktree_admin_dir};
    use crate::commands::worktree::repo::{
        discover_repo, git_failure, git_output_at, git_output_common, resolve_commit, run_git_at,
        state_dir,
    };
    use crate::commands::worktree::test_support::{
        add_git_worktree, branch_exists, git, resolve_commit_at, Fixture,
    };
    use crate::commands::worktree::{cow, overlay};
    use anyhow::{Context, Result};
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    #[test]
    fn parse_duration_accepts_units_and_rejects_overflow() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("5w").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("18446744073709551615d").is_err());
    }

    #[test]
    fn gc_reaps_ephemeral_and_by_prefix_but_spares_others() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;

        let eph = fixture.root.join("eph");
        add_git_worktree(&repo, "exp/eph", &eph, &base, WorktreeKind::NewBranch, &[])?;
        mark_ephemeral(&eph)?;
        let keep = fixture.root.join("keep");
        add_git_worktree(
            &repo,
            "exp/keep",
            &keep,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let other = fixture.root.join("other");
        add_git_worktree(
            &repo,
            "feat/other",
            &other,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        mark_ephemeral(&other)?;

        // Ephemeral-only is the default selection; nothing has to ask for it.
        let args = WorktreeGc {
            prefix: Some("exp".to_owned()),
            older_than: "0s".to_owned(),
            ..Default::default()
        };
        let outcome = run_gc(&repo, &args)?;
        assert_eq!(outcome.reaped, vec![eph]);
        assert_eq!(outcome.skipped, vec![(keep.clone(), "persistent")]);

        let branches: Vec<_> = list_worktrees(&repo)?
            .iter()
            .filter_map(|worktree| worktree.branch.clone())
            .collect();
        assert!(branches
            .iter()
            .any(|branch| branch == "refs/heads/exp/keep"));
        assert!(branches
            .iter()
            .any(|branch| branch == "refs/heads/feat/other"));
        assert!(!branches.iter().any(|branch| branch == "refs/heads/exp/eph"));
        Ok(())
    }

    #[test]
    fn gc_skips_dirty_worktrees_without_discard_dirty() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let mut dirty = Vec::new();
        for name in ["untracked", "modified", "staged"] {
            let path = fixture.root.join(name);
            add_git_worktree(&repo, name, &path, &base, WorktreeKind::NewBranch, &[])?;
            dirty.push(path);
        }
        let [untracked, modified, staged] = &dirty[..] else {
            unreachable!()
        };
        fs::write(untracked.join("scratch.txt"), "uncommitted")?;
        fs::write(modified.join("file.txt"), "edited\n")?;
        fs::write(staged.join("staged.txt"), "staged\n")?;
        git(staged, ["add", "staged.txt"])?;

        let gc = |discard_dirty| {
            run_gc(
                &repo,
                &WorktreeGc {
                    older_than: "0s".to_owned(),
                    include_persistent: true,
                    discard_dirty,
                    ..Default::default()
                },
            )
        };
        let outcome = gc(false)?;
        assert!(outcome.reaped.is_empty());
        let mut skipped = outcome.skipped;
        skipped.sort();
        let mut expected: Vec<_> = dirty.iter().cloned().map(|path| (path, "dirty")).collect();
        expected.sort();
        assert_eq!(skipped, expected);
        // The skipped worktrees still hold every kind of uncommitted work.
        assert_eq!(
            fs::read_to_string(untracked.join("scratch.txt"))?,
            "uncommitted"
        );
        assert_eq!(fs::read_to_string(modified.join("file.txt"))?, "edited\n");
        assert_eq!(fs::read_to_string(staged.join("staged.txt"))?, "staged\n");
        assert!(git_output_at(staged, ["diff", "--cached", "--name-only"])?
            .stdout
            .starts_with(b"staged.txt"));

        let mut reaped = gc(true)?.reaped;
        reaped.sort();
        assert_eq!(
            reaped,
            expected
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    /// A detached worktree's commits live only in its HEAD; reaping it would
    /// leave them dangling, and no flag permits that.
    #[test]
    fn gc_skips_a_detached_worktree_whose_commits_no_ref_reaches() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let detached = fixture.root.join("detached");
        add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
        mark_ephemeral(&detached)?;
        fs::write(detached.join("agent.txt"), "result")?;
        run_git_at(&detached, ["add", "."])?;
        run_git_at(&detached, ["commit", "-q", "-m", "agent result"])?;
        let head = resolve_commit_at(&detached)?;

        for discard_dirty in [false, true] {
            let outcome = run_gc(
                &repo,
                &WorktreeGc {
                    older_than: "0s".to_owned(),
                    discard_dirty,
                    ..Default::default()
                },
            )?;
            assert!(outcome.reaped.is_empty());
            assert_eq!(
                outcome.skipped,
                vec![(detached.clone(), "unreached-commits")]
            );
        }
        assert!(detached.is_dir());
        assert_eq!(resolve_commit_at(&detached)?, head);
        Ok(())
    }

    #[test]
    fn gc_reaps_a_detached_worktree_at_a_reached_commit() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let detached = fixture.root.join("detached");
        add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
        mark_ephemeral(&detached)?;

        let outcome = run_gc(
            &repo,
            &WorktreeGc {
                older_than: "0s".to_owned(),
                ..Default::default()
            },
        )?;
        assert_eq!(outcome.reaped, vec![detached]);
        assert!(outcome.skipped.is_empty());
        Ok(())
    }

    /// A workspace created moments ago is the one a user is most likely to ask
    /// about, and the documented recipe (`gc --older-than 1h`) always spares it.
    /// Silence there reads as a bug in gc; the age filter has to say so.
    #[test]
    fn gc_reports_worktrees_it_spared_as_recently_active() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let fresh = fixture.root.join("fresh");
        add_git_worktree(
            &repo,
            "agent/fresh",
            &fresh,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        mark_ephemeral(&fresh)?;

        let outcome = run_gc(
            &repo,
            &WorktreeGc {
                older_than: "1h".to_owned(),
                ..Default::default()
            },
        )?;
        assert!(outcome.reaped.is_empty());
        assert_eq!(outcome.skipped, vec![(fresh.clone(), "recently-active")]);
        assert!(fresh.is_dir());
        Ok(())
    }

    #[test]
    fn gc_deletes_merged_branches_when_requested() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("merged");
        add_git_worktree(
            &repo,
            "agent/merged",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;

        let outcome = run_gc(
            &repo,
            &WorktreeGc {
                older_than: "0s".to_owned(),
                include_persistent: true,
                delete_branches: true,
                ..Default::default()
            },
        )?;
        assert_eq!(outcome.reaped, vec![target]);
        assert!(outcome.retained_branches.is_empty());
        assert_eq!(outcome.deleted_branches, vec!["agent/merged"]);
        assert!(!branch_exists(&repo, "agent/merged")?);
        Ok(())
    }

    #[test]
    fn gc_retains_unmerged_branches_without_delete_unmerged() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("unmerged");
        add_git_worktree(
            &repo,
            "agent/unmerged",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        fs::write(target.join("agent.txt"), "result")?;
        run_git_at(&target, ["add", "."])?;
        run_git_at(&target, ["commit", "-q", "-m", "agent result"])?;

        let outcome = run_gc(
            &repo,
            &WorktreeGc {
                older_than: "0s".to_owned(),
                include_persistent: true,
                delete_branches: true,
                ..Default::default()
            },
        )?;
        assert_eq!(outcome.reaped, vec![target]);
        assert_eq!(outcome.retained_branches, vec!["agent/unmerged"]);
        assert!(branch_exists(&repo, "agent/unmerged")?);
        Ok(())
    }

    #[test]
    fn gc_deletes_unmerged_branches_when_permitted() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("unmerged");
        add_git_worktree(
            &repo,
            "agent/unmerged",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        fs::write(target.join("agent.txt"), "result")?;
        run_git_at(&target, ["add", "."])?;
        run_git_at(&target, ["commit", "-q", "-m", "agent result"])?;

        let outcome = run_gc(
            &repo,
            &WorktreeGc {
                older_than: "0s".to_owned(),
                include_persistent: true,
                delete_branches: true,
                delete_unmerged: true,
                ..Default::default()
            },
        )?;
        assert_eq!(outcome.reaped, vec![target]);
        assert!(outcome.retained_branches.is_empty());
        assert_eq!(outcome.deleted_branches, vec!["agent/unmerged"]);
        assert!(!branch_exists(&repo, "agent/unmerged")?);
        Ok(())
    }

    #[test]
    fn gc_refuses_delete_unmerged_without_delete_branches() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("kept");
        add_git_worktree(
            &repo,
            "agent/kept",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;

        let outcome = run_gc(
            &repo,
            &WorktreeGc {
                older_than: "0s".to_owned(),
                include_persistent: true,
                delete_unmerged: true,
                ..Default::default()
            },
        );
        let error = match outcome {
            Ok(_) => panic!("--delete-unmerged alone must be refused"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("--delete-branches"),
            "error must name the missing flag: {error}"
        );
        assert!(target.is_dir(), "nothing may be reaped before validation");
        assert!(branch_exists(&repo, "agent/kept")?);
        Ok(())
    }

    #[test]
    fn prune_leaves_a_baseline_an_allocation_holds() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let output = git_output_common(
            &repo,
            ["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "orphan"],
        )?;
        if !output.status.success() {
            return Err(git_failure("git commit-tree", &output));
        }
        let orphan = String::from_utf8(output.stdout)?.trim().to_owned();

        // An allocation using a baseline holds the cache lock shared, so even one
        // nothing reaches is kept, plain prune says why, and --all refuses.
        let in_use = cow::ensure_baseline(&repo, &orphan)?;
        let tree = in_use.tree.clone();
        let busy = prune_baseline_cache(&repo, false, &HashSet::new)?;
        assert!(busy.cache_busy);
        assert!(busy.removed.is_empty(), "{:?}", busy.removed);
        assert!(prune_baseline_cache(&repo, true, &HashSet::new).is_err());
        assert!(tree.join("file.txt").is_file());

        drop(in_use);
        let freed = prune_baseline_cache(&repo, false, &HashSet::new)?;
        assert!(!freed.cache_busy);
        assert_eq!(freed.removed, vec![orphan]);
        assert!(!tree.exists());
        Ok(())
    }

    #[test]
    fn prune_finishes_deleting_what_a_killed_prune_moved_aside() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        // A fresh `ready` must not make it look like a build in progress.
        let moved = state_dir(&repo.common_git_dir)
            .join("baselines")
            .join(format!(".doomed-{}-1", "a".repeat(40)));
        fs::create_dir_all(moved.join("tree"))?;
        fs::write(moved.join("ready"), "a")?;
        let outcome = prune_baseline_cache(&repo, false, &HashSet::new)?;
        assert!(!moved.exists());
        assert!(outcome.retained.is_empty(), "{:?}", outcome.retained);
        Ok(())
    }

    #[test]
    fn pruning_preserves_baselines_used_by_active_overlays() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let commit = resolve_commit(&repo, "HEAD")?;
        let baseline = cow::ensure_baseline(&repo, &commit)?.tree;
        // Other tests in this process fork children (`pre_exec`), and one forked
        // while this test held the cache lock holds a copy of it until it execs.
        // `prune --all` rightly refuses a busy cache instead of waiting, so wait
        // out that moment here; no simgit process forks while it holds a lock.
        let prune_all = |protected: &dyn Fn() -> HashSet<PathBuf>| {
            for _ in 0..100 {
                match prune_baseline_cache(&repo, true, protected) {
                    Err(error) if format!("{error:#}").contains("in use by an allocation") => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    outcome => return outcome,
                }
            }
            prune_baseline_cache(&repo, true, protected)
        };
        let protected = || HashSet::from([baseline.clone()]);
        let protected_run = prune_all(&protected)?;
        assert!(protected_run.removed.is_empty());
        assert_eq!(protected_run.retained.len(), 1);
        assert!(
            protected_run.retained_bytes > 0,
            "a retained baseline must report its disk cost"
        );
        assert!(baseline.is_dir());
        let full_run = prune_all(&HashSet::new)?;
        assert_eq!(full_run.removed.len(), 1);
        assert_eq!(full_run.retained_bytes, 0);
        assert!(!baseline.exists());
        Ok(())
    }

    #[test]
    fn pruning_drops_baselines_nothing_reaches_any_more() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let new_commit = |message: &str| -> Result<String> {
            let output = git_output_common(
                &repo,
                ["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", message],
            )?;
            if !output.status.success() {
                return Err(git_failure("git commit-tree", &output));
            }
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        let on_branch = resolve_commit(&repo, "HEAD")?;
        // What a force-push leaves behind: a commit no ref points at any more.
        let rewritten = new_commit("rewritten")?;
        let detached = new_commit("detached")?;
        let detached_path = fixture.root.join("detached");
        add_git_worktree(
            &repo,
            "",
            &detached_path,
            &detached,
            WorktreeKind::Detached,
            &[],
        )?;
        for commit in [&on_branch, &rewritten, &detached] {
            cow::ensure_baseline(&repo, commit)?;
        }
        // A baseline whose commit Git has since collected.
        let collected = "1".repeat(on_branch.len());
        let collected_dir = state_dir(&repo.common_git_dir)
            .join("baselines")
            .join(&collected);
        fs::create_dir_all(collected_dir.join("tree"))?;
        fs::write(collected_dir.join("ready"), &collected)?;
        // The branch moves on: its old tip is still reachable, as an ancestor.
        git(
            &fixture.repo,
            ["commit", "-q", "--allow-empty", "-m", "advance"],
        )?;

        let sorted = |mut names: Vec<String>| {
            names.sort();
            names
        };
        let first = prune_baseline_cache(&repo, false, &HashSet::new)?;
        assert_eq!(
            sorted(first.removed),
            sorted(vec![rewritten.clone(), collected])
        );
        assert_eq!(
            sorted(first.retained),
            sorted(vec![on_branch.clone(), detached.clone()])
        );

        // Once the detached worktree is gone, nothing reaches its commit.
        git(
            &fixture.repo,
            [
                "worktree",
                "remove",
                detached_path.to_str().context("utf-8 path")?,
            ],
        )?;
        let second = prune_baseline_cache(&repo, false, &HashSet::new)?;
        assert_eq!(second.removed, vec![detached]);
        assert_eq!(second.retained, vec![on_branch]);
        Ok(())
    }

    #[test]
    fn prune_retains_old_unmounted_overlay_registration() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let worktree = fixture.root.join("recoverable");
        add_git_worktree(
            &repo,
            "recoverable",
            &worktree,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let admin = worktree_admin_dir(&worktree)?;
        let overlay_dir = overlay::root(&repo.common_git_dir).join("saved");
        fs::create_dir_all(overlay_dir.join("upper"))?;
        overlay::write_marker(
            &admin,
            &overlay::State {
                overlay_dir,
                lower: None,
            },
        )?;
        fs::remove_file(worktree.join(".git"))?;
        fs::File::open(admin.join("gitdir"))?
            .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
        prune_git_worktrees(&repo)?;
        assert!(admin.is_dir());
        assert!(!admin.join("locked").exists());
        assert_eq!(
            overlay::worktree_for_branch(&repo, "recoverable"),
            Some(worktree)
        );
        Ok(())
    }
}
