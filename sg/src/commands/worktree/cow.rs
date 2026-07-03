use super::{run_command, state_dir, RepoContext};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

const BASELINE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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
            "cached baseline {} is incomplete; run `sg worktree prune --all`",
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

pub(super) fn clone_tree(source: &Path, destination: &Path) -> Result<()> {
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

pub(super) fn prune_baselines(
    common_git_dir: &Path,
    all: bool,
    protected_trees: &HashSet<PathBuf>,
) -> Result<usize> {
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
        if protected_trees.contains(&path.join("tree")) {
            continue;
        }
        let temporary = entry.file_name().to_string_lossy().starts_with('.');
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
