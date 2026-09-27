//! Native Git linked worktrees populated with filesystem copy-on-write clones.
//!
//! `simgit` deliberately has no daemon dependency. Git owns the refs,
//! index, commits, and lifecycle; simgit only avoids repeatedly inflating the
//! same checkout by cloning an immutable cached baseline when the filesystem
//! supports it.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::json;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

mod cow;
mod doctor;
mod index;
mod launch;
mod locks;
mod maintenance;
mod overlay;

pub use doctor::doctor;
pub use launch::{run_in_worktree, WorktreeRun};
pub use locks::{unlock, WorktreeUnlock};
pub use maintenance::{gc, prune, repair, WorktreeGc, WorktreePrune};

use locks::{ensure_unlocked, PathClaim, WorktreeLock};
use maintenance::{delete_local_branch, prune_git_worktrees};

#[cfg(test)]
use locks::{lock_owner_of, lock_owner_pid, process_alive, worktree_lock_path};
#[cfg(test)]
use maintenance::run_gc;

#[derive(Args)]
pub struct WorktreeAdd {
    /// Branch name to create (for example, feat/my-feature). When omitted,
    /// creates a unique `agent/<uuid>` branch unless --detach is used.
    pub branch: Option<String>,

    /// Create a detached worktree without creating or deleting a branch.
    #[arg(long, conflicts_with = "branch")]
    pub detach: bool,

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
    let created = create_worktree(&args, false)?;

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

fn create_worktree(args: &WorktreeAdd, attach: bool) -> Result<CreatedWorktree> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    if attach && args.detach {
        bail!("cannot attach an existing branch with --detach");
    }
    let kind = if args.detach {
        WorktreeKind::Detached
    } else if attach {
        WorktreeKind::ExistingBranch
    } else {
        WorktreeKind::NewBranch
    };
    let branch = args.branch.as_deref();
    if kind == WorktreeKind::NewBranch {
        validate_new_branch(
            &repo,
            branch.context("branch is required unless --detach is used")?,
        )?;
    } else if kind == WorktreeKind::ExistingBranch && args.base.is_some() {
        bail!("--base cannot be used with an existing branch");
    }
    if kind == WorktreeKind::Detached && branch.is_some() {
        bail!("a branch cannot be combined with --detach");
    }
    let reference = branch.map(|name| format!("refs/heads/{name}"));
    let base = resolve_commit(
        &repo,
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
    let target = canonical_path(&absolute_path(match requested {
        Some(path) => path,
        None => default_worktree_path(&repo.common_git_dir, &path_label)?,
    })?);

    // One allocation at a time per destination. Two `add --path <same>` runs
    // interleaving through `git worktree add` both used to succeed, leaving
    // two registrations for one directory and a reported branch that was not
    // the one checked out there.
    let _claim = PathClaim::acquire(&repo, &target)?;

    // An empty directory holds nothing to lose, and harnesses routinely
    // pre-create one per job; anything else is someone's data.
    if target.exists() && !directory_is_empty(&target) {
        bail!("worktree path already exists: {}", target.display());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
    }

    let target_parent = target.parent().context("worktree path has no parent")?;
    let mode = select_populate_mode(&repo, target_parent, args.require_cow)?;

    let sparse = validate_sparse(&args.sparse)?;
    if !sparse.is_empty() && mode == PopulateMode::Overlay {
        bail!(
            "--sparse is not supported by the fuse-overlayfs backend: the lower \
             layer is the whole baseline, so nothing is saved by narrowing the \
             view. Use SIMGIT_POPULATE=reflink or =checkout."
        );
    }

    let branch_arg = branch.unwrap_or("");
    let populated = match mode {
        PopulateMode::CowClone => {
            add_cow_worktree(&repo, branch_arg, &target, &base, kind, &sparse)
        }
        PopulateMode::Overlay => add_overlay_worktree(&repo, branch_arg, &target, &base, kind),
        PopulateMode::GitCheckout => {
            add_git_worktree(&repo, branch_arg, &target, &base, kind, &sparse)
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
            teardown_worktree(&repo, &target, true)
                .context("cleanup after ephemeral marker failure")?;
            if kind == WorktreeKind::NewBranch {
                delete_local_branch(
                    &repo,
                    reference
                        .as_deref()
                        .context("new branch has no reference")?,
                    true,
                )?;
            }
            return Err(error).context("mark worktree ephemeral");
        }
    }
    warn_if_inside_repository(&repo, &target);

    Ok(CreatedWorktree {
        target,
        branch: branch.map(str::to_owned),
        base,
        mode,
    })
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

    if cow::clone_supported(&repo.common_git_dir, target_parent)? {
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
             get a worktree for it with: simgit run {branch} -- <command>"
        ),
        _ => Err(git_failure("git show-ref --verify", &exists)),
    }
}

fn register_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    checkout: bool,
    kind: WorktreeKind,
) -> Result<()> {
    let mut command = git_common_command(repo);
    command.args(["worktree", "add"]);
    if !checkout {
        command.arg("--no-checkout");
    }
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
    run_command(&mut command, "create linked worktree")
}

fn add_cow_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
    sparse: &[String],
) -> Result<()> {
    let baseline = cow::ensure_baseline(repo, base)?;
    register_worktree(repo, branch, target, base, false, kind)?;

    let populate_result = if sparse.is_empty() {
        populate_cow_worktree(target, &baseline)
    } else {
        populate_sparse_cow_worktree(target, &baseline, sparse)
    };

    if let Err(error) = populate_result {
        rollback_created_worktree(repo, target, branch, kind == WorktreeKind::NewBranch)?;
        return Err(error);
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
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(target)
        .args(["sparse-checkout", "set"])
        .args(sparse);
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

fn add_git_worktree(
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
    register_worktree(repo, branch, target, base, sparse.is_empty(), kind)?;
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
        rollback_created_worktree(repo, target, branch, kind == WorktreeKind::NewBranch)?;
        return Err(error);
    }
    Ok(())
}

/// Create a linked worktree whose files are served by a fuse-overlayfs mount:
/// `lowerdir` is the shared read-only baseline, and a per-worktree `upperdir`
/// captures writes. Unchanged files cost no disk, on any Linux filesystem.
fn add_overlay_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
) -> Result<()> {
    let baseline = cow::ensure_baseline(repo, base)?;

    let overlay_dir = overlay::root(&repo.common_git_dir).join(Uuid::new_v4().to_string());
    let upper = overlay_dir.join("upper");
    let work = overlay_dir.join("work");
    fs::create_dir_all(&upper).context("create overlay upperdir")?;
    fs::create_dir_all(&work).context("create overlay workdir")?;

    if let Err(error) = register_worktree(repo, branch, target, base, false, kind) {
        fs::remove_dir_all(&overlay_dir).context("cleanup unused overlay state")?;
        return Err(error);
    }

    let populate = (|| -> Result<()> {
        // The `.git` gitlink file must survive the overlay mount (which replaces
        // the directory view), so stage it into the upperdir before mounting.
        fs::rename(target.join(".git"), upper.join(".git"))
            .context("stage worktree gitlink into overlay upperdir")?;
        overlay::mount(&baseline, &upper, &work, target)?;
        run_git_at(target, ["read-tree", "HEAD"]).context("initialize overlay worktree index")?;
        ensure_clean(target).context("verify overlay worktree")?;
        let admin = worktree_admin_dir(target)?;
        overlay::write_marker(
            &admin,
            &overlay::State {
                overlay_dir: overlay_dir.clone(),
                lower: Some(baseline.clone()),
            },
        )?;
        Ok(())
    })();

    if let Err(error) = populate {
        overlay::unmount(target)
            .context("detach failed overlay; workspace retained for recovery")?;
        // Restore Git's link before asking Git to remove its registration.
        if !target.join(".git").exists() && upper.join(".git").exists() {
            fs::rename(upper.join(".git"), target.join(".git"))?;
        }
        rollback_created_worktree(repo, target, branch, kind == WorktreeKind::NewBranch)?;
        fs::remove_dir_all(&overlay_dir).context("cleanup failed overlay state")?;
        return Err(error);
    }
    Ok(())
}

/// Recheck dirty state at teardown so a command finishing during GC selection
/// cannot turn a previously clean workspace into silently discarded work.
fn teardown_worktree(repo: &RepoContext, target: &Path, discard_dirty: bool) -> Result<()> {
    ensure_unlocked(repo, target)?;
    // Report uncommitted work in simgit's own terms on every path: Git's
    // `worktree remove` only knows about --force, not about --commit.
    if !discard_dirty && worktree_dirty(target)? {
        bail!(
            "worktree has uncommitted changes: {}\n\
             keep the work with --commit, or discard it with --discard-dirty",
            target.display()
        );
    }
    let result = if let Some(state) = overlay::state(repo, target) {
        let lock = WorktreeLock::acquire(repo, target)?;
        if !discard_dirty && worktree_dirty(target)? {
            bail!("worktree has uncommitted changes; pass --commit or --discard-dirty");
        }
        overlay::unmount(target)?;
        remove_dir_if_present(target)?;
        remove_dir_if_present(&state.overlay_dir)?;
        lock.release()?;
        prune_git_worktrees(repo)
    } else if discard_dirty {
        remove_worktree_force(repo, target)
    } else {
        run_git_common(
            repo,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                target.as_os_str(),
            ],
        )
    };
    if result.is_ok() {
        prune_empty_worktree_dirs(repo, target);
    }
    result
}

/// Drop the `.simgit/<repo>` scaffolding once its last worktree is gone, so
/// removing every worktree leaves no empty directories beside the repository.
/// Only empty directories are removed, and only up to the `.simgit` root.
fn prune_empty_worktree_dirs(repo: &RepoContext, target: &Path) {
    if std::env::var_os("SIMGIT_WORKTREE_ROOT").is_some() {
        // A directory the user chose is theirs to keep, empty or not.
        return;
    }
    let Some(root) = default_worktree_path(&repo.common_git_dir, "unused")
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return;
    };
    if target.parent() != Some(root.as_path()) {
        return;
    }
    if fs::remove_dir(&root).is_ok() {
        if let Some(parent) = root.parent() {
            if parent.file_name() == Some(OsStr::new(".simgit")) {
                let _ = fs::remove_dir(parent);
            }
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
    let target = match lookup_worktree_target(&repo, args.target.as_deref())? {
        TargetLookup::Found(path) => path,
        // Removing what is already gone is the state the caller asked for. An
        // agent that crashed mid-cleanup, or handed the same cleanup token
        // back twice, must not have to tell the two cases apart.
        TargetLookup::Absent(spec) => return remove_absent(&repo, &spec, &args, json),
        // A directory that is no longer registered is not a worktree, however
        // much it looks like one: finishing that teardown is its own path.
        TargetLookup::Stray(path) => return remove_stray(&path, &args, json),
    };
    ensure_unlocked(&repo, &target)?;
    let branch = list_worktrees(&repo)?
        .into_iter()
        .find(|entry| entry.path == target)
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
    // A commit that succeeded is never rolled back when a later step fails: it
    // is the work the caller asked to keep, and an "atomic" remove that
    // un-commits it would trade a visible seam for destroyed work. The failure
    // says so instead.
    let mut commit = None;
    if args.commit {
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

    teardown_worktree(&repo, &target, args.discard_dirty)
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
            "already_absent": false,
            "committed": commit.is_some(),
            "commit": commit,
            "branch_deleted": branch_deleted,
        }));
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
    let stray = if as_path.exists() {
        let canonical = canonical_path(&as_path);
        if is_registered_worktree(repo, &canonical)? {
            return Ok(TargetLookup::Found(canonical));
        }
        // An unregistered directory must not shadow a branch of the same name,
        // so it is remembered rather than returned here.
        Some(canonical)
    } else {
        None
    };
    if let Some(path) = worktree_path_for_branch(repo, spec)? {
        return Ok(TargetLookup::Found(path));
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

/// Find the worktree checked out on `branch`, if any, via Git's registry.
fn worktree_path_for_branch(repo: &RepoContext, branch: &str) -> Result<Option<PathBuf>> {
    let wanted = format!("refs/heads/{branch}");
    Ok(list_worktrees(repo)?
        .into_iter()
        .find(|entry| entry.branch.as_deref() == Some(wanted.as_str()))
        .map(|entry| entry.path))
}

fn worktree_description(repo: &RepoContext, entry: &WorktreeEntry) -> String {
    let branch = entry.branch.as_deref().unwrap_or("(detached)");
    let persistence = if is_ephemeral(repo, &entry.path) {
        "ephemeral"
    } else {
        "persistent"
    };
    let locked = if ensure_unlocked(repo, &entry.path).is_err() {
        " locked"
    } else {
        ""
    };
    // Mode is appended, not inserted: existing scripts read fields 1-3.
    let mode = worktree_mode(repo, &entry.path).unwrap_or_else(|| "-".to_owned());
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
        for entry in list_worktrees(&repo)? {
            println!("{}", worktree_description(&repo, &entry));
        }
        return Ok(());
    }

    let output = git_output_common(&repo, ["worktree", "list", "--porcelain", "-z"])?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let mut entries = Vec::new();
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
    for entry in &mut entries {
        if let Some(path) = entry
            .get("worktree")
            .and_then(|value| value.as_str())
            .map(PathBuf::from)
        {
            entry["ephemeral"] = json!(is_ephemeral(&repo, &path));
            entry["mode"] = json!(worktree_mode(&repo, &path));
            if entry.get("locked").is_none() && ensure_unlocked(&repo, &path).is_err() {
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
    branch: Option<String>,
    is_main: bool,
}

fn list_worktrees(repo: &RepoContext) -> Result<Vec<WorktreeEntry>> {
    let output = git_output_common(repo, ["worktree", "list", "--porcelain", "-z"])?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in text.split('\0') {
        if line.is_empty() {
            if let Some(path) = path.take() {
                let is_main = entries.is_empty();
                entries.push(WorktreeEntry {
                    path,
                    branch: branch.take(),
                    is_main,
                });
            }
            branch = None;
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.to_owned());
        }
    }
    if let Some(path) = path.take() {
        let is_main = entries.is_empty();
        entries.push(WorktreeEntry {
            path,
            branch,
            is_main,
        });
    }
    Ok(entries)
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

/// The recorded populate mode, or `None` for the main worktree and any
/// worktree simgit did not create.
fn worktree_mode(repo: &RepoContext, worktree: &Path) -> Option<String> {
    if overlay::state(repo, worktree).is_some() {
        return Some(PopulateMode::Overlay.label().to_owned());
    }
    let admin = overlay::admin_dir(repo, worktree)?;
    let recorded = fs::read_to_string(admin.join("simgit-mode")).ok()?;
    let recorded = recorded.trim();
    (!recorded.is_empty()).then(|| recorded.to_owned())
}

fn is_ephemeral(repo: &RepoContext, worktree: &Path) -> bool {
    overlay::admin_dir(repo, worktree)
        .map(|admin| admin.join("simgit-ephemeral").is_file())
        .unwrap_or(false)
}

/// Time since the worktree was last touched, approximated by the mtime of its
/// index (updated on add/commit/checkout), falling back to the directory mtime.
fn worktree_idle(worktree: &Path) -> Duration {
    let index = worktree_admin_dir(worktree).ok().map(|a| a.join("index"));
    let probe = match index {
        Some(index) if index.is_file() => index,
        _ => worktree.to_path_buf(),
    };

    fs::metadata(&probe)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or(Duration::ZERO)
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
    let source = canonical_path(main_worktree(&repo.common_git_dir));
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
    Ok(RepoContext {
        top_level: PathBuf::from(top_level),
        common_git_dir: PathBuf::from(common),
    })
}

fn git_path_output<const N: usize>(path: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .context("run git")?;
    if !output.status.success() {
        return Err(git_failure("git rev-parse", &output));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn resolve_commit(repo: &RepoContext, base: &str) -> Result<String> {
    let spec = format!("{base}^{{commit}}");
    let output = git_output_common(repo, ["rev-parse", "--verify", &spec])?;
    if !output.status.success() {
        // A fresh `git init` has an unborn HEAD, which is the first thing a
        // new user hits; say so instead of reporting an unresolvable rev.
        if base == "HEAD"
            && !git_output_common(repo, ["rev-parse", "--verify", "--quiet", "HEAD"])?
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
///
/// This must stay outside the common git dir. Agent harnesses and editors
/// treat everything under `.git/` as off-limits — Claude Code refuses to edit
/// files there — so a worktree nested in the git dir is unusable by exactly
/// the tools simgit exists to serve. It must also stay on the repository's
/// filesystem, since reflink/clonefile cannot cross volumes; a sibling of the
/// main working tree satisfies both. `SIMGIT_WORKTREE_ROOT` overrides it.
fn default_worktree_path(common_git_dir: &Path, branch: &str) -> Result<PathBuf> {
    let root = match std::env::var_os("SIMGIT_WORKTREE_ROOT") {
        Some(root) if !root.is_empty() => PathBuf::from(root),
        _ => {
            let home = main_worktree(common_git_dir);
            let name = home
                .file_name()
                .context("repository has no directory name")?;
            let parent = home.parent().with_context(|| {
                format!(
                    "{} has no parent directory for worktrees; pass --path or set SIMGIT_WORKTREE_ROOT",
                    home.display()
                )
            })?;
            parent.join(".simgit").join(name)
        }
    };
    Ok(root.join(safe_path_component(branch)))
}

/// The directory holding the main working tree, or the git dir itself for a
/// bare repository.
fn main_worktree(common_git_dir: &Path) -> &Path {
    if common_git_dir.file_name() == Some(OsStr::new(".git")) {
        common_git_dir.parent().unwrap_or(common_git_dir)
    } else {
        common_git_dir
    }
}

fn safe_path_component(branch: &str) -> String {
    let mut name: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() || name == "." || name == ".." {
        name.insert_str(0, "branch-");
    }
    if name != branch {
        let hash = branch
            .as_bytes()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        name.push_str(&format!("-{:08x}", hash as u32));
    }
    name
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

fn rollback_created_worktree(
    repo: &RepoContext,
    target: &Path,
    branch: &str,
    new_branch: bool,
) -> Result<()> {
    remove_worktree_force(repo, target).context("rollback incomplete worktree")?;
    if new_branch {
        delete_local_branch(repo, branch, true)?;
    }
    Ok(())
}

fn remove_worktree_force(repo: &RepoContext, target: &Path) -> Result<()> {
    let mut command = git_common_command(repo);
    command.args(["worktree", "remove", "--force"]).arg(target);
    run_command(&mut command, "git worktree remove --force")
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
        .current_dir(main_worktree(&repo.common_git_dir))
        .arg(format!("--git-dir={}", repo.common_git_dir.display()));
    command
}

fn run_git_common<I, S>(repo: &RepoContext, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_common_command(repo);
    command.args(args);
    run_command(&mut command, "git")
}

fn run_git_at<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(args);
    run_command(&mut command, "git")
}

fn git_output_common<const N: usize>(repo: &RepoContext, args: [&str; N]) -> Result<Output> {
    git_common_command(repo)
        .args(args)
        .output()
        .context("run git")
}

fn git_output_at<const N: usize>(path: &Path, args: [&str; N]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .context("run git")
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
