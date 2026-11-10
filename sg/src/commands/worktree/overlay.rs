use super::registry::{admin_head, AdminHead};
use super::repo::{canonical_path, run_command, state_dir};
use super::RepoContext;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

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

/// Refuse teardown until the mount table confirms the overlay is detached.
pub(super) fn unmount(mountpoint: &Path) -> Result<()> {
    if !is_mounted(mountpoint) {
        return Ok(());
    }
    for tool in ["fusermount3", "fusermount", "umount"] {
        let mut command = Command::new(tool);
        if tool != "umount" {
            command.arg("-u");
        }
        let _ = command
            .arg(mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !is_mounted(mountpoint) {
            return Ok(());
        }
    }
    bail!(
        "could not unmount {}; files and overlay state retained",
        mountpoint.display()
    )
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

pub(super) fn read_state(admin: &Path) -> Option<State> {
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
        if let Some(worktree) = resolve_gitdir(&admin, &gitdir).parent() {
            registrations.push((worktree.to_path_buf(), state));
        }
    }
    registrations
}

pub(super) fn worktree_for_branch(repo: &RepoContext, branch: &str) -> Option<PathBuf> {
    let wanted = format!("refs/heads/{branch}");
    let entries = fs::read_dir(repo.common_git_dir.join("worktrees")).ok()?;
    for entry in entries.flatten() {
        let admin = entry.path();
        if read_state(&admin).is_none() {
            continue;
        }
        if !matches!(admin_head(repo, &admin), Ok(AdminHead::Branch(head)) if head == wanted) {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        if let Some(worktree) = resolve_gitdir(&admin, &gitdir).parent() {
            return Some(worktree.to_path_buf());
        }
    }
    None
}

pub(super) fn branch(repo: &RepoContext, worktree: &Path) -> Option<String> {
    let admin = admin_dir(repo, worktree)?;
    match admin_head(repo, &admin) {
        Ok(AdminHead::Branch(head)) => Some(head),
        _ => None,
    }
}

pub(super) fn admin_dir(repo: &RepoContext, worktree: &Path) -> Option<PathBuf> {
    // Without its `.git`, git walks up from `worktree` and answers for whatever
    // repository encloses it; only the registry knows this worktree then.
    let live = live_git_dir(worktree);
    // When `worktree` is an empty, unmounted mountpoint nested inside the
    // common git dir (e.g. `.git/simgit/worktrees/<name>`), git's directory
    // discovery walks past it and finds the outer repository instead. Reject
    // that escape rather than mistaking the whole repo for this worktree's
    // admin dir.
    if let Some(live) = &live {
        if live != &repo.common_git_dir {
            return Some(live.clone());
        }
    }

    let registrations = repo.common_git_dir.join("worktrees");
    let worktree = canonical_path(worktree);
    for entry in fs::read_dir(registrations).ok()?.flatten() {
        let Ok(gitdir) = fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        if resolve_gitdir(&entry.path(), &gitdir).parent() == Some(worktree.as_path()) {
            return Some(entry.path());
        }
    }
    None
}

/// The git dir a worktree's own `.git` names, as Git reads it: a directory
/// is the git dir itself, and a file says `gitdir: <path>`, relative to the
/// worktree when Git wrote it so. `None` when there is none to find, or it
/// names nothing. Reading it costs no `git`, which every command used to
/// spawn several times to learn this.
pub(super) fn live_git_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let recorded = fs::read_to_string(&dot_git).ok()?;
        let named = recorded.lines().next()?.strip_prefix("gitdir: ")?.trim();
        worktree.join(named)
    };
    git_dir.is_dir().then(|| canonical_path(&git_dir))
}

/// The `.git` path an admin dir's `gitdir` file records, in canonical form,
/// as Git compares it. Git writes it relative to the admin dir under
/// `worktree.useRelativePaths`.
pub(super) fn resolve_gitdir(admin: &Path, recorded: &str) -> PathBuf {
    canonical_path(&admin.join(recorded.trim()))
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
            // Failure to inspect mounts must never authorize deleting a live view.
            .unwrap_or(true)
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
    let upper = state.overlay_dir.join("upper");
    if is_mounted(worktree) && upper_visible(&upper, worktree) {
        return Ok(false);
    }
    if is_mounted(worktree) {
        // FUSE can leave a disconnected mount-table entry after an interrupted
        // or lazy unmount. Detach that stale entry before mounting the saved
        // upperdir again.
        unmount(worktree)?;
        for _ in 0..40 {
            if !is_mounted(worktree) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if is_mounted(worktree) {
            bail!(
                "stale overlay mount could not be detached: {}",
                worktree.display()
            );
        }
    }
    let lower = state.lower.context(
        "overlay was created by v0.1.2 and lacks recovery metadata; remove and recreate it",
    )?;
    if !lower.is_dir() {
        bail!("overlay baseline is missing: {}", lower.display());
    }
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
        unmount(worktree)?;
        bail!("repaired overlay does not expose its Git worktree metadata");
    }
    Ok(true)
}

/// Compare the two views exactly without allocating for the size of each edit.
/// `run` checks this on every overlay reuse, even for large upperdir files.
fn files_match(upper: &Path, view: &Path) -> std::io::Result<bool> {
    let mut expected = fs::File::open(upper)?;
    let mut actual = fs::File::open(view)?;
    let mut expected_bytes = [0_u8; 16 * 1024];
    let mut actual_bytes = [0_u8; 16 * 1024];
    loop {
        let size = expected.read(&mut expected_bytes)?;
        if size == 0 {
            return Ok(actual.read(&mut actual_bytes[..1])? == 0);
        }
        actual.read_exact(&mut actual_bytes[..size])?;
        if expected_bytes[..size] != actual_bytes[..size] {
            return Ok(false);
        }
    }
}

/// A FUSE mount can remain listed after its userspace connection is gone. A
/// healthy overlay must expose every normal upperdir entry through the merged
/// view. Overlay whiteout/special entries are intentionally ignored here.
pub(super) fn upper_visible(upper: &Path, view: &Path) -> bool {
    let Ok(entries) = fs::read_dir(upper) else {
        return false;
    };
    for entry in entries.flatten() {
        let upper_path = entry.path();
        let view_path = view.join(entry.file_name());
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        if kind.is_dir() {
            if !view_path.is_dir() || !upper_visible(&upper_path, &view_path) {
                return false;
            }
        } else if kind.is_file() {
            if !files_match(&upper_path, &view_path).unwrap_or(false) {
                return false;
            }
        } else if kind.is_symlink() {
            let matches = fs::read_link(&upper_path)
                .and_then(|expected| fs::read_link(&view_path).map(|actual| actual == expected))
                .unwrap_or(false);
            if !matches {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktree::add::WorktreeKind;
    use crate::commands::worktree::registry::worktree_admin_dir;
    use crate::commands::worktree::repo::{discover_repo, resolve_commit};
    use crate::commands::worktree::test_support::{add_git_worktree, Fixture};
    use anyhow::Result;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn overlay_marker_round_trips_through_common_admin_dir() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let worktree = fixture.root.join("wt");
        add_git_worktree(&repo, "wt", &worktree, &base, WorktreeKind::NewBranch, &[])?;
        assert!(state(&repo, &worktree).is_none());

        let state = State {
            overlay_dir: fixture.root.join("overlays/abc"),
            lower: Some(fixture.root.join("baseline")),
        };
        write_marker(&worktree_admin_dir(&worktree)?, &state)?;
        assert_eq!(super::state(&repo, &worktree), Some(state));

        let saved_gitlink = fixture.root.join("saved-gitlink");
        fs::rename(worktree.join(".git"), &saved_gitlink)?;
        let registrations = registrations(&repo);
        assert!(registrations.iter().any(|(path, _)| path == &worktree));
        assert_eq!(branch(&repo, &worktree).as_deref(), Some("refs/heads/wt"));
        assert_eq!(worktree_for_branch(&repo, "wt"), Some(worktree.clone()));
        fs::rename(saved_gitlink, worktree.join(".git"))?;
        Ok(())
    }

    #[test]
    fn admin_dir_does_not_escape_to_the_common_git_dir_when_unmounted() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        // Mirrors production overlay worktrees, whose mountpoint lives inside the
        // common git dir itself (e.g. `.git/simgit/worktrees/<name>`).
        let worktree = repo.common_git_dir.join("simgit/worktrees/nested");
        add_git_worktree(
            &repo,
            "nested",
            &worktree,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let admin = admin_dir(&repo, &worktree).expect("admin dir while mounted");
        assert_ne!(admin, repo.common_git_dir);

        // An unmounted overlay exposes an empty mountpoint. Git's directory
        // discovery would otherwise climb past it and find the outer repo.
        fs::remove_file(worktree.join(".git"))?;
        assert_eq!(admin_dir(&repo, &worktree), Some(admin));
        Ok(())
    }

    #[test]
    fn overlay_health_requires_the_view_to_reflect_upperdir_data() -> Result<()> {
        let root = std::env::temp_dir().join(format!("simgit-overlay-health-{}", Uuid::new_v4()));
        let upper = root.join("upper");
        let view = root.join("view");
        fs::create_dir_all(upper.join("nested"))?;
        fs::create_dir_all(view.join("nested"))?;
        fs::write(upper.join("nested/result.txt"), "agent result")?;
        fs::write(view.join("nested/result.txt"), "agent result")?;
        assert!(upper_visible(&upper, &view));
        fs::write(view.join("nested/result.txt"), "stale result")?;
        assert!(!upper_visible(&upper, &view));
        let original = vec![b'x'; 32 * 1024 + 1];
        fs::write(upper.join("nested/result.txt"), &original)?;
        fs::write(view.join("nested/result.txt"), &original)?;
        assert!(upper_visible(&upper, &view));

        let mut changed = original.clone();
        changed[16 * 1024 + 1] = b'y';
        fs::write(view.join("nested/result.txt"), &changed)?;
        assert!(!upper_visible(&upper, &view));
        fs::write(
            view.join("nested/result.txt"),
            &original[..original.len() - 1],
        )?;
        assert!(!upper_visible(&upper, &view));
        changed = original;
        changed.push(b'y');
        fs::write(view.join("nested/result.txt"), &changed)?;
        assert!(!upper_visible(&upper, &view));
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
