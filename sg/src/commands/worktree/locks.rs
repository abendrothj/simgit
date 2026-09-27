use super::{
    discover_repo, emit, lookup_worktree_target, overlay, remove_file_if_present, state_dir,
    worktree_admin_dir, RepoContext, TargetLookup,
};
use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(super) fn worktree_lock_path(repo: &RepoContext, target: &Path) -> Result<PathBuf> {
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

pub(super) fn ensure_unlocked(repo: &RepoContext, target: &Path) -> Result<()> {
    if worktree_lock_path(repo, target).is_ok_and(|path| path.exists()) {
        bail!(
            "worktree is locked (a command may be running): {}",
            target.display()
        );
    }
    Ok(())
}

pub(super) struct WorktreeLock {
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
pub(super) fn lock_owner_pid(contents: &str) -> Option<i32> {
    let rest = contents.split_once("(pid ")?.1;
    rest.split_once(')')?.0.trim().parse().ok()
}

pub(super) fn lock_owner_of(path: &Path) -> Option<i32> {
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
pub(super) fn process_alive(pid: i32) -> bool {
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
    pub(super) fn acquire(repo: &RepoContext, target: &Path) -> Result<Self> {
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

    pub(super) fn release(mut self) -> Result<()> {
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
pub(super) struct PathClaim {
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
    pub(super) fn acquire(repo: &RepoContext, target: &Path) -> Result<Self> {
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
        TargetLookup::Stray(path) => (path.display().to_string(), None),
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
