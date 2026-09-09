//! Native Git linked worktrees populated with filesystem copy-on-write clones.
//!
//! `sg worktree` deliberately has no daemon dependency. Git owns the refs,
//! index, commits, and lifecycle; simgit only avoids repeatedly inflating the
//! same checkout by cloning an immutable cached baseline when the filesystem
//! supports it.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

mod cow;
mod index;
mod launch;
mod overlay;

pub use launch::WorktreeRun;

#[derive(Subcommand)]
pub enum Worktree {
    /// Create a real Git linked worktree, using CoW clones when supported.
    Add(WorktreeAdd),
    /// Remove a linked worktree, by path or by branch name.
    Remove(WorktreeRemove),
    /// List linked worktrees using Git's native registry.
    List(WorktreeList),
    /// Prune stale Git registrations and old cached baselines.
    Prune(WorktreePrune),
    /// Reap idle/ephemeral worktrees (e.g. abandoned agent sandboxes).
    Gc(WorktreeGc),
    /// Create or reuse a persistent worktree and run a command inside it.
    Run(WorktreeRun),
    /// Remount overlay-backed worktrees after a reboot or interrupted mount.
    Repair(WorktreeRepair),
}

#[derive(Args)]
pub struct WorktreeAdd {
    /// Branch name to create (for example, feat/my-feature).
    pub branch: String,

    /// Worktree path. Defaults to `../.simgit/<repo>/<branch>`.
    pub path: Option<PathBuf>,

    /// Same as the positional path argument; accepted because `sg run` spells
    /// it this way.
    #[arg(long = "path", value_name = "PATH", conflicts_with = "path")]
    pub path_flag: Option<PathBuf>,

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

    /// JSON output.
    #[arg(long)]
    pub json: bool,
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
    #[arg(short, long, default_value = "simgit worktree remove")]
    pub message: String,

    /// Discard uncommitted changes. Without this flag, Git refuses dirty removal.
    #[arg(long, conflicts_with = "commit")]
    pub force: bool,

    /// Delete the worktree branch too. Refuses unmerged branches unless --force.
    #[arg(long)]
    pub delete_branch: bool,

    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct WorktreeList {
    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Default)]
pub struct WorktreePrune {
    /// Also delete every cached baseline, including recently used entries.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Default)]
pub struct WorktreeGc {
    /// Only reap ephemeral worktrees (the default).
    #[arg(long)]
    pub ephemeral: bool,

    /// Also allow GC to remove persistent worktrees.
    #[arg(long, conflicts_with = "ephemeral")]
    pub include_persistent: bool,

    /// Only reap worktrees whose branch starts with this prefix.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Reap worktrees idle at least this long (e.g. 90s, 30m, 24h, 7d).
    #[arg(long, default_value = "24h")]
    pub older_than: String,

    /// Reap even if the worktree has uncommitted changes (discards them).
    #[arg(long)]
    pub force: bool,

    /// Delete each reaped worktree's branch. Refuses unmerged branches unless --force.
    #[arg(long)]
    pub delete_branches: bool,

    /// Report what would be reaped without removing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Default)]
pub struct WorktreeRepair {
    /// JSON output.
    #[arg(long)]
    pub json: bool,
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

pub fn run(cmd: Worktree, global_json: bool) -> Result<()> {
    match cmd {
        Worktree::Add(args) => {
            let json = args.json || global_json;
            add(args, json)
        }
        Worktree::Remove(args) => {
            let json = args.json || global_json;
            remove(args, json)
        }
        Worktree::List(args) => list(args.json || global_json),
        Worktree::Prune(args) => prune(args, global_json),
        Worktree::Gc(args) => {
            let json = args.json || global_json;
            gc(args, json)
        }
        Worktree::Run(args) => launch::run_in_worktree(args, global_json),
        Worktree::Repair(args) => repair(args.json || global_json),
    }
}

fn add(args: WorktreeAdd, json: bool) -> Result<()> {
    let created = create_worktree(&args, false)?;

    if json {
        emit(&json!({
            "worktree": created.target.display().to_string(),
            "branch": args.branch,
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
    base: String,
    mode: PopulateMode,
}

fn create_worktree(args: &WorktreeAdd, attach: bool) -> Result<CreatedWorktree> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    let new_branch = !attach;
    if new_branch {
        validate_new_branch(&repo, &args.branch)?;
    } else if args.base.is_some() {
        bail!("--base cannot be used with an existing branch");
    }
    let reference = format!("refs/heads/{}", args.branch);
    let base = resolve_commit(
        &repo,
        if attach {
            &reference
        } else {
            args.base.as_deref().unwrap_or("HEAD")
        },
    )?;
    let requested = args.path.clone().or_else(|| args.path_flag.clone());
    let target = absolute_path(match requested {
        Some(path) => path,
        None => default_worktree_path(&repo.common_git_dir, &args.branch)?,
    })?;

    if target.exists() {
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

    match mode {
        PopulateMode::CowClone => {
            add_cow_worktree(&repo, &args.branch, &target, &base, new_branch, &sparse)?
        }
        PopulateMode::Overlay => {
            add_overlay_worktree(&repo, &args.branch, &target, &base, new_branch)?
        }
        PopulateMode::GitCheckout => {
            add_git_worktree(&repo, &args.branch, &target, &base, new_branch, &sparse)?
        }
    }

    // Best effort: a worktree that works but cannot report its mode is far
    // better than tearing down a good checkout over a marker file.
    let _ = mark_mode(&target, mode);

    if args.ephemeral {
        if let Err(error) = mark_ephemeral(&target) {
            teardown_worktree(&repo, &target, true)
                .context("cleanup after ephemeral marker failure")?;
            if new_branch {
                delete_local_branch(&repo, &reference, true)?;
            }
            return Err(error).context("mark worktree ephemeral");
        }
    }

    Ok(CreatedWorktree { target, base, mode })
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
        Some(0) => bail!("branch '{branch}' already exists"),
        _ => Err(git_failure("git show-ref --verify", &exists)),
    }
}

fn register_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    checkout: bool,
    new_branch: bool,
) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["worktree", "add"]);
    if !checkout {
        command.arg("--no-checkout");
    }
    if new_branch {
        command.args(["-b", branch]);
    }
    command
        .arg(target)
        .arg(if new_branch { base } else { branch });
    run_command(&mut command, "create linked worktree")
}

fn add_cow_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    new_branch: bool,
    sparse: &[String],
) -> Result<()> {
    let baseline = cow::ensure_baseline(repo, base)?;
    register_worktree(repo, branch, target, base, false, new_branch)?;

    let populate_result = if sparse.is_empty() {
        populate_cow_worktree(target, &baseline)
    } else {
        populate_sparse_cow_worktree(target, &baseline, sparse)
    };

    if let Err(error) = populate_result {
        rollback_created_worktree(repo, target, branch, new_branch)?;
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

/// Reject sparse arguments that would escape the worktree or confuse cone mode.
fn validate_sparse(sparse: &[String]) -> Result<Vec<String>> {
    let mut clean = Vec::with_capacity(sparse.len());
    for entry in sparse {
        let trimmed = entry.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            bail!("--sparse needs a directory inside the repository");
        }
        let path = Path::new(trimmed);
        if path.is_absolute() || path.components().any(|part| part.as_os_str() == "..") {
            bail!("--sparse {entry} must be a path inside the repository");
        }
        clean.push(trimmed.to_owned());
    }
    Ok(clean)
}

/// Fill a registered, empty linked worktree from the cached baseline.
///
/// Prefers one directory-level clone plus the baseline's stat-refreshed index.
/// That makes creation and the worktree's first `git status` an order of
/// magnitude cheaper than cloning file by file and building an index with
/// `read-tree`, which leaves Git to rescan every file. Falls back to the
/// per-file clone when the platform or the cached baseline cannot support it.
fn populate_cow_worktree(target: &Path, baseline: &Path) -> Result<()> {
    if cow::ROOT_CLONE && root_clone_worktree(target, baseline).is_ok() {
        return ensure_clean(target).context("verify cloned worktree");
    }
    run_git_at(target, ["read-tree", "HEAD"]).context("initialize linked-worktree index")?;
    cow::clone_tree(baseline, target).context("clone cached baseline")?;
    ensure_clean(target).context("verify cloned worktree")
}

/// Replace the empty registered worktree with a whole-tree clone.
///
/// `clonefile(2)` requires a destination that does not exist, so the
/// directory Git just created is removed and its `.git` pointer restored
/// afterwards. Any failure restores the empty worktree so the caller can fall
/// back without leaving a half-populated checkout behind.
fn root_clone_worktree(target: &Path, baseline: &Path) -> Result<()> {
    let index = cow::baseline_index(baseline).context("baseline has no published index")?;
    let pointer = target.join(".git");
    let pointer_bytes = fs::read(&pointer).context("read linked-worktree pointer")?;

    fs::remove_file(&pointer).context("detach linked-worktree pointer")?;
    if let Err(error) = fs::remove_dir(target) {
        fs::write(&pointer, &pointer_bytes)?;
        return Err(error).context("registered worktree was not empty");
    }
    if let Err(error) = cow::clone_root(baseline, target) {
        fs::create_dir_all(target)?;
        fs::write(&pointer, &pointer_bytes)?;
        return Err(error);
    }
    fs::write(&pointer, &pointer_bytes).context("restore linked-worktree pointer")?;

    let git_dir = PathBuf::from(git_path_output(
        target,
        ["rev-parse", "--absolute-git-dir"],
    )?);
    let worktree_index = git_dir.join("index");
    fs::copy(&index, &worktree_index).context("install baseline index")?;
    // The copied index describes the baseline's inodes, which Git would treat
    // as stale and rehash. Re-record the clone's own stat data instead; Git's
    // strict staleness checks are untouched.
    index::adopt_stat_data(&worktree_index, target).context("adopt cloned worktree stat data")
}

fn add_git_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    new_branch: bool,
    sparse: &[String],
) -> Result<()> {
    // Without CoW there is nothing to clone, so Git does the whole job. The
    // index is built first and the cone configured after it, which is the
    // order in which `sparse-checkout set` materializes the cone itself.
    register_worktree(repo, branch, target, base, sparse.is_empty(), new_branch)?;
    let populate = (|| -> Result<()> {
        if !sparse.is_empty() {
            run_git_at(target, ["read-tree", "HEAD"])
                .context("initialize linked-worktree index")?;
            configure_sparse(target, sparse)?;
            reapply_sparse(target)?;
        }
        ensure_clean(target)
    })();
    if let Err(error) = populate {
        rollback_created_worktree(repo, target, branch, new_branch)?;
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
    new_branch: bool,
) -> Result<()> {
    let baseline = cow::ensure_baseline(repo, base)?;

    let overlay_dir = overlay::root(&repo.common_git_dir).join(Uuid::new_v4().to_string());
    let upper = overlay_dir.join("upper");
    let work = overlay_dir.join("work");
    fs::create_dir_all(&upper).context("create overlay upperdir")?;
    fs::create_dir_all(&work).context("create overlay workdir")?;

    if let Err(error) = register_worktree(repo, branch, target, base, false, new_branch) {
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
        rollback_created_worktree(repo, target, branch, new_branch)?;
        fs::remove_dir_all(&overlay_dir).context("cleanup failed overlay state")?;
        return Err(error);
    }
    Ok(())
}

/// Recheck dirty state at teardown so a command finishing during GC selection
/// cannot turn a previously clean workspace into silently discarded work.
fn teardown_worktree(repo: &RepoContext, target: &Path, force: bool) -> Result<()> {
    ensure_unlocked(repo, target)?;
    // Report uncommitted work in simgit's own terms on every path: Git's
    // `worktree remove` only knows about --force, not about --commit.
    if !force && worktree_dirty(target)? {
        bail!(
            "worktree has uncommitted changes: {}\n\
             keep the work with --commit, or discard it with --force",
            target.display()
        );
    }
    let result = if let Some(state) = overlay::state(repo, target) {
        let lock = WorktreeLock::acquire(repo, target)?;
        if !force && worktree_dirty(target)? {
            bail!("worktree has uncommitted changes; pass --commit or --force");
        }
        overlay::unmount(target)?;
        remove_dir_if_present(target)?;
        remove_dir_if_present(&state.overlay_dir)?;
        lock.release()?;
        prune_git_worktrees(repo)
    } else if force {
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

fn worktree_lock_path(repo: &RepoContext, target: &Path) -> Result<PathBuf> {
    if let Some(admin) = overlay::admin_dir(repo, target) {
        return Ok(admin.join("locked"));
    }
    // Git cannot lock its main worktree, which it already refuses to remove.
    // Use a separate marker there only to serialize simgit launches.
    if target.join(".git").exists() && worktree_admin_dir(target)? == repo.common_git_dir {
        return Ok(repo.common_git_dir.join("simgit-run.lock"));
    }
    bail!("cannot find worktree registration for {}", target.display())
}

fn ensure_unlocked(repo: &RepoContext, target: &Path) -> Result<()> {
    if worktree_lock_path(repo, target).is_ok_and(|path| path.exists()) {
        bail!(
            "worktree is locked (a command may be running): {}",
            target.display()
        );
    }
    Ok(())
}

struct WorktreeLock {
    path: Option<PathBuf>,
}

impl WorktreeLock {
    fn acquire(repo: &RepoContext, target: &Path) -> Result<Self> {
        let path = worktree_lock_path(repo, target)?;
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "workspace is already in use by a running command: {}\n\
                 if nothing is running there, release it with: git worktree unlock {}",
                target.display(),
                target.display()
            ),
            Err(error) => {
                return Err(error).with_context(|| format!("lock workspace at {}", path.display()))
            }
        };
        use std::io::Write;
        let lock = Self { path: Some(path) };
        file.write_all(b"simgit: workspace in use\n")?;
        Ok(lock)
    }

    fn release(mut self) -> Result<()> {
        if let Some(path) = self.path.take() {
            remove_file_if_present(&path)?;
        }
        Ok(())
    }
}

impl Drop for WorktreeLock {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if let Err(error) = remove_file_if_present(path) {
                eprintln!("could not unlock worktree: {error:#}");
            }
        }
    }
}

fn remove(args: WorktreeRemove, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    let target = resolve_worktree_target(&repo, args.target.as_deref())?;
    ensure_unlocked(&repo, &target)?;
    let branch = list_worktrees(&repo)?
        .into_iter()
        .find(|entry| entry.path == target)
        .and_then(|entry| entry.branch)
        .or_else(|| overlay::branch(&repo, &target));
    let mut committed = false;
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
                committed = true;
            }
            _ => return Err(git_failure("git diff --cached --quiet", &diff)),
        }
    }

    teardown_worktree(&repo, &target, args.force)?;

    let mut branch_deleted = false;
    if args.delete_branch {
        let branch = branch.context("cannot delete branch for a detached worktree")?;
        delete_local_branch(&repo, &branch, args.force)?;
        branch_deleted = true;
    }

    if json {
        emit(&json!({
            "removed": target.display().to_string(),
            "committed": committed,
            "branch_deleted": branch_deleted,
        }));
    } else {
        println!("{}", target.display());
    }
    Ok(())
}

/// Resolve a user-supplied worktree reference — an explicit path, a branch
/// name, or (when omitted) the worktree containing the current directory — to
/// an absolute worktree path.
fn resolve_worktree_target(repo: &RepoContext, target: Option<&str>) -> Result<PathBuf> {
    let Some(spec) = target else {
        return Ok(repo.top_level.clone());
    };
    let as_path = absolute_path(PathBuf::from(spec))?;
    if as_path.exists() {
        return Ok(as_path);
    }
    if let Some(path) = worktree_path_for_branch(repo, spec)? {
        return Ok(path);
    }
    if let Some(path) = overlay::worktree_for_branch(repo, spec) {
        return Ok(path);
    }
    bail!("no worktree found for '{spec}' (not an existing path or a checked-out branch)");
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

fn list(json_output: bool) -> Result<()> {
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

/// Keep recoverable unmounted overlays registered during native pruning.
fn prune_git_worktrees(repo: &RepoContext) -> Result<()> {
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

fn prune(args: WorktreePrune, json: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    let protected: HashSet<PathBuf> = overlay::registrations(&repo)
        .into_iter()
        .filter_map(|(_, state)| state.lower)
        .collect();
    prune_git_worktrees(&repo)?;
    let outcome = cow::prune_baselines(&repo.common_git_dir, args.all, &protected)?;

    if json {
        emit(&json!({
            "pruned": outcome.removed,
            "retained": outcome.retained,
            "retained_bytes": outcome.retained_bytes,
        }));
        return Ok(());
    }
    println!(
        "pruned {} cached baseline(s); {} retained ({:.1} MiB)",
        outcome.removed.len(),
        outcome.retained.len(),
        outcome.retained_bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

fn gc(args: WorktreeGc, json: bool) -> Result<()> {
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

fn repair(json_output: bool) -> Result<()> {
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

struct GcOutcome {
    reaped: Vec<PathBuf>,
    skipped: Vec<(PathBuf, &'static str)>,
    retained_branches: Vec<String>,
    deleted_branches: Vec<String>,
}

/// Core reaping logic, separated from output for testability. Returns the
/// worktrees reaped (or that would be, under `--dry-run`) and those skipped.
fn run_gc(repo: &RepoContext, args: &WorktreeGc) -> Result<GcOutcome> {
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
            continue;
        }
        if !args.force {
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
        match teardown_worktree(repo, &entry.path, args.force) {
            Ok(()) => {
                if args.delete_branches {
                    if let Some(branch) = &entry.branch {
                        if delete_local_branch(repo, branch, args.force).is_err() {
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

fn delete_local_branch(repo: &RepoContext, branch_ref: &str, force: bool) -> Result<()> {
    let branch = branch_ref.strip_prefix("refs/heads/").unwrap_or(branch_ref);
    if branch == "main" || branch == "master" {
        bail!("refusing to delete primary branch '{branch}'");
    }
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["branch", if force { "-D" } else { "-d" }, branch]);
    run_command(&mut command, "delete worktree branch")
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
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["worktree", "remove", "--force"])
        .arg(target);
    run_command(&mut command, "git worktree remove --force")
}

fn run_git_common<I, S>(repo: &RepoContext, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(args);
    run_command(&mut command, "git")
}

fn run_git_at<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(args);
    run_command(&mut command, "git")
}

fn git_output_common<const N: usize>(repo: &RepoContext, args: [&str; N]) -> Result<Output> {
    Command::new("git")
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
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
