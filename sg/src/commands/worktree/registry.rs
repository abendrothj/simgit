//! Reading Git's worktree registry: `list`, target lookup, markers.

use super::locks::{lock_present, lock_state, RegistryLock};
use super::maintenance::registered_worktree;
use super::repo::{
    absolute_path, canonical_path, discover_repo, git_failure, git_output_at, git_output_common,
};
use super::{overlay, PopulateMode, RepoContext};
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

/// Run git on the registration `admin`, from the main checkout.
pub(super) fn git_at_admin(repo: &RepoContext, admin: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .current_dir(&repo.main_worktree)
        .arg(format!("--git-dir={}", admin.display()))
        .args(args)
        .output()
        .context("run git")
}

/// A registration's HEAD: the branch ref it names, or the commit it holds.
pub(super) enum AdminHead {
    Branch(String),
    Detached(String),
}

/// Read the HEAD of the registration `admin` through Git, never from its
/// `HEAD` file: under reftable that file is a stub naming
/// `refs/heads/.invalid`, detached or not. A HEAD Git cannot read is an error.
pub(super) fn admin_head(repo: &RepoContext, admin: &Path) -> Result<AdminHead> {
    let symbolic = git_at_admin(repo, admin, &["symbolic-ref", "-q", "HEAD"])?;
    match symbolic.status.code() {
        Some(0) => Ok(AdminHead::Branch(
            String::from_utf8(symbolic.stdout)?.trim().to_owned(),
        )),
        Some(1) => {
            let commit = git_at_admin(
                repo,
                admin,
                &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            )?;
            if !commit.status.success() {
                return Err(git_failure("git rev-parse HEAD", &commit));
            }
            Ok(AdminHead::Detached(
                String::from_utf8(commit.stdout)?.trim().to_owned(),
            ))
        }
        _ => Err(git_failure("git symbolic-ref HEAD", &symbolic)),
    }
}

pub(super) fn short_branch(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

/// Whether a removal target names a filesystem location rather than a branch.
/// Branch names may contain slashes, so only the spellings Git rejects as ref
/// names — absolute and explicitly relative paths — are decided here.
pub(super) fn spec_is_path(spec: &str) -> bool {
    Path::new(spec).is_absolute()
        || spec == "."
        || spec == ".."
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('~')
}

/// What a user-supplied worktree reference resolved to.
pub(super) enum TargetLookup {
    /// A live worktree: an existing registered path, or the worktree checked
    /// out on a branch.
    Found(PathBuf),
    /// A directory that exists where a worktree used to be, but that Git no
    /// longer registers — the residue of an interrupted teardown.
    Stray(PathBuf),
    /// A registered worktree whose directory was deleted by hand; see
    /// `registration_gone`.
    Gone(PathBuf),
    /// The reference names no worktree: an already-removed path, or a branch
    /// that has no worktree.
    Absent(String),
}

/// Resolve a user-supplied worktree reference — an explicit path, a branch
/// name, or (when omitted) the worktree containing the current directory — to
/// an absolute worktree path.
pub(super) fn lookup_worktree_target(
    repo: &RepoContext,
    target: Option<&str>,
) -> Result<TargetLookup> {
    // No target is the worktree the caller stands in: no listing, which a
    // broken registration elsewhere could make fail.
    if target.is_none() {
        return Ok(TargetLookup::Found(repo.top_level.clone()));
    }
    lookup_worktree_target_in(repo, &list_worktrees(repo)?, target)
}

/// `lookup_worktree_target` against a listing of the registry already made,
/// so a caller that needs the entry too lists it once.
pub(super) fn lookup_worktree_target_in(
    repo: &RepoContext,
    entries: &[WorktreeEntry],
    target: Option<&str>,
) -> Result<TargetLookup> {
    let Some(spec) = target else {
        return Ok(TargetLookup::Found(repo.top_level.clone()));
    };
    let as_path = absolute_path(PathBuf::from(spec))?;
    let canonical = canonical_path(&as_path);
    let stray = if as_path.exists() {
        if is_registered_worktree(repo, entries, &canonical) {
            return Ok(TargetLookup::Found(canonical));
        }
        // An unregistered directory must not shadow a branch of the same name,
        // so it is remembered rather than returned here.
        Some(canonical)
    } else if registration_gone(repo, &canonical)
        && is_registered_worktree(repo, entries, &canonical)
    {
        return Ok(TargetLookup::Gone(canonical));
    } else {
        None
    };
    if let Some(path) = path_for_branch(entries, spec) {
        return Ok(if registration_gone(repo, &path) {
            TargetLookup::Gone(path)
        } else {
            TargetLookup::Found(path)
        });
    }
    if let Some(path) = overlay::worktree_for_branch(repo, spec) {
        return Ok(TargetLookup::Found(path));
    }
    match stray {
        Some(path) => Ok(TargetLookup::Stray(path)),
        None => Ok(TargetLookup::Absent(spec.to_owned())),
    }
}

/// Whether `path` is still a registered worktree of this repository.
///
/// Existence is not the test. A `git worktree remove` that deleted the
/// registration but could not unlink the directory leaves a shell behind, and
/// treating that as a live worktree makes every later command interrogate a
/// checkout Git has already forgotten.
fn is_registered_worktree(repo: &RepoContext, entries: &[WorktreeEntry], path: &Path) -> bool {
    entries
        .iter()
        .any(|entry| canonical_path(&entry.path).as_path() == path)
        || overlay::state(repo, path).is_some()
}

/// Whether a worktree's directory is gone while its registration stays: what
/// `rm -rf` leaves. An unreadable path is not gone, and neither is an overlay,
/// which `simgit repair` remounts from the state it keeps elsewhere.
pub(super) fn registration_gone(repo: &RepoContext, path: &Path) -> bool {
    matches!(path.try_exists(), Ok(false)) && overlay::state(repo, path).is_none()
}

/// Find the worktree checked out on `branch`, if any, via Git's registry.
pub(super) fn worktree_path_for_branch(
    repo: &RepoContext,
    branch: &str,
) -> Result<Option<PathBuf>> {
    Ok(path_for_branch(&list_worktrees(repo)?, branch))
}

/// `worktree_path_for_branch` against a listing already made.
fn path_for_branch(entries: &[WorktreeEntry], branch: &str) -> Option<PathBuf> {
    let wanted = format!("refs/heads/{branch}");
    entries
        .iter()
        .find(|entry| entry.branch.as_deref() == Some(wanted.as_str()))
        .map(|entry| entry.path.clone())
}

pub(super) fn worktree_description(
    repo: &RepoContext,
    admins: &HashMap<PathBuf, PathBuf>,
    entry: &WorktreeEntry,
) -> String {
    let branch = entry.branch.as_deref().unwrap_or("(detached)");
    let listed = listed(repo, admins, &entry.path);
    let persistence = if listed.ephemeral {
        "ephemeral"
    } else {
        "persistent"
    };
    let locked = if listed.locked { " locked" } else { "" };
    // Mode is appended, not inserted: existing scripts read fields 1-3.
    let mode = listed.mode.unwrap_or_else(|| "-".to_owned());
    format!(
        "{}\t{}\t{}{}\t{}",
        branch.strip_prefix("refs/heads/").unwrap_or(branch),
        entry.path.to_string_lossy().escape_debug(),
        persistence,
        locked,
        mode
    )
}

pub fn list(json_output: bool) -> Result<()> {
    let repo = discover_repo(&std::env::current_dir()?)?;
    if !json_output {
        let entries = list_worktrees(&repo)?;
        let admins = admin_dirs(&repo);
        for entry in entries {
            println!("{}", worktree_description(&repo, &admins, &entry));
        }
        return Ok(());
    }

    let output = git_worktree_list(&repo)?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut current = serde_json::Map::new();
    for line in String::from_utf8_lossy(&output.stdout).split('\0') {
        if line.is_empty() {
            if !current.is_empty() {
                entries.push(serde_json::Value::Object(std::mem::take(&mut current)));
            }
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, "true"));
        let value = if value == "true" {
            json!(true)
        } else {
            json!(value)
        };
        current.insert(key.to_owned(), value);
    }
    if !current.is_empty() {
        entries.push(serde_json::Value::Object(current));
    }
    if let Some(main) = entries.first_mut() {
        main["worktree"] = json!(repo.main_worktree.display().to_string());
    }
    let admins = admin_dirs(&repo);
    for entry in &mut entries {
        if let Some(path) = entry
            .get("worktree")
            .and_then(|value| value.as_str())
            .map(PathBuf::from)
        {
            let listed = listed(&repo, &admins, &path);
            entry["ephemeral"] = json!(listed.ephemeral);
            entry["mode"] = json!(listed.mode);
            if entry.get("locked").is_none() && listed.locked {
                entry["locked"] = json!("simgit: workspace in use");
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}

/// A linked worktree as reported by `git worktree list --porcelain`.
pub(super) struct WorktreeEntry {
    pub(super) path: PathBuf,
    pub(super) head: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) is_main: bool,
}

/// `git worktree list --porcelain -z`, which reads every registration.
pub(super) fn git_worktree_list(repo: &RepoContext) -> Result<Output> {
    let _registry = RegistryLock::shared(repo)?;
    git_output_common(repo, ["worktree", "list", "--porcelain", "-z"])
}

pub(super) fn list_worktrees(repo: &RepoContext) -> Result<Vec<WorktreeEntry>> {
    let output = git_worktree_list(repo)?;
    if !output.status.success() {
        return Err(git_failure("git worktree list --porcelain", &output));
    }
    let mut entries = parse_worktree_list(&output.stdout);
    // Git names a submodule's git dir here; the main-checkout guards need its checkout.
    if let Some(main) = entries.first_mut() {
        main.path = repo.main_worktree.clone();
    }
    Ok(entries)
}

/// Parse `git worktree list --porcelain -z`; the first entry is the main one.
fn parse_worktree_list(stdout: &[u8]) -> Vec<WorktreeEntry> {
    let text = String::from_utf8_lossy(stdout);
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut head: Option<String> = None;
    let mut branch: Option<String> = None;
    for line in text.split('\0') {
        if line.is_empty() {
            if let Some(path) = path.take() {
                let is_main = entries.is_empty();
                entries.push(WorktreeEntry {
                    path,
                    head: head.take(),
                    branch: branch.take(),
                    is_main,
                });
            }
            head = None;
            branch = None;
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.to_owned());
        }
    }
    if let Some(path) = path.take() {
        let is_main = entries.is_empty();
        entries.push(WorktreeEntry {
            path,
            head,
            branch,
            is_main,
        });
    }
    entries
}

pub(super) fn worktree_admin_dir(worktree: &Path) -> Result<PathBuf> {
    overlay::live_git_dir(worktree)
        .with_context(|| format!("{} names no git dir", worktree.display()))
}

pub(super) fn mark_ephemeral(worktree: &Path) -> Result<()> {
    let admin = worktree_admin_dir(worktree)?;
    fs::write(admin.join("simgit-ephemeral"), b"").context("write ephemeral marker")?;
    Ok(())
}

/// Record how a worktree was populated, so `list` can answer the question the
/// whole tool exists for — is this checkout actually sharing disk? — long
/// after the creating command printed it.
pub(super) fn mark_mode(worktree: &Path, mode: PopulateMode) -> Result<()> {
    let admin = worktree_admin_dir(worktree)?;
    fs::write(admin.join("simgit-mode"), mode.label()).context("write mode marker")?;
    Ok(())
}

/// Every registration's admin dir, keyed by the worktree its `gitdir` names.
/// Read once, so listing spawns no `git` per worktree; an entry that cannot be
/// read is left out, and `listed` finds that worktree's admin dir as before.
pub(super) fn admin_dirs(repo: &RepoContext) -> HashMap<PathBuf, PathBuf> {
    let mut admins = HashMap::new();
    let Ok(entries) = fs::read_dir(repo.common_git_dir.join("worktrees")) else {
        return admins;
    };
    for admin in entries.flatten().map(|entry| entry.path()) {
        if let Ok(Some(worktree)) = registered_worktree(&admin) {
            admins.entry(worktree).or_insert(admin);
        }
    }
    admins
}

/// What `list` reports about a worktree beyond Git's own fields.
struct Listed {
    ephemeral: bool,
    /// The recorded populate mode, or `None` for the main worktree and any
    /// worktree simgit did not create.
    mode: Option<String>,
    locked: bool,
}

fn listed(repo: &RepoContext, admins: &HashMap<PathBuf, PathBuf>, worktree: &Path) -> Listed {
    let admin = admins
        .get(&canonical_path(worktree))
        .cloned()
        .or_else(|| overlay::admin_dir(repo, worktree));
    let Some(admin) = admin else {
        // The main worktree has no admin dir; only `run`'s lock can be there.
        return Listed {
            ephemeral: false,
            mode: None,
            locked: lock_state(repo, worktree) == Some(true),
        };
    };
    let mode = if overlay::read_state(&admin).is_some() {
        Some(PopulateMode::Overlay.label().to_owned())
    } else {
        fs::read_to_string(admin.join("simgit-mode"))
            .ok()
            .map(|recorded| recorded.trim().to_owned())
            .filter(|recorded| !recorded.is_empty())
    };
    Listed {
        ephemeral: admin.join("simgit-ephemeral").is_file(),
        mode,
        locked: lock_present(&admin.join("locked")),
    }
}

/// Whether the worktree whose admin dir is `admin` is marked ephemeral; one
/// with no admin dir is not.
pub(super) fn admin_is_ephemeral(admin: Option<&Path>) -> bool {
    admin.is_some_and(|admin| admin.join("simgit-ephemeral").is_file())
}

/// Time since the worktree was last touched, approximated by the mtime of its
/// index (updated on add/commit/checkout), falling back to the directory mtime.
/// The index is found through the registry, so a deleted worktree ages too.
/// With neither to read nothing records activity, so it counts as idle; gc's
/// lock and uncommitted-work checks still decide whether it can go.
/// `admin` is the worktree's admin dir, `None` when it has none.
pub(super) fn worktree_idle(admin: Option<&Path>, worktree: &Path) -> Duration {
    let index = admin.map(|a| a.join("index"));
    let probe = match index {
        Some(index) if index.is_file() => index,
        _ => worktree.to_path_buf(),
    };

    match fs::metadata(&probe).and_then(|meta| meta.modified()) {
        // A modification time in the future is a write just now on a skewed clock.
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO),
        Err(_) => Duration::MAX,
    }
}

pub(super) fn stale_worktree_registrations(output: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8_lossy(output);
    let mut stale = Vec::new();
    let mut path: Option<&str> = None;
    let mut reason: Option<&str> = None;
    for field in text.split('\0') {
        if let Some(next) = field.strip_prefix("worktree ") {
            if let (Some(path), Some(reason)) = (path.take(), reason.take()) {
                stale.push(json!({ "worktree": path, "reason": reason }));
            }
            path = Some(next);
        } else if let Some(next) = field.strip_prefix("prunable ") {
            reason = Some(next);
        }
    }
    if let (Some(path), Some(reason)) = (path, reason) {
        stale.push(json!({ "worktree": path, "reason": reason }));
    }
    stale
}

pub(super) fn worktree_dirty(worktree: &Path) -> Result<bool> {
    let output = git_output_at(worktree, ["status", "--porcelain"])?;
    if !output.status.success() {
        return Err(git_failure("git status --porcelain", &output));
    }
    Ok(!output.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktree::add::WorktreeKind;
    use crate::commands::worktree::remove::teardown_worktree;
    use crate::commands::worktree::repo::{canonical_path, discover_repo, resolve_commit};
    use crate::commands::worktree::test_support::{add_git_worktree, Fixture};
    use crate::commands::worktree::Caller;
    use anyhow::Result;
    use std::fs;

    #[test]
    fn registry_identifies_main_when_invoked_from_linked_worktree() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("linked");
        add_git_worktree(
            &repo,
            "linked",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let entries = list_worktrees(&discover_repo(&target)?)?;
        assert!(entries[0].is_main);
        assert!(
            !entries
                .iter()
                .find(|entry| entry.path == target)
                .unwrap()
                .is_main
        );
        Ok(())
    }

    /// Git can delete a worktree's contents and its registration and still fail to
    /// unlink the directory itself — a read-only parent is enough. The empty shell
    /// that survives is not a worktree, and resolving it as one is what strands a
    /// retried cleanup on `fatal: not a git repository`.
    #[test]
    fn an_unregistered_leftover_directory_does_not_resolve_as_a_worktree() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let target = fixture.root.join("leftover");
        add_git_worktree(
            &repo,
            "leftover",
            &target,
            &base,
            WorktreeKind::NewBranch,
            &[],
        )?;
        let spec = target.display().to_string();

        match lookup_worktree_target(&repo, Some(&spec))? {
            TargetLookup::Found(path) => assert_eq!(path, canonical_path(&target)),
            _ => panic!("a registered worktree must resolve as found"),
        }

        teardown_worktree(&repo, &target, false, Caller::Cli)?;
        fs::create_dir_all(&target)?;

        match lookup_worktree_target(&repo, Some(&spec))? {
            TargetLookup::Stray(path) => assert_eq!(path, canonical_path(&target)),
            _ => panic!("an unregistered directory must not resolve as a live worktree"),
        }
        Ok(())
    }
}
