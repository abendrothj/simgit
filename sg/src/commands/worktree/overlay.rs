use super::{run_command, state_dir, RepoContext};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const OVERLAY_MARKER: &str = "simgit-overlay";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct State {
    pub(super) overlay_dir: PathBuf,
    pub(super) lower: Option<PathBuf>,
}

pub(super) fn supported() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("fuse-overlayfs").is_file()))
        .unwrap_or(false)
}

pub(super) fn root(common_git_dir: &Path) -> PathBuf {
    state_dir(common_git_dir).join("overlays")
}

pub(super) fn mount(lower: &Path, upper: &Path, work: &Path, mountpoint: &Path) -> Result<()> {
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

/// Best-effort unmount. An already-unmounted path is a successful no-op.
pub(super) fn unmount(mountpoint: &Path) {
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

pub(super) fn write_marker(admin: &Path, state: &State) -> Result<()> {
    let value = json!({
        "overlay_dir": state.overlay_dir.display().to_string(),
        "lower": state.lower.as_ref().map(|path| path.display().to_string()),
    });
    fs::write(admin.join(OVERLAY_MARKER), serde_json::to_vec(&value)?)
        .context("write overlay marker")
}

pub(super) fn state(repo: &RepoContext, worktree: &Path) -> Option<State> {
    let admin = admin_dir(repo, worktree)?;
    read_state(&admin)
}

fn read_state(admin: &Path) -> Option<State> {
    let content = fs::read_to_string(admin.join(OVERLAY_MARKER)).ok()?;
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
        let overlay_dir = PathBuf::from(value.get("overlay_dir")?.as_str()?);
        let lower = value
            .get("lower")
            .and_then(|value| value.as_str())
            .map(PathBuf::from);
        return Some(State { overlay_dir, lower });
    }

    // Backward compatibility for v0.1.2 markers, which stored only the state
    // directory. They can be cleaned up but cannot be remounted automatically.
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| State {
        overlay_dir: PathBuf::from(trimmed),
        lower: None,
    })
}

/// Enumerate overlay registrations directly from Git's common admin directory.
/// Unlike `git worktree list`, this retains entries whose mountpoint currently
/// lacks its `.git` file after a reboot or manual unmount.
pub(super) fn registrations(repo: &RepoContext) -> Vec<(PathBuf, State)> {
    let mut registrations = Vec::new();
    let Ok(entries) = fs::read_dir(repo.common_git_dir.join("worktrees")) else {
        return registrations;
    };
    for entry in entries.flatten() {
        let admin = entry.path();
        let Some(state) = read_state(&admin) else {
            continue;
        };
        let Ok(gitdir) = fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        let gitdir = PathBuf::from(gitdir.trim());
        if let Some(worktree) = gitdir.parent() {
            registrations.push((worktree.to_path_buf(), state));
        }
    }
    registrations
}

pub(super) fn worktree_for_branch(repo: &RepoContext, branch: &str) -> Option<PathBuf> {
    let wanted = format!("ref: refs/heads/{branch}");
    let entries = fs::read_dir(repo.common_git_dir.join("worktrees")).ok()?;
    for entry in entries.flatten() {
        let admin = entry.path();
        if read_state(&admin).is_none() {
            continue;
        }
        let Ok(head) = fs::read_to_string(admin.join("HEAD")) else {
            continue;
        };
        if head.trim() != wanted {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        if let Some(worktree) = PathBuf::from(gitdir.trim()).parent() {
            return Some(worktree.to_path_buf());
        }
    }
    None
}

pub(super) fn branch(repo: &RepoContext, worktree: &Path) -> Option<String> {
    let admin = admin_dir(repo, worktree)?;
    let head = fs::read_to_string(admin.join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: ").map(str::to_owned)
}

pub(super) fn admin_dir(repo: &RepoContext, worktree: &Path) -> Option<PathBuf> {
    let live = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| PathBuf::from(path.trim()));
    if live.is_some() {
        return live;
    }

    let registrations = repo.common_git_dir.join("worktrees");
    for entry in fs::read_dir(registrations).ok()?.flatten() {
        let Ok(gitdir) = fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        let registered = PathBuf::from(gitdir.trim());
        if registered.parent() == Some(worktree) {
            return Some(entry.path());
        }
    }
    None
}

pub(super) fn is_mounted(worktree: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let wanted = worktree
            .canonicalize()
            .unwrap_or_else(|_| worktree.to_path_buf());
        fs::read_to_string("/proc/self/mountinfo")
            .map(|mounts| {
                mounts.lines().any(|line| {
                    line.split_whitespace()
                        .nth(4)
                        .map(decode_mount_path)
                        .is_some_and(|path| path == wanted)
                })
            })
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    {
        worktree.join(".git").is_file()
    }
}

#[cfg(target_os = "linux")]
fn decode_mount_path(encoded: &str) -> PathBuf {
    PathBuf::from(
        encoded
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\"),
    )
}

pub(super) fn repair(repo: &RepoContext, worktree: &Path) -> Result<bool> {
    let Some(state) = state(repo, worktree) else {
        return Ok(false);
    };
    if is_mounted(worktree) {
        return Ok(false);
    }
    let lower = state.lower.context(
        "overlay was created by v0.1.2 and lacks recovery metadata; remove and recreate it",
    )?;
    if !lower.is_dir() {
        bail!("overlay baseline is missing: {}", lower.display());
    }
    let upper = state.overlay_dir.join("upper");
    let work = state.overlay_dir.join("work");
    if !upper.is_dir() || !work.is_dir() {
        bail!(
            "overlay state is incomplete: {}",
            state.overlay_dir.display()
        );
    }
    fs::create_dir_all(worktree)?;
    mount(&lower, &upper, &work, worktree)?;
    if !worktree.join(".git").is_file() {
        unmount(worktree);
        bail!("repaired overlay does not expose its Git worktree metadata");
    }
    Ok(true)
}
