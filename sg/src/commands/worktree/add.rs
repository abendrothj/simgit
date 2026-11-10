//! `add`: allocating a worktree and populating it.

use super::locks::{
    allocation_lock_reason, read_lock, remove_read_lock, worktree_lock_path, BranchClaim,
    PathClaim, RegistryLock,
};
use super::maintenance::delete_branch_unguarded;
use super::registry::{mark_ephemeral, mark_mode, worktree_admin_dir, worktree_path_for_branch};
use super::remove::{remove_worktree_force, teardown_worktree};
use super::repo::{
    canonical_path, default_worktree_path, discover_repo, enclosing_working_tree, git_command_at,
    git_common_command, git_failure, git_output_at, git_output_common, git_path_output,
    resolve_commit, run_command, run_git_at,
};
use super::{
    cow, emit, index, overlay, shell_quote, Caller, PopulateMode, RepoContext, WorktreeAdd,
};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

pub fn add(mut args: WorktreeAdd, json: bool) -> Result<()> {
    if !args.detach && args.branch.is_none() {
        args.branch = Some(format!("agent/{}", Uuid::new_v4()));
    }
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    let created = create_worktree(&repo, &cwd, &args, None)?;
    // Only now, with every marker written: until then gc and prune must see
    // the worktree locked. Keep it last; see
    // `create_worktree_writes_its_markers_before_releasing_the_lock`.
    release_allocation_lock(&repo, &created.target)?;

    if json {
        let path = created.target.display().to_string();
        emit(&json!({
            "worktree": path,
            "path": path,
            "cleanup_token": path,
            "branch": created.branch,
            "base": created.base,
            "mode": created.mode.label(),
            "ephemeral": args.ephemeral,
        }));
    } else {
        eprintln!("mode: {}", created.mode.label());
        println!("{}", created.target.display());
    }
    Ok(())
}

pub(super) struct CreatedWorktree {
    pub(super) target: PathBuf,
    pub(super) branch: Option<String>,
    pub(super) base: String,
    pub(super) mode: PopulateMode,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum WorktreeKind {
    NewBranch,
    ExistingBranch,
    Detached,
}

/// Create a worktree for `args`, resolving a relative `--path` against `cwd`.
/// A `claim` the caller already holds on the branch is used as is; with
/// `None`, one is acquired without waiting.
///
/// The worktree is returned still under its allocation lock. The caller
/// releases it with `release_allocation_lock`, or `run` takes it over with
/// `WorktreeLock::take_over_allocation`, so gc never sees it unlocked first.
pub(super) fn create_worktree(
    repo: &RepoContext,
    cwd: &Path,
    args: &WorktreeAdd,
    claim: Option<BranchClaim>,
) -> Result<CreatedWorktree> {
    check_creation_args(args)?;
    let kind = if args.detach {
        WorktreeKind::Detached
    } else if args.attach {
        WorktreeKind::ExistingBranch
    } else {
        WorktreeKind::NewBranch
    };
    let branch = args.branch.as_deref();
    // Checked and registered under one claim, so two allocations can never
    // both find the branch free; see `BranchClaim`.
    let _branch_claim = match (claim, branch) {
        (Some(claim), _) => Some(claim),
        (None, Some(name)) if kind != WorktreeKind::Detached => {
            Some(BranchClaim::acquire(repo, name)?)
        }
        _ => None,
    };
    if kind == WorktreeKind::NewBranch {
        validate_new_branch(
            repo,
            branch.context("branch is required unless --detach is used")?,
        )?;
    } else if kind == WorktreeKind::ExistingBranch {
        validate_existing_branch(repo, branch.context("existing worktree requires a branch")?)?;
    }
    let reference = branch.map(|name| format!("refs/heads/{name}"));
    let base = resolve_commit(
        repo,
        if kind == WorktreeKind::ExistingBranch {
            reference
                .as_deref()
                .context("existing worktree requires a branch")?
        } else {
            args.base.as_deref().unwrap_or("HEAD")
        },
    )?;
    let path_label = branch
        .map(str::to_owned)
        .unwrap_or_else(|| format!("detached-{}", Uuid::new_v4()));
    let requested = args.path.clone();
    let target = canonical_path(&cwd.join(match requested {
        Some(path) => path,
        None => default_worktree_path(repo, &path_label)?,
    }));

    // One allocation at a time per destination. Two `add --path <same>` runs
    // interleaving through `git worktree add` both used to succeed, leaving
    // two registrations for one directory and a reported branch that was not
    // the one checked out there.
    let _claim = PathClaim::acquire(repo, &target)?;

    refuse_occupied(&target)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
    }

    let target_parent = target.parent().context("worktree path has no parent")?;
    let mode = select_populate_mode(repo, target_parent, args.require_cow)?;

    let sparse = validate_sparse(&args.sparse)?;
    refuse_sparse_overlay(&sparse, mode)?;

    let branch_arg = branch.unwrap_or("");
    let populated = match mode {
        PopulateMode::CowClone => {
            allocate_cow_worktree(repo, branch_arg, &target, &base, kind, &sparse)
        }
        PopulateMode::Overlay => allocate_overlay_worktree(repo, branch_arg, &target, &base, kind),
        PopulateMode::GitCheckout => {
            allocate_git_worktree(repo, branch_arg, &target, &base, kind, &sparse)
        }
    };
    // Registration can fail after another process creates the branch. Git can
    // also leave a branch behind on a partial failure; existence alone cannot
    // tell which process owns it. Never delete an unproven ref here.
    populated?;

    // Best effort: a worktree that works but cannot report its mode is far
    // better than tearing down a good checkout over a marker file.
    let _ = mark_mode(&target, mode);

    if args.ephemeral {
        if let Err(error) = mark_ephemeral(&target) {
            let mut error = error.context("mark worktree ephemeral");
            // Teardown refuses a locked worktree.
            let teardown = release_allocation_lock(repo, &target)
                .and_then(|()| teardown_worktree(repo, &target, true, Caller::Cli));
            if let Err(teardown) = teardown {
                return Err(error.context(format!(
                    "cleanup after ephemeral marker failure: {teardown:#}"
                )));
            }
            if kind == WorktreeKind::NewBranch {
                // This call created the branch and still holds its claim, so
                // the primary-branch guard has nothing to protect.
                let deleted = match reference.as_deref() {
                    Some(reference) => {
                        let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference);
                        delete_branch_unguarded(repo, branch, true)
                    }
                    None => Err(anyhow::anyhow!("new branch has no reference")),
                };
                if let Err(delete) = deleted {
                    error = error.context(format!("delete the new branch: {delete:#}"));
                }
            }
            return Err(error);
        }
    }
    warn_if_inside_repository(repo, &target);

    Ok(CreatedWorktree {
        target,
        branch: branch.map(str::to_owned),
        base,
        mode,
    })
}

/// The checks of `args` that need nothing but the arguments, so a caller can
/// make them before it changes anything.
pub(super) fn check_creation_args(args: &WorktreeAdd) -> Result<()> {
    if args.attach && args.detach {
        bail!("cannot attach an existing branch with --detach");
    }
    if args.attach && args.base.is_some() {
        bail!("--base cannot be used with an existing branch");
    }
    if args.detach && args.branch.is_some() {
        bail!("a branch cannot be combined with --detach");
    }
    validate_sparse(&args.sparse).map(drop)
}

pub(super) fn refuse_sparse_overlay(sparse: &[String], mode: PopulateMode) -> Result<()> {
    if !sparse.is_empty() && mode == PopulateMode::Overlay {
        bail!(
            "--sparse is not supported by the fuse-overlayfs backend: the lower \
             layer is the whole baseline, so nothing is saved by narrowing the \
             view. Use SIMGIT_POPULATE=reflink or =checkout."
        );
    }
    Ok(())
}

/// Refuse a destination that holds something. An empty directory holds
/// nothing to lose, and harnesses routinely pre-create one per job; anything
/// else is someone's data.
pub(super) fn refuse_occupied(target: &Path) -> Result<()> {
    if target.exists() && !directory_is_empty(target) {
        bail!("worktree path already exists: {}", target.display());
    }
    Ok(())
}

/// Choose how to populate the worktree. Prefers per-file reflink, then
/// fuse-overlayfs, then a plain checkout. `SIMGIT_POPULATE` (reflink | overlay |
/// checkout) forces a specific mode and errors if it is unavailable.
pub(super) fn select_populate_mode(
    repo: &RepoContext,
    target_parent: &Path,
    require_cow: bool,
) -> Result<PopulateMode> {
    if let Ok(forced) = std::env::var("SIMGIT_POPULATE") {
        return match forced.to_lowercase().as_str() {
            "reflink" | "cow" | "cow-clone" => {
                if cow::clone_supported(&repo.common_git_dir, target_parent)? {
                    Ok(PopulateMode::CowClone)
                } else {
                    bail!("SIMGIT_POPULATE=reflink but reflink cloning is unsupported here")
                }
            }
            "overlay" => {
                if overlay::supported() {
                    Ok(PopulateMode::Overlay)
                } else {
                    bail!("SIMGIT_POPULATE=overlay but fuse-overlayfs is not installed")
                }
            }
            "checkout" | "git-checkout" => {
                if require_cow {
                    bail!("SIMGIT_POPULATE=checkout conflicts with --require-cow");
                }
                Ok(PopulateMode::GitCheckout)
            }
            other => bail!("unknown SIMGIT_POPULATE={other} (use reflink, overlay, or checkout)"),
        };
    }

    // A directory no probe can be written in has no CoW to offer; only
    // `doctor` tells that apart from "unsupported".
    if cow::clone_supported(&repo.common_git_dir, target_parent).unwrap_or(false) {
        Ok(PopulateMode::CowClone)
    } else if overlay::supported() {
        Ok(PopulateMode::Overlay)
    } else if require_cow {
        bail!(
            "no CoW method available: reflink cloning is unsupported here and \
             fuse-overlayfs is not installed. Omit --require-cow to use a normal \
             Git checkout, or install fuse-overlayfs."
        )
    } else {
        Ok(PopulateMode::GitCheckout)
    }
}

fn validate_new_branch(repo: &RepoContext, branch: &str) -> Result<()> {
    let format = git_output_common(repo, ["check-ref-format", "--branch", branch])?;
    if !format.status.success() {
        bail!("invalid branch name '{branch}'");
    }
    let reference = format!("refs/heads/{branch}");
    let exists = git_output_common(repo, ["show-ref", "--verify", "--quiet", &reference])?;
    match exists.status.code() {
        Some(1) => Ok(()),
        Some(0) => bail!(
            "branch '{branch}' already exists\n\
             check it out with: simgit add --attach {branch}, \
             or run a command in it with: simgit run {branch} -- <command>"
        ),
        _ => Err(git_failure("git show-ref --verify", &exists)),
    }
}

/// An attached branch must exist and must not be checked out anywhere yet:
/// Git allows one worktree per branch, and the cleanup token `add` returns has
/// to name a worktree this allocation created, never someone else's.
fn validate_existing_branch(repo: &RepoContext, branch: &str) -> Result<()> {
    let reference = format!("refs/heads/{branch}");
    let exists = git_output_common(repo, ["show-ref", "--verify", "--quiet", &reference])?;
    match exists.status.code() {
        Some(0) => {}
        Some(1) => bail!(
            "branch '{branch}' does not exist\n\
             create it with: simgit add {branch}"
        ),
        _ => return Err(git_failure("git show-ref --verify", &exists)),
    }
    if let Some(path) = worktree_path_for_branch(repo, branch)?
        .or_else(|| overlay::worktree_for_branch(repo, branch))
    {
        bail!(
            "branch '{branch}' is already checked out at {}\n\
             run a command in it with: simgit run {branch} -- <command>",
            path.display()
        );
    }
    Ok(())
}

/// Register an empty worktree. Population happens afterwards, outside the
/// registry lock, so allocations still fill their worktrees in parallel.
///
/// The registration stays locked until `create_worktree` has marked the
/// worktree and calls `release_allocation_lock`: CoW population takes `.git`
/// away while it clones, Git's prune drops an unlocked registration whose
/// `.git` is missing, and gc must not reap a worktree before it is whole.
fn register_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
) -> Result<()> {
    let mut command = git_common_command(repo);
    command
        .args(["worktree", "add", "--no-checkout", "--lock", "--reason"])
        .arg(allocation_lock_reason());
    match kind {
        WorktreeKind::NewBranch => {
            command.args(["-b", branch]);
        }
        WorktreeKind::Detached => {
            command.arg("--detach");
        }
        WorktreeKind::ExistingBranch => {}
    }
    command.arg(target).arg(match kind {
        WorktreeKind::ExistingBranch => branch,
        WorktreeKind::NewBranch | WorktreeKind::Detached => base,
    });
    let _registry = RegistryLock::exclusive(repo)?;
    run_command(&mut command, "create linked worktree")
}

/// Drop the lock `register_worktree` took, once the worktree is whole or its
/// allocation has failed.
pub(super) fn release_allocation_lock(repo: &RepoContext, target: &Path) -> Result<()> {
    // Remove this allocation's own lock file directly. `git worktree unlock`
    // does no more, but reads every registration to find it, under the
    // registry lock, which made each allocation wait on all the others.
    (|| {
        let path = worktree_lock_path(repo, target)?;
        match read_lock(&path)? {
            None => Ok(()),
            Some(read) if read.contents.trim_end() == allocation_lock_reason() => {
                remove_read_lock(&path, &read, &target.display().to_string())
            }
            // Cleared by hand and locked again since: that lock is not ours.
            Some(_) => bail!("{} is now locked by something else", target.display()),
        }
    })()
    .with_context(|| still_locked(target))
}

/// How to clear the allocation lock a failed allocation keeps.
pub(super) fn still_locked(target: &Path) -> String {
    format!(
        "the worktree is still locked; release it with: simgit unlock {}",
        shell_quote(target)
    )
}

/// Register and populate a worktree by cloning the baseline. On success the
/// registration is still locked; on failure it is rolled back, or kept locked
/// for recovery. The same holds for the other two backends.
pub(super) fn allocate_cow_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
    sparse: &[String],
) -> Result<()> {
    // The guard keeps `prune` off the baseline until population is done.
    let baseline = cow::ensure_baseline(repo, base)?;
    register_worktree(repo, branch, target, base, kind)?;

    let populate_result = if sparse.is_empty() {
        populate_cow_worktree(target, &baseline.tree)
    } else {
        populate_sparse_cow_worktree(target, &baseline.tree, sparse)
    };

    if let Err(error) = populate_result {
        return Err(rolled_back(
            repo,
            target,
            branch,
            kind == WorktreeKind::NewBranch,
            error,
        ));
    }
    Ok(())
}

/// Populate only `sparse` directories, cloning each from the baseline.
///
/// The point is to never materialize the rest: a worktree costs filesystem and
/// index metadata per path, so checking out a tenth of the tree costs about a
/// tenth as much. Two things have to be narrow for that to hold — the files on
/// disk *and* the index. Cone patterns are written while the index is still
/// empty so Git checks nothing out from the object store; the directories are
/// then cloned from the baseline; and `reapply` under `index.sparse` both
/// marks everything outside the cone `skip-worktree` and collapses those paths
/// into single directory entries instead of listing all of them.
fn populate_sparse_cow_worktree(target: &Path, baseline: &Path, sparse: &[String]) -> Result<()> {
    for directory in sparse {
        if !baseline.join(directory).is_dir() {
            bail!("--sparse {directory} is not a directory in this commit");
        }
    }
    configure_sparse(target, sparse)?;
    run_git_at(target, ["read-tree", "HEAD"]).context("initialize linked-worktree index")?;

    for directory in sparse {
        let destination = target.join(directory);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).context("create sparse parent directory")?;
        }
        clone_cone_ancestor_files(target, baseline, Path::new(directory))?;
        if cow::ROOT_CLONE {
            cow::clone_root(&baseline.join(directory), &destination)
        } else {
            fs::create_dir_all(&destination)?;
            cow::clone_tree(&baseline.join(directory), &destination)
        }
        .with_context(|| format!("clone sparse directory {directory}"))?;
    }

    reapply_sparse(target)?;
    ensure_clean(target).context("verify sparse worktree")
}

/// Clone the loose files Git's cone mode keeps alongside a sparse directory.
///
/// `sparse-checkout set services/api` writes the patterns `/*`, `!/*/`,
/// `/services/`, `!/services/*/`, `/services/api/`: every file directly in the
/// repository root and in each ancestor of the cone is *inside* the cone, only
/// their sibling directories are not. Cloning just the cone directory
/// therefore leaves those files missing, and `ensure_clean` reports them as
/// deleted. Directories are skipped here; the cone directory itself is cloned
/// by the caller.
fn clone_cone_ancestor_files(target: &Path, baseline: &Path, directory: &Path) -> Result<()> {
    let mut ancestor = PathBuf::new();
    let parents = directory.parent().unwrap_or(Path::new(""));
    // The repository root first, then every directory above the cone.
    for component in std::iter::once(None).chain(parents.components().map(Some)) {
        if let Some(component) = component {
            ancestor.push(component);
        }
        let source = baseline.join(&ancestor);
        let destination = target.join(&ancestor);
        fs::create_dir_all(&destination)
            .with_context(|| format!("create {}", destination.display()))?;
        for entry in fs::read_dir(&source)
            .with_context(|| format!("read baseline directory {}", source.display()))?
        {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                continue;
            }
            let to = destination.join(entry.file_name());
            if to.exists() {
                continue;
            }
            cow::clone_path(&entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Turn on cone-mode sparse checkout. Called before the index has entries on
/// the CoW path (so nothing is written from the object store) and after it is
/// populated on the plain-checkout path (so Git materializes the cone itself).
fn configure_sparse(target: &Path, sparse: &[String]) -> Result<()> {
    run_git_at(target, ["sparse-checkout", "init", "--cone"])
        .context("enable cone-mode sparse checkout")?;
    let mut command = git_command_at(target)?;
    command.args(["sparse-checkout", "set"]).args(sparse);
    run_command(&mut command, "set sparse-checkout directories")
}

/// Rewrite the index in sparse format: one entry per collapsed directory
/// rather than one per path outside the cone. On a 100k-path tree that is the
/// difference between a 7.9 MiB index per worktree and 0.8 MiB.
fn reapply_sparse(target: &Path) -> Result<()> {
    run_git_at(target, ["config", "index.sparse", "true"])
        .context("enable the sparse index for this worktree")?;
    run_git_at(target, ["sparse-checkout", "reapply"])
        .context("apply sparse patterns to the worktree index")
}

/// Validate and collapse overlapping cones so no directory is cloned twice.
fn validate_sparse(sparse: &[String]) -> Result<Vec<String>> {
    let mut clean: Vec<String> = Vec::with_capacity(sparse.len());
    for entry in sparse {
        let trimmed = entry.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            bail!("--sparse needs a directory inside the repository");
        }
        let path = Path::new(trimmed);
        if path.is_absolute() || path.components().any(|part| part.as_os_str() == "..") {
            bail!("--sparse {entry} must be a path inside the repository");
        }
        if clean
            .iter()
            .any(|parent| path.starts_with(Path::new(parent)))
        {
            continue;
        }
        clean.retain(|child| !Path::new(child).starts_with(path));
        clean.push(trimmed.to_owned());
    }
    Ok(clean)
}

/// Fill a registered, empty linked worktree from the cached baseline.
///
/// Prefers one directory-level clone where the platform supports it, falling
/// back to per-file clones. Both paths then adopt the baseline's
/// stat-refreshed index, which makes the worktree's first `git status` an
/// order of magnitude cheaper than building an index with `read-tree` and
/// leaving Git to rescan every file.
fn populate_cow_worktree(target: &Path, baseline: &Path) -> Result<()> {
    if cow::ROOT_CLONE {
        match root_clone_worktree(target, baseline) {
            Ok(()) => return ensure_clean(target).context("verify cloned worktree"),
            Err(error) => {
                if !is_empty_linked_worktree(target) {
                    return Err(error);
                }
            }
        }
    }
    populate_by_file(target, baseline)
}

/// A root-clone failure may fall back only after the original Git registration
/// has been restored and the target contains no cloned entries.
fn is_empty_linked_worktree(target: &Path) -> bool {
    if !target.join(".git").is_file() {
        return false;
    }
    let Ok(entries) = fs::read_dir(target) else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .all(|entry| entry.file_name() == ".git")
}

/// Per-file population for filesystems without directory-level cloning:
/// clone every file from the baseline in parallel, then adopt the baseline's
/// published index. Caches published by versions that wrote no index fall
/// back to `read-tree`, which leaves Git to rescan on first use.
fn populate_by_file(target: &Path, baseline: &Path) -> Result<()> {
    cow::clone_tree(baseline, target).context("clone cached baseline")?;
    if adopt_baseline_index(target, baseline).is_err() {
        run_git_at(target, ["read-tree", "HEAD"]).context("initialize linked-worktree index")?;
    }
    ensure_clean(target).context("verify cloned worktree")
}

/// Install the baseline's published, stat-refreshed index as this worktree's
/// index, re-recorded to describe the clone's own inodes; see `index`.
fn adopt_baseline_index(target: &Path, baseline: &Path) -> Result<()> {
    let index = cow::baseline_index(baseline).context("baseline has no published index")?;
    let git_dir = PathBuf::from(git_path_output(
        target,
        ["rev-parse", "--absolute-git-dir"],
    )?);
    let worktree_index = git_dir.join("index");
    fs::copy(&index, &worktree_index).context("install baseline index")?;
    if let Err(error) = index::adopt_stat_data(&worktree_index, target) {
        // Leave no half-adopted index behind; the caller's `read-tree`
        // fallback rebuilds one from scratch.
        let _ = fs::remove_file(&worktree_index);
        return Err(error).context("adopt cloned worktree stat data");
    }
    Ok(())
}

/// Replace the empty registered worktree with a whole-tree clone.
///
/// `clonefile(2)` requires a destination that does not exist, so the
/// directory Git just created is removed and its `.git` pointer restored
/// afterwards. Any failure restores the empty worktree so the caller can fall
/// back without leaving a half-populated checkout behind.
fn root_clone_worktree(target: &Path, baseline: &Path) -> Result<()> {
    // Check before the pointer dance: without a published index the per-file
    // path is no worse, and nothing destructive has happened yet.
    cow::baseline_index(baseline).context("baseline has no published index")?;
    let pointer = target.join(".git");
    let pointer_bytes = fs::read(&pointer).context("read linked-worktree pointer")?;

    fs::remove_file(&pointer).context("detach linked-worktree pointer")?;
    if let Err(error) = fs::remove_dir(target) {
        fs::write(&pointer, &pointer_bytes)?;
        return Err(error).context("registered worktree was not empty");
    }
    if let Err(error) = cow::clone_root(baseline, target) {
        restore_empty_worktree(target, &pointer_bytes)?;
        return Err(error);
    }

    let result = (|| {
        fs::write(&pointer, &pointer_bytes).context("restore linked-worktree pointer")?;
        adopt_baseline_index(target, baseline)
    })();
    if let Err(error) = result {
        // The clone exists by this point. Restore the empty linked worktree so
        // the caller's per-file fallback never walks into a populated target.
        if let Err(cleanup) = restore_empty_worktree(target, &pointer_bytes) {
            return Err(error).context(format!("restore failed root clone: {cleanup}"));
        }
        return Err(error);
    }
    Ok(())
}

fn restore_empty_worktree(target: &Path, pointer_bytes: &[u8]) -> Result<()> {
    if target.exists() {
        fs::remove_dir_all(target).context("remove failed cloned worktree")?;
    }
    fs::create_dir_all(target).context("recreate empty linked worktree")?;
    fs::write(target.join(".git"), pointer_bytes).context("restore linked-worktree pointer")?;
    Ok(())
}

pub(super) fn allocate_git_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
    sparse: &[String],
) -> Result<()> {
    // Without CoW there is nothing to clone, so Git does the whole job. The
    // index is built first and the cone configured after it, which is the
    // order in which `sparse-checkout set` materializes the cone itself.
    register_worktree(repo, branch, target, base, kind)?;
    if sparse.is_empty() {
        // The checkout `git worktree add` itself would have run.
        if let Err(error) = run_git_at(
            target,
            ["reset", "--hard", "--no-recurse-submodules", "--quiet"],
        ) {
            return Err(rolled_back(
                repo,
                target,
                branch,
                kind == WorktreeKind::NewBranch,
                error.context("check out linked worktree"),
            ));
        }
        // As with `git worktree add`, a failing hook fails the allocation but
        // keeps the worktree.
        if let Err(error) = run_post_checkout_hook(target) {
            return Err(match release_allocation_lock(repo, target) {
                Ok(()) => error,
                Err(unlock) => error.context(format!("{unlock:#}")),
            });
        }
    }
    let populate = (|| -> Result<()> {
        if !sparse.is_empty() {
            run_git_at(target, ["read-tree", "HEAD"])
                .context("initialize linked-worktree index")?;
            configure_sparse(target, sparse)?;
            reapply_sparse(target)?;
            // `sparse-checkout` only writes paths whose sparsity *changed*, so
            // files that were in the cone all along — everything directly in
            // the repository root and in the cone's ancestors — are never
            // materialized in a `--no-checkout` worktree. Check them out
            // explicitly; this honors skip-worktree, so nothing outside the
            // cone appears.
            run_git_at(target, ["checkout", "--", "."]).context("materialize in-cone files")?;
        }
        ensure_clean(target)
    })();
    if let Err(error) = populate {
        return Err(rolled_back(
            repo,
            target,
            branch,
            kind == WorktreeKind::NewBranch,
            error,
        ));
    }
    Ok(())
}

/// Run the `post-checkout` hook the way `git worktree add` does after its own
/// checkout: from the new worktree, with a null old HEAD and the branch flag.
/// `git hook run` also runs hooks defined in config and does nothing when there
/// are none; Git before 2.36 has no `git hook`, so there the hook file runs
/// directly.
fn run_post_checkout_hook(target: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut command = if git_has_hook_command(target)? {
        let mut command = Command::new("git");
        command.args(["hook", "run", "--ignore-missing", "post-checkout", "--"]);
        command
    } else {
        let hook = target.join(git_path_output(
            target,
            ["rev-parse", "--git-path", "hooks/post-checkout"],
        )?);
        let installed = fs::metadata(&hook)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
        if !installed {
            return Ok(());
        }
        Command::new(&hook)
    };
    let head = git_path_output(target, ["rev-parse", "HEAD"])?;
    let null = "0".repeat(head.len());
    command.current_dir(target).args([&null, &head, "1"]);
    run_command(&mut command, "post-checkout hook")
}

/// Whether this Git has `git hook`, which arrived in 2.36. An unreadable
/// version is taken as a modern Git, whose own error then names the problem.
fn git_has_hook_command(cwd: &Path) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(cwd)
        .arg("version")
        .output()
        .context("run git version")?;
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version
        .trim()
        .trim_start_matches("git version ")
        .split('.')
        .map(|part| part.parse::<u32>());
    Ok(match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) => (major, minor) >= (2, 36),
        _ => true,
    })
}

/// Create a linked worktree whose files are served by a fuse-overlayfs mount:
/// `lowerdir` is the shared read-only baseline, and a per-worktree `upperdir`
/// captures writes. Unchanged files cost no disk, on any Linux filesystem.
fn allocate_overlay_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
) -> Result<()> {
    // The guard keeps `prune` off the baseline until the marker below names it
    // as this overlay's lower; from then on the marker protects it.
    let baseline = cow::ensure_baseline(repo, base)?;

    let overlay_dir = overlay::root(&repo.common_git_dir).join(Uuid::new_v4().to_string());
    let upper = overlay_dir.join("upper");
    let work = overlay_dir.join("work");
    fs::create_dir_all(&upper).context("create overlay upperdir")?;
    fs::create_dir_all(&work).context("create overlay workdir")?;

    if let Err(error) = register_worktree(repo, branch, target, base, kind) {
        fs::remove_dir_all(&overlay_dir).context("cleanup unused overlay state")?;
        return Err(error);
    }

    let populate = (|| -> Result<()> {
        // The `.git` gitlink file must survive the overlay mount (which replaces
        // the directory view), so stage it into the upperdir before mounting.
        fs::rename(target.join(".git"), upper.join(".git"))
            .context("stage worktree gitlink into overlay upperdir")?;
        overlay::mount(&baseline.tree, &upper, &work, target)?;
        run_git_at(target, ["read-tree", "HEAD"]).context("initialize overlay worktree index")?;
        ensure_clean(target).context("verify overlay worktree")?;
        let admin = worktree_admin_dir(target)?;
        overlay::write_marker(
            &admin,
            &overlay::State {
                overlay_dir: overlay_dir.clone(),
                lower: Some(baseline.tree.clone()),
            },
        )?;
        Ok(())
    })();

    // A failure that cannot roll back keeps the allocation lock, so prune
    // cannot drop a registration whose `.git` may exist only in the upperdir.
    // As in `rolled_back`, the population failure stays the cause.
    if let Err(error) = populate {
        if let Err(unmount) = overlay::unmount(target) {
            return Err(error.context(format!(
                "detach failed overlay: {unmount:#}; workspace retained for recovery; {}",
                still_locked(target)
            )));
        }
        // Restore Git's link before asking Git to remove its registration.
        if !target.join(".git").exists() && upper.join(".git").exists() {
            if let Err(restore) = fs::rename(upper.join(".git"), target.join(".git")) {
                return Err(error.context(format!(
                    "restore the gitlink of a failed overlay: {restore}; {}",
                    still_locked(target)
                )));
            }
        }
        let error = rolled_back(repo, target, branch, kind == WorktreeKind::NewBranch, error);
        return Err(match fs::remove_dir_all(&overlay_dir) {
            Ok(()) => error,
            Err(cleanup) => error.context(format!("cleanup failed overlay state: {cleanup}")),
        });
    }
    Ok(())
}

/// True for an existing directory with no entries. A harness that pre-creates
/// one directory per job has put nothing in it yet, so taking it over loses
/// nothing; anything else at that path is someone's data.
fn directory_is_empty(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// Warn — on stderr, never in the JSON record — when a worktree lands inside
/// the repository it branches from. Git reports the whole checkout as
/// untracked content of the source tree, `git clean` can delete it, and
/// builds and searches walk the tree twice. The path is the caller's choice,
/// so this does not refuse it.
fn warn_if_inside_repository(repo: &RepoContext, target: &Path) {
    let source = canonical_path(enclosing_working_tree(repo));
    if !canonical_path(target).starts_with(&source) {
        return;
    }
    eprintln!(
        "warning: {} is inside the repository at {}\n\
         it will show up as untracked clutter in git status and can be removed by git clean; \
         pass --path outside the repository, or set SIMGIT_WORKTREE_ROOT",
        target.display(),
        source.display()
    );
}

fn ensure_clean(worktree: &Path) -> Result<()> {
    let output = git_output_at(worktree, ["status", "--porcelain"])?;
    if !output.status.success() {
        return Err(git_failure("git status --porcelain", &output));
    }
    if !output.stdout.is_empty() {
        bail!(
            "populated worktree does not match its index:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(())
}

/// Roll back a failed allocation and return `error`, the cause. A failing
/// rollback is added as context, so the cause is never replaced by it.
fn rolled_back(
    repo: &RepoContext,
    target: &Path,
    branch: &str,
    new_branch: bool,
    error: anyhow::Error,
) -> anyhow::Error {
    match rollback_created_worktree(repo, target, branch, new_branch) {
        Ok(()) => error,
        Err(rollback) => error.context(format!("{rollback:#}")),
    }
}

fn rollback_created_worktree(
    repo: &RepoContext,
    target: &Path,
    branch: &str,
    new_branch: bool,
) -> Result<()> {
    // One `--force` removes a worktree only once it is unlocked. Should the
    // unlock fail, the removal's own error says why.
    let _ = release_allocation_lock(repo, target);
    remove_worktree_force(repo, target).context("rollback incomplete worktree")?;
    if new_branch {
        // This call created the branch and holds its claim, so no commit can
        // be on it: the primary-branch guard has nothing to protect.
        delete_branch_unguarded(repo, branch, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktree::locks::{allocation_lock_reason, BranchClaim};
    use crate::commands::worktree::remove::remove_worktree_force;
    use crate::commands::worktree::repo::{
        discover_repo, git_common_command, git_output_common, git_path_output, resolve_commit,
        run_command,
    };
    use crate::commands::worktree::test_support::{add_cow_worktree, add_git_worktree, Fixture};
    use crate::commands::worktree::{cow, overlay, WorktreeAdd};
    use anyhow::{Context, Result};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn worktree_paths_with_spaces_are_supported() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("path with spaces");
        add_git_worktree(
            &repo,
            "spaces",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        ensure_clean(&target)?;
        assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
        Ok(())
    }

    /// gc and prune judge an unlocked worktree by its markers, so they must all be
    /// written while the allocation lock holds: `create_worktree` returns with
    /// both in place, and its callers release the lock only afterwards.
    #[test]
    fn create_worktree_writes_its_markers_before_releasing_the_lock() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let args = WorktreeAdd {
            branch: Some("agent/marked".to_owned()),
            detach: false,
            attach: false,
            path: Some(fixture.root.join("marked")),
            sparse: Vec::new(),
            base: None,
            require_cow: false,
            ephemeral: true,
        };
        let created = create_worktree(&repo, &fixture.root, &args, None)?;
        let admin = overlay::admin_dir(&repo, &created.target).context("admin dir")?;
        assert!(admin.join("simgit-ephemeral").is_file());
        assert!(admin.join("simgit-mode").is_file());
        assert_eq!(
            fs::read_to_string(admin.join("locked"))?.trim_end(),
            allocation_lock_reason()
        );
        release_allocation_lock(&repo, &created.target)?;
        Ok(())
    }

    #[test]
    fn native_worktree_fallback_is_clean_and_registered() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let target = fixture.root.join("fallback");
        let base = resolve_commit(&repo, "HEAD")?;
        add_git_worktree(
            &repo,
            "fallback-test",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        ensure_clean(&target)?;
        let listed = git_output_common(&repo, ["worktree", "list", "--porcelain"])?;
        assert!(String::from_utf8_lossy(&listed.stdout).contains(target.to_string_lossy().as_ref()));
        Ok(())
    }

    #[test]
    fn cow_worktree_is_clean_when_filesystem_supports_clones() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
            return Ok(());
        }
        let target = fixture.root.join("cow");
        let base = resolve_commit(&repo, "HEAD")?;
        add_cow_worktree(
            &repo,
            "cow-test",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        ensure_clean(&target)?;
        assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
        assert_eq!(
            fs::read_to_string(target.join("archive-excluded.txt"))?,
            "still part of a checkout\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("filtered.txt"))?,
            "smudged:content\n"
        );
        #[cfg(unix)]
        assert!(fs::symlink_metadata(target.join("file-link"))?
            .file_type()
            .is_symlink());
        Ok(())
    }

    #[test]
    fn create_worktree_accepts_a_claim_its_caller_holds() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let claim = BranchClaim::acquire(&repo, "held/branch")?;
        let args = WorktreeAdd {
            branch: Some("held/branch".to_owned()),
            detach: false,
            attach: false,
            path: Some(fixture.root.join("held")),
            sparse: Vec::new(),
            base: None,
            require_cow: false,
            ephemeral: false,
        };
        let created = create_worktree(&repo, &fixture.repo, &args, Some(claim))?;
        assert_eq!(created.branch.as_deref(), Some("held/branch"));
        Ok(())
    }

    #[test]
    fn root_clone_failure_restores_empty_worktree_before_fallback() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        if !cow::ROOT_CLONE || !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
            return Ok(());
        }
        let base = resolve_commit(&repo, "HEAD")?;
        let baseline = cow::ensure_baseline(&repo, &base)?.tree;
        let published = cow::baseline_index(&baseline).context("baseline publishes an index")?;
        fs::write(published, b"corrupt index")?;

        let target = fixture.root.join("root-fallback");
        register_worktree(
            &repo,
            "root-fallback",
            &target,
            &base,
            WorktreeKind::NewBranch,
        )?;
        populate_cow_worktree(&target, &baseline)?;

        ensure_clean(&target)?;
        assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
        assert!(target.join(".git").is_file());
        Ok(())
    }

    #[test]
    fn per_file_adopted_stat_data_matches_git_refresh() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
            return Ok(());
        }
        let base = resolve_commit(&repo, "HEAD")?;
        let baseline = cow::ensure_baseline(&repo, &base)?.tree;
        let target = fixture.root.join("per-file-index");
        register_worktree(
            &repo,
            "per-file-index",
            &target,
            &base,
            WorktreeKind::NewBranch,
        )?;
        populate_by_file(&target, &baseline)?;

        let git_dir = PathBuf::from(git_path_output(
            &target,
            ["rev-parse", "--absolute-git-dir"],
        )?);
        let installed = git_dir.join("index");
        let adopted = fixture.root.join("per-file-adopted.index");
        fs::copy(&installed, &adopted)?;
        let refreshed = fixture.root.join("per-file-refreshed.index");
        fs::copy(&installed, &refreshed)?;
        let mut refresh = Command::new("git");
        refresh
            .env("GIT_INDEX_FILE", &refreshed)
            .arg(format!("--git-dir={}", repo.common_git_dir.display()))
            .arg(format!("--work-tree={}", target.display()))
            .args(["update-index", "--really-refresh"]);
        run_command(&mut refresh, "git update-index --really-refresh")?;

        assert_eq!(
            fs::read(&adopted)?,
            fs::read(&refreshed)?,
            "per-file stat adoption differs from Git's own refresh"
        );
        Ok(())
    }

    #[test]
    fn per_file_population_is_clean_and_preserves_file_kinds() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
            return Ok(());
        }
        let base = resolve_commit(&repo, "HEAD")?;
        let baseline = cow::ensure_baseline(&repo, &base)?.tree;
        let target = fixture.root.join("per-file");
        register_worktree(&repo, "per-file", &target, &base, WorktreeKind::NewBranch)?;
        populate_by_file(&target, &baseline)?;
        assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
        assert_eq!(
            fs::read_to_string(target.join("filtered.txt"))?,
            "smudged:content\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(fs::symlink_metadata(target.join("file-link"))?
                .file_type()
                .is_symlink());
            let mode = fs::metadata(target.join("tool.sh"))?.permissions().mode();
            assert_ne!(
                mode & 0o100,
                0,
                "executable bit must survive the per-file clone"
            );
        }
        Ok(())
    }

    /// Baseline caches published before the stat-refreshed index existed must
    /// still populate correctly through the `read-tree` fallback.
    #[test]
    fn per_file_population_survives_a_baseline_without_published_index() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
            return Ok(());
        }
        let base = resolve_commit(&repo, "HEAD")?;
        let baseline = cow::ensure_baseline(&repo, &base)?.tree;
        let published = cow::baseline_index(&baseline).context("baseline publishes an index")?;
        fs::remove_file(published)?;
        let target = fixture.root.join("pre-index");
        register_worktree(&repo, "pre-index", &target, &base, WorktreeKind::NewBranch)?;
        populate_by_file(&target, &baseline)?;
        assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
        Ok(())
    }

    #[test]
    fn cow_attachment_and_failed_population_preserve_existing_branch() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
            return Ok(());
        }
        let base = resolve_commit(&repo, "HEAD")?;
        run_command(
            git_common_command(&repo).args(["branch", "existing", &base]),
            "git",
        )?;
        let target = fixture.root.join("attached");
        add_cow_worktree(
            &repo,
            "existing",
            &target,
            &base,
            WorktreeKind::ExistingBranch,
            &[],
        )?;
        ensure_clean(&target)?;
        assert_eq!(resolve_commit(&discover_repo(&target)?, "HEAD")?, base);
        remove_worktree_force(&repo, &target)?;

        // Corrupt only the disposable fixture's baseline to force verification failure.
        let baseline = cow::ensure_baseline(&repo, &base)?.tree;
        fs::write(baseline.join("file.txt"), "incorrect baseline")?;
        assert!(add_cow_worktree(
            &repo,
            "existing",
            &target,
            &base,
            WorktreeKind::ExistingBranch,
            &[]
        )
        .is_err());
        assert!(!target.exists());
        assert_eq!(resolve_commit(&repo, "refs/heads/existing")?, base);
        Ok(())
    }
}
