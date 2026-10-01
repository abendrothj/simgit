use super::{main_worktree, run_command, state_dir, RepoContext};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use uuid::Uuid;

const BASELINE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Cap on per-file clone workers. Reflink clones are metadata operations, so
/// a handful of threads saturates the filesystem's allocation structures and
/// more only adds contention.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const CLONE_WORKERS_MAX: usize = 8;

pub(super) fn clone_supported(common_git_dir: &Path, destination_dir: &Path) -> Result<bool> {
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
        // Probe with the exact operation `clone_tree` performs, so a positive
        // probe can never select a clone mechanism that then fails.
        let cloned = clone_file(&source, &destination).is_ok();
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&destination);
        Ok(cloned)
    }
}

/// Clone one file's extents with the `FICLONE` ioctl — the operation behind
/// `cp --reflink=always`, without spawning a process per tree.
#[cfg(target_os = "linux")]
fn clone_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    /// `_IOW(0x94, 9, int)` from `linux/fs.h`.
    const FICLONE: std::ffi::c_ulong = 0x4004_9409;
    extern "C" {
        fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
    }

    let from = fs::File::open(source)?;
    let mode = from.metadata()?.mode();
    let to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(destination)?;
    // SAFETY: both descriptors are owned and open for the duration of the
    // call; FICLONE reads extents from `from` and links them into `to`.
    if unsafe { ioctl(to.as_raw_fd(), FICLONE, from.as_raw_fd()) } != 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    // The mode passed to `create_new` is filtered by the umask; re-assert the
    // baseline's mode so the executable bit always survives the clone.
    to.set_permissions(fs::Permissions::from_mode(mode))
}

/// Clone one path with `clonefile(2)`. Files and directory trees alike; the
/// destination must not exist.
#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::{c_char, CString};
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn clonefile(source: *const c_char, destination: *const c_char, flags: u32) -> i32;
    }

    let nul = |_| std::io::Error::from(std::io::ErrorKind::InvalidInput);
    let from = CString::new(source.as_os_str().as_bytes()).map_err(nul)?;
    let to = CString::new(destination.as_os_str().as_bytes()).map_err(nul)?;
    // SAFETY: both pointers are NUL-terminated and outlive the call, and
    // clonefile only reads them.
    if unsafe { clonefile(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A shared or exclusive hold on the baseline cache, released on drop, or by
/// the kernel when the process dies, so it can never be stranded.
///
/// Allocations hold it shared from publishing or reusing a baseline until they
/// no longer read from it. `prune` takes it exclusively, without waiting, only
/// to move the baselines it drops out of the way. A baseline an allocation is
/// using is therefore never removed underneath it.
pub(super) struct BaselineCacheLock {
    _file: fs::File,
}

impl BaselineCacheLock {
    /// Wait for a shared hold. `prune` holds the lock exclusively only for a
    /// few renames, so this wait is short.
    fn shared(common_git_dir: &Path) -> Result<Self> {
        let file = cache_lock_file(common_git_dir)?;
        file.lock_shared().context("lock the baseline cache")?;
        Ok(Self { _file: file })
    }

    /// An exclusive hold, or `None` while any allocation holds it shared.
    pub(super) fn try_exclusive(common_git_dir: &Path) -> Result<Option<Self>> {
        let file = cache_lock_file(common_git_dir)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error).context("lock the baseline cache"),
        }
    }
}

fn cache_lock_file(common_git_dir: &Path) -> Result<fs::File> {
    let directory = state_dir(common_git_dir);
    fs::create_dir_all(&directory)?;
    let path = directory.join("baselines.lock");
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))
}

/// A published baseline tree, with the shared cache lock that keeps `prune`
/// from removing it. Keep it until nothing reads from the tree any more.
pub(super) struct Baseline {
    pub(super) tree: PathBuf,
    _lock: BaselineCacheLock,
}

fn is_published(final_dir: &Path) -> bool {
    final_dir.join("ready").is_file() && final_dir.join("tree").is_dir()
}

/// Return `commit`'s published baseline, building it first if needed.
///
/// Building runs without the lock, in a unique temporary directory that
/// `prune` leaves alone until it is a week old, so a slow first checkout never
/// holds `prune` off. Publishing is one atomic rename: concurrent losers
/// discard their tree and use the winner's. A published baseline is never
/// mutated, and only `prune`, holding the lock exclusively, removes one.
pub(super) fn ensure_baseline(repo: &RepoContext, commit: &str) -> Result<Baseline> {
    let root = state_dir(&repo.common_git_dir).join("baselines");
    let final_dir = root.join(commit);
    let final_tree = final_dir.join("tree");

    let lock = BaselineCacheLock::shared(&repo.common_git_dir)?;
    if is_published(&final_dir) {
        touch(&final_dir.join("ready"))?;
        return Ok(Baseline {
            tree: final_tree,
            _lock: lock,
        });
    }
    // Publishing is atomic and `prune` only moves baselines while it holds
    // the lock exclusively, so under the shared lock a partial entry is damage.
    if final_dir.exists() {
        bail!(
            "cached baseline {} is incomplete; run `simgit prune --all`",
            final_dir.display()
        );
    }
    drop(lock);

    fs::create_dir_all(&root)?;
    let temporary = root.join(format!(".{commit}.{}", Uuid::new_v4()));
    let cache = temporary.join("cache");
    let temporary_tree = cache.join("tree");
    let built = fs::create_dir_all(&temporary_tree)
        .map_err(anyhow::Error::from)
        .and_then(|()| materialize_baseline(repo, commit, &temporary_tree))
        .and_then(|()| Ok(fs::write(cache.join("ready"), commit)?));
    if let Err(error) = built {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    let lock = match BaselineCacheLock::shared(&repo.common_git_dir) {
        Ok(lock) => lock,
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
    };
    let published = match fs::rename(&cache, &final_dir) {
        Ok(()) => Ok(()),
        // Another process won the atomic publish race.
        Err(_) if is_published(&final_dir) => Ok(()),
        Err(error) => Err(error).with_context(|| format!("publish baseline {commit}")),
    };
    let _ = fs::remove_dir_all(&temporary);
    published.map(|()| Baseline {
        tree: final_tree,
        _lock: lock,
    })
}

fn materialize_baseline(repo: &RepoContext, commit: &str, destination: &Path) -> Result<()> {
    let index = destination
        .parent()
        .context("baseline destination has no parent")?
        .join("index");
    let mut read_tree = Command::new("git");
    read_tree
        .current_dir(main_worktree(&repo.common_git_dir))
        .env("GIT_INDEX_FILE", &index)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .args(["read-tree", commit]);
    run_command(&mut read_tree, "initialize baseline index")?;

    // `-u` records each file's stat data in the baseline index. A worktree
    // cloned from this tree can then adopt that index and skip Git's initial
    // full rescan; see `populate_cow_worktree`. The index is published with
    // the tree and shares its lifetime.
    let mut checkout = Command::new("git");
    checkout
        .current_dir(main_worktree(&repo.common_git_dir))
        .env("GIT_INDEX_FILE", &index)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .arg(format!("--work-tree={}", destination.display()))
        .args(["checkout-index", "--all", "--force", "-u"]);
    run_command(
        &mut checkout,
        "materialize baseline with Git checkout-index",
    )
}

/// True when this platform can clone a whole directory tree in one call.
pub(super) const ROOT_CLONE: bool = cfg!(target_os = "macos");

/// Clone an entire baseline tree with a single `clonefile(2)` call.
///
/// APFS clones a directory hierarchy recursively in one syscall, sharing the
/// same extents as a per-file clone but without walking the tree: 0.20 s vs
/// 2.50 s for a 23k-entry checkout. `destination` must not exist. Linux has no
/// directory-level reflink, so there is no equivalent path there.
#[cfg(target_os = "macos")]
pub(super) fn clone_root(source: &Path, destination: &Path) -> Result<()> {
    clone_file(source, destination).with_context(|| {
        format!(
            "clonefile {} -> {}",
            source.display(),
            destination.display()
        )
    })
}

#[cfg(not(target_os = "macos"))]
pub(super) fn clone_root(source: &Path, destination: &Path) -> Result<()> {
    let _ = (source, destination);
    bail!("directory-level cloning is unavailable on this platform")
}

/// The stat-refreshed index published beside a baseline tree, if the baseline
/// was materialized by a version that writes one.
pub(super) fn baseline_index(baseline_tree: &Path) -> Option<PathBuf> {
    let index = baseline_tree.parent()?.join("index");
    index.is_file().then_some(index)
}

/// Clone `source`'s contents into the existing directory `destination`, file
/// by file. Directories and symlinks are recreated; regular files share their
/// extents with the baseline via reflink clones issued from a small thread
/// pool — each clone is an independent metadata operation, and the serial
/// walk `cp -R` performs is what made this path slow on large trees.
pub(super) fn clone_tree(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, destination);
        bail!("CoW tree cloning is not supported on this platform");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut files = Vec::new();
        collect_tree(source, destination, &mut files)
            .with_context(|| format!("replicate baseline tree {}", source.display()))?;
        clone_files(&files)
    }
}

/// Clone one non-directory baseline entry. Symlinks are recreated rather than
/// cloned, matching `clone_tree`'s handling; regular files share extents.
pub(super) fn clone_path(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (source, destination);
        bail!("CoW cloning is not supported on this platform");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if fs::symlink_metadata(source)?.file_type().is_symlink() {
            let link = fs::read_link(source)?;
            std::os::unix::fs::symlink(link, destination)?;
            return Ok(());
        }
        clone_file(source, destination)
            .with_context(|| format!("clone {} -> {}", source.display(), destination.display()))
    }
}

/// Recreate directories and symlinks now; queue regular files for cloning.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn collect_tree(
    source: &Path,
    destination: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if kind.is_dir() {
            fs::create_dir(&to)?;
            collect_tree(&from, &to, files)?;
        } else if kind.is_symlink() {
            let link = fs::read_link(&from)?;
            std::os::unix::fs::symlink(link, &to)?;
        } else if kind.is_file() {
            files.push((from, to));
        } else {
            // Baselines come from `git checkout-index`, which only writes
            // files and symlinks; anything else means the cache was tampered
            // with, and a partial clone must not pass for a checkout.
            bail!("unsupported baseline entry: {}", from.display());
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn clone_files(jobs: &[(PathBuf, PathBuf)]) -> Result<()> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    if jobs.is_empty() {
        return Ok(());
    }
    let workers = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(CLONE_WORKERS_MAX)
        .min(jobs.len());
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| loop {
                    if failed.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let Some((from, to)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        return Ok(());
                    };
                    if let Err(error) = clone_file(from, to) {
                        failed.store(true, Ordering::Relaxed);
                        return Err(error).with_context(|| {
                            format!("clone {} -> {}", from.display(), to.display())
                        });
                    }
                })
            })
            .collect();
        let mut result = Ok(());
        for handle in handles {
            let outcome = handle.join().expect("clone worker panicked");
            if result.is_ok() {
                result = outcome;
            }
        }
        result
    })
}

/// What a prune did, and what the baseline cache still costs.
///
/// The retained size is the cache's own on-disk footprint. Baselines are the
/// originals every clone shares extents with, so this is the physical price of
/// keeping them — the one disk number about simgit that `du` reports honestly.
pub(super) struct PruneOutcome {
    pub removed: Vec<String>,
    pub retained: Vec<String>,
    pub retained_bytes: u64,
    /// An allocation held the cache lock, so nothing published was dropped.
    pub cache_busy: bool,
}

/// Name prefix of a baseline `prune` has moved out of the cache to delete.
/// Nothing reads from one, so every `prune` finishes deleting any it finds,
/// such as one left behind by a `prune` that was killed mid-deletion.
const DOOMED_PREFIX: &str = ".doomed-";

/// A read-only snapshot of the immutable checkout cache.
pub(super) struct BaselineInventory {
    pub root: PathBuf,
    pub retained: Vec<String>,
    pub retained_bytes: u64,
}

/// Inventory cached baselines without touching their access times or pruning
/// incomplete/old entries.
pub(super) fn baseline_inventory(common_git_dir: &Path) -> Result<BaselineInventory> {
    let root = state_dir(common_git_dir).join("baselines");
    let mut retained = Vec::new();
    let mut retained_bytes = 0;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BaselineInventory {
                root,
                retained,
                retained_bytes,
            });
        }
        Err(error) => return Err(error).context("read baseline cache"),
    };
    for entry in entries {
        let entry = entry?;
        retained_bytes += tree_size(&entry.path());
        retained.push(entry.file_name().to_string_lossy().into_owned());
    }
    retained.sort();
    Ok(BaselineInventory {
        root,
        retained,
        retained_bytes,
    })
}

/// Drop cached baselines nothing will ask for again, keeping overlay lowers.
///
/// A published baseline goes when `all` is set, when it has not been used for
/// `BASELINE_MAX_AGE`, or when `reachable` says no ref or worktree reaches its
/// commit: after a rebase and force-push, only an explicit object id could
/// reuse it. Those candidates are chosen without the lock, because deciding
/// reachability runs Git. Then, holding the cache lock exclusively, which no
/// allocation using a baseline can be holding, `protected` is read and every
/// candidate it does not name is renamed out of the cache. Deleting happens
/// after the lock is released, so allocations wait only for those renames.
///
/// When an allocation holds the lock, nothing published is dropped and
/// `cache_busy` is set; with `all`, which promises an emptied cache, that is
/// an error to retry instead. Temporary trees are builds in progress, and go
/// only after `BASELINE_MAX_AGE`, never merely because `all` is set.
pub(super) fn prune_baselines(
    common_git_dir: &Path,
    all: bool,
    protected: &dyn Fn() -> HashSet<PathBuf>,
    reachable: &dyn Fn(&str) -> Result<bool>,
) -> Result<PruneOutcome> {
    let mut outcome = PruneOutcome {
        removed: Vec::new(),
        retained: Vec::new(),
        retained_bytes: 0,
        cache_busy: false,
    };
    let root = state_dir(common_git_dir).join("baselines");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(error) => return Err(error).context("read baseline cache"),
    };
    let now = SystemTime::now();
    let mut candidates = Vec::new();
    let mut doomed = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(DOOMED_PREFIX) {
            doomed.push(path);
            continue;
        }
        let ready = path.join("ready");
        let age_source = if ready.is_file() { &ready } else { &path };
        let metadata = match age_source.metadata() {
            Ok(metadata) => metadata,
            // A concurrent prune moved it out of the cache.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("inspect baseline {name}")),
        };
        let old = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= BASELINE_MAX_AGE);
        if name.starts_with('.') {
            if old {
                doomed.push(path);
                outcome.removed.push(name);
            } else {
                outcome.retained_bytes += tree_size(&path);
                outcome.retained.push(name);
            }
        } else if all || old || (ready.is_file() && is_object_id(&name) && !reachable(&name)?) {
            candidates.push((name, path));
        } else {
            outcome.retained_bytes += tree_size(&path);
            outcome.retained.push(name);
        }
    }

    if !candidates.is_empty() {
        let Some(lock) = BaselineCacheLock::try_exclusive(common_git_dir)? else {
            if all {
                bail!(
                    "the baseline cache is in use by an allocation; \
                     retry `simgit prune --all` once it finishes"
                );
            }
            outcome.cache_busy = true;
            for (name, path) in candidates {
                outcome.retained_bytes += tree_size(&path);
                outcome.retained.push(name);
            }
            return finish_pruning(outcome, doomed);
        };
        // Read only now: an overlay that finished mounting before the lock
        // was taken has written the marker naming its lower by this point.
        let protected = protected();
        for (name, path) in candidates {
            if protected.contains(&path.join("tree")) {
                outcome.retained_bytes += tree_size(&path);
                outcome.retained.push(name);
                continue;
            }
            let destination = root.join(format!("{DOOMED_PREFIX}{name}-{}", Uuid::new_v4()));
            match fs::rename(&path, &destination) {
                Ok(()) => {
                    doomed.push(destination);
                    outcome.removed.push(name);
                }
                // A concurrent prune moved it first.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("drop baseline {name}"));
                }
            }
        }
        drop(lock);
    }
    finish_pruning(outcome, doomed)
}

/// Delete what pruning moved out of the cache. A concurrent prune may be
/// deleting the same entries, so anything already gone is fine.
fn finish_pruning(outcome: PruneOutcome, doomed: Vec<PathBuf>) -> Result<PruneOutcome> {
    for path in doomed {
        let removed = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match removed {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("delete {}", path.display()));
            }
        }
    }
    Ok(outcome)
}

/// True for a full SHA-1 or SHA-256 object id: the only names `ensure_baseline`
/// publishes, and the only ones safe to hand to Git as a commit.
fn is_object_id(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Bytes allocated under `path`, skipping anything unreadable.
fn tree_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        total += if metadata.is_dir() {
            tree_size(&entry.path())
        } else {
            metadata.blocks() * 512
        };
    }
    total
}

fn touch(path: &Path) -> Result<()> {
    let contents = fs::read(path)?;
    fs::write(path, contents)?;
    Ok(())
}
