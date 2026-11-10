//! Repository discovery, paths, and the `git` process helpers.

use super::registry::worktree_admin_dir;
use super::{RepoContext, Superproject};
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

pub(super) fn nearest_existing_directory(path: &Path) -> Result<PathBuf> {
    let mut candidate = path;
    loop {
        if candidate.is_dir() {
            return Ok(candidate.to_path_buf());
        }
        candidate = candidate
            .parent()
            .with_context(|| format!("no existing parent for {}", path.display()))?;
    }
}

pub(super) fn resolved_path(path: &Path) -> Result<PathBuf> {
    let mut candidate = path;
    let mut missing = Vec::new();
    while !candidate.exists() {
        let name = candidate
            .file_name()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
        missing.push(name.to_os_string());
        candidate = candidate
            .parent()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
    }
    let mut resolved = candidate
        .canonicalize()
        .with_context(|| format!("resolve {}", candidate.display()))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

pub(super) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// One stable form for a path simgit prints, returns or compares: symlinks
/// resolved as far as the path exists, then `.` and `..` removed.
///
/// On macOS `/tmp/x` and `/private/tmp/x` are one directory, and `--path
/// ../wt` from inside the repository yields `/repo/../wt`. An orchestrator
/// comparing the path it passed in against the path simgit hands back would
/// otherwise see a mismatch, and a lexical inside-the-repository test would
/// reject a destination that is nowhere near it.
pub(super) fn canonical_path(path: &Path) -> PathBuf {
    // A path whose ancestors cannot be resolved is still usable as given.
    let resolved = resolved_path(path).unwrap_or_else(|_| path.to_path_buf());
    lexical_normalize(&resolved)
}

/// The outermost working tree holding the main one: a submodule's superproject.
pub(super) fn enclosing_working_tree(repo: &RepoContext) -> &Path {
    match &repo.superproject {
        Some(superproject) => &superproject.working_tree,
        None => &repo.main_worktree,
    }
}

/// The filesystem type backing `path`, or `"unknown"`.
///
/// macOS `stat -f %T` prints a file-*type* suffix (`/` for a directory), not
/// the filesystem, so the mount table is consulted instead: the longest mount
/// point that prefixes the path owns it, and its type is the first attribute
/// in parentheses (`/dev/disk3s5 on / (apfs, local, journaled)`).
#[cfg(target_os = "macos")]
pub(super) fn filesystem_name(path: &Path) -> String {
    const UNKNOWN: &str = "unknown";
    let Ok(target) = path.canonicalize() else {
        return UNKNOWN.to_owned();
    };
    let Ok(output) = Command::new("/sbin/mount").output() else {
        return UNKNOWN.to_owned();
    };
    if !output.status.success() {
        return UNKNOWN.to_owned();
    }
    let table = String::from_utf8_lossy(&output.stdout);
    let mut best: Option<(usize, &str)> = None;
    for line in table.lines() {
        let Some((_, rest)) = line.split_once(" on ") else {
            continue;
        };
        let Some((mount_point, attributes)) = rest.rsplit_once(" (") else {
            continue;
        };
        if !target.starts_with(mount_point) {
            continue;
        }
        let kind = attributes
            .trim_end_matches(')')
            .split(',')
            .next()
            .unwrap_or("")
            .trim();
        if kind.is_empty() {
            continue;
        }
        let depth = mount_point.len();
        match best {
            Some((deepest, _)) if deepest >= depth => {}
            _ => best = Some((depth, kind)),
        }
    }
    best.map_or_else(|| UNKNOWN.to_owned(), |(_, kind)| kind.to_owned())
}

/// The filesystem type backing `path`, or `"unknown"`.
#[cfg(target_os = "linux")]
pub(super) fn filesystem_name(path: &Path) -> String {
    Command::new("stat")
        .args(["-f", "-c", "%T", "--"])
        .arg(path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The filesystem type backing `path`, or `"unknown"`.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn filesystem_name(_path: &Path) -> String {
    "unknown".to_owned()
}

pub(super) fn discover_repo(path: &Path) -> Result<RepoContext> {
    let (top_level, common_git_dir) = working_tree_at(path)?;
    let main_worktree = find_main_worktree(&top_level, &common_git_dir);
    let superproject = find_superproject(&main_worktree, &common_git_dir);
    Ok(RepoContext {
        top_level,
        common_git_dir,
        main_worktree,
        superproject,
    })
}

/// The top level of the working tree at `path` and its repository's common
/// git dir, from one git: every command starts here.
pub(super) fn working_tree_at(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let found = git_path_output(
        path,
        [
            "rev-parse",
            "--show-toplevel",
            "--path-format=absolute",
            "--git-common-dir",
        ],
    )
    .context("not in a Git working tree")?;
    let mut lines = found.lines();
    let (Some(top_level), Some(common)) = (lines.next(), lines.next()) else {
        bail!("not in a Git working tree: git rev-parse printed {found:?}");
    };
    Ok((PathBuf::from(top_level), PathBuf::from(common)))
}

/// The outermost superproject of the submodule checked out at `main_worktree`,
/// asked from that checkout: a linked worktree of the submodule lies in no
/// superproject. `None` when there is none, or it cannot be reached; default
/// worktrees then go beside `main_worktree`, as for any repository.
///
/// The submodule's directory is its path in that superproject, flattened, plus
/// a digest of its common git dir: same-named submodules, and the module git
/// dirs each superproject worktree keeps, each get their own.
fn find_superproject(main_worktree: &Path, common_git_dir: &Path) -> Option<Superproject> {
    // Git finds a superproject from the directory above, so it needs a `.git`
    // somewhere up there; most repositories have none and run no `git` here.
    if !main_worktree
        .ancestors()
        .skip(1)
        .any(|dir| fs::symlink_metadata(dir.join(".git")).is_ok())
    {
        return None;
    }
    // One `git` per level of nesting. Each answer must be a strict ancestor of
    // the last, so the walk ends.
    let mut working_tree = None;
    let mut current = main_worktree.to_path_buf();
    while let Some(parent) =
        git_path_output(&current, ["rev-parse", "--show-superproject-working-tree"])
            .ok()
            .filter(|parent| !parent.is_empty())
            .map(PathBuf::from)
            .filter(|parent| *parent != current && current.starts_with(parent))
    {
        current = parent.clone();
        working_tree = Some(parent);
    }
    let working_tree = working_tree?;
    let common = git_path_output(
        &working_tree,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let super_main = find_main_worktree(&working_tree, Path::new(&common));
    let relative = main_worktree.strip_prefix(&working_tree).ok()?;
    let module_dir = format!(
        "{}-{}",
        flatten_path_component(&relative.to_string_lossy()),
        short_digest(canonical_path(common_git_dir).to_string_lossy().as_bytes())
    );
    Some(Superproject {
        working_tree,
        main_worktree: super_main,
        module_dir,
    })
}

/// The directory holding the main working tree. It costs a `git`, so it is
/// found once, with the `RepoContext`.
///
/// `git worktree list` cannot say: it names the common git dir whenever that is
/// not called `.git` (a submodule, `--separate-git-dir`), ignoring the
/// `core.worktree` that a submodule's git dir sets. Asked from the common dir,
/// `rev-parse --show-toplevel` honours it, answers with the git dir itself when
/// it is unset, and fails in a bare repository. With no main working tree (a
/// bare repository not called `.git`), or none recorded (a linked worktree of a
/// `--separate-git-dir` repository), this is the common git dir, as before.
fn find_main_worktree(top_level: &Path, common_git_dir: &Path) -> PathBuf {
    let common = canonical_path(common_git_dir);
    let configured = Command::new("git")
        .current_dir(common_git_dir)
        .arg(format!("--git-dir={}", common_git_dir.display()))
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|top| canonical_path(Path::new(top.trim())))
        .filter(|top| *top != common);
    let main = if let Some(configured) = configured {
        configured
    } else if common_git_dir.file_name() == Some(OsStr::new(".git")) {
        common_git_dir
            .parent()
            .unwrap_or(common_git_dir)
            .to_path_buf()
    } else if worktree_admin_dir(top_level).is_ok_and(|admin| canonical_path(&admin) == common) {
        // Standing in the main working tree of a `--separate-git-dir`
        // repository. Ask its top level, where `.git` is: a subdirectory has none.
        top_level.to_path_buf()
    } else {
        common_git_dir.to_path_buf()
    };
    // Every `git_common_command` runs there, so it must exist.
    if main.is_dir() {
        main
    } else {
        common_git_dir.to_path_buf()
    }
}

pub(super) fn git_path_output<const N: usize>(path: &Path, args: [&str; N]) -> Result<String> {
    let output = git_command_at(path)?
        .args(args)
        .output()
        .context("run git")?;
    if !output.status.success() {
        return Err(git_failure("git rev-parse", &output));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Resolves `base` as the caller sees it, so `HEAD` and `HEAD~1` mean the
/// worktree simgit was run from; the main checkout's `HEAD` is not the caller's
/// inside a linked worktree.
pub(super) fn resolve_commit(repo: &RepoContext, base: &str) -> Result<String> {
    let spec = format!("{base}^{{commit}}");
    let output = git_output_at(&repo.top_level, ["rev-parse", "--verify", &spec])?;
    if !output.status.success() {
        // A fresh `git init` has an unborn HEAD, which is the first thing a
        // new user hits; say so instead of reporting an unresolvable rev.
        if base == "HEAD"
            && !git_output_at(
                &repo.top_level,
                ["rev-parse", "--verify", "--quiet", "HEAD"],
            )?
            .status
            .success()
        {
            bail!("repository has no commits yet; make one before creating a worktree");
        }
        bail!("cannot resolve base commit '{base}'");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Default location for a new worktree: `<repo-parent>/.simgit/<repo>/<branch>`.
/// A submodule's go beside its outermost superproject's main working tree, at
/// `.simgit/<super>/.modules/<submodule>/<branch>`, outside every enclosing
/// working tree.
///
/// This must stay outside the common git dir. Agent harnesses and editors
/// treat everything under `.git/` as off-limits — Claude Code refuses to edit
/// files there — so a worktree nested in the git dir is unusable by exactly
/// the tools simgit exists to serve. It must also stay on the repository's
/// filesystem, since reflink/clonefile cannot cross volumes; a sibling of the
/// main working tree satisfies both. `SIMGIT_WORKTREE_ROOT` overrides it.
pub(super) fn default_worktree_path(repo: &RepoContext, branch: &str) -> Result<PathBuf> {
    let root = match std::env::var_os("SIMGIT_WORKTREE_ROOT") {
        Some(root) if !root.is_empty() => PathBuf::from(root),
        _ => match &repo.superproject {
            // `.modules` is never a branch's directory: Git refuses ref
            // components that start with `.`.
            Some(superproject) => worktree_root_beside(&superproject.main_worktree)?
                .join(".modules")
                .join(&superproject.module_dir),
            None => worktree_root_beside(&repo.main_worktree)?,
        },
    };
    Ok(root.join(safe_path_component(branch)))
}

/// `<home-parent>/.simgit/<home>`.
fn worktree_root_beside(home: &Path) -> Result<PathBuf> {
    let name = home
        .file_name()
        .context("repository has no directory name")?;
    let parent = home.parent().with_context(|| {
        format!(
            "{} has no parent directory for worktrees; pass --path or set SIMGIT_WORKTREE_ROOT",
            home.display()
        )
    })?;
    Ok(parent.join(".simgit").join(name))
}

pub(super) fn safe_path_component(branch: &str) -> String {
    let mut name = flatten_path_component(branch);
    if name.is_empty() || name == "." || name == ".." {
        name.insert_str(0, "branch-");
    }
    if name != branch {
        name.push('-');
        name.push_str(&short_digest(branch.as_bytes()));
    }
    name
}

/// `text` with every character a single path component should not hold
/// replaced by `-`.
fn flatten_path_component(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Eight hex digits of FNV-1a over `bytes`.
fn short_digest(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{:08x}", hash as u32)
}

pub(super) fn absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(super) fn state_dir(common_git_dir: &Path) -> PathBuf {
    common_git_dir.join("simgit")
}

/// Run a Git command that reads every worktree registration without the
/// registry lock, retrying when it died on one another allocation was half-way
/// through writing: its `commondir` not yet written, or its HEAD still the
/// placeholder of zeros `git worktree add` writes first. Used where holding
/// the lock would stall every allocation (a network fetch, a large removal,
/// a reachability check); a Git that skipped half-written registrations
/// would make it unnecessary.
pub(super) fn output_retrying_half_written_registration(command: &mut Command) -> Result<Output> {
    // Untranslated messages: the match below, and callers, read them.
    command.env("LC_ALL", "C");
    let mut backoff = [50, 100].into_iter();
    loop {
        let output = command.output().context("run git")?;
        let complaint = String::from_utf8_lossy(&output.stderr);
        let raced = !output.status.success()
            && ((complaint.contains("failed to read") && complaint.contains("commondir"))
                || complaint.lines().any(|line| {
                    line.contains("bad object worktrees/") && line.trim_end().ends_with("/HEAD")
                }));
        match backoff.next() {
            Some(millis) if raced => std::thread::sleep(Duration::from_millis(millis)),
            _ => return Ok(output),
        }
    }
}

pub(super) fn run_retrying_half_written_registration(
    command: &mut Command,
    description: &str,
) -> Result<()> {
    let output = output_retrying_half_written_registration(command)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(description, &output))
    }
}

/// A `git` invocation against the repository's common git dir, anchored in a
/// working directory that outlives the operation.
///
/// simgit routinely deletes the directory it was launched from: `remove` and
/// `gc` both reap the very worktree the caller is standing in. Every later
/// `git` that inherited that directory then dies with `fatal: Unable to read
/// current working directory`, which is how `remove --delete-branch` came to
/// delete a worktree, keep its branch and report failure. The main working
/// tree is the one directory a worktree operation cannot remove;
/// `repo.top_level` is not a substitute, because inside a linked worktree it
/// *is* the directory being removed.
pub(super) fn git_common_command(repo: &RepoContext) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(&repo.main_worktree)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()));
    command
}

/// `git -C <path>`, started in `path` or its nearest existing ancestor rather
/// than in the inherited working directory, which simgit may have deleted.
pub(super) fn git_command_at(path: &Path) -> Result<Command> {
    let path = absolute_path(path.to_path_buf())?;
    let mut command = Command::new("git");
    command
        .current_dir(nearest_existing_directory(&path)?)
        .arg("-C")
        .arg(&path);
    Ok(command)
}

pub(super) fn run_git_at<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
    let mut command = git_command_at(path)?;
    command.args(args);
    run_command(&mut command, "git")
}

pub(super) fn git_output_common<const N: usize>(
    repo: &RepoContext,
    args: [&str; N],
) -> Result<Output> {
    git_common_command(repo)
        .args(args)
        .output()
        .context("run git")
}

pub(super) fn git_output_at<const N: usize>(path: &Path, args: [&str; N]) -> Result<Output> {
    git_command_at(path)?.args(args).output().context("run git")
}

pub(super) fn run_command(command: &mut Command, description: &str) -> Result<()> {
    let output = command.output().with_context(|| description.to_owned())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(description, &output))
    }
}

pub(super) fn git_failure(description: &str, output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    anyhow::anyhow!("{description} failed ({}): {detail}", output.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_become_unique_safe_path_components() {
        let nested = safe_path_component("feat/auth");
        assert!(nested.starts_with("feat-auth-"));
        assert_ne!(nested, safe_path_component("feat-auth"));
        assert!(!safe_path_component("../../escape").contains('/'));
        assert!(safe_path_component("..").starts_with("branch-"));
    }
}
