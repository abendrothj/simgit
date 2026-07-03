//! Native Git linked worktrees populated with filesystem copy-on-write clones.
//!
//! `sg worktree` deliberately has no daemon dependency. Git owns the refs,
//! index, commits, and lifecycle; simgit only avoids repeatedly inflating the
//! same checkout by cloning an immutable cached baseline when the filesystem
//! supports it.

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

const BASELINE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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
}

#[derive(Args)]
pub struct WorktreeAdd {
    /// Branch name to create (for example, feat/my-feature).
    pub branch: String,

    /// Worktree path. Defaults to `.git/simgit/worktrees/<branch>`.
    pub path: Option<PathBuf>,

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
    /// Only reap worktrees created with `--ephemeral`.
    #[arg(long)]
    pub ephemeral: bool,

    /// Only reap worktrees whose branch starts with this prefix.
    #[arg(long)]
    pub prefix: Option<String>,

    /// Reap worktrees idle at least this long (e.g. 90s, 30m, 24h, 7d).
    #[arg(long, default_value = "24h")]
    pub older_than: String,

    /// Reap even if the worktree has uncommitted changes (discards them).
    #[arg(long)]
    pub force: bool,

    /// Report what would be reaped without removing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug)]
struct RepoContext {
    top_level: PathBuf,
    common_git_dir: PathBuf,
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
        Worktree::Prune(args) => prune(args),
        Worktree::Gc(args) => {
            let json = args.json || global_json;
            gc(args, json)
        }
    }
}

fn add(args: WorktreeAdd, json: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    validate_new_branch(&repo, &args.branch)?;
    let base = resolve_commit(&repo, args.base.as_deref().unwrap_or("HEAD"))?;
    let target = absolute_path(
        args.path
            .unwrap_or_else(|| default_worktree_path(&repo.common_git_dir, &args.branch)),
    )?;

    if target.exists() {
        bail!("worktree path already exists: {}", target.display());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create worktree parent {}", parent.display()))?;
    }

    let target_parent = target.parent().context("worktree path has no parent")?;
    let mode = select_populate_mode(&repo, target_parent, args.require_cow)?;

    match mode {
        PopulateMode::CowClone => add_cow_worktree(&repo, &args.branch, &target, &base)?,
        PopulateMode::Overlay => add_overlay_worktree(&repo, &args.branch, &target, &base)?,
        PopulateMode::GitCheckout => add_git_worktree(&repo, &args.branch, &target, &base)?,
    }

    if args.ephemeral {
        mark_ephemeral(&target)?;
    }

    if json {
        emit(&json!({
            "worktree": target.display().to_string(),
            "branch": args.branch,
            "base": base,
            "mode": mode.label(),
            "ephemeral": args.ephemeral,
        }));
    } else {
        eprintln!("mode: {}", mode.label());
        println!("{}", target.display());
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
                if cow_clone_supported(&repo.common_git_dir, target_parent)? {
                    Ok(PopulateMode::CowClone)
                } else {
                    bail!("SIMGIT_POPULATE=reflink but reflink cloning is unsupported here")
                }
            }
            "overlay" => {
                if overlay_supported() {
                    Ok(PopulateMode::Overlay)
                } else {
                    bail!("SIMGIT_POPULATE=overlay but fuse-overlayfs is not installed")
                }
            }
            "checkout" | "git-checkout" => Ok(PopulateMode::GitCheckout),
            other => bail!("unknown SIMGIT_POPULATE={other} (use reflink, overlay, or checkout)"),
        };
    }

    if cow_clone_supported(&repo.common_git_dir, target_parent)? {
        Ok(PopulateMode::CowClone)
    } else if overlay_supported() {
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

fn add_cow_worktree(repo: &RepoContext, branch: &str, target: &Path, base: &str) -> Result<()> {
    let baseline = ensure_baseline(repo, base)?;
    let add_result = run_git_common(
        repo,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--no-checkout"),
            OsStr::new("-b"),
            OsStr::new(branch),
            target.as_os_str(),
            OsStr::new(base),
        ],
    );
    if let Err(error) = add_result {
        return Err(error.context("create linked worktree"));
    }

    let populate_result = (|| -> Result<()> {
        run_git_at(target, ["read-tree", "HEAD"]).context("initialize linked-worktree index")?;
        clone_tree(&baseline, target).context("clone cached baseline")?;
        ensure_clean(target).context("verify cloned worktree")
    })();

    if let Err(error) = populate_result {
        rollback_created_worktree(repo, target, branch);
        return Err(error);
    }
    Ok(())
}

fn add_git_worktree(repo: &RepoContext, branch: &str, target: &Path, base: &str) -> Result<()> {
    run_git_common(
        repo,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(branch),
            target.as_os_str(),
            OsStr::new(base),
        ],
    )
    .context("create linked worktree with normal Git checkout")?;
    ensure_clean(target)
}

/// Marker file (in the worktree's Git admin dir) recording that a worktree is
/// backed by a fuse-overlayfs mount, and where its upper/work dirs live.
const OVERLAY_MARKER: &str = "simgit-overlay";

/// Whether the unprivileged `fuse-overlayfs` mount helper is available. This is
/// the CoW path for Linux filesystems without reflink support (e.g. ext4).
fn overlay_supported() -> bool {
    binary_on_path("fuse-overlayfs")
}

fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn overlay_root(common_git_dir: &Path) -> PathBuf {
    state_dir(common_git_dir).join("overlays")
}

/// Create a linked worktree whose files are served by a fuse-overlayfs mount:
/// `lowerdir` is the shared read-only baseline, and a per-worktree `upperdir`
/// captures writes. Unchanged files cost no disk, on any Linux filesystem.
fn add_overlay_worktree(repo: &RepoContext, branch: &str, target: &Path, base: &str) -> Result<()> {
    let baseline = ensure_baseline(repo, base)?;

    let overlay_dir = overlay_root(&repo.common_git_dir).join(Uuid::new_v4().to_string());
    let upper = overlay_dir.join("upper");
    let work = overlay_dir.join("work");
    fs::create_dir_all(&upper).context("create overlay upperdir")?;
    fs::create_dir_all(&work).context("create overlay workdir")?;

    run_git_common(
        repo,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--no-checkout"),
            OsStr::new("-b"),
            OsStr::new(branch),
            target.as_os_str(),
            OsStr::new(base),
        ],
    )
    .context("create linked worktree")?;

    let populate = (|| -> Result<()> {
        // The `.git` gitlink file must survive the overlay mount (which replaces
        // the directory view), so stage it into the upperdir before mounting.
        fs::rename(target.join(".git"), upper.join(".git"))
            .context("stage worktree gitlink into overlay upperdir")?;
        mount_overlay(&baseline, &upper, &work, target)?;
        run_git_at(target, ["read-tree", "HEAD"]).context("initialize overlay worktree index")?;
        ensure_clean(target).context("verify overlay worktree")?;
        write_overlay_marker(target, &overlay_dir)?;
        Ok(())
    })();

    if let Err(error) = populate {
        unmount_overlay(target);
        let _ = fs::remove_dir_all(target);
        let _ = fs::remove_dir_all(&overlay_dir);
        let _ = run_git_common(repo, [OsStr::new("worktree"), OsStr::new("prune")]);
        let _ = Command::new("git")
            .arg(format!("--git-dir={}", repo.common_git_dir.display()))
            .args(["branch", "-D", branch])
            .status();
        return Err(error);
    }
    Ok(())
}

fn mount_overlay(lower: &Path, upper: &Path, work: &Path, mountpoint: &Path) -> Result<()> {
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let mut command = Command::new("fuse-overlayfs");
    command.arg("-o").arg(opts).arg(mountpoint);
    run_command(&mut command, "mount fuse-overlayfs overlay")
}

/// Best-effort unmount of a fuse-overlayfs mount. Tolerates an already-unmounted
/// path so teardown is idempotent.
fn unmount_overlay(mountpoint: &Path) {
    for tool in ["fusermount3", "fusermount"] {
        let status = Command::new(tool)
            .arg("-u")
            .arg(mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(status, Ok(status) if status.success()) {
            return;
        }
    }
    let _ = Command::new("umount").arg(mountpoint).status();
}

fn write_overlay_marker(worktree: &Path, overlay_dir: &Path) -> Result<()> {
    let admin = worktree_admin_dir(worktree)?;
    fs::write(
        admin.join(OVERLAY_MARKER),
        overlay_dir.display().to_string(),
    )
    .context("write overlay marker")?;
    Ok(())
}

/// If `worktree` is overlay-backed, return its overlay dir (holding upper/work).
/// Reads the marker via Git's admin dir, so call it while the worktree is still
/// mounted and registered.
fn overlay_state(worktree: &Path) -> Option<PathBuf> {
    let admin = worktree_admin_dir(worktree).ok()?;
    let content = fs::read_to_string(admin.join(OVERLAY_MARKER)).ok()?;
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// Tear down a worktree unconditionally, handling the overlay case (unmount +
/// remove mount dir + drop upper/work + prune the registry) or delegating to
/// `git worktree remove --force` for plain worktrees.
fn force_teardown(repo: &RepoContext, target: &Path) -> Result<()> {
    if let Some(overlay_dir) = overlay_state(target) {
        unmount_overlay(target);
        let _ = fs::remove_dir_all(target);
        let _ = fs::remove_dir_all(&overlay_dir);
        run_git_common(repo, [OsStr::new("worktree"), OsStr::new("prune")])
    } else {
        remove_worktree_force(repo, target)
    }
}

fn remove(args: WorktreeRemove, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    let target = resolve_worktree_target(&repo, args.target.as_deref())?;
    // Detect overlay backing while the worktree is still mounted/registered.
    let overlay = overlay_state(&target);

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

    if let Some(overlay_dir) = overlay {
        if !args.force && !committed && worktree_dirty(&target).unwrap_or(false) {
            bail!("worktree has uncommitted changes; pass --commit or --force");
        }
        unmount_overlay(&target);
        let _ = fs::remove_dir_all(&target);
        let _ = fs::remove_dir_all(&overlay_dir);
        run_git_common(&repo, [OsStr::new("worktree"), OsStr::new("prune")])?;
    } else {
        let mut command = Command::new("git");
        command
            .arg(format!("--git-dir={}", repo.common_git_dir.display()))
            .args(["worktree", "remove"]);
        if args.force {
            command.arg("--force");
        }
        command.arg(&target);
        run_command(&mut command, "git worktree remove")?;
    }

    if json {
        emit(&json!({
            "removed": target.display().to_string(),
            "committed": committed,
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

fn list(json_output: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    if !json_output {
        let output = git_output_common(&repo, ["worktree", "list"])?;
        if !output.status.success() {
            return Err(git_failure("git worktree list", &output));
        }
        print!("{}", String::from_utf8_lossy(&output.stdout));
        return Ok(());
    }

    let output = git_output_common(&repo, ["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let mut entries = Vec::new();
    let mut current = serde_json::Map::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
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
    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}

fn prune(args: WorktreePrune) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    run_git_common(&repo, [OsStr::new("worktree"), OsStr::new("prune")])?;
    let removed = prune_baselines(&repo.common_git_dir, args.all)?;
    println!("pruned {removed} cached baseline(s)");
    Ok(())
}

fn gc(args: WorktreeGc, json: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    let (reaped, skipped) = run_gc(&repo, &args)?;

    if json {
        emit(&json!({
            "dry_run": args.dry_run,
            "reaped": reaped
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            "skipped": skipped
                .iter()
                .map(|(p, why)| json!({ "worktree": p.display().to_string(), "reason": why }))
                .collect::<Vec<_>>(),
        }));
    } else {
        let verb = if args.dry_run { "would reap" } else { "reaped" };
        for path in &reaped {
            println!("{verb}: {}", path.display());
        }
        for (path, why) in &skipped {
            eprintln!("skipped ({why}): {}", path.display());
        }
        println!("{verb} {} worktree(s)", reaped.len());
    }
    Ok(())
}

type GcOutcome = (Vec<PathBuf>, Vec<(PathBuf, &'static str)>);

/// Core reaping logic, separated from output for testability. Returns the
/// worktrees reaped (or that would be, under `--dry-run`) and those skipped.
fn run_gc(repo: &RepoContext, args: &WorktreeGc) -> Result<GcOutcome> {
    let older_than = parse_duration(&args.older_than)?;
    let mut reaped: Vec<PathBuf> = Vec::new();
    let mut skipped: Vec<(PathBuf, &'static str)> = Vec::new();

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
        if args.ephemeral && !is_ephemeral(&entry.path) {
            continue;
        }
        if worktree_idle(&entry.path) < older_than {
            continue;
        }
        if !args.force && worktree_dirty(&entry.path).unwrap_or(false) {
            skipped.push((entry.path.clone(), "dirty"));
            continue;
        }
        if args.dry_run {
            reaped.push(entry.path);
            continue;
        }
        match force_teardown(repo, &entry.path) {
            Ok(()) => reaped.push(entry.path),
            Err(_) => skipped.push((entry.path, "remove-failed")),
        }
    }

    if !args.dry_run {
        run_git_common(repo, [OsStr::new("worktree"), OsStr::new("prune")])?;
    }
    Ok((reaped, skipped))
}

/// A linked worktree as reported by `git worktree list --porcelain`.
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
    is_main: bool,
}

fn list_worktrees(repo: &RepoContext) -> Result<Vec<WorktreeEntry>> {
    let output = git_output_common(repo, ["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in text.lines() {
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

fn is_ephemeral(worktree: &Path) -> bool {
    worktree_admin_dir(worktree)
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
    let seconds = match unit {
        "s" | "" => count,
        "m" => count * 60,
        "h" => count * 3600,
        "d" => count * 86400,
        other => bail!("invalid duration unit '{other}' (use s, m, h, or d)"),
    };
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
        bail!("cannot resolve base commit '{base}'");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn default_worktree_path(common_git_dir: &Path, branch: &str) -> PathBuf {
    common_git_dir
        .join("simgit")
        .join("worktrees")
        .join(safe_path_component(branch))
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

fn cow_clone_supported(common_git_dir: &Path, destination_dir: &Path) -> Result<bool> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (common_git_dir, destination_dir);
        return Ok(false);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let probe = state_dir(common_git_dir).join("clone-probes");
        fs::create_dir_all(&probe)?;
        let token = Uuid::new_v4().to_string();
        let source = probe.join(format!("{token}.source"));
        let destination = destination_dir.join(format!(".simgit-clone-probe-{token}"));
        fs::write(&source, b"simgit-cow-probe")?;
        let status = clone_file_command(&source, &destination)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&destination);
        Ok(status.map(|status| status.success()).unwrap_or(false))
    }
}

#[cfg(target_os = "macos")]
fn clone_file_command(source: &Path, destination: &Path) -> Command {
    let mut command = Command::new("cp");
    command.args([
        OsStr::new("-c"),
        source.as_os_str(),
        destination.as_os_str(),
    ]);
    command
}

#[cfg(target_os = "linux")]
fn clone_file_command(source: &Path, destination: &Path) -> Command {
    let mut command = Command::new("cp");
    command.args([
        OsStr::new("--reflink=always"),
        source.as_os_str(),
        destination.as_os_str(),
    ]);
    command
}

fn ensure_baseline(repo: &RepoContext, commit: &str) -> Result<PathBuf> {
    let root = state_dir(&repo.common_git_dir).join("baselines");
    let final_dir = root.join(commit);
    let final_tree = final_dir.join("tree");
    let ready = final_dir.join("ready");
    if ready.is_file() && final_tree.is_dir() {
        touch(&ready)?;
        let _ = prune_baselines(&repo.common_git_dir, false);
        return Ok(final_tree);
    }

    fs::create_dir_all(&root)?;
    if final_dir.exists() {
        fs::remove_dir_all(&final_dir).context("remove incomplete cached baseline")?;
    }
    let temporary = root.join(format!(".{commit}.{}", Uuid::new_v4()));
    let cache = temporary.join("cache");
    let temporary_tree = cache.join("tree");
    fs::create_dir_all(&temporary_tree)?;
    if let Err(error) = materialize_baseline(repo, commit, &temporary_tree) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    fs::write(cache.join("ready"), commit)?;

    match fs::rename(&cache, &final_dir) {
        Ok(()) => {}
        Err(_) if ready.is_file() && final_tree.is_dir() => {
            let _ = fs::remove_dir_all(&temporary);
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error).with_context(|| format!("publish baseline {commit}"));
        }
    }
    let _ = fs::remove_dir_all(&temporary);
    let _ = prune_baselines(&repo.common_git_dir, false);
    Ok(final_tree)
}

fn materialize_baseline(repo: &RepoContext, commit: &str, destination: &Path) -> Result<()> {
    let index = destination
        .parent()
        .context("baseline destination has no parent")?
        .join("index");
    let mut read_tree = Command::new("git");
    read_tree
        .env("GIT_INDEX_FILE", &index)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["read-tree", commit]);
    run_command(&mut read_tree, "initialize baseline index")?;

    let mut checkout = Command::new("git");
    checkout
        .env("GIT_INDEX_FILE", &index)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .arg(format!("--work-tree={}", destination.display()))
        .args(["checkout-index", "--all", "--force"]);
    let result = run_command(
        &mut checkout,
        "materialize baseline with Git checkout-index",
    );
    let _ = fs::remove_file(index);
    result
}

fn clone_tree(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("cp");
        command.args(["-c", "-R"]);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("cp");
        command.args(["--reflink=always", "-R"]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("CoW tree cloning is not supported on this platform");

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        command.arg(source.join(".")).arg(destination);
        run_command(&mut command, "copy-on-write tree clone")
    }
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

fn rollback_created_worktree(repo: &RepoContext, target: &Path, branch: &str) {
    let _ = remove_worktree_force(repo, target);
    let _ = Command::new("git")
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["branch", "-D", branch])
        .status();
}

fn remove_worktree_force(repo: &RepoContext, target: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["worktree", "remove", "--force"])
        .arg(target);
    run_command(&mut command, "git worktree remove --force")
}

fn prune_baselines(common_git_dir: &Path, all: bool) -> Result<usize> {
    let root = state_dir(common_git_dir).join("baselines");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("read baseline cache"),
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let age_source = if path.join("ready").is_file() {
            path.join("ready")
        } else {
            path.clone()
        };
        let old = age_source
            .metadata()?
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age >= BASELINE_MAX_AGE)
            .unwrap_or(false);
        if all || old {
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
            removed += 1;
        }
    }
    Ok(removed)
}

fn touch(path: &Path) -> Result<()> {
    let contents = fs::read(path)?;
    fs::write(path, contents)?;
    Ok(())
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

fn run_command(command: &mut Command, description: &str) -> Result<()> {
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
mod tests {
    use super::*;

    #[test]
    fn branch_names_become_single_safe_path_components() {
        assert_eq!(safe_path_component("feat/auth"), "feat-auth");
        assert_eq!(safe_path_component("../../escape"), "..-..-escape");
        assert_eq!(safe_path_component(".."), "branch-..");
    }

    #[test]
    fn parse_duration_accepts_common_units() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("5w").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn gc_reaps_ephemeral_and_by_prefix_but_spares_others() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;

        let eph = fixture.root.join("eph");
        add_git_worktree(&repo, "exp/eph", &eph, &base)?;
        mark_ephemeral(&eph)?;
        let keep = fixture.root.join("keep");
        add_git_worktree(&repo, "exp/keep", &keep, &base)?;
        let other = fixture.root.join("other");
        add_git_worktree(&repo, "feat/other", &other, &base)?;
        mark_ephemeral(&other)?;

        // ephemeral + prefix `exp`: only exp/eph qualifies (keep is not
        // ephemeral; other is ephemeral but wrong prefix).
        let args = WorktreeGc {
            ephemeral: true,
            prefix: Some("exp".to_owned()),
            older_than: "0s".to_owned(),
            ..Default::default()
        };
        let (reaped, skipped) = run_gc(&repo, &args)?;
        assert_eq!(reaped, vec![eph.clone()]);
        assert!(skipped.is_empty());

        let remaining = list_worktrees(&repo)?;
        let branches: Vec<_> = remaining.iter().filter_map(|w| w.branch.clone()).collect();
        assert!(branches.iter().any(|b| b == "refs/heads/exp/keep"));
        assert!(branches.iter().any(|b| b == "refs/heads/feat/other"));
        assert!(!branches.iter().any(|b| b == "refs/heads/exp/eph"));
        Ok(())
    }

    #[test]
    fn overlay_marker_round_trips_through_admin_dir() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let wt = fixture.root.join("wt");
        add_git_worktree(&repo, "wt", &wt, &base)?;

        assert!(overlay_state(&wt).is_none());
        let overlay_dir = fixture.root.join("overlays").join("abc");
        fs::create_dir_all(&overlay_dir)?;
        write_overlay_marker(&wt, &overlay_dir)?;
        assert_eq!(overlay_state(&wt), Some(overlay_dir));
        Ok(())
    }

    #[test]
    fn gc_skips_dirty_worktrees_without_force() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;

        let dirty = fixture.root.join("dirty");
        add_git_worktree(&repo, "dirty", &dirty, &base)?;
        fs::write(dirty.join("scratch.txt"), "uncommitted")?;

        let args = WorktreeGc {
            older_than: "0s".to_owned(),
            ..Default::default()
        };
        let (reaped, skipped) = run_gc(&repo, &args)?;
        assert!(reaped.is_empty());
        assert_eq!(skipped, vec![(dirty.clone(), "dirty")]);

        // With --force it is reaped despite the dirty state.
        let forced = WorktreeGc {
            older_than: "0s".to_owned(),
            force: true,
            ..Default::default()
        };
        let (reaped, _) = run_gc(&repo, &forced)?;
        assert_eq!(reaped, vec![dirty]);
        Ok(())
    }

    #[test]
    fn native_worktree_fallback_is_clean_and_registered() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let target = fixture.root.join("fallback");
        let base = resolve_commit(&repo, "HEAD")?;
        add_git_worktree(&repo, "fallback-test", &target, &base)?;
        ensure_clean(&target)?;
        let listed = git_output_common(&repo, ["worktree", "list", "--porcelain"])?;
        assert!(String::from_utf8_lossy(&listed.stdout).contains(target.to_string_lossy().as_ref()));
        Ok(())
    }

    #[test]
    fn cow_worktree_is_clean_when_filesystem_supports_clones() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        if !cow_clone_supported(&repo.common_git_dir, &fixture.root)? {
            return Ok(());
        }
        let target = fixture.root.join("cow");
        let base = resolve_commit(&repo, "HEAD")?;
        add_cow_worktree(&repo, "cow-test", &target, &base)?;
        ensure_clean(&target)?;
        assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
        assert_eq!(
            fs::read_to_string(target.join("archive-excluded.txt"))?,
            "still part of a checkout\n"
        );
        Ok(())
    }

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let root =
                std::env::temp_dir().join(format!("simgit-worktree-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&root)?;
            // Canonicalize so paths match what `git worktree list` reports
            // (macOS resolves the `/var` -> `/private/var` symlink).
            let root = root.canonicalize()?;
            let repo = root.join("repo");
            fs::create_dir_all(&repo)?;
            git(&repo, ["init", "-q"])?;
            git(&repo, ["config", "user.email", "test@example.com"])?;
            git(&repo, ["config", "user.name", "Test User"])?;
            fs::write(repo.join("file.txt"), "content\n")?;
            fs::write(
                repo.join(".gitattributes"),
                "archive-excluded.txt export-ignore\n",
            )?;
            fs::write(
                repo.join("archive-excluded.txt"),
                "still part of a checkout\n",
            )?;
            git(&repo, ["add", "."])?;
            git(&repo, ["commit", "-q", "-m", "initial"])?;
            Ok(Self { root, repo })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn git<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
        let mut command = Command::new("git");
        command.arg("-C").arg(path).args(args);
        run_command(&mut command, "test git")
    }
}
