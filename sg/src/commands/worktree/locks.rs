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

/// Refuse a locked worktree, and one whose lock cannot be found: an unknown
/// lock state is not permission to tear anything down.
pub(super) fn ensure_unlocked(repo: &RepoContext, target: &Path) -> Result<()> {
    let path = worktree_lock_path(repo, target).map_err(|cause| {
        anyhow::anyhow!(
            "cannot tell whether {} is locked: {cause:#}",
            target.display()
        )
    })?;
    if !lock_present(&path) {
        return Ok(());
    }
    match fs::read_to_string(&path) {
        Ok(contents) if !is_simgit_lock(&contents) => bail!(
            "worktree is locked by Git{}: {}\n\
             release it with: git worktree unlock {}",
            git_lock_reason(&contents),
            target.display(),
            target.display()
        ),
        Ok(_) => bail!(
            "worktree is locked (a command may be running): {}",
            target.display()
        ),
        Err(error) => Err(unreadable_lock(&path, &error)),
    }
}

/// Whether `target` is locked, `None` when its lock cannot be found: `list`
/// then shows nothing and `gc` skips it, where `ensure_unlocked` refuses.
pub(super) fn lock_state(repo: &RepoContext, target: &Path) -> Option<bool> {
    worktree_lock_path(repo, target)
        .ok()
        .map(|path| lock_present(&path))
}

/// A lock that is there but cannot be read, such as a symlink to nothing.
/// Git counts it as a lock, and its own `worktree unlock` dies reading it too.
fn unreadable_lock(path: &Path, error: &std::io::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "worktree is locked, but its lock {} cannot be read: {error}\n\
         Git cannot read or clear it either; if nothing holds it, remove it with: rm {}",
        path.display(),
        path.display()
    )
}

/// Whether a lock file is there. Like Git, this asks about the entry, not
/// what it links to: a dangling symlink is a lock. An entry that cannot be
/// checked counts as one.
pub(super) fn lock_present(path: &Path) -> bool {
    !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
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

/// Git's lock reason for a worktree that is still being allocated. It names
/// the allocator in the shape `lock_owner_pid` reads, so `unlock` clears it
/// once a killed allocator is gone and refuses while it runs.
pub(super) fn allocation_lock_reason() -> String {
    format!("simgit: allocating (pid {})", std::process::id())
}

/// True for a lock simgit wrote: `run` and allocation locks all start with
/// `simgit:`. Anything else was set with `git worktree lock` and guards
/// something simgit cannot see, such as a worktree on removable media.
fn is_simgit_lock(contents: &str) -> bool {
    contents.starts_with("simgit:")
}

/// A Git lock's reason as a message suffix, empty when it was set without one.
fn git_lock_reason(contents: &str) -> String {
    match contents.trim() {
        "" => String::new(),
        reason => format!(" (reason: {reason})"),
    }
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

/// The process named by a temporary file's `<pid>.<uuid>` token.
pub(super) fn temp_owner(token: &str) -> Option<i32> {
    let (pid, uuid) = token.split_once('.')?;
    Uuid::parse_str(uuid).ok()?;
    pid.parse().ok().filter(|pid| *pid > 0)
}

/// Remove the pending files of `WorktreeLock::acquire` calls killed before
/// they could: `<lock>.<pid>.<uuid>.pending` beside each lock. A live
/// process's are kept.
pub(super) fn remove_dead_pending(repo: &RepoContext) {
    let mut dirs = vec![(repo.common_git_dir.clone(), "simgit-run.lock.")];
    if let Ok(admins) = fs::read_dir(repo.common_git_dir.join("worktrees")) {
        dirs.extend(admins.flatten().map(|admin| (admin.path(), "locked.")));
    }
    for (dir, lock) in dirs {
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let token = name
                .to_str()
                .and_then(|name| name.strip_prefix(lock))
                .and_then(|name| name.strip_suffix(".pending"));
            if token
                .and_then(temp_owner)
                .is_some_and(|pid| !process_alive(pid))
            {
                let _ = fs::remove_file(file.path());
            }
        }
    }
}

/// Link the written lock into place, failing if one is already there.
fn publish_lock(pending: &Path, path: &Path) -> std::io::Result<()> {
    match fs::hard_link(pending, path) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
            // No hard links here (exFAT, some network mounts): an exclusive
            // create still excludes, but a reader can see it empty for an instant.
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(lock_contents().as_bytes()).inspect_err(|_| {
                let _ = fs::remove_file(path);
            })
        }
        linked => linked,
    }
}

impl WorktreeLock {
    pub(super) fn acquire(repo: &RepoContext, target: &Path) -> Result<Self> {
        let path = worktree_lock_path(repo, target)?;
        // Published complete: a lock created empty and then written reads, for
        // an instant, as Git's lock with no reason, which is refused as not
        // simgit's. The pending file sits beside the lock so the link is local.
        let name = path.file_name().context("lock path has no file name")?;
        let pending = path.with_file_name(format!(
            "{}.{}.{}.pending",
            name.to_string_lossy(),
            std::process::id(),
            Uuid::new_v4()
        ));
        let linked =
            fs::write(&pending, lock_contents()).and_then(|()| publish_lock(&pending, &path));
        let _ = fs::remove_file(&pending);
        match linked {
            Ok(()) => Ok(Self { path: Some(path) }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                match fs::read_to_string(&path) {
                    Ok(contents) if !is_simgit_lock(&contents) => bail!(
                        "workspace is locked by Git{}: {}\n\
                         release it with: git worktree unlock {}",
                        git_lock_reason(&contents),
                        target.display(),
                        target.display()
                    ),
                    Err(error) if lock_present(&path) => {
                        return Err(unreadable_lock(&path, &error))
                    }
                    _ => {}
                }
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
                Err(error).with_context(|| format!("lock workspace at {}", path.display()))
            }
        }
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

/// The claim file for `key` under `.git/simgit/claims/<kind>/`, created on
/// first use. It is never deleted: a claim is the OS lock on it, which the
/// kernel releases with its holder however that exits, so none goes stale.
fn open_claim_file(repo: &RepoContext, kind: &str, key: &[u8]) -> Result<fs::File> {
    let directory = state_dir(&repo.common_git_dir).join("claims").join(kind);
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let path = directory.join(format!("{}.lock", key_digest(key)));
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))
}

/// A stable file name for a key: claims live in flat directories, and a branch
/// or worktree path contains separators and may be long.
fn key_digest(key: &[u8]) -> String {
    let hash = key.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

/// An exclusive claim on one destination path for the length of an allocation.
///
/// Creating a worktree is several Git operations, and two `add --path <same>`
/// runs interleaving through them both used to finish: Git kept two
/// registrations for one directory, the winner's JSON named the loser's
/// branch, and afterwards neither allocation could be removed by its token.
/// The claim is an OS lock on a per-path file, like `BranchClaim`, so the
/// second allocator fails immediately and changes nothing, and a killed one
/// leaves no claim for two others to both take over.
pub(super) struct PathClaim {
    _file: fs::File,
}

impl PathClaim {
    pub(super) fn acquire(repo: &RepoContext, target: &Path) -> Result<Self> {
        let file = open_claim_file(repo, "paths", target.as_os_str().as_encoded_bytes())?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => bail!(
                "another simgit is already creating a worktree at {}\n\
                 wait for it to finish, or allocate a different path",
                target.display()
            ),
            Err(fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("claim the worktree path {}", target.display()))
            }
        }
    }
}

/// An exclusive claim on one branch for the length of an allocation.
///
/// Checking that a branch is not checked out anywhere and then registering a
/// worktree on it are separate steps, and Git's own check between them is no
/// lock either: two allocations for one branch at different paths both used
/// to finish, leaving two worktrees on one branch. The claim is an OS lock on
/// a per-branch file that is never deleted, so a second allocator fails at
/// once, a killed one releases it with its process, and there is no stale
/// claim to take over.
pub(super) struct BranchClaim {
    _file: fs::File,
}

impl BranchClaim {
    fn open(repo: &RepoContext, branch: &str) -> Result<fs::File> {
        open_claim_file(repo, "branches", branch.as_bytes())
    }

    pub(super) fn acquire(repo: &RepoContext, branch: &str) -> Result<Self> {
        let file = Self::open(repo, branch)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => bail!(
                "another simgit is already creating a worktree for branch '{branch}'\n\
                 wait for it to finish, then work in that worktree"
            ),
            Err(fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("claim the branch '{branch}'"))
            }
        }
    }

    /// Like `acquire`, but waits for the holder instead of failing.
    pub(super) fn acquire_blocking(repo: &RepoContext, branch: &str) -> Result<Self> {
        let file = Self::open(repo, branch)?;
        file.lock()
            .with_context(|| format!("claim the branch '{branch}'"))?;
        Ok(Self { _file: file })
    }
}

/// A repository-wide lock on Git's worktree registry, `.git/worktrees/`.
///
/// While `git worktree add` registers a worktree its admin directory is
/// half-written, and any Git command that reads every registration then dies
/// with "failed to read .../commondir". simgit holds this exclusively around
/// `git worktree add` and `prune`, and shared around the quick Git commands
/// that read all registrations (`list`, `branch -d`), each only for that one
/// command. `worktree remove` and `fetch` can take seconds, so they run
/// unlocked and retry that failure instead.
///
/// Lock order: `BranchClaim`, then `PathClaim`, then this. It is taken last
/// and never held while acquiring anything else, so it cannot deadlock.
pub(super) struct RegistryLock {
    _file: Option<fs::File>,
}

impl RegistryLock {
    fn path(repo: &RepoContext) -> PathBuf {
        state_dir(&repo.common_git_dir)
            .join("claims")
            .join("registry.lock")
    }

    fn create(path: &Path) -> std::io::Result<fs::File> {
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory)?;
        }
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
    }

    /// Held while a Git command creates or deletes a registration.
    pub(super) fn exclusive(repo: &RepoContext) -> Result<Self> {
        let path = Self::path(repo);
        let file = Self::create(&path).with_context(|| format!("open {}", path.display()))?;
        file.lock().context("lock the worktree registry")?;
        Ok(Self { _file: Some(file) })
    }

    /// Held while a Git command reads every registration. A process that
    /// cannot create the lock file cannot register a worktree either, so in a
    /// repository it may only read, it reads unlocked rather than failing.
    pub(super) fn shared(repo: &RepoContext) -> Result<Self> {
        let path = Self::path(repo);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match Self::create(&path) {
                    Ok(file) => file,
                    Err(_) => return Ok(Self { _file: None }),
                }
            }
            Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
        };
        file.lock_shared().context("lock the worktree registry")?;
        Ok(Self { _file: Some(file) })
    }
}

/// A lock file as `unlock` judged it: its text and which file held it.
#[derive(PartialEq)]
pub(super) struct ReadLock {
    contents: String,
    identity: (u64, u64),
}

/// The lock at `path`, or `None` when there is none. Text and identity come
/// from one open, so they describe the same file.
pub(super) fn read_lock(path: &Path) -> Result<Option<ReadLock>> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !lock_present(path) => {
            return Ok(None)
        }
        // A dangling symlink is still a lock to Git.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(unreadable_lock(path, &error))
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("read {}", path.display()))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(Some(ReadLock {
        contents,
        identity: (metadata.dev(), metadata.ino()),
    }))
}

/// Remove the lock `read` describes and no other: since it was read, another
/// `unlock` may have cleared it and a new `run` taken the workspace. The
/// contents check covers a reused inode; only the gap to the unlink remains.
pub(super) fn remove_read_lock(path: &Path, read: &ReadLock, label: &str) -> Result<()> {
    match read_lock(path)? {
        Some(current) if current == *read => remove_file_if_present(path),
        Some(_) => bail!(
            "workspace was locked again while it was being unlocked: {label}\n\
             run simgit unlock again to see what holds it"
        ),
        None => Ok(()),
    }
}

#[derive(Args)]
pub struct WorktreeUnlock {
    /// Worktree path or branch name. Defaults to the worktree containing the
    /// current directory.
    pub target: Option<String>,
}

/// Clear a lock simgit itself left behind: the `run` lock of a killed launcher
/// or the allocation lock of a killed `add`. A lock Git holds for someone else
/// (`git worktree lock`) is refused, whatever it says, because it names no
/// simgit process to have died.
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
        TargetLookup::Found(target) | TargetLookup::Gone(target) => {
            let path = worktree_lock_path(&repo, &target)?;
            (target.display().to_string(), Some(path))
        }
        TargetLookup::Stray(path) => (path.display().to_string(), None),
        TargetLookup::Absent(spec) => (spec, None),
    };
    let read = match &lock_path {
        Some(path) => read_lock(path)?,
        None => None,
    };
    let contents = read.as_ref().map(|lock| lock.contents.as_str());
    if let Some(contents) = contents.filter(|c| !is_simgit_lock(c)) {
        bail!(
            "workspace is locked by Git{}, not by simgit: {label}\n\
             simgit unlock only clears its own locks; release this one with: \
             git worktree unlock {label}",
            git_lock_reason(contents)
        );
    }
    let owner_pid = contents.and_then(lock_owner_pid);
    if let Some(pid) = owner_pid.filter(|pid| process_alive(*pid)) {
        bail!(
            "workspace is in use by a running command (pid {pid}): {label}\n\
             stop that process, then run simgit unlock again"
        );
    }
    let was_locked = read.is_some();
    if let (Some(path), Some(read)) = (&lock_path, &read) {
        remove_read_lock(path, read, &label)?;
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
