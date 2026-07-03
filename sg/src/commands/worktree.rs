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
    /// Remove a linked worktree.
    Remove(WorktreeRemove),
    /// List linked worktrees using Git's native registry.
    List(WorktreeList),
    /// Prune stale Git registrations and old cached baselines.
    Prune(WorktreePrune),
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
}

#[derive(Args)]
pub struct WorktreeRemove {
    /// Worktree path. Defaults to the worktree containing the current directory.
    pub target: Option<PathBuf>,

    /// Commit all changes before removing the worktree.
    #[arg(long)]
    pub commit: bool,

    /// Commit message for --commit.
    #[arg(short, long, default_value = "simgit worktree remove")]
    pub message: String,

    /// Discard uncommitted changes. Without this flag, Git refuses dirty removal.
    #[arg(long, conflicts_with = "commit")]
    pub force: bool,
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

#[derive(Debug)]
struct RepoContext {
    top_level: PathBuf,
    common_git_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PopulateMode {
    CowClone,
    GitCheckout,
}

impl PopulateMode {
    fn label(self) -> &'static str {
        match self {
            Self::CowClone => "cow-clone",
            Self::GitCheckout => "git-checkout",
        }
    }
}

pub fn run(cmd: Worktree, global_json: bool) -> Result<()> {
    match cmd {
        Worktree::Add(args) => add(args),
        Worktree::Remove(args) => remove(args),
        Worktree::List(args) => list(args.json || global_json),
        Worktree::Prune(args) => prune(args),
    }
}

fn add(args: WorktreeAdd) -> Result<()> {
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
    let mode = if cow_clone_supported(&repo.common_git_dir, target_parent)? {
        PopulateMode::CowClone
    } else if args.require_cow {
        bail!(
            "filesystem CoW cloning is unavailable; omit --require-cow to use a normal Git checkout"
        );
    } else {
        PopulateMode::GitCheckout
    };

    match mode {
        PopulateMode::CowClone => add_cow_worktree(&repo, &args.branch, &target, &base)?,
        PopulateMode::GitCheckout => add_git_worktree(&repo, &args.branch, &target, &base)?,
    }

    eprintln!("mode: {}", mode.label());
    println!("{}", target.display());
    Ok(())
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

fn remove(args: WorktreeRemove) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    let target = match args.target {
        Some(path) => absolute_path(path)?,
        None => repo.top_level.clone(),
    };

    if args.commit {
        run_git_at(&target, ["add", "-A"]).context("stage worktree changes")?;
        let diff = git_output_at(&target, ["diff", "--cached", "--quiet"])?;
        match diff.status.code() {
            Some(0) => eprintln!("no changes to commit"),
            Some(1) => {
                run_git_at(&target, ["commit", "-m", &args.message])
                    .context("commit worktree changes")?;
            }
            _ => return Err(git_failure("git diff --cached --quiet", &diff)),
        }
    }

    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["worktree", "remove"]);
    if args.force {
        command.arg("--force");
    }
    command.arg(&target);
    run_command(&mut command, "git worktree remove")?;
    println!("{}", target.display());
    Ok(())
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
