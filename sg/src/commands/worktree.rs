//! Native Git linked worktrees populated with filesystem copy-on-write clones.
//!
//! `simgit` deliberately has no daemon dependency. Git owns the refs,
//! index, commits, and lifecycle; simgit only avoids repeatedly inflating the
//! same checkout by cloning an immutable cached baseline when the filesystem
//! supports it.

use anyhow::{bail, Context, Result};
use clap::Args;
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

pub use launch::{run_in_worktree, WorktreeRun};

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

/// Report repository and platform capabilities without changing Git
/// registrations or pruning caches.
///
/// Being outside a repository is an answer, not a failure: `doctor` is the
/// command an agent runs to find out whether this directory is usable at all,
/// so the repository-dependent facts are reported as `null` and the checks
/// that only need the filesystem still run.
pub fn doctor(json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match discover_repo(&cwd) {
        Ok(repo) => doctor_in_repository(&repo, json),
        Err(_) => doctor_without_repository(&cwd, json),
    }
}

fn doctor_in_repository(repo: &RepoContext, json: bool) -> Result<()> {
    let root = absolute_path(
        default_worktree_path(&repo.common_git_dir, "doctor-probe")?
            .parent()
            .context("default worktree has no root")?
            .to_path_buf(),
    )?;
    let root = lexical_normalize(&root);
    let probe = nearest_existing_directory(&root)?;
    let filesystem = filesystem_name(&probe);
    let cow_supported = cow::clone_supported(&repo.common_git_dir, &probe).ok();
    let populate_mode = select_populate_mode(repo, &probe, false)?.label();
    let git_worktree = git_output_common(repo, ["worktree", "list", "--porcelain", "-z"])?;
    let git_worktree_supported = git_worktree.status.success();
    let stale = if git_worktree_supported {
        stale_worktree_registrations(&git_worktree.stdout)
    } else {
        Vec::new()
    };
    let baselines = cow::baseline_inventory(&repo.common_git_dir)?;
    let baseline_count = baselines.retained.len();
    let repository = repo.top_level.display().to_string();
    let common_git_owner = main_worktree(&repo.common_git_dir);
    let root_inside_repository = resolved_path(&root)?.starts_with(resolved_path(&repo.top_level)?);
    let is_main_worktree = resolved_path(&repo.top_level)? == resolved_path(common_git_owner)?;

    if json {
        emit(&json!({
            "product": "simgit",
            "version": env!("CARGO_PKG_VERSION"),
            "identity": "simgit",
            "repository": repository,
            "repository_details": {
                "top_level": repository,
                "common_git_dir": repo.common_git_dir.display().to_string(),
                "common_git_owner": common_git_owner.display().to_string(),
                "is_main_worktree": is_main_worktree,
            },
            "filesystem": filesystem,
            "cow_supported": cow_supported,
            "populate_mode": populate_mode,
            "default_worktree_root": root.display().to_string(),
            "default_worktree_root_inside_repository": root_inside_repository,
            "git_worktree_supported": git_worktree_supported,
            "stale_worktree_registrations": stale,
            "baseline_cache": {
                "root": baselines.root.display().to_string(),
                "retained": baselines.retained,
                "retained_count": baseline_count,
                "retained_bytes": baselines.retained_bytes,
            },
        }));
    } else {
        println!("simgit {}", env!("CARGO_PKG_VERSION"));
        println!("repository: {}", repo.top_level.display());
        println!("common git dir: {}", repo.common_git_dir.display());
        println!("filesystem: {filesystem}");
        println!("CoW: {}", cow_status(cow_supported));
        println!("populate mode: {populate_mode}");
        println!(
            "worktree root: {} ({})",
            root.display(),
            if root_inside_repository {
                "unsafe: inside repository"
            } else {
                "safe"
            }
        );
        println!(
            "Git worktrees: {} ({} stale registration(s))",
            if git_worktree_supported {
                "supported"
            } else {
                "unavailable"
            },
            stale.len()
        );
        println!(
            "baseline cache: {} retained ({:.1} MiB)",
            baseline_count,
            baselines.retained_bytes as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

/// Report what can be known without a repository: simgit's identity and
/// version, and what the current directory's filesystem can do. Everything
/// derived from Git registrations, the worktree root or the baseline cache is
/// `null`, since there is nothing to derive it from.
fn doctor_without_repository(cwd: &Path, json: bool) -> Result<()> {
    let probe = nearest_existing_directory(cwd)?;
    let filesystem = filesystem_name(&probe);
    let cow_supported = cow_supported_without_repository(&probe);

    if json {
        emit(&json!({
            "product": "simgit",
            "version": env!("CARGO_PKG_VERSION"),
            "identity": "simgit",
            "repository": null,
            "repository_details": null,
            "filesystem": filesystem,
            "cow_supported": cow_supported,
            "populate_mode": null,
            "default_worktree_root": null,
            "default_worktree_root_inside_repository": null,
            "git_worktree_supported": null,
            "stale_worktree_registrations": [],
            "baseline_cache": null,
        }));
    } else {
        println!("simgit {}", env!("CARGO_PKG_VERSION"));
        println!(
            "repository: none ({} is not in a Git working tree)",
            cwd.display()
        );
        println!("filesystem: {filesystem}");
        println!("CoW: {}", cow_status(cow_supported));
        println!(
            "worktree root, populate mode, Git worktree support and baseline cache: \
             run simgit doctor inside a repository"
        );
    }
    Ok(())
}

/// Probe filesystem cloning where there is no repository state directory to
/// hold the probe's source file.
///
/// Both probe files go in a scratch directory inside the directory being
/// probed — they have to share its filesystem for the answer to mean anything
/// — and the scratch directory takes them with it when it is removed.
fn cow_supported_without_repository(dir: &Path) -> Option<bool> {
    let scratch = dir.join(format!(".simgit-doctor-probe-{}", Uuid::new_v4()));
    if fs::create_dir(&scratch).is_err() {
        // Nothing was measured: an unwritable directory says something about
        // this process's permissions, not about what the filesystem can do.
        return None;
    }
    let supported = cow::clone_supported(&scratch, &scratch).ok();
    let _ = fs::remove_dir_all(&scratch);
    supported
}

/// How `doctor` words a copy-on-write verdict. "Unavailable" is a measured
/// answer, so an unmeasurable one must not borrow it: reporting `filesystem:
/// apfs, CoW: unavailable` because a probe could not be written contradicts
/// itself.
fn cow_status(supported: Option<bool>) -> &'static str {
    match supported {
        Some(true) => "supported",
        Some(false) => "unavailable",
        None => "unknown (cannot write a probe here)",
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
    warn_if_inside_repository(&repo, &target);

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
    if let Err(error) = populated {
        // `git worktree add` can create the branch and then fail on the
        // destination, so a refused allocation would otherwise leave an
        // `agent/<uuid>` ref nobody can attribute.
        if kind == WorktreeKind::NewBranch {
            discard_partial_branch(&repo, reference.as_deref());
        }
        return Err(error);
    }

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

/// The lock file's single line. It doubles as Git's lock reason — `git
/// worktree list` and `simgit list` echo it — so it stays one readable line,
/// and it names the launching process so a stranded lock can be told apart
/// from a live one.
fn lock_contents() -> String {
    format!("simgit: workspace in use (pid {})\n", std::process::id())
}

/// The process id recorded in a lock file. Locks written before simgit
/// recorded one have an unknown owner, and clearing those cannot be refused.
fn lock_owner_pid(contents: &str) -> Option<i32> {
    let rest = contents.split_once("(pid ")?.1;
    rest.split_once(')')?.0.trim().parse().ok()
}

fn lock_owner_of(path: &Path) -> Option<i32> {
    fs::read_to_string(path)
        .ok()
        .as_deref()
        .and_then(lock_owner_pid)
}

/// True while `pid` still names a live process.
///
/// `kill(pid, 0)` runs the existence and permission checks without delivering
/// anything. Success means the process is there; `EPERM` means it is there but
/// owned by another user, which is still a reason not to steal its lock; only
/// `ESRCH` means it is gone.
fn process_alive(pid: i32) -> bool {
    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // SAFETY: `kill` takes two integers and dereferences nothing, and signal 0
    // delivers no signal, so the call only reports whether the pid is live.
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
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
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = lock_owner_of(&path)
                    .map(|pid| format!(" (pid {pid})"))
                    .unwrap_or_default();
                bail!(
                    "workspace is already in use by a running command{owner}: {}\n\
                     if nothing is running there, release it with: simgit unlock {}",
                    target.display(),
                    target.display()
                )
            }
            Err(error) => {
                return Err(error).with_context(|| format!("lock workspace at {}", path.display()))
            }
        };
        use std::io::Write;
        let lock = Self { path: Some(path) };
        file.write_all(lock_contents().as_bytes())?;
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

/// An exclusive claim on one destination path for the length of an allocation.
///
/// Creating a worktree is several Git operations, and two `add --path <same>`
/// runs interleaving through them both used to finish: Git kept two
/// registrations for one directory, the winner's JSON named the loser's
/// branch, and afterwards neither allocation could be removed by its token.
/// The claim is a `create_new` file named for the destination, so the second
/// allocator fails immediately and changes nothing.
struct PathClaim {
    path: PathBuf,
}

/// The claim file's single line, in the shape `lock_owner_pid` reads, so a
/// claim left behind by a killed allocator can be told from a live one.
fn claim_contents() -> String {
    format!(
        "simgit: allocating a worktree here (pid {})\n",
        std::process::id()
    )
}

/// A stable file name for one destination path: claims live in a flat
/// directory, and a worktree path contains separators and may be long.
fn path_digest(path: &Path) -> String {
    let hash = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("{hash:016x}")
}

impl PathClaim {
    fn acquire(repo: &RepoContext, target: &Path) -> Result<Self> {
        let directory = state_dir(&repo.common_git_dir).join("claims");
        fs::create_dir_all(&directory).context("create the allocation claim directory")?;
        let digest = path_digest(target);
        // The claim is published complete. Creating an empty file and then
        // writing the owner into it leaves an instant where a second
        // allocator reads an ownerless claim, concludes it is stale, and
        // takes it — which is the very race this exists to close.
        let pending = directory.join(format!("{digest}.{}.pending", Uuid::new_v4()));
        fs::write(&pending, claim_contents())
            .with_context(|| format!("write {}", pending.display()))?;
        let claimed = Self::publish(&pending, directory.join(format!("{digest}.claim")), target);
        let _ = fs::remove_file(&pending);
        claimed
    }

    /// Link the prepared claim into place, taking over one left behind by an
    /// allocator that is no longer running.
    fn publish(pending: &Path, path: PathBuf, target: &Path) -> Result<Self> {
        let mut stole = false;
        loop {
            match fs::hard_link(pending, &path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner = lock_owner_of(&path);
                    if !stole && !owner.is_some_and(process_alive) {
                        // The allocator that wrote this claim is gone, so it
                        // is holding nothing; whatever it half-created is
                        // Git's registration to report, not a live claim.
                        stole = true;
                        remove_file_if_present(&path)?;
                        continue;
                    }
                    let owner = owner.map(|pid| format!(" (pid {pid})")).unwrap_or_default();
                    bail!(
                        "another simgit is already creating a worktree at {}{owner}\n\
                         wait for it to finish, or allocate a different path",
                        target.display()
                    );
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("claim the worktree path {}", target.display()))
                }
            }
        }
    }
}

impl Drop for PathClaim {
    fn drop(&mut self) {
        if let Err(error) = remove_file_if_present(&self.path) {
            eprintln!("could not release the worktree path claim: {error:#}");
        }
    }
}

#[derive(Args)]
pub struct WorktreeUnlock {
    /// Worktree path or branch name. Defaults to the worktree containing the
    /// current directory.
    pub target: Option<String>,
}

/// Clear the `run` lock a killed launcher left behind.
///
/// The lock names the launching process, so this refuses while that process is
/// alive: the fix then is to stop it, not to force the lock and let two
/// commands write the same checkout. That is why there is no override flag.
/// Unlocking a workspace that is not locked is the state the caller asked for,
/// so it succeeds — including when the workspace itself is already gone, which
/// is what a recovery pass that crashed after cleanup sees when it retries.
pub fn unlock(args: WorktreeUnlock, json: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    let (label, lock_path) = match lookup_worktree_target(&repo, args.target.as_deref())? {
        TargetLookup::Found(target) => {
            let path = worktree_lock_path(&repo, &target)?;
            (target.display().to_string(), Some(path))
        }
        TargetLookup::Absent(spec) => (spec, None),
    };
    let contents = match &lock_path {
        Some(path) => match fs::read_to_string(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        },
        None => None,
    };
    let owner_pid = contents.as_deref().and_then(lock_owner_pid);
    if let Some(pid) = owner_pid.filter(|pid| process_alive(*pid)) {
        bail!(
            "workspace is in use by a running command (pid {pid}): {label}\n\
             stop that process, then run simgit unlock again"
        );
    }
    let was_locked = contents.is_some();
    if let Some(path) = &lock_path {
        remove_file_if_present(path)?;
    }

    if json {
        emit(&json!({
            "unlocked": label,
            "was_locked": was_locked,
            "owner_pid": owner_pid,
        }));
    } else if was_locked {
        println!("{label}");
    } else {
        println!("{label} (was not locked)");
    }
    Ok(())
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

    teardown_worktree(&repo, &target, args.discard_dirty)?;

    let mut branch_deleted = false;
    if let (true, Some(branch)) = (args.delete_branch, branch) {
        delete_local_branch(&repo, &branch, args.delete_unmerged)?;
        branch_deleted = true;
    }

    if json {
        emit(&json!({
            "removed": target.display().to_string(),
            "already_absent": false,
            "committed": committed,
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
            "branch_deleted": branch_deleted,
        }));
    } else {
        println!("{spec} (already absent)");
    }
    Ok(())
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
    /// An existing path, or the worktree checked out on a branch.
    Found(PathBuf),
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
    if as_path.exists() {
        return Ok(TargetLookup::Found(canonical_path(&as_path)));
    }
    if let Some(path) = worktree_path_for_branch(repo, spec)? {
        return Ok(TargetLookup::Found(path));
    }
    if let Some(path) = overlay::worktree_for_branch(repo, spec) {
        return Ok(TargetLookup::Found(path));
    }
    Ok(TargetLookup::Absent(spec.to_owned()))
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

struct GcOutcome {
    reaped: Vec<PathBuf>,
    skipped: Vec<(PathBuf, &'static str)>,
    retained_branches: Vec<String>,
    deleted_branches: Vec<String>,
}

/// Core reaping logic, separated from output for testability. Returns the
/// worktrees reaped (or that would be, under `--dry-run`) and those skipped.
fn run_gc(repo: &RepoContext, args: &WorktreeGc) -> Result<GcOutcome> {
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

fn delete_local_branch(repo: &RepoContext, branch_ref: &str, force: bool) -> Result<()> {
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

/// Delete a branch a failed allocation may have created, if it is still there.
///
/// `git worktree add -b` creates the branch before it validates the
/// destination, so a refused allocation leaves an `agent/<uuid>` ref that
/// belongs to nobody. Best effort: the branch outliving a failure is untidy,
/// while failing the failure path hides the error that caused it.
fn discard_partial_branch(repo: &RepoContext, reference: Option<&str>) {
    let Some(reference) = reference else {
        return;
    };
    let exists = git_output_common(repo, ["show-ref", "--verify", "--quiet", reference]);
    if exists.is_ok_and(|output| output.status.success()) {
        let _ = delete_local_branch(repo, reference, true);
    }
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
