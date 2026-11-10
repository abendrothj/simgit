//! Fixtures shared by the worktree unit tests.

use super::add::{
    allocate_cow_worktree, allocate_git_worktree, release_allocation_lock, WorktreeKind,
};
use super::repo::{git_output_at, git_output_common, run_command};
use super::RepoContext;
use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

/// A checkout-backed allocation through to its unlock, as `create_worktree`
/// finishes it, without the markers.
pub(super) fn add_git_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
    sparse: &[String],
) -> Result<()> {
    allocate_git_worktree(repo, branch, target, base, kind, sparse)?;
    release_allocation_lock(repo, target)
}

/// `add_git_worktree` for the CoW backend.
pub(super) fn add_cow_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
    sparse: &[String],
) -> Result<()> {
    allocate_cow_worktree(repo, branch, target, base, kind, sparse)?;
    release_allocation_lock(repo, target)
}

pub(super) fn resolve_commit_at(worktree: &Path) -> Result<String> {
    let output = git_output_at(worktree, ["rev-parse", "HEAD"])?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub(super) fn branch_exists(repo: &RepoContext, branch: &str) -> Result<bool> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output_common(repo, ["show-ref", "--verify", "--quiet", &reference])?;
    Ok(output.status.success())
}

/// Threads that keep spawning children, as parallel tests and a busy
/// machine do. A child forked while a claim's file is open keeps its lock
/// until it execs, even after the claim is dropped.
pub(super) struct SpawnLoad {
    stop: Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl SpawnLoad {
    pub(super) fn start() -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threads = (0..6)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = std::process::Command::new("true").status();
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for SpawnLoad {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

pub(super) struct Fixture {
    pub(super) root: PathBuf,
    pub(super) repo: PathBuf,
}

/// Keep the developer's Git configuration out of every `git` these tests run,
/// including the ones the code under test starts: a global `init.defaultBranch`
/// or `commit.gpgsign` must not change a result. The variables are process
/// wide because the code under test inherits its environment; each is set to
/// the same value by every test, and std serializes `set_var` against spawns.
pub(crate) fn isolate_git_config() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("HOME", std::env::temp_dir().join("simgit-test-no-home"));
        std::env::remove_var("XDG_CONFIG_HOME");
    });
}

impl Fixture {
    pub(super) fn new() -> Result<Self> {
        isolate_git_config();
        let root = std::env::temp_dir().join(format!("simgit-worktree-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let repo = root.join("repo");
        fs::create_dir_all(&repo)?;
        git(&repo, ["init", "-q"])?;
        git(&repo, ["config", "user.email", "test@example.com"])?;
        git(&repo, ["config", "user.name", "Test User"])?;
        git(
            &repo,
            [
                "config",
                "filter.simgit.clean",
                "sed 's/^smudged:/stored:/'",
            ],
        )?;
        git(
            &repo,
            [
                "config",
                "filter.simgit.smudge",
                "sed 's/^stored:/smudged:/'",
            ],
        )?;
        fs::write(repo.join("file.txt"), "content\n")?;
        fs::write(repo.join("file with spaces.txt"), "spaces\n")?;
        fs::write(repo.join("filtered.txt"), "smudged:content\n")?;
        fs::write(
            repo.join(".gitattributes"),
            "archive-excluded.txt export-ignore\nfiltered.txt filter=simgit\n",
        )?;
        fs::write(
            repo.join("archive-excluded.txt"),
            "still part of a checkout\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::os::unix::fs::symlink("file.txt", repo.join("file-link"))?;
            fs::write(repo.join("tool.sh"), "#!/bin/sh\n")?;
            fs::set_permissions(repo.join("tool.sh"), fs::Permissions::from_mode(0o755))?;
        }
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

pub(super) fn git<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
    isolate_git_config();
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(args);
    run_command(&mut command, "test git")
}

/// Every file under `dir`, at any depth.
pub(super) fn files_under(dir: &Path) -> Result<HashSet<PathBuf>> {
    let mut files = HashSet::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(files_under(&path)?);
        } else {
            files.insert(path);
        }
    }
    Ok(files)
}

/// Which file is at `path`, if any.
pub(super) fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}
