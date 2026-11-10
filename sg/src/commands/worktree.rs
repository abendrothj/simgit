//! Native Git linked worktrees populated with filesystem copy-on-write clones.
//!
//! `simgit` deliberately has no daemon dependency. Git owns the refs,
//! index, commits, and lifecycle; simgit only avoids repeatedly inflating the
//! same checkout by cloning an immutable cached baseline when the filesystem
//! supports it.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::json;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

mod cow;
mod doctor;
mod hook;
mod index;
mod launch;
mod locks;
mod maintenance;
mod overlay;

pub use doctor::doctor;
pub use hook::{check_requires_before_parsing, hook, hook_arguments, WorktreeHook};
pub use launch::{run_in_worktree, WorktreeRun};
pub use locks::{unlock, WorktreeUnlock};
pub use maintenance::{gc, prune, repair, WorktreeGc, WorktreePrune};

use locks::{
    allocation_lock_reason, ensure_unlocked, lock_present, lock_state, refuse_unfinished,
    unfinished, unfinished_worktree, BranchClaim, PathClaim, RegistryLock, WorktreeLock,
};
use maintenance::{delete_branch_unguarded, delete_local_branch, registered_worktree};

#[cfg(test)]
use locks::{lock_owner_of, lock_owner_pid, process_alive, worktree_lock_path};
#[cfg(test)]
use maintenance::{prune_baseline_cache, prune_git_worktrees, run_gc};

#[derive(Args)]
pub struct WorktreeAdd {
    /// Branch name to create (for example, feat/my-feature). When omitted,
    /// creates a unique `agent/<uuid>` branch unless --detach is used. With
    /// --attach, an existing branch to check out instead.
    pub branch: Option<String>,

    /// Create a detached worktree without creating or deleting a branch.
    #[arg(long, conflicts_with = "branch")]
    pub detach: bool,

    /// Check out an existing branch at its current commit instead of creating
    /// one. Refused when the branch is missing or already checked out.
    #[arg(long, requires = "branch", conflicts_with = "base")]
    pub attach: bool,

    /// Worktree path. Defaults to a slug of the branch under
    /// `../.simgit/<repo>/`.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Check out only these directories (Git cone-mode sparse checkout).
    /// Repeatable. Cuts the worktree's metadata cost proportionally.
    #[arg(long = "sparse", value_name = "DIR")]
    pub sparse: Vec<String>,

    /// Commit-ish to start from. Defaults to HEAD.
    #[arg(long)]
    pub base: Option<String>,

    /// Fail instead of using a normal Git checkout when CoW is unavailable.
    #[arg(long)]
    pub require_cow: bool,

    /// Mark the worktree as ephemeral so `gc` can reap it automatically.
    #[arg(long)]
    pub ephemeral: bool,
}

#[derive(Args)]
pub struct WorktreeRemove {
    /// Worktree path or branch name. Defaults to the worktree containing the
    /// current directory.
    pub target: Option<String>,

    /// Commit all changes before removing the worktree.
    #[arg(long)]
    pub commit: bool,

    /// Commit message for --commit.
    #[arg(short, long, default_value = "simgit remove")]
    pub message: String,

    /// Discard uncommitted changes. Without this flag, removal refuses a dirty
    /// worktree.
    #[arg(long, conflicts_with = "commit")]
    pub discard_dirty: bool,

    /// Delete the worktree branch too.
    #[arg(long)]
    pub delete_branch: bool,

    /// Permit deleting a branch that is not merged. Requires --delete-branch.
    #[arg(long)]
    pub delete_unmerged: bool,
}

#[derive(Debug)]
struct RepoContext {
    pub(super) top_level: PathBuf,
    pub(super) common_git_dir: PathBuf,
    /// The main working tree; the parent of a bare repository named `.git`;
    /// else the common git dir. See `find_main_worktree`.
    pub(super) main_worktree: PathBuf,
    /// The outermost superproject when this is a submodule.
    pub(super) superproject: Option<Superproject>,
}

/// Where a submodule's default worktrees go; see `find_superproject`.
#[derive(Debug)]
struct Superproject {
    /// The outermost working tree enclosing the submodule's checkout.
    working_tree: PathBuf,
    /// That superproject's main working tree, which the worktrees go beside.
    main_worktree: PathBuf,
    /// The submodule's directory under `.modules`.
    module_dir: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PopulateMode {
    /// Per-file reflink clone (APFS `clonefile`, Linux reflink).
    CowClone,
    /// `fuse-overlayfs` mount — CoW on any Linux filesystem (incl. ext4).
    Overlay,
    /// Ordinary `git checkout` — a full copy, no CoW benefit.
    GitCheckout,
}

impl PopulateMode {
    fn label(self) -> &'static str {
        match self {
            Self::CowClone => "cow-clone",
            Self::Overlay => "overlay",
            Self::GitCheckout => "git-checkout",
        }
    }
}

pub fn add(mut args: WorktreeAdd, json: bool) -> Result<()> {
    if !args.detach && args.branch.is_none() {
        args.branch = Some(format!("agent/{}", Uuid::new_v4()));
    }
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    let created = create_worktree(&repo, &cwd, &args, None)?;
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

struct CreatedWorktree {
    target: PathBuf,
    branch: Option<String>,
    base: String,
    mode: PopulateMode,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WorktreeKind {
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
fn create_worktree(
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
fn check_creation_args(args: &WorktreeAdd) -> Result<()> {
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

fn refuse_sparse_overlay(sparse: &[String], mode: PopulateMode) -> Result<()> {
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
fn refuse_occupied(target: &Path) -> Result<()> {
    if target.exists() && !directory_is_empty(target) {
        bail!("worktree path already exists: {}", target.display());
    }
    Ok(())
}

/// Choose how to populate the worktree. Prefers per-file reflink, then
/// fuse-overlayfs, then a plain checkout. `SIMGIT_POPULATE` (reflink | overlay |
/// checkout) forces a specific mode and errors if it is unavailable.
fn select_populate_mode(
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
fn release_allocation_lock(repo: &RepoContext, target: &Path) -> Result<()> {
    (|| {
        let mut command = git_common_command(repo);
        command.args(["worktree", "unlock"]).arg(target);
        // Git finds the worktree by reading every registration.
        let _registry = RegistryLock::shared(repo)?;
        run_command(&mut command, "unlock allocated worktree")
    })()
    .with_context(|| still_locked(target))
}

/// How to clear the allocation lock a failed allocation keeps.
fn still_locked(target: &Path) -> String {
    format!(
        "the worktree is still locked; release it with: simgit unlock {}",
        shell_quote(target)
    )
}

/// Register and populate a worktree by cloning the baseline. On success the
/// registration is still locked; on failure it is rolled back, or kept locked
/// for recovery. The same holds for the other two backends.
fn allocate_cow_worktree(
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

fn allocate_git_worktree(
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

/// Who tears a worktree down, so that its refusals name a fix that caller's
/// user can run: `remove` takes a flag, a hook's user a whole command.
#[derive(Clone, Copy)]
enum Caller {
    Cli,
    Hook,
}

impl Caller {
    /// Adding `flag` to the removal of `target`, in this caller's terms.
    fn flag(self, repo: &RepoContext, target: &Path, flag: &str) -> String {
        match self {
            Caller::Cli => flag.to_owned(),
            Caller::Hook => format!(
                "`{}`",
                simgit_anywhere(repo, &format!("remove {} {flag}", shell_quote(target)))
            ),
        }
    }

    /// How to recreate a deleted worktree whose sparse checkout cannot be read
    /// back: the CLI names the cone to pass, a hook's user drops the registration.
    fn sparse_recovery(self, repo: &RepoContext, target: &Path) -> String {
        match self {
            Caller::Cli => "pass --sparse with the directories to check out".to_owned(),
            Caller::Hook => format!(
                "drop its registration with `{}`, then create the worktree again",
                simgit_anywhere(repo, &format!("remove {}", shell_quote(target)))
            ),
        }
    }

    /// The step after keeping a detached worktree's commit.
    fn then_remove(self, repo: &RepoContext, target: &Path) -> String {
        match self {
            Caller::Cli => "then remove it".to_owned(),
            Caller::Hook => format!(
                "then remove it with `{}`",
                simgit_anywhere(repo, &format!("remove {}", shell_quote(target)))
            ),
        }
    }
}

/// `simgit <arguments>` as a command that works from any directory: simgit
/// finds its repository from the one it runs in, and the hook's worktrees lie
/// outside it.
fn simgit_anywhere(repo: &RepoContext, arguments: &str) -> String {
    format!(
        "cd {} && simgit {arguments}",
        shell_quote(&repo.main_worktree)
    )
}

/// `text` as one shell word, for a command a person pastes: bare when it has
/// only characters no shell treats specially, else single-quoted.
fn shell_quote(text: impl AsRef<std::ffi::OsStr>) -> String {
    let text = text.as_ref().to_string_lossy();
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Recheck dirty state at teardown so a command finishing during GC selection
/// cannot turn a previously clean workspace into silently discarded work.
fn teardown_worktree(
    repo: &RepoContext,
    target: &Path,
    discard_dirty: bool,
    caller: Caller,
) -> Result<()> {
    let lock = ensure_unlocked(repo, target)?;
    // Every caller is covered, and a commit landing after the caller's own
    // check cannot turn a dirty worktree into a clean one that dangles it.
    refuse_unreached_head(repo, target, caller)?;
    let discard = caller.flag(repo, target, "--discard-dirty");
    // An unpopulated worktree shows every file as deleted: never offer to
    // commit that.
    let dirty_refusal = || match unfinished(&lock) {
        true => unfinished_worktree(target, &discard),
        false => uncommitted_changes(repo, target, caller),
    };
    if registration_gone(repo, target) {
        // Deleted by hand: only this registration is left to drop.
        {
            let _registry = RegistryLock::exclusive(repo)?;
            drop_gone_registration(repo, target, discard_dirty, &discard)?;
        }
        prune_empty_worktree_dirs(repo, target);
        return Ok(());
    }
    // Report uncommitted work in simgit's own terms on every path: Git's
    // `worktree remove` only knows about --force, not about --commit.
    if !discard_dirty && worktree_dirty(target)? {
        return Err(dirty_refusal());
    }
    let result = if let Some(state) = overlay::state(repo, target) {
        let lock = WorktreeLock::acquire(repo, target)?;
        if !discard_dirty && worktree_dirty(target)? {
            return Err(dirty_refusal());
        }
        // Dropping the registration below would refuse this only after the
        // files are gone.
        if !discard_dirty && registration_holds_uncommitted(repo, target)? {
            bail!(
                "the registration of {} holds submodule repositories or refs of its own; \
                 keep them, or discard them with {discard}",
                target.display()
            );
        }
        overlay::unmount(target)?;
        remove_dir_if_present(target)?;
        remove_dir_if_present(&state.overlay_dir)?;
        lock.release()?;
        let _registry = RegistryLock::exclusive(repo)?;
        drop_gone_registration(repo, target, discard_dirty, &discard)
    } else if discard_dirty {
        remove_worktree_force(repo, target)
    } else {
        let mut command = git_common_command(repo);
        command.args(["worktree", "remove"]).arg(target);
        run_retrying_half_written_registration(&mut command, "git worktree remove")
    };
    if result.is_ok() {
        prune_empty_worktree_dirs(repo, target);
    }
    result
}

/// The refusal of a worktree with uncommitted changes, naming what keeps them.
fn uncommitted_changes(repo: &RepoContext, target: &Path, caller: Caller) -> anyhow::Error {
    let branch = list_worktrees(repo)
        .ok()
        .and_then(|entries| entries.into_iter().find(|entry| entry.path == target))
        .and_then(|entry| entry.branch)
        .or_else(|| overlay::branch(repo, target));
    let keep = match branch {
        Some(_) => format!(
            "keep the work with {}",
            caller.flag(repo, target, "--commit")
        ),
        None => keep_detached_changes(repo, target, caller),
    };
    anyhow::anyhow!(
        "worktree has uncommitted changes: {}\n{keep}, or discard it with {}",
        target.display(),
        caller.flag(repo, target, "--discard-dirty")
    )
}

/// How to keep a detached worktree's changes: `--commit` alone would make a
/// commit no ref reaches, which removal then refuses to drop.
fn keep_detached_changes(repo: &RepoContext, target: &Path, caller: Caller) -> String {
    let commit = match caller {
        Caller::Cli => "`--commit`".to_owned(),
        Caller::Hook => caller.flag(repo, target, "--commit"),
    };
    format!(
        "keep the work by creating a branch with `git -C {} switch -c <name>`, \
         then removing the worktree with {commit}",
        shell_quote(target)
    )
}

/// Drop the registration of a worktree whose directory is gone, and only that
/// one; the caller holds the exclusive registry lock. The directory is checked
/// again last, since Git would delete one that reappeared if it is clean. Git
/// refuses a registration that was locked meanwhile. A refusal names `discard`
/// as the way to discard what the registration holds.
fn drop_gone_registration(
    repo: &RepoContext,
    target: &Path,
    discard_dirty: bool,
    discard: &str,
) -> Result<()> {
    let holds = !discard_dirty
        && registration_holds_uncommitted(repo, target).with_context(|| {
            format!(
                "cannot tell whether the registration of {} holds work",
                target.display()
            )
        })?;
    if holds {
        let admin = overlay::admin_dir(repo, target).unwrap_or_default();
        bail!(
            "{} was deleted, but its registration {admin} still holds staged changes, a merge, \
             rebase, cherry-pick or revert in progress, submodule repositories (`modules/`) \
             or refs of its own (`refs/worktree`, `refs/bisect`, `refs/rewritten`)\n\
             inspect what is staged with `git --git-dir={quoted} diff --cached --stat`, \
             its refs with `git --git-dir={quoted} for-each-ref refs/worktree/ refs/bisect/ \
             refs/rewritten/`, and its submodule repositories under {admin}/modules; \
             or discard all of it with {discard}",
            target.display(),
            admin = admin.display(),
            quoted = shell_quote(&admin),
        );
    }
    if !matches!(target.try_exists(), Ok(false)) {
        bail!(
            "{} reappeared while its registration was being dropped; run the command again",
            target.display()
        );
    }
    let mut command = git_common_command(repo);
    command.args(["worktree", "remove"]).arg(target);
    run_command(&mut command, "drop the registration of a deleted worktree")
}

/// Drop the registration of a worktree deleted by hand, so that it can be
/// allocated again, with `remove`'s refusals of whatever that would lose: a
/// lock, a detached HEAD no ref reaches, or work the registration holds. Each
/// refusal names its recovery; `then` is the caller's step after keeping a
/// detached commit.
fn drop_deleted_registration(
    repo: &RepoContext,
    entry: &WorktreeEntry,
    then: &str,
    caller: Caller,
) -> Result<()> {
    ensure_unlocked(repo, &entry.path)?;
    if entry.branch.is_none() {
        refuse_unreached_entry(repo, entry, then)?;
    }
    let spec = match &entry.branch {
        Some(reference) => short_branch(reference).to_owned(),
        None => shell_quote(&entry.path),
    };
    let _registry = RegistryLock::exclusive(repo)?;
    let discard = match caller {
        Caller::Cli => format!("`simgit remove {spec} --discard-dirty`"),
        Caller::Hook => caller.flag(repo, &entry.path, "--discard-dirty"),
    };
    drop_gone_registration(repo, &entry.path, false, &discard)
}

/// What a deleted worktree's registration records that reattaching keeps:
/// whether it is ephemeral, and its cone-mode sparse directories (read only
/// when `sparse` is true). An entry whose settings cannot be read is an error.
fn deleted_worktree_settings(
    repo: &RepoContext,
    entry: &WorktreeEntry,
    sparse: bool,
    caller: Caller,
) -> Result<(bool, Vec<String>)> {
    let target = &entry.path;
    let admin = overlay::admin_dir(repo, target)
        .with_context(|| format!("cannot find the registration of {}", target.display()))?;
    let marker = admin.join("simgit-ephemeral");
    let ephemeral = marker
        .try_exists()
        .with_context(|| format!("check {}", marker.display()))?;
    if !sparse {
        return Ok((ephemeral, Vec::new()));
    }
    let unreadable = || format!("cannot read the sparse checkout of {}", target.display());
    let config = git_at_admin(
        repo,
        &admin,
        &["config", "--get-regexp", "^core\\.sparsecheckout"],
    )?;
    let settings = match config.status.code() {
        Some(0) => String::from_utf8(config.stdout).with_context(unreadable)?,
        Some(1) => String::new(),
        _ => return Err(git_failure("git config", &config)).with_context(unreadable),
    };
    let set = |key: &str| settings.lines().any(|line| line == format!("{key} true"));
    if !set("core.sparsecheckout") {
        return Ok((ephemeral, Vec::new()));
    }
    if !set("core.sparsecheckoutcone") {
        bail!(
            "{}: its sparse checkout is not in cone mode, so --sparse cannot recreate it; {}",
            unreadable(),
            caller.sparse_recovery(repo, target)
        );
    }
    let list = git_at_admin(repo, &admin, &["sparse-checkout", "list"])?;
    if !list.status.success() {
        return Err(git_failure("git sparse-checkout list", &list)).with_context(unreadable);
    }
    let directories: Vec<String> = String::from_utf8(list.stdout)
        .with_context(unreadable)?
        .lines()
        .map(str::to_owned)
        .collect();
    // Git quotes a name it cannot print plainly; never guess at its unquoting.
    if directories.iter().any(|dir| dir.starts_with('"')) {
        bail!(
            "{}: a directory name is quoted; {}",
            unreadable(),
            caller.sparse_recovery(repo, target)
        );
    }
    Ok((ephemeral, directories))
}

/// Whether a worktree's registration holds uncommitted work of its own: an
/// operation in progress, submodule repositories, per-worktree refs, or an
/// index that differs from HEAD. It reads only the registration, so it answers
/// for a worktree whose directory is gone.
fn registration_holds_uncommitted(repo: &RepoContext, worktree: &Path) -> Result<bool> {
    let admin = overlay::admin_dir(repo, worktree)
        .with_context(|| format!("cannot find the registration of {}", worktree.display()))?;
    admin_holds_uncommitted(repo, &admin)
}

/// `registration_holds_uncommitted` for the admin dir `admin`.
fn admin_holds_uncommitted(repo: &RepoContext, admin: &Path) -> Result<bool> {
    const IN_PROGRESS: [&str; 6] = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ];
    // An entry that cannot be checked is not known to be absent.
    let present = |name: &str| {
        let path = admin.join(name);
        path.try_exists()
            .with_context(|| format!("check {}", path.display()))
    };
    for name in IN_PROGRESS {
        if present(name)? {
            return Ok(true);
        }
    }
    // The worktree's submodule repositories live here, and may hold the only
    // copy of a commit.
    if holds_a_file(&admin.join("modules"))? {
        return Ok(true);
    }
    let git = |args: &[&str]| git_at_admin(repo, admin, args);
    // So do its own refs, which may be all that reaches a commit. Git lists
    // them, since under reftable they are not files of their own.
    let refs = git(&[
        "for-each-ref",
        "--count=1",
        "--format=%(refname)",
        "refs/worktree/",
        "refs/bisect/",
        "refs/rewritten/",
    ])?;
    if !refs.status.success() {
        return Err(git_failure("git for-each-ref", &refs));
    }
    if !refs.stdout.is_empty() {
        return Ok(true);
    }
    // Nothing was ever staged without an index.
    if !present("index")? {
        return Ok(false);
    }
    let head = git(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
    match head.status.code() {
        Some(0) => {
            let head = String::from_utf8(head.stdout)?;
            let diff = git(&["diff-index", "--cached", "--quiet", head.trim()])?;
            match diff.status.code() {
                Some(0) => Ok(false),
                Some(1) => Ok(true),
                _ => Err(git_failure("git diff-index --cached", &diff)),
            }
        }
        // An unborn HEAD has no commit to compare with: any entry is staged.
        Some(1) => {
            let files = git(&["ls-files", "-z"])?;
            if !files.status.success() {
                return Err(git_failure("git ls-files", &files));
            }
            Ok(!files.stdout.is_empty())
        }
        _ => Err(git_failure("git rev-parse HEAD", &head)),
    }
}

/// Run git on the registration `admin`, from the main checkout.
fn git_at_admin(repo: &RepoContext, admin: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .current_dir(&repo.main_worktree)
        .arg(format!("--git-dir={}", admin.display()))
        .args(args)
        .output()
        .context("run git")
}

/// A registration's HEAD: the branch ref it names, or the commit it holds.
enum AdminHead {
    Branch(String),
    Detached(String),
}

/// Read the HEAD of the registration `admin` through Git, never from its
/// `HEAD` file: under reftable that file is a stub naming
/// `refs/heads/.invalid`, detached or not. A HEAD Git cannot read is an error.
fn admin_head(repo: &RepoContext, admin: &Path) -> Result<AdminHead> {
    let symbolic = git_at_admin(repo, admin, &["symbolic-ref", "-q", "HEAD"])?;
    match symbolic.status.code() {
        Some(0) => Ok(AdminHead::Branch(
            String::from_utf8(symbolic.stdout)?.trim().to_owned(),
        )),
        Some(1) => {
            let commit = git_at_admin(
                repo,
                admin,
                &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            )?;
            if !commit.status.success() {
                return Err(git_failure("git rev-parse HEAD", &commit));
            }
            Ok(AdminHead::Detached(
                String::from_utf8(commit.stdout)?.trim().to_owned(),
            ))
        }
        _ => Err(git_failure("git symbolic-ref HEAD", &symbolic)),
    }
}

/// Whether `dir` holds a file at any depth. A missing `dir` holds none; one
/// that cannot be read is an error, never an answer.
fn holds_a_file(dir: &Path) -> Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("check {}", entry.path().display()))?;
        if !file_type.is_dir() || holds_a_file(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Drop the `.simgit/<repo>` scaffolding once its last worktree is gone, so
/// removing every worktree leaves no empty directories beside the repository.
/// Only empty directories are removed, and only up to the `.simgit` root.
fn prune_empty_worktree_dirs(repo: &RepoContext, target: &Path) {
    if std::env::var_os("SIMGIT_WORKTREE_ROOT").is_some() {
        // A directory the user chose is theirs to keep, empty or not.
        return;
    }
    let Some(root) = default_worktree_path(repo, "unused")
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return;
    };
    if target.parent() != Some(root.as_path())
        || !root
            .ancestors()
            .any(|dir| dir.file_name() == Some(OsStr::new(".simgit")))
    {
        return;
    }
    // Up to and including `.simgit`, stopping at the first that is not empty.
    for dir in root.ancestors() {
        if fs::remove_dir(dir).is_err() || dir.file_name() == Some(OsStr::new(".simgit")) {
            break;
        }
    }
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn remove(args: WorktreeRemove, json: bool) -> Result<()> {
    // Refuse before touching the repository: --delete-unmerged widens branch
    // deletion, so on its own it silently promises something it cannot do.
    if args.delete_unmerged && !args.delete_branch {
        bail!("--delete-unmerged only relaxes branch deletion; pass --delete-branch too");
    }
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    let (target, gone) = match lookup_worktree_target(&repo, args.target.as_deref())? {
        TargetLookup::Found(path) => (path, false),
        // Deleted by hand: only the registration is left, and it still names
        // the branch for --delete-branch.
        TargetLookup::Gone(path) => (path, true),
        // Removing what is already gone is the state the caller asked for. An
        // agent that crashed mid-cleanup, or handed the same cleanup token
        // back twice, must not have to tell the two cases apart.
        TargetLookup::Absent(spec) => return remove_absent(&repo, &spec, &args, json),
        // A directory that is no longer registered is not a worktree, however
        // much it looks like one: finishing that teardown is its own path.
        TargetLookup::Stray(path) => return remove_stray(&path, &args, json),
    };
    let entry = list_worktrees(&repo)?
        .into_iter()
        .find(|entry| entry.path == target);
    // Git cannot remove the main worktree, so refuse before --commit commits
    // the user's files in it and teardown then fails. First, so a main entry
    // with no lock to find (a bare repository) is still named as such.
    if entry.as_ref().is_some_and(|entry| entry.is_main) {
        bail!("{} is the main checkout, not a worktree", target.display());
    }
    let lock = ensure_unlocked(&repo, &target)?;
    // Committing an unpopulated worktree would record every file as deleted.
    if args.commit && !gone && unfinished(&lock) {
        return Err(unfinished_worktree(&target, "--discard-dirty"));
    }
    let branch = entry
        .and_then(|entry| entry.branch)
        .or_else(|| overlay::branch(&repo, &target));
    // Fail before anything is torn down: a detached worktree has no branch to
    // delete, and silently reporting success would hide the mistake.
    if args.delete_branch && branch.is_none() {
        bail!(
            "--delete-branch needs a branch: {} is detached",
            target.display()
        );
    }
    // Before --commit makes a commit; teardown rechecks in case a commit lands
    // between here and there.
    refuse_unreached_head(&repo, &target, Caller::Cli)?;
    // A detached worktree's commit would be reached by no ref, so teardown
    // would refuse it, after the commit, on every retry.
    if args.commit && !gone && branch.is_none() && worktree_dirty(&target)? {
        bail!(
            "{} is detached, so --commit would make a commit no branch reaches\n{}",
            target.display(),
            keep_detached_changes(&repo, &target, Caller::Cli)
        );
    }
    // A commit that succeeded is never rolled back when a later step fails: it
    // is the work the caller asked to keep, and an "atomic" remove that
    // un-commits it would trade a visible seam for destroyed work. The failure
    // says so instead.
    let mut commit = None;
    // A deleted worktree holds nothing left to commit.
    if args.commit && !gone {
        run_git_at(&target, ["add", "-A"]).context("stage worktree changes")?;
        let diff = git_output_at(&target, ["diff", "--cached", "--quiet"])?;
        match diff.status.code() {
            Some(0) => {
                if !json {
                    eprintln!("no changes to commit");
                }
            }
            Some(1) => {
                run_git_at(&target, ["commit", "-m", &args.message])
                    .context("commit worktree changes")?;
                commit = Some(git_path_output(&target, ["rev-parse", "HEAD"])?);
            }
            _ => return Err(git_failure("git diff --cached --quiet", &diff)),
        }
    }

    teardown_worktree(&repo, &target, args.discard_dirty, Caller::Cli)
        .map_err(|error| annotate_commit_kept(error, commit.as_deref(), branch.as_deref()))?;

    let mut branch_deleted = false;
    if let (true, Some(reference)) = (args.delete_branch, branch) {
        delete_local_branch(&repo, &reference, args.delete_unmerged).map_err(|error| {
            // The worktree is gone by now, so a retry by path can no longer
            // name this branch. Hand back the form that still works.
            let name = short_branch(&reference);
            error.context(format!(
                "the worktree is removed, but branch {name} was not deleted; \
                 retry with `simgit remove {name} --delete-branch`"
            ))
        })?;
        branch_deleted = true;
    }

    if json {
        emit(&json!({
            "removed": target.display().to_string(),
            "already_absent": gone,
            "committed": commit.is_some(),
            "commit": commit,
            "branch_deleted": branch_deleted,
        }));
    } else if gone {
        println!("{} (already absent)", target.display());
    } else {
        println!("{}", target.display());
    }
    Ok(())
}

/// Report a target that names no worktree, and still honour `--delete-branch`
/// for a branch whose worktree is already gone — the ref outliving its
/// checkout is exactly the leftover the flag exists to clean up.
fn remove_absent(repo: &RepoContext, spec: &str, args: &WorktreeRemove, json: bool) -> Result<()> {
    let mut branch_deleted = false;
    if args.delete_branch {
        // A path that is gone names no branch: the registration that mapped
        // one to the other went with it. Reporting `branch_deleted: false`
        // here reads as "there was nothing to delete", which is a guess.
        if spec_is_path(spec) {
            bail!(
                "cannot infer a branch from an already-removed path; pass the branch name\n\
                 {spec} no longer exists, so nothing records which branch it held"
            );
        }
        // `show-ref --verify` failing means the ref is gone too, and then
        // nothing of this workspace is left to delete.
        let reference = format!("refs/heads/{spec}");
        let exists = git_output_common(repo, ["show-ref", "--verify", "--quiet", &reference])?;
        if exists.status.success() {
            delete_local_branch(repo, &reference, args.delete_unmerged)?;
            branch_deleted = true;
        }
    }

    if json {
        emit(&json!({
            "removed": spec,
            "already_absent": true,
            "committed": false,
            "commit": null,
            "branch_deleted": branch_deleted,
        }));
    } else {
        println!("{spec} (already absent)");
    }
    Ok(())
}

/// Finish a teardown Git left half-done.
///
/// `git worktree remove` can delete a worktree's contents and its registration
/// and still fail to unlink the directory itself — a read-only parent is
/// enough. What survives is an empty directory that is no longer a worktree,
/// and a retried cleanup has to converge on it instead of asking Git about a
/// checkout Git has already forgotten.
fn remove_stray(path: &Path, args: &WorktreeRemove, json: bool) -> Result<()> {
    if args.delete_branch {
        bail!(
            "cannot infer a branch from a path that is no longer a worktree; pass the branch name\n\
             {} is not registered, so nothing records which branch it held",
            path.display()
        );
    }
    // Emptiness is the whole safety argument for deleting this at all.
    if !is_empty_dir(path)? {
        bail!(
            "{} is not a worktree of this repository, and it is not empty\n\
             simgit does not delete directories it does not manage; inspect it and remove it yourself",
            path.display()
        );
    }
    // Best effort: whatever stopped Git from unlinking this directory stops
    // simgit too, and the worktree the caller asked about is gone either way.
    let _ = fs::remove_dir(path);

    if json {
        emit(&json!({
            "removed": path.display().to_string(),
            "already_absent": true,
            "committed": false,
            "commit": null,
            "branch_deleted": false,
        }));
    } else {
        println!("{} (already absent)", path.display());
    }
    Ok(())
}

fn is_empty_dir(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path).with_context(|| format!("read {}", path.display()))?;
    Ok(entries.next().is_none())
}

/// Name the commit a partially completed `remove --commit` already created.
///
/// A `--json` failure emits no JSON, so this diagnostic is the only place the
/// fact can be said — and without it "removing the worktree failed" is
/// indistinguishable from a failure that committed nothing, which is exactly
/// the ambiguity that makes a script afraid to retry.
fn annotate_commit_kept(
    error: anyhow::Error,
    commit: Option<&str>,
    branch: Option<&str>,
) -> anyhow::Error {
    let Some(commit) = commit else {
        return error;
    };
    let short = commit.get(..12).unwrap_or(commit);
    let location = match branch {
        Some(reference) => format!(" on {}", short_branch(reference)),
        None => String::new(),
    };
    error.context(format!(
        "the worktree was not removed, but its changes are committed as {short}{location}; \
         that commit is kept, and repeating this remove will not create a second one"
    ))
}

fn short_branch(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

/// Whether a removal target names a filesystem location rather than a branch.
/// Branch names may contain slashes, so only the spellings Git rejects as ref
/// names — absolute and explicitly relative paths — are decided here.
fn spec_is_path(spec: &str) -> bool {
    Path::new(spec).is_absolute()
        || spec == "."
        || spec == ".."
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('~')
}

/// What a user-supplied worktree reference resolved to.
enum TargetLookup {
    /// A live worktree: an existing registered path, or the worktree checked
    /// out on a branch.
    Found(PathBuf),
    /// A directory that exists where a worktree used to be, but that Git no
    /// longer registers — the residue of an interrupted teardown.
    Stray(PathBuf),
    /// A registered worktree whose directory was deleted by hand; see
    /// `registration_gone`.
    Gone(PathBuf),
    /// The reference names no worktree: an already-removed path, or a branch
    /// that has no worktree.
    Absent(String),
}

/// Resolve a user-supplied worktree reference — an explicit path, a branch
/// name, or (when omitted) the worktree containing the current directory — to
/// an absolute worktree path.
fn lookup_worktree_target(repo: &RepoContext, target: Option<&str>) -> Result<TargetLookup> {
    let Some(spec) = target else {
        return Ok(TargetLookup::Found(repo.top_level.clone()));
    };
    let as_path = absolute_path(PathBuf::from(spec))?;
    let canonical = canonical_path(&as_path);
    let stray = if as_path.exists() {
        if is_registered_worktree(repo, &canonical)? {
            return Ok(TargetLookup::Found(canonical));
        }
        // An unregistered directory must not shadow a branch of the same name,
        // so it is remembered rather than returned here.
        Some(canonical)
    } else if registration_gone(repo, &canonical) && is_registered_worktree(repo, &canonical)? {
        return Ok(TargetLookup::Gone(canonical));
    } else {
        None
    };
    if let Some(path) = worktree_path_for_branch(repo, spec)? {
        return Ok(if registration_gone(repo, &path) {
            TargetLookup::Gone(path)
        } else {
            TargetLookup::Found(path)
        });
    }
    if let Some(path) = overlay::worktree_for_branch(repo, spec) {
        return Ok(TargetLookup::Found(path));
    }
    match stray {
        Some(path) => Ok(TargetLookup::Stray(path)),
        None => Ok(TargetLookup::Absent(spec.to_owned())),
    }
}

/// Whether `path` is still a registered worktree of this repository.
///
/// Existence is not the test. A `git worktree remove` that deleted the
/// registration but could not unlink the directory leaves a shell behind, and
/// treating that as a live worktree makes every later command interrogate a
/// checkout Git has already forgotten.
fn is_registered_worktree(repo: &RepoContext, path: &Path) -> Result<bool> {
    if list_worktrees(repo)?
        .iter()
        .any(|entry| canonical_path(&entry.path).as_path() == path)
    {
        return Ok(true);
    }
    Ok(overlay::state(repo, path).is_some())
}

/// Whether a worktree's directory is gone while its registration stays: what
/// `rm -rf` leaves. An unreadable path is not gone, and neither is an overlay,
/// which `simgit repair` remounts from the state it keeps elsewhere.
fn registration_gone(repo: &RepoContext, path: &Path) -> bool {
    matches!(path.try_exists(), Ok(false)) && overlay::state(repo, path).is_none()
}

/// Find the worktree checked out on `branch`, if any, via Git's registry.
fn worktree_path_for_branch(repo: &RepoContext, branch: &str) -> Result<Option<PathBuf>> {
    let wanted = format!("refs/heads/{branch}");
    Ok(list_worktrees(repo)?
        .into_iter()
        .find(|entry| entry.branch.as_deref() == Some(wanted.as_str()))
        .map(|entry| entry.path))
}

fn worktree_description(
    repo: &RepoContext,
    admins: &HashMap<PathBuf, PathBuf>,
    entry: &WorktreeEntry,
) -> String {
    let branch = entry.branch.as_deref().unwrap_or("(detached)");
    let listed = listed(repo, admins, &entry.path);
    let persistence = if listed.ephemeral {
        "ephemeral"
    } else {
        "persistent"
    };
    let locked = if listed.locked { " locked" } else { "" };
    // Mode is appended, not inserted: existing scripts read fields 1-3.
    let mode = listed.mode.unwrap_or_else(|| "-".to_owned());
    format!(
        "{}\t{}\t{}{}\t{}",
        branch.strip_prefix("refs/heads/").unwrap_or(branch),
        entry.path.to_string_lossy().escape_debug(),
        persistence,
        locked,
        mode
    )
}

pub fn list(json_output: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    if !json_output {
        let entries = list_worktrees(&repo)?;
        let admins = admin_dirs(&repo);
        for entry in entries {
            println!("{}", worktree_description(&repo, &admins, &entry));
        }
        return Ok(());
    }

    let output = git_worktree_list(&repo)?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut current = serde_json::Map::new();
    for line in String::from_utf8_lossy(&output.stdout).split('\0') {
        if line.is_empty() {
            if !current.is_empty() {
                entries.push(serde_json::Value::Object(std::mem::take(&mut current)));
            }
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, "true"));
        let value = if value == "true" {
            json!(true)
        } else {
            json!(value)
        };
        current.insert(key.to_owned(), value);
    }
    if !current.is_empty() {
        entries.push(serde_json::Value::Object(current));
    }
    if let Some(main) = entries.first_mut() {
        main["worktree"] = json!(repo.main_worktree.display().to_string());
    }
    let admins = admin_dirs(&repo);
    for entry in &mut entries {
        if let Some(path) = entry
            .get("worktree")
            .and_then(|value| value.as_str())
            .map(PathBuf::from)
        {
            let listed = listed(&repo, &admins, &path);
            entry["ephemeral"] = json!(listed.ephemeral);
            entry["mode"] = json!(listed.mode);
            if entry.get("locked").is_none() && listed.locked {
                entry["locked"] = json!("simgit: workspace in use");
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}

/// A linked worktree as reported by `git worktree list --porcelain`.
struct WorktreeEntry {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    is_main: bool,
}

/// `git worktree list --porcelain -z`, which reads every registration.
fn git_worktree_list(repo: &RepoContext) -> Result<Output> {
    let _registry = RegistryLock::shared(repo)?;
    git_output_common(repo, ["worktree", "list", "--porcelain", "-z"])
}

fn list_worktrees(repo: &RepoContext) -> Result<Vec<WorktreeEntry>> {
    let output = git_worktree_list(repo)?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let mut entries = parse_worktree_list(&output.stdout);
    // Git names a submodule's git dir here; the main-checkout guards need its checkout.
    if let Some(main) = entries.first_mut() {
        main.path = repo.main_worktree.clone();
    }
    Ok(entries)
}

/// Parse `git worktree list --porcelain -z`; the first entry is the main one.
fn parse_worktree_list(stdout: &[u8]) -> Vec<WorktreeEntry> {
    let text = String::from_utf8_lossy(stdout);
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut head: Option<String> = None;
    let mut branch: Option<String> = None;
    for line in text.split('\0') {
        if line.is_empty() {
            if let Some(path) = path.take() {
                let is_main = entries.is_empty();
                entries.push(WorktreeEntry {
                    path,
                    head: head.take(),
                    branch: branch.take(),
                    is_main,
                });
            }
            head = None;
            branch = None;
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.to_owned());
        }
    }
    if let Some(path) = path.take() {
        let is_main = entries.is_empty();
        entries.push(WorktreeEntry {
            path,
            head,
            branch,
            is_main,
        });
    }
    entries
}

fn worktree_admin_dir(worktree: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git_path_output(
        worktree,
        ["rev-parse", "--absolute-git-dir"],
    )?))
}

fn mark_ephemeral(worktree: &Path) -> Result<()> {
    let admin = worktree_admin_dir(worktree)?;
    fs::write(admin.join("simgit-ephemeral"), b"").context("write ephemeral marker")?;
    Ok(())
}

/// Record how a worktree was populated, so `list` can answer the question the
/// whole tool exists for — is this checkout actually sharing disk? — long
/// after the creating command printed it.
fn mark_mode(worktree: &Path, mode: PopulateMode) -> Result<()> {
    let admin = worktree_admin_dir(worktree)?;
    fs::write(admin.join("simgit-mode"), mode.label()).context("write mode marker")?;
    Ok(())
}

/// Every registration's admin dir, keyed by the worktree its `gitdir` names.
/// Read once, so listing spawns no `git` per worktree; an entry that cannot be
/// read is left out, and `listed` finds that worktree's admin dir as before.
fn admin_dirs(repo: &RepoContext) -> HashMap<PathBuf, PathBuf> {
    let mut admins = HashMap::new();
    let Ok(entries) = fs::read_dir(repo.common_git_dir.join("worktrees")) else {
        return admins;
    };
    for admin in entries.flatten().map(|entry| entry.path()) {
        if let Ok(Some(worktree)) = registered_worktree(&admin) {
            admins.entry(worktree).or_insert(admin);
        }
    }
    admins
}

/// What `list` reports about a worktree beyond Git's own fields.
struct Listed {
    ephemeral: bool,
    /// The recorded populate mode, or `None` for the main worktree and any
    /// worktree simgit did not create.
    mode: Option<String>,
    locked: bool,
}

fn listed(repo: &RepoContext, admins: &HashMap<PathBuf, PathBuf>, worktree: &Path) -> Listed {
    let admin = admins
        .get(&canonical_path(worktree))
        .cloned()
        .or_else(|| overlay::admin_dir(repo, worktree));
    let Some(admin) = admin else {
        // The main worktree has no admin dir; only `run`'s lock can be there.
        return Listed {
            ephemeral: false,
            mode: None,
            locked: lock_state(repo, worktree) == Some(true),
        };
    };
    let mode = if overlay::read_state(&admin).is_some() {
        Some(PopulateMode::Overlay.label().to_owned())
    } else {
        fs::read_to_string(admin.join("simgit-mode"))
            .ok()
            .map(|recorded| recorded.trim().to_owned())
            .filter(|recorded| !recorded.is_empty())
    };
    Listed {
        ephemeral: admin.join("simgit-ephemeral").is_file(),
        mode,
        locked: lock_present(&admin.join("locked")),
    }
}

fn is_ephemeral(repo: &RepoContext, worktree: &Path) -> bool {
    overlay::admin_dir(repo, worktree)
        .map(|admin| admin.join("simgit-ephemeral").is_file())
        .unwrap_or(false)
}

/// Time since the worktree was last touched, approximated by the mtime of its
/// index (updated on add/commit/checkout), falling back to the directory mtime.
/// The index is found through the registry, so a deleted worktree ages too.
/// With neither to read nothing records activity, so it counts as idle; gc's
/// lock and uncommitted-work checks still decide whether it can go.
fn worktree_idle(repo: &RepoContext, worktree: &Path) -> Duration {
    let index = overlay::admin_dir(repo, worktree).map(|a| a.join("index"));
    let probe = match index {
        Some(index) if index.is_file() => index,
        _ => worktree.to_path_buf(),
    };

    match fs::metadata(&probe).and_then(|meta| meta.modified()) {
        // A modification time in the future is a write just now on a skewed clock.
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO),
        Err(_) => Duration::MAX,
    }
}
fn nearest_existing_directory(path: &Path) -> Result<PathBuf> {
    let mut candidate = path;
    loop {
        if candidate.is_dir() {
            return Ok(candidate.to_path_buf());
        }
        candidate = candidate
            .parent()
            .with_context(|| format!("no existing parent for {}", path.display()))?;
    }
}

fn resolved_path(path: &Path) -> Result<PathBuf> {
    let mut candidate = path;
    let mut missing = Vec::new();
    while !candidate.exists() {
        let name = candidate
            .file_name()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
        missing.push(name.to_os_string());
        candidate = candidate
            .parent()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
    }
    let mut resolved = candidate
        .canonicalize()
        .with_context(|| format!("resolve {}", candidate.display()))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// One stable form for a path simgit prints, returns or compares: symlinks
/// resolved as far as the path exists, then `.` and `..` removed.
///
/// On macOS `/tmp/x` and `/private/tmp/x` are one directory, and `--path
/// ../wt` from inside the repository yields `/repo/../wt`. An orchestrator
/// comparing the path it passed in against the path simgit hands back would
/// otherwise see a mismatch, and a lexical inside-the-repository test would
/// reject a destination that is nowhere near it.
fn canonical_path(path: &Path) -> PathBuf {
    // A path whose ancestors cannot be resolved is still usable as given.
    let resolved = resolved_path(path).unwrap_or_else(|_| path.to_path_buf());
    lexical_normalize(&resolved)
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

/// The outermost working tree holding the main one: a submodule's superproject.
fn enclosing_working_tree(repo: &RepoContext) -> &Path {
    match &repo.superproject {
        Some(superproject) => &superproject.working_tree,
        None => &repo.main_worktree,
    }
}

/// The filesystem type backing `path`, or `"unknown"`.
///
/// macOS `stat -f %T` prints a file-*type* suffix (`/` for a directory), not
/// the filesystem, so the mount table is consulted instead: the longest mount
/// point that prefixes the path owns it, and its type is the first attribute
/// in parentheses (`/dev/disk3s5 on / (apfs, local, journaled)`).
#[cfg(target_os = "macos")]
fn filesystem_name(path: &Path) -> String {
    const UNKNOWN: &str = "unknown";
    let Ok(target) = path.canonicalize() else {
        return UNKNOWN.to_owned();
    };
    let Ok(output) = Command::new("/sbin/mount").output() else {
        return UNKNOWN.to_owned();
    };
    if !output.status.success() {
        return UNKNOWN.to_owned();
    }
    let table = String::from_utf8_lossy(&output.stdout);
    let mut best: Option<(usize, &str)> = None;
    for line in table.lines() {
        let Some((_, rest)) = line.split_once(" on ") else {
            continue;
        };
        let Some((mount_point, attributes)) = rest.rsplit_once(" (") else {
            continue;
        };
        if !target.starts_with(mount_point) {
            continue;
        }
        let kind = attributes
            .trim_end_matches(')')
            .split(',')
            .next()
            .unwrap_or("")
            .trim();
        if kind.is_empty() {
            continue;
        }
        let depth = mount_point.len();
        match best {
            Some((deepest, _)) if deepest >= depth => {}
            _ => best = Some((depth, kind)),
        }
    }
    best.map_or_else(|| UNKNOWN.to_owned(), |(_, kind)| kind.to_owned())
}

/// The filesystem type backing `path`, or `"unknown"`.
#[cfg(target_os = "linux")]
fn filesystem_name(path: &Path) -> String {
    Command::new("stat")
        .args(["-f", "-c", "%T", "--"])
        .arg(path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The filesystem type backing `path`, or `"unknown"`.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn filesystem_name(_path: &Path) -> String {
    "unknown".to_owned()
}

fn stale_worktree_registrations(output: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8_lossy(output);
    let mut stale = Vec::new();
    let mut path: Option<&str> = None;
    let mut reason: Option<&str> = None;
    for field in text.split('\0') {
        if let Some(next) = field.strip_prefix("worktree ") {
            if let (Some(path), Some(reason)) = (path.take(), reason.take()) {
                stale.push(json!({ "worktree": path, "reason": reason }));
            }
            path = Some(next);
        } else if let Some(next) = field.strip_prefix("prunable ") {
            reason = Some(next);
        }
    }
    if let (Some(path), Some(reason)) = (path, reason) {
        stale.push(json!({ "worktree": path, "reason": reason }));
    }
    stale
}

fn worktree_dirty(worktree: &Path) -> Result<bool> {
    let output = git_output_at(worktree, ["status", "--porcelain"])?;
    if !output.status.success() {
        return Err(git_failure("git status --porcelain", &output));
    }
    Ok(!output.stdout.is_empty())
}

/// Refuse to tear down a detached worktree whose HEAD commit no ref reaches.
/// The HEAD comes from the registry, never from the worktree's own gitfile: a
/// missing gitfile makes git walk up and measure the main checkout instead.
fn refuse_unreached_head(repo: &RepoContext, target: &Path, caller: Caller) -> Result<()> {
    let entry = list_worktrees(repo)?
        .into_iter()
        .find(|entry| entry.path == target)
        .with_context(|| format!("{} is not a registered worktree", target.display()))?;
    if entry.branch.is_some() || overlay::branch(repo, target).is_some() {
        return Ok(());
    }
    refuse_unreached_entry(repo, &entry, &caller.then_remove(repo, target))
}

/// `refuse_unreached_head` for a detached `entry` already read from the
/// registry; `then` is the step that follows keeping the commit.
fn refuse_unreached_entry(repo: &RepoContext, entry: &WorktreeEntry, then: &str) -> Result<()> {
    let target = &entry.path;
    let head = entry.head.as_deref();
    let unreached = head_unreached(repo, head).with_context(|| {
        format!(
            "cannot tell whether {} holds unreached commits",
            target.display()
        )
    })?;
    if unreached {
        // Without its gitfile, git run there walks up to another repository,
        // so name the commit from the main checkout instead.
        let keep = if matches!(target.join(".git").try_exists(), Ok(true)) {
            format!("git -C {} branch <name>", shell_quote(target))
        } else {
            format!(
                "git -C {} branch <name> {}",
                shell_quote(&repo.main_worktree),
                head.unwrap_or_default()
            )
        };
        bail!(
            "{} is detached at a commit no branch or tag reaches; removing it would \
             lose that work. Keep it with `{keep}`, {then}",
            target.display()
        );
    }
    Ok(())
}

/// True when the registered HEAD commit is reached by no ref and by no other
/// worktree's HEAD: removing the worktree would leave its commits dangling.
/// A missing HEAD or a git failure is an error, never an answer, so it cannot
/// lead to deletion.
fn head_unreached(repo: &RepoContext, head: Option<&str>) -> Result<bool> {
    let head = head.context("the registry records no HEAD for the worktree")?;
    // --single-worktree limits --all to the main checkout's HEAD, so this
    // worktree's own detached HEAD does not count as a ref reaching itself.
    let output = git_output_common(
        repo,
        [
            "rev-list",
            "-n1",
            "--single-worktree",
            head,
            "--not",
            "--all",
        ],
    )?;
    if !output.status.success() {
        return Err(git_failure("git rev-list --not --all", &output));
    }
    Ok(!output.stdout.is_empty())
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

fn emit(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

fn discover_repo(path: &Path) -> Result<RepoContext> {
    let top_level = git_path_output(path, ["rev-parse", "--show-toplevel"])
        .context("not in a Git working tree")?;
    let common = git_path_output(
        path,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .context("resolve Git common directory")?;
    let top_level = PathBuf::from(top_level);
    let common_git_dir = PathBuf::from(common);
    let main_worktree = find_main_worktree(path, &top_level, &common_git_dir);
    let superproject = find_superproject(&main_worktree, &common_git_dir);
    Ok(RepoContext {
        top_level,
        common_git_dir,
        main_worktree,
        superproject,
    })
}

/// The outermost superproject of the submodule checked out at `main_worktree`,
/// asked from that checkout: a linked worktree of the submodule lies in no
/// superproject. `None` when there is none, or it cannot be reached; default
/// worktrees then go beside `main_worktree`, as for any repository.
///
/// The submodule's directory is its path in that superproject, flattened, plus
/// a digest of its common git dir: same-named submodules, and the module git
/// dirs each superproject worktree keeps, each get their own.
fn find_superproject(main_worktree: &Path, common_git_dir: &Path) -> Option<Superproject> {
    // Git finds a superproject from the directory above, so it needs a `.git`
    // somewhere up there; most repositories have none and run no `git` here.
    if !main_worktree
        .ancestors()
        .skip(1)
        .any(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())
    {
        return None;
    }
    // One `git` per level of nesting. Each answer must be a strict ancestor of
    // the last, so the walk ends.
    let mut working_tree = None;
    let mut current = main_worktree.to_path_buf();
    while let Some(parent) =
        git_path_output(&current, ["rev-parse", "--show-superproject-working-tree"])
            .ok()
            .filter(|parent| !parent.is_empty())
            .map(PathBuf::from)
            .filter(|parent| *parent != current && current.starts_with(parent))
    {
        current = parent.clone();
        working_tree = Some(parent);
    }
    let working_tree = working_tree?;
    let common = git_path_output(
        &working_tree,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let super_main = find_main_worktree(&working_tree, &working_tree, Path::new(&common));
    let relative = main_worktree.strip_prefix(&working_tree).ok()?;
    let module_dir = format!(
        "{}-{}",
        flatten_path_component(&relative.to_string_lossy()),
        short_digest(canonical_path(common_git_dir).to_string_lossy().as_bytes())
    );
    Some(Superproject {
        working_tree,
        main_worktree: super_main,
        module_dir,
    })
}

/// The directory holding the main working tree. It costs a `git`, so it is
/// found once, with the `RepoContext`.
///
/// `git worktree list` cannot say: it names the common git dir whenever that is
/// not called `.git` (a submodule, `--separate-git-dir`), ignoring the
/// `core.worktree` that a submodule's git dir sets. Asked from the common dir,
/// `rev-parse --show-toplevel` honours it, answers with the git dir itself when
/// it is unset, and fails in a bare repository. With no main working tree (a
/// bare repository not called `.git`), or none recorded (a linked worktree of a
/// `--separate-git-dir` repository), this is the common git dir, as before.
fn find_main_worktree(path: &Path, top_level: &Path, common_git_dir: &Path) -> PathBuf {
    let common = canonical_path(common_git_dir);
    let configured = Command::new("git")
        .current_dir(common_git_dir)
        .arg(format!("--git-dir={}", common_git_dir.display()))
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|top| canonical_path(Path::new(top.trim())))
        .filter(|top| *top != common);
    let main = if let Some(configured) = configured {
        configured
    } else if common_git_dir.file_name() == Some(OsStr::new(".git")) {
        common_git_dir
            .parent()
            .unwrap_or(common_git_dir)
            .to_path_buf()
    } else if worktree_admin_dir(path).is_ok_and(|admin| canonical_path(&admin) == common) {
        // Standing in the main working tree of a `--separate-git-dir` repository.
        top_level.to_path_buf()
    } else {
        common_git_dir.to_path_buf()
    };
    // Every `git_common_command` runs there, so it must exist.
    if main.is_dir() {
        main
    } else {
        common_git_dir.to_path_buf()
    }
}

fn git_path_output<const N: usize>(path: &Path, args: [&str; N]) -> Result<String> {
    let output = git_command_at(path)?
        .args(args)
        .output()
        .context("run git")?;
    if !output.status.success() {
        return Err(git_failure("git rev-parse", &output));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Resolves `base` as the caller sees it, so `HEAD` and `HEAD~1` mean the
/// worktree simgit was run from; the main checkout's `HEAD` is not the caller's
/// inside a linked worktree.
fn resolve_commit(repo: &RepoContext, base: &str) -> Result<String> {
    let spec = format!("{base}^{{commit}}");
    let output = git_output_at(&repo.top_level, ["rev-parse", "--verify", &spec])?;
    if !output.status.success() {
        // A fresh `git init` has an unborn HEAD, which is the first thing a
        // new user hits; say so instead of reporting an unresolvable rev.
        if base == "HEAD"
            && !git_output_at(
                &repo.top_level,
                ["rev-parse", "--verify", "--quiet", "HEAD"],
            )?
            .status
            .success()
        {
            bail!("repository has no commits yet; make one before creating a worktree");
        }
        bail!("cannot resolve base commit '{base}'");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Default location for a new worktree: `<repo-parent>/.simgit/<repo>/<branch>`.
/// A submodule's go beside its outermost superproject's main working tree, at
/// `.simgit/<super>/.modules/<submodule>/<branch>`, outside every enclosing
/// working tree.
///
/// This must stay outside the common git dir. Agent harnesses and editors
/// treat everything under `.git/` as off-limits — Claude Code refuses to edit
/// files there — so a worktree nested in the git dir is unusable by exactly
/// the tools simgit exists to serve. It must also stay on the repository's
/// filesystem, since reflink/clonefile cannot cross volumes; a sibling of the
/// main working tree satisfies both. `SIMGIT_WORKTREE_ROOT` overrides it.
fn default_worktree_path(repo: &RepoContext, branch: &str) -> Result<PathBuf> {
    let root = match std::env::var_os("SIMGIT_WORKTREE_ROOT") {
        Some(root) if !root.is_empty() => PathBuf::from(root),
        _ => match &repo.superproject {
            // `.modules` is never a branch's directory: Git refuses ref
            // components that start with `.`.
            Some(superproject) => worktree_root_beside(&superproject.main_worktree)?
                .join(".modules")
                .join(&superproject.module_dir),
            None => worktree_root_beside(&repo.main_worktree)?,
        },
    };
    Ok(root.join(safe_path_component(branch)))
}

/// `<home-parent>/.simgit/<home>`.
fn worktree_root_beside(home: &Path) -> Result<PathBuf> {
    let name = home
        .file_name()
        .context("repository has no directory name")?;
    let parent = home.parent().with_context(|| {
        format!(
            "{} has no parent directory for worktrees; pass --path or set SIMGIT_WORKTREE_ROOT",
            home.display()
        )
    })?;
    Ok(parent.join(".simgit").join(name))
}

fn safe_path_component(branch: &str) -> String {
    let mut name = flatten_path_component(branch);
    if name.is_empty() || name == "." || name == ".." {
        name.insert_str(0, "branch-");
    }
    if name != branch {
        name.push('-');
        name.push_str(&short_digest(branch.as_bytes()));
    }
    name
}

/// `text` with every character a single path component should not hold
/// replaced by `-`.
fn flatten_path_component(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Eight hex digits of FNV-1a over `bytes`.
fn short_digest(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{:08x}", hash as u32)
}

fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn state_dir(common_git_dir: &Path) -> PathBuf {
    common_git_dir.join("simgit")
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

fn remove_worktree_force(repo: &RepoContext, target: &Path) -> Result<()> {
    let mut command = git_common_command(repo);
    command.args(["worktree", "remove", "--force"]).arg(target);
    run_retrying_half_written_registration(&mut command, "git worktree remove --force")
}

/// Run a Git command that reads every worktree registration without the
/// registry lock, retrying when it died on one another allocation was half-way
/// through writing. Used where holding the lock would stall every allocation
/// (a network fetch, a large removal); a Git that skips half-written
/// registrations would make it unnecessary.
fn output_retrying_half_written_registration(command: &mut Command) -> Result<Output> {
    // Untranslated messages: the match below, and callers, read them.
    command.env("LC_ALL", "C");
    let mut backoff = [50, 100].into_iter();
    loop {
        let output = command.output().context("run git")?;
        let complaint = String::from_utf8_lossy(&output.stderr);
        let raced = !output.status.success()
            && complaint.contains("failed to read")
            && complaint.contains("commondir");
        match backoff.next() {
            Some(millis) if raced => std::thread::sleep(Duration::from_millis(millis)),
            _ => return Ok(output),
        }
    }
}

fn run_retrying_half_written_registration(command: &mut Command, description: &str) -> Result<()> {
    let output = output_retrying_half_written_registration(command)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(description, &output))
    }
}

/// A `git` invocation against the repository's common git dir, anchored in a
/// working directory that outlives the operation.
///
/// simgit routinely deletes the directory it was launched from: `remove` and
/// `gc` both reap the very worktree the caller is standing in. Every later
/// `git` that inherited that directory then dies with `fatal: Unable to read
/// current working directory`, which is how `remove --delete-branch` came to
/// delete a worktree, keep its branch and report failure. The main working
/// tree is the one directory a worktree operation cannot remove;
/// `repo.top_level` is not a substitute, because inside a linked worktree it
/// *is* the directory being removed.
fn git_common_command(repo: &RepoContext) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(&repo.main_worktree)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()));
    command
}

/// `git -C <path>`, started in `path` or its nearest existing ancestor rather
/// than in the inherited working directory, which simgit may have deleted.
fn git_command_at(path: &Path) -> Result<Command> {
    let path = absolute_path(path.to_path_buf())?;
    let mut command = Command::new("git");
    command
        .current_dir(nearest_existing_directory(&path)?)
        .arg("-C")
        .arg(&path);
    Ok(command)
}

fn run_git_at<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
    let mut command = git_command_at(path)?;
    command.args(args);
    run_command(&mut command, "git")
}

fn git_output_common<const N: usize>(repo: &RepoContext, args: [&str; N]) -> Result<Output> {
    git_common_command(repo)
        .args(args)
        .output()
        .context("run git")
}

fn git_output_at<const N: usize>(path: &Path, args: [&str; N]) -> Result<Output> {
    git_command_at(path)?.args(args).output().context("run git")
}

pub(super) fn run_command(command: &mut Command, description: &str) -> Result<()> {
    let output = command.output().with_context(|| description.to_owned())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(description, &output))
    }
}

fn git_failure(description: &str, output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    anyhow::anyhow!("{description} failed ({}): {detail}", output.status)
}

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod tests;
