use super::{
    absolute_path, cow, default_worktree_path, discover_repo, emit, filesystem_name,
    git_output_common, lexical_normalize, main_worktree, nearest_existing_directory, resolved_path,
    select_populate_mode, stale_worktree_registrations, RepoContext,
};
use anyhow::{Context, Result};
use serde_json::json;
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Report repository and platform capabilities without changing Git
/// registrations or pruning caches.
///
/// Being outside a repository is an answer, not a failure: `doctor` is the
/// command an agent runs to find out whether this directory is usable at all,
/// so the repository-dependent facts are reported as `null` and the checks
/// that only need the filesystem still run.
pub fn doctor(json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match discover_repo(&cwd) {
        Ok(repo) => doctor_in_repository(&repo, json),
        Err(_) => doctor_without_repository(&cwd, json),
    }
}

fn doctor_in_repository(repo: &RepoContext, json: bool) -> Result<()> {
    let root = absolute_path(
        default_worktree_path(&repo.common_git_dir, "doctor-probe")?
            .parent()
            .context("default worktree has no root")?
            .to_path_buf(),
    )?;
    let root = lexical_normalize(&root);
    let probe = nearest_existing_directory(&root)?;
    let filesystem = filesystem_name(&probe);
    let cow_supported = cow::clone_supported(&repo.common_git_dir, &probe).ok();
    let populate_mode = select_populate_mode(repo, &probe, false)?.label();
    let git_worktree = git_output_common(repo, ["worktree", "list", "--porcelain", "-z"])?;
    let git_worktree_supported = git_worktree.status.success();
    let stale = if git_worktree_supported {
        stale_worktree_registrations(&git_worktree.stdout)
    } else {
        Vec::new()
    };
    let baselines = cow::baseline_inventory(&repo.common_git_dir)?;
    let baseline_count = baselines.retained.len();
    let repository = repo.top_level.display().to_string();
    let common_git_owner = main_worktree(&repo.common_git_dir);
    let root_inside_repository = resolved_path(&root)?.starts_with(resolved_path(&repo.top_level)?);
    let is_main_worktree = resolved_path(&repo.top_level)? == resolved_path(common_git_owner)?;

    if json {
        emit(&json!({
            "product": "simgit",
            "version": env!("CARGO_PKG_VERSION"),
            "identity": "simgit",
            "repository": repository,
            "repository_details": {
                "top_level": repository,
                "common_git_dir": repo.common_git_dir.display().to_string(),
                "common_git_owner": common_git_owner.display().to_string(),
                "is_main_worktree": is_main_worktree,
            },
            "filesystem": filesystem,
            "cow_supported": cow_supported,
            "populate_mode": populate_mode,
            "default_worktree_root": root.display().to_string(),
            "default_worktree_root_inside_repository": root_inside_repository,
            "git_worktree_supported": git_worktree_supported,
            "stale_worktree_registrations": stale,
            "baseline_cache": {
                "root": baselines.root.display().to_string(),
                "retained": baselines.retained,
                "retained_count": baseline_count,
                "retained_bytes": baselines.retained_bytes,
            },
        }));
    } else {
        println!("simgit {}", env!("CARGO_PKG_VERSION"));
        println!("repository: {}", repo.top_level.display());
        println!("common git dir: {}", repo.common_git_dir.display());
        println!("filesystem: {filesystem}");
        println!("CoW: {}", cow_status(cow_supported));
        println!("populate mode: {populate_mode}");
        println!(
            "worktree root: {} ({})",
            root.display(),
            if root_inside_repository {
                "unsafe: inside repository"
            } else {
                "safe"
            }
        );
        println!(
            "Git worktrees: {} ({} stale registration(s))",
            if git_worktree_supported {
                "supported"
            } else {
                "unavailable"
            },
            stale.len()
        );
        println!(
            "baseline cache: {} retained ({:.1} MiB)",
            baseline_count,
            baselines.retained_bytes as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

/// Report what can be known without a repository: simgit's identity and
/// version, and what the current directory's filesystem can do. Everything
/// derived from Git registrations, the worktree root or the baseline cache is
/// `null`, since there is nothing to derive it from.
fn doctor_without_repository(cwd: &Path, json: bool) -> Result<()> {
    let probe = nearest_existing_directory(cwd)?;
    let filesystem = filesystem_name(&probe);
    let cow_supported = cow_supported_without_repository(&probe);

    if json {
        emit(&json!({
            "product": "simgit",
            "version": env!("CARGO_PKG_VERSION"),
            "identity": "simgit",
            "repository": null,
            "repository_details": null,
            "filesystem": filesystem,
            "cow_supported": cow_supported,
            "populate_mode": null,
            "default_worktree_root": null,
            "default_worktree_root_inside_repository": null,
            "git_worktree_supported": null,
            "stale_worktree_registrations": [],
            "baseline_cache": null,
        }));
    } else {
        println!("simgit {}", env!("CARGO_PKG_VERSION"));
        println!(
            "repository: none ({} is not in a Git working tree)",
            cwd.display()
        );
        println!("filesystem: {filesystem}");
        println!("CoW: {}", cow_status(cow_supported));
        println!(
            "worktree root, populate mode, Git worktree support and baseline cache: \
             run simgit doctor inside a repository"
        );
    }
    Ok(())
}

/// Probe filesystem cloning where there is no repository state directory to
/// hold the probe's source file.
///
/// Both probe files go in a scratch directory inside the directory being
/// probed — they have to share its filesystem for the answer to mean anything
/// — and the scratch directory takes them with it when it is removed.
fn cow_supported_without_repository(dir: &Path) -> Option<bool> {
    let scratch = dir.join(format!(".simgit-doctor-probe-{}", Uuid::new_v4()));
    if fs::create_dir(&scratch).is_err() {
        // Nothing was measured: an unwritable directory says something about
        // this process's permissions, not about what the filesystem can do.
        return None;
    }
    let supported = cow::clone_supported(&scratch, &scratch).ok();
    let _ = fs::remove_dir_all(&scratch);
    supported
}

/// How `doctor` words a copy-on-write verdict. "Unavailable" is a measured
/// answer, so an unmeasurable one must not borrow it: reporting `filesystem:
/// apfs, CoW: unavailable` because a probe could not be written contradicts
/// itself.
fn cow_status(supported: Option<bool>) -> &'static str {
    match supported {
        Some(true) => "supported",
        Some(false) => "unavailable",
        None => "unknown (cannot write a probe here)",
    }
}
