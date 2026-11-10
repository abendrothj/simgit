use super::add::still_locked;
use super::registry::{lookup_worktree_target, worktree_admin_dir, TargetLookup};
use super::remove::remove_file_if_present;
use super::repo::{discover_repo, state_dir};
use super::{emit, overlay, shell_quote, simgit_anywhere, RepoContext};
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
/// lock state is not permission to tear anything down. Returns the lock's
/// path, which for a linked worktree lies in its registration.
pub(super) fn ensure_unlocked(repo: &RepoContext, target: &Path) -> Result<PathBuf> {
    let path = worktree_lock_path(repo, target).map_err(|cause| {
        anyhow::anyhow!(
            "cannot tell whether {} is locked: {cause:#}",
            target.display()
        )
    })?;
    if !lock_present(&path) {
        return Ok(path);
    }
    match fs::read_to_string(&path) {
        Ok(contents) if !is_simgit_lock(&contents) => bail!(
            "worktree is locked by Git{}: {}\n\
             release it with: git worktree unlock {}",
            git_lock_reason(&contents),
            target.display(),
            shell_quote(target)
        ),
        // A killed `run` or allocation leaves its lock naming a dead pid.
        Ok(contents) => match lock_owner_pid(&contents).filter(|pid| !process_alive(*pid)) {
            Some(pid) => {
                let unlock = simgit_anywhere(repo, &format!("unlock {}", shell_quote(target)));
                // Killed before it was populated: unlocking alone hands out a
                // worktree that reuse then refuses.
                let then = if unfinished(&path) {
                    format!(
                        ", then remove the unfinished worktree with `{}`",
                        discard_unfinished(repo, target)
                    )
                } else {
                    String::new()
                };
                bail!(
                    "worktree is locked by a simgit process that is no longer running \
                     (pid {pid}): {}\nrelease the lock with `{unlock}`{then}",
                    target.display()
                )
            }
            None => bail!(
                "worktree is locked (a command may be running): {}",
                target.display()
            ),
        },
        Err(error) => Err(unreadable_lock(&path, &error)),
    }
}

/// Whether the linked worktree whose lock is `lock` was never populated. Every
/// populate path but a sparse one writes the registration's `index` last, so
/// without it an allocation was killed part-way, and every tracked file shows
/// as deleted. One stat. The main checkout's lock is not in a registration.
pub(super) fn unfinished(lock: &Path) -> bool {
    lock.file_name() == Some("locked".as_ref())
        && lock
            .parent()
            .is_some_and(|admin| matches!(admin.join("index").try_exists(), Ok(false)))
}

/// The removal that clears an unfinished worktree, from any directory.
fn discard_unfinished(repo: &RepoContext, target: &Path) -> String {
    simgit_anywhere(
        repo,
        &format!("remove {} --discard-dirty", shell_quote(target)),
    )
}

/// The refusal to hand out an unfinished worktree, naming the removal that
/// clears it with `discard`.
pub(super) fn unfinished_worktree(target: &Path, discard: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{} was never populated: Git has no index for it, as after an allocation \
         killed part-way; remove it with {discard}",
        target.display()
    )
}

/// Refuse to reuse the worktree at `target`, whose lock is `lock`, when it
/// was never populated; see `unfinished`.
pub(super) fn refuse_unfinished(repo: &RepoContext, lock: &Path, target: &Path) -> Result<()> {
    if unfinished(lock) {
        let discard = format!("`{}`", discard_unfinished(repo, target));
        return Err(unfinished_worktree(target, &discard));
    }
    Ok(())
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

/// Whether the lock file at `path` is one a live simgit process holds.
pub(super) fn held_by_live_simgit(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| {
        is_simgit_lock(&contents) && lock_owner_pid(&contents).is_some_and(process_alive)
    })
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
    use crate::kill;
    // SAFETY: `kill` takes two integers and dereferences nothing, and signal 0
    // delivers no signal, so the call only reports whether the pid is live.
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

/// The process named by a `<pid><separator><uuid>` token exactly as simgit
/// writes one: a temporary file's with `.`, a fetch ref's with `-`. Anything
/// else names no process, so it is never taken for a dead one's.
pub(super) fn temp_owner(token: &str, separator: char) -> Option<i32> {
    let (pid, uuid) = token.split_once(separator)?;
    let canonical = Uuid::parse_str(uuid)
        .is_ok_and(|parsed| parsed.hyphenated().to_string() == uuid.to_ascii_lowercase());
    let digits = !pid.starts_with('0') && pid.bytes().all(|byte| byte.is_ascii_digit());
    if !canonical || !digits {
        return None;
    }
    pid.parse().ok()
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
                .and_then(|token| temp_owner(token, '.'))
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

/// Where to write a lock before it is published at `path`: beside it, so the
/// link or rename stays on one filesystem, and named for `remove_dead_pending`.
fn pending_path(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("lock path has no file name")?;
    Ok(path.with_file_name(format!(
        "{}.{}.{}.pending",
        name.to_string_lossy(),
        std::process::id(),
        Uuid::new_v4()
    )))
}

impl WorktreeLock {
    pub(super) fn acquire(repo: &RepoContext, target: &Path) -> Result<Self> {
        let path = worktree_lock_path(repo, target)?;
        // Published complete: a lock created empty and then written reads, for
        // an instant, as Git's lock with no reason, which is refused as not
        // simgit's. The pending file sits beside the lock so the link is local.
        let pending = pending_path(&path)?;
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
                        shell_quote(target)
                    ),
                    Err(error) if lock_present(&path) => {
                        return Err(unreadable_lock(&path, &error))
                    }
                    _ => {}
                }
                let owner = lock_owner_of(&path);
                if let Some(pid) = owner.filter(|pid| !process_alive(*pid)) {
                    bail!(
                        "workspace is locked by a simgit process that is no longer running \
                         (pid {pid}): {}\n\
                         release the lock with: simgit unlock {}",
                        target.display(),
                        shell_quote(target)
                    )
                }
                let owner = owner.map(|pid| format!(" (pid {pid})")).unwrap_or_default();
                bail!(
                    "workspace is already in use by a running command{owner}: {}\n\
                     if nothing is running there, release it with: simgit unlock {}",
                    target.display(),
                    shell_quote(target)
                )
            }
            Err(error) => {
                Err(error).with_context(|| format!("lock workspace at {}", path.display()))
            }
        }
    }

    /// Turn the allocation lock this process holds on a worktree it has just
    /// created into its `run` lock, so gc never finds that worktree unlocked
    /// before the command starts. The rename replaces one complete lock with
    /// another, so no reader sees it missing or empty, and the owner pid stays.
    pub(super) fn take_over_allocation(repo: &RepoContext, target: &Path) -> Result<Self> {
        let path = worktree_lock_path(repo, target)?;
        let ours = fs::read_to_string(&path)
            .is_ok_and(|contents| contents.trim_end() == allocation_lock_reason());
        if !ours {
            // Cleared by hand, or another lock since: never replace that one.
            return Self::acquire(repo, target);
        }
        let pending = pending_path(&path)?;
        let renamed =
            fs::write(&pending, lock_contents()).and_then(|()| fs::rename(&pending, &path));
        if let Err(error) = renamed {
            let _ = fs::remove_file(&pending);
            return Err(error).with_context(|| {
                format!(
                    "lock workspace at {}\n{}",
                    path.display(),
                    still_locked(target)
                )
            });
        }
        Ok(Self { path: Some(path) })
    }

    /// The lock file while it is held, in the worktree's registration.
    pub(super) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
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

/// An exclusive claim on building one commit's baseline.
///
/// Allocations that find no baseline used to each check out the whole tree
/// and keep only the first to publish: ten agents on a new commit did the
/// work ten times, and on one disk the copies slowed each other down. The
/// claim makes later allocators wait for the first build and reuse it. It is
/// taken before the cache lock and never while holding it, and `prune` never
/// takes it, so a slow build still does not hold `prune` up.
pub(super) struct BaselineBuildClaim {
    _file: fs::File,
}

impl BaselineBuildClaim {
    pub(super) fn acquire(repo: &RepoContext, commit: &str) -> Result<Self> {
        let file = open_claim_file(repo, "baselines", commit.as_bytes())?;
        let context = || format!("claim building the baseline for {commit}");
        match file.try_lock() {
            Ok(()) => {}
            // The wait has no bound: it lasts as long as the other build.
            Err(fs::TryLockError::WouldBlock) => {
                eprintln!("waiting for another simgit to build the baseline for {commit}");
                file.lock().with_context(context)?;
            }
            Err(fs::TryLockError::Error(error)) => return Err(error).with_context(context),
        }
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
    pub(super) contents: String,
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
             git worktree unlock {}",
            git_lock_reason(contents),
            shell_quote(&label)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktree::add::{allocate_git_worktree, WorktreeKind};
    use crate::commands::worktree::overlay;
    use crate::commands::worktree::repo::{discover_repo, resolve_commit, state_dir};
    use crate::commands::worktree::test_support::{
        add_git_worktree, file_identity, files_under, Fixture,
    };
    use anyhow::{Context, Result};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn a_blocking_claim_waits_for_the_holder() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = Arc::new(discover_repo(&fixture.repo)?);
        let held = BranchClaim::acquire(&repo, "b")?;
        let started = std::time::Instant::now();
        let waiter = {
            let repo = Arc::clone(&repo);
            std::thread::spawn(move || {
                let claim = BranchClaim::acquire_blocking(&repo, "b");
                (claim.is_ok(), started.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
        let (acquired, elapsed) = waiter.join().expect("waiter thread");
        assert!(acquired);
        assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
        Ok(())
    }

    /// Recovery from a stranded lock turns entirely on the lock naming its holder:
    /// without that, clearing one is a guess about whether a command is still
    /// writing the checkout.
    #[test]
    fn a_lock_names_its_owner_and_an_exited_owner_is_not_alive() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("locked");
        add_git_worktree(
            &repo,
            "locked",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let path = worktree_lock_path(&repo, &target)?;
        let self_pid = std::process::id() as i32;

        let lock = WorktreeLock::acquire(&repo, &target)?;
        assert_eq!(lock_owner_of(&path), Some(self_pid));
        assert!(process_alive(self_pid));
        assert!(ensure_unlocked(&repo, &target).is_err());

        let refused = match WorktreeLock::acquire(&repo, &target) {
            Ok(_) => panic!("a second launcher acquired a lock this process holds"),
            Err(error) => error.to_string(),
        };
        assert!(
            refused.contains(&format!("pid {self_pid}")) && refused.contains("simgit unlock"),
            "a second launcher is not told who holds the lock: {refused}"
        );
        lock.release()?;
        assert!(ensure_unlocked(&repo, &target).is_ok());

        // A process that has exited holds nothing, so its lock is clearable.
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn()?;
        let dead_pid = child.id() as i32;
        child.wait()?;
        assert!(!process_alive(dead_pid));

        // Locks written before simgit recorded a pid have an unknown owner, which
        // must not be mistaken for a live one.
        assert_eq!(lock_owner_pid("simgit: workspace in use\n"), None);
        Ok(())
    }

    /// `unlock` judges a lock, then removes it. Removing whatever was at the path
    /// by then deleted the live lock of a `run` that started after another
    /// `unlock` cleared the stranded one, leaving that command unprotected.
    #[test]
    fn unlock_removes_only_the_lock_it_read() -> Result<()> {
        use super::{read_lock, remove_read_lock};
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("recovered");
        add_git_worktree(
            &repo,
            "recovered",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let path = worktree_lock_path(&repo, &target)?;
        let label = target.display().to_string();
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn()?;
        let dead_pid = child.id();
        child.wait()?;
        let strand = || {
            fs::write(
                &path,
                format!("simgit: workspace in use (pid {dead_pid})\n"),
            )
        };

        // No lock read means no lock to remove.
        assert!(read_lock(&path)?.is_none());

        // Cleared by another unlock, then taken by a run: that lock stays.
        strand()?;
        let judged = read_lock(&path)?.expect("a stranded lock");
        fs::remove_file(&path)?;
        let live = WorktreeLock::acquire(&repo, &target)?;
        assert!(remove_read_lock(&path, &judged, &label).is_err());
        assert_eq!(
            lock_owner_of(&path),
            Some(std::process::id() as i32),
            "unlock removed a live run's lock"
        );
        live.release()?;

        // Cleared by another unlock and not retaken: the workspace is unlocked.
        strand()?;
        let judged = read_lock(&path)?.expect("a stranded lock");
        fs::remove_file(&path)?;
        remove_read_lock(&path, &judged, &label)?;
        assert!(!path.exists());

        // Unchanged since it was read: removed.
        strand()?;
        let judged = read_lock(&path)?.expect("a stranded lock");
        remove_read_lock(&path, &judged, &label)?;
        assert!(!path.exists());
        Ok(())
    }

    /// A lock created empty and written afterwards reads, for an instant, as Git's
    /// lock with no reason, which `run` and `unlock` refuse as not simgit's.
    #[test]
    fn a_lock_is_never_observed_empty() -> Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("watched");
        add_git_worktree(
            &repo,
            "watched",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let path = worktree_lock_path(&repo, &target)?;
        let self_pid = std::process::id() as i32;
        let done = AtomicBool::new(false);
        let (acquired, (reads, ownerless)) = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let (mut reads, mut ownerless) = (0, 0);
                while !done.load(Ordering::Relaxed) {
                    if let Ok(contents) = fs::read_to_string(&path) {
                        reads += 1;
                        if lock_owner_pid(&contents) != Some(self_pid) {
                            ownerless += 1;
                        }
                    }
                }
                (reads, ownerless)
            });
            let acquired =
                (0..100).try_for_each(|_| WorktreeLock::acquire(&repo, &target)?.release());
            done.store(true, Ordering::Relaxed);
            (acquired, reader.join().expect("reader thread"))
        });
        acquired?;
        assert!(reads > 0, "the reader never saw the lock");
        assert_eq!(
            ownerless, 0,
            "{ownerless} of {reads} reads saw a lock that did not name its owner"
        );

        // Neither a lock taken nor one refused leaves its pending file behind.
        let held = WorktreeLock::acquire(&repo, &target)?;
        assert!(WorktreeLock::acquire(&repo, &target).is_err());
        held.release()?;
        let leftovers: Vec<_> = files_under(path.parent().unwrap())?
            .into_iter()
            .filter(|file| file.to_string_lossy().ends_with(".pending"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        Ok(())
    }

    /// `run` takes over its new worktree's allocation lock: between the two, no
    /// reader may find the worktree unlocked, or a lock that is empty or does not
    /// name its owner, or gc could reap the worktree before the command starts.
    #[test]
    fn taking_over_an_allocation_lock_never_leaves_the_worktree_unlocked() -> Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("handed");
        // Registered and still under its allocation lock, as `create_worktree` returns it.
        allocate_git_worktree(
            &repo,
            "handed",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let path = worktree_lock_path(&repo, &target)?;
        let allocation = fs::read_to_string(&path)?;
        assert!(allocation.starts_with("simgit: allocating"), "{allocation}");
        let self_pid = std::process::id() as i32;
        let done = AtomicBool::new(false);
        let (taken, (reads, unlocked)) = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let (mut reads, mut unlocked) = (0, 0);
                while !done.load(Ordering::Relaxed) {
                    match fs::read_to_string(&path) {
                        Ok(contents) if lock_owner_pid(&contents) == Some(self_pid) => reads += 1,
                        _ => unlocked += 1,
                    }
                }
                (reads, unlocked)
            });
            let taken = (0..100).try_for_each(|_| -> Result<()> {
                let lock = WorktreeLock::take_over_allocation(&repo, &target)?;
                let contents = fs::read_to_string(&path)?;
                assert!(
                    contents.starts_with("simgit: workspace in use"),
                    "{contents}"
                );
                // Keep the lock file, and put Git's allocation lock back in one step.
                std::mem::forget(lock);
                let staged = path.with_file_name("allocation.staged");
                fs::write(&staged, &allocation)?;
                fs::rename(&staged, &path)?;
                Ok(())
            });
            done.store(true, Ordering::Relaxed);
            (taken, reader.join().expect("reader thread"))
        });
        taken?;
        assert!(reads > 0, "the reader never saw the lock");
        assert_eq!(
            unlocked,
            0,
            "{unlocked} of {} reads found no lock naming its owner",
            reads + unlocked
        );
        let leftovers: Vec<_> = files_under(path.parent().unwrap())?
            .into_iter()
            .filter(|file| file.to_string_lossy().ends_with(".pending"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        Ok(())
    }

    /// Two allocators that find a path claim left by a killed one used to both
    /// conclude it was stale, both unlink it and both retake it, so both went on
    /// to allocate the one path.
    #[test]
    fn two_takeovers_of_a_stale_path_claim_never_both_succeed() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        // Claim files are named for the target alone, so a scratch repository
        // shows which file a claim on it uses without locking the real one.
        let scratch = Fixture::new()?;
        let scratch_repo = discover_repo(&scratch.repo)?;
        let scratch_claims = state_dir(&scratch_repo.common_git_dir).join("claims");
        let claims = state_dir(&repo.common_git_dir).join("claims");
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn()?;
        let dead_pid = child.id();
        child.wait()?;

        // What a killed allocator leaves behind: its claim file, naming it.
        let leave_claim = |target: &Path, pid: u32| -> Result<PathBuf> {
            let before = files_under(&scratch_claims)?;
            let probe = PathClaim::acquire(&scratch_repo, target)?;
            let created: Vec<_> = files_under(&scratch_claims)?
                .difference(&before)
                .cloned()
                .collect();
            drop(probe);
            assert_eq!(created.len(), 1, "a claim made {created:?}");
            let claim = claims.join(created[0].strip_prefix(&scratch_claims)?);
            fs::create_dir_all(claim.parent().unwrap())?;
            fs::write(
                &claim,
                format!("simgit: allocating a worktree here (pid {pid})\n"),
            )?;
            Ok(claim)
        };

        for round in 0..50 {
            let target = fixture.root.join(format!("contested-{round}"));
            let stale = leave_claim(&target, dead_pid)?;
            let original = file_identity(&stale);

            let barrier = Barrier::new(2);
            let won = std::thread::scope(|scope| {
                let racers: Vec<_> = (0..2)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            let claim = PathClaim::acquire(&repo, &target);
                            // Hold it until both have tried: only claims held at
                            // the same time compete.
                            barrier.wait();
                            claim.is_ok()
                        })
                    })
                    .collect();
                racers
                    .into_iter()
                    .map(|racer| racer.join().expect("racer thread"))
                    .filter(|won| *won)
                    .count()
            });
            assert_eq!(
                won, 1,
                "round {round}: {won} allocators took one stale claim"
            );
            // The claim is the OS lock on one file that is never replaced: a lock
            // on a replacement would not exclude a holder of the original.
            assert_eq!(
                file_identity(&stale),
                original,
                "round {round}: taking over and releasing the claim replaced its file"
            );

            // A claim file naming a live process, but not locked, holds nothing.
            // It is a fresh target: a child a parallel test forks can hold a
            // lock this process released until it execs.
            let target = fixture.root.join(format!("unlocked-{round}"));
            let unlocked = leave_claim(&target, std::process::id())?;
            let original = file_identity(&unlocked);
            let claim = PathClaim::acquire(&repo, &target);
            assert!(
                claim.is_ok(),
                "round {round}: an unlocked claim file was refused"
            );
            assert_eq!(file_identity(&unlocked), original, "round {round}");
            drop(claim);
            assert_eq!(file_identity(&unlocked), original, "round {round}");
        }
        Ok(())
    }

    /// Git 2.48+ under `worktree.useRelativePaths` records a worktree relative to
    /// its admin dir. Written by hand here, so it runs on any Git: a deleted
    /// worktree's registration and lock must still be found by its path.
    #[test]
    fn a_relative_gitdir_still_finds_a_deleted_worktrees_lock() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("relative");
        add_git_worktree(
            &repo,
            "relative",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let admin = overlay::admin_dir(&repo, &target).context("no admin dir")?;
        fs::remove_dir_all(&target)?;
        fs::write(admin.join("gitdir"), "../../../../relative/.git\n")?;

        assert_eq!(overlay::admin_dir(&repo, &target), Some(admin.clone()));
        assert!(ensure_unlocked(&repo, &target).is_ok());
        fs::write(admin.join("locked"), "usb")?;
        let refused = ensure_unlocked(&repo, &target).unwrap_err().to_string();
        assert!(refused.contains("usb"), "{refused}");
        Ok(())
    }

    /// A `locked` symlink to nothing is a lock to Git, which then cannot read its
    /// reason or unlock it either; the advice must be the one thing that works.
    #[test]
    fn a_dangling_lock_is_refused_with_advice_that_works() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("dangling");
        add_git_worktree(
            &repo,
            "dangling",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let lock = worktree_lock_path(&repo, &target)?;
        std::os::unix::fs::symlink(fixture.root.join("nowhere"), &lock)?;
        let advice = format!("rm {}", lock.display());

        let checked = ensure_unlocked(&repo, &target).unwrap_err().to_string();
        assert!(checked.contains(&advice), "{checked}");
        let acquired = match WorktreeLock::acquire(&repo, &target) {
            Ok(_) => panic!("acquired a workspace Git counts as locked"),
            Err(error) => error.to_string(),
        };
        assert!(acquired.contains(&advice), "{acquired}");
        assert!(fs::symlink_metadata(&lock).is_ok());
        Ok(())
    }

    /// An absolute `gitdir` spelled through a symlink names the same worktree as
    /// its real path, as Git's own path lookup treats it.
    #[test]
    fn a_gitdir_spelled_through_a_symlink_still_finds_the_lock() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("aliased");
        add_git_worktree(
            &repo,
            "aliased",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let admin = overlay::admin_dir(&repo, &target).context("no admin dir")?;
        fs::remove_dir_all(&target)?;
        let alias = fixture.root.join("alias");
        std::os::unix::fs::symlink(&fixture.root, &alias)?;
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", alias.join("aliased/.git").display()),
        )?;

        assert_eq!(overlay::admin_dir(&repo, &target), Some(admin.clone()));
        assert_eq!(
            overlay::admin_dir(&repo, &alias.join("aliased")),
            Some(admin.clone())
        );
        assert!(ensure_unlocked(&repo, &target).is_ok());
        fs::write(admin.join("locked"), "usb")?;
        assert!(ensure_unlocked(&repo, &target).is_err());
        Ok(())
    }
}
