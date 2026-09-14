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

/// Return an immutable checkout cache for `commit`.
///
/// Every creator materializes into a unique temporary directory and publishes
/// with one atomic rename. Concurrent losers discard their temporary tree and
/// use the winner; no process removes or mutates a published baseline.
pub(super) fn ensure_baseline(repo: &RepoContext, commit: &str) -> Result<PathBuf> {
    let root = state_dir(&repo.common_git_dir).join("baselines");
    let final_dir = root.join(commit);
    let final_tree = final_dir.join("tree");
    let ready = final_dir.join("ready");
    if ready.is_file() && final_tree.is_dir() {
        touch(&ready)?;
        return Ok(final_tree);
    }

    fs::create_dir_all(&root)?;
    if final_dir.exists() {
        bail!(
            "cached baseline {} is incomplete; run `simgit prune --all`",
            final_dir.display()
        );
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
            // Another process won the atomic publish race.
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error).with_context(|| format!("publish baseline {commit}"));
        }
    }
    let _ = fs::remove_dir_all(&temporary);
    Ok(final_tree)
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
}

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

pub(super) fn prune_baselines(
    common_git_dir: &Path,
    all: bool,
    protected_trees: &HashSet<PathBuf>,
) -> Result<PruneOutcome> {
    let mut outcome = PruneOutcome {
        removed: Vec::new(),
        retained: Vec::new(),
        retained_bytes: 0,
    };
    let root = state_dir(common_git_dir).join("baselines");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(error) => return Err(error).context("read baseline cache"),
    };
    let now = SystemTime::now();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if protected_trees.contains(&path.join("tree")) {
            outcome.retained.push(name);
            outcome.retained_bytes += tree_size(&path);
            continue;
        }
        let temporary = name.starts_with('.');
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
        // `--all` must not race a concurrent creator. Temporary trees are
        // removed only after the normal stale threshold, never merely because
        // an explicit full prune is running.
        if (all && !temporary) || old {
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
            outcome.removed.push(name);
        } else {
            outcome.retained_bytes += tree_size(&path);
            outcome.retained.push(name);
        }
    }
    Ok(outcome)
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
