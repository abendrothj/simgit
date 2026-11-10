//! `remove` and teardown, including registrations whose directory is gone.

use super::locks::{ensure_unlocked, unfinished, unfinished_worktree, RegistryLock, WorktreeLock};
use super::maintenance::delete_local_branch;
use super::registry::{
    admin_head, git_at_admin, list_worktrees, lookup_worktree_target_in, registration_gone,
    short_branch, spec_is_path, worktree_dirty, AdminHead, TargetLookup, WorktreeEntry,
};
use super::repo::{
    default_worktree_path, discover_repo, git_common_command, git_failure, git_output_at,
    git_output_common, git_path_output, run_command, run_git_at,
    run_retrying_half_written_registration,
};
use super::{emit, overlay, shell_quote, Caller, RepoContext, WorktreeRemove};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

/// Recheck dirty state at teardown so a command finishing during GC selection
/// cannot turn a previously clean workspace into silently discarded work.
pub(super) fn teardown_worktree(
    repo: &RepoContext,
    target: &Path,
    discard_dirty: bool,
    caller: Caller,
) -> Result<()> {
    let lock = ensure_unlocked(repo, target)?;
    // Every caller is covered, and a commit landing after the caller's own
    // check cannot turn a dirty worktree into a clean one that dangles it.
    refuse_unreached_head(repo, target, caller)?;
    let discard = caller.flag(repo, target, "--discard-dirty");
    // An unpopulated worktree shows every file as deleted: never offer to
    // commit that.
    let dirty_refusal = || match unfinished(&lock) {
        true => unfinished_worktree(target, &discard),
        false => uncommitted_changes(repo, target, caller),
    };
    if registration_gone(repo, target) {
        // Deleted by hand: only this registration is left to drop.
        {
            let _registry = RegistryLock::exclusive(repo)?;
            drop_gone_registration(repo, target, discard_dirty, &discard)?;
        }
        prune_empty_worktree_dirs(repo, target);
        return Ok(());
    }
    // Report uncommitted work in simgit's own terms on every path: Git's
    // `worktree remove` only knows about --force, not about --commit.
    if !discard_dirty && worktree_dirty(target)? {
        return Err(dirty_refusal());
    }
    let result = if let Some(state) = overlay::state(repo, target) {
        let lock = WorktreeLock::acquire(repo, target)?;
        if !discard_dirty && worktree_dirty(target)? {
            return Err(dirty_refusal());
        }
        // Dropping the registration below would refuse this only after the
        // files are gone.
        if !discard_dirty && registration_holds_uncommitted(repo, target)? {
            bail!(
                "the registration of {} holds submodule repositories or refs of its own; \
                 keep them, or discard them with {discard}",
                target.display()
            );
        }
        overlay::unmount(target)?;
        remove_dir_if_present(target)?;
        remove_dir_if_present(&state.overlay_dir)?;
        lock.release()?;
        let _registry = RegistryLock::exclusive(repo)?;
        drop_gone_registration(repo, target, discard_dirty, &discard)
    } else if discard_dirty {
        remove_worktree_force(repo, target)
    } else {
        let mut command = git_common_command(repo);
        command.args(["worktree", "remove"]).arg(target);
        run_retrying_half_written_registration(&mut command, "git worktree remove")
    };
    if result.is_ok() {
        prune_empty_worktree_dirs(repo, target);
    }
    result
}

/// The refusal of a worktree with uncommitted changes, naming what keeps them.
fn uncommitted_changes(repo: &RepoContext, target: &Path, caller: Caller) -> anyhow::Error {
    let branch = list_worktrees(repo)
        .ok()
        .and_then(|entries| entries.into_iter().find(|entry| entry.path == target))
        .and_then(|entry| entry.branch)
        .or_else(|| overlay::branch(repo, target));
    let keep = match branch {
        Some(_) => format!(
            "keep the work with {}",
            caller.flag(repo, target, "--commit")
        ),
        None => keep_detached_changes(repo, target, caller),
    };
    anyhow::anyhow!(
        "worktree has uncommitted changes: {}\n{keep}, or discard it with {}",
        target.display(),
        caller.flag(repo, target, "--discard-dirty")
    )
}

/// How to keep a detached worktree's changes: `--commit` alone would make a
/// commit no ref reaches, which removal then refuses to drop.
fn keep_detached_changes(repo: &RepoContext, target: &Path, caller: Caller) -> String {
    let commit = match caller {
        Caller::Cli => "`--commit`".to_owned(),
        Caller::Hook => caller.flag(repo, target, "--commit"),
    };
    format!(
        "keep the work by creating a branch with `git -C {} switch -c <name>`, \
         then removing the worktree with {commit}",
        shell_quote(target)
    )
}

/// Drop the registration of a worktree whose directory is gone, and only that
/// one; the caller holds the exclusive registry lock. The directory is checked
/// again last, since Git would delete one that reappeared if it is clean. Git
/// refuses a registration that was locked meanwhile. A refusal names `discard`
/// as the way to discard what the registration holds.
fn drop_gone_registration(
    repo: &RepoContext,
    target: &Path,
    discard_dirty: bool,
    discard: &str,
) -> Result<()> {
    let holds = !discard_dirty
        && registration_holds_uncommitted(repo, target).with_context(|| {
            format!(
                "cannot tell whether the registration of {} holds work",
                target.display()
            )
        })?;
    if holds {
        let admin = overlay::admin_dir(repo, target).unwrap_or_default();
        bail!(
            "{} was deleted, but its registration {admin} still holds staged changes, a merge, \
             rebase, cherry-pick or revert in progress, submodule repositories (`modules/`) \
             or refs of its own (`refs/worktree`, `refs/bisect`, `refs/rewritten`)\n\
             inspect what is staged with `git --git-dir={quoted} diff --cached --stat`, \
             its refs with `git --git-dir={quoted} for-each-ref refs/worktree/ refs/bisect/ \
             refs/rewritten/`, and its submodule repositories under {admin}/modules; \
             or discard all of it with {discard}",
            target.display(),
            admin = admin.display(),
            quoted = shell_quote(&admin),
        );
    }
    if !matches!(target.try_exists(), Ok(false)) {
        bail!(
            "{} reappeared while its registration was being dropped; run the command again",
            target.display()
        );
    }
    let mut command = git_common_command(repo);
    command.args(["worktree", "remove"]).arg(target);
    run_command(&mut command, "drop the registration of a deleted worktree")
}

/// Drop the registration of a worktree deleted by hand, so that it can be
/// allocated again, with `remove`'s refusals of whatever that would lose: a
/// lock, a detached HEAD no ref reaches, or work the registration holds. Each
/// refusal names its recovery; `then` is the caller's step after keeping a
/// detached commit.
pub(super) fn drop_deleted_registration(
    repo: &RepoContext,
    entry: &WorktreeEntry,
    then: &str,
    caller: Caller,
) -> Result<()> {
    ensure_unlocked(repo, &entry.path)?;
    if entry.branch.is_none() {
        refuse_unreached_entry(repo, entry, then)?;
    }
    let spec = match &entry.branch {
        Some(reference) => short_branch(reference).to_owned(),
        None => shell_quote(&entry.path),
    };
    let _registry = RegistryLock::exclusive(repo)?;
    let discard = match caller {
        Caller::Cli => format!("`simgit remove {spec} --discard-dirty`"),
        Caller::Hook => caller.flag(repo, &entry.path, "--discard-dirty"),
    };
    drop_gone_registration(repo, &entry.path, false, &discard)
}

/// What a deleted worktree's registration records that reattaching keeps:
/// whether it is ephemeral, and its cone-mode sparse directories (read only
/// when `sparse` is true). An entry whose settings cannot be read is an error.
pub(super) fn deleted_worktree_settings(
    repo: &RepoContext,
    entry: &WorktreeEntry,
    sparse: bool,
    caller: Caller,
) -> Result<(bool, Vec<String>)> {
    let target = &entry.path;
    let admin = overlay::admin_dir(repo, target)
        .with_context(|| format!("cannot find the registration of {}", target.display()))?;
    let marker = admin.join("simgit-ephemeral");
    let ephemeral = marker
        .try_exists()
        .with_context(|| format!("check {}", marker.display()))?;
    if !sparse {
        return Ok((ephemeral, Vec::new()));
    }
    let unreadable = || format!("cannot read the sparse checkout of {}", target.display());
    let config = git_at_admin(
        repo,
        &admin,
        &["config", "--get-regexp", "^core\\.sparsecheckout"],
    )?;
    let settings = match config.status.code() {
        Some(0) => String::from_utf8(config.stdout).with_context(unreadable)?,
        Some(1) => String::new(),
        _ => return Err(git_failure("git config", &config)).with_context(unreadable),
    };
    let set = |key: &str| settings.lines().any(|line| line == format!("{key} true"));
    if !set("core.sparsecheckout") {
        return Ok((ephemeral, Vec::new()));
    }
    if !set("core.sparsecheckoutcone") {
        bail!(
            "{}: its sparse checkout is not in cone mode, so --sparse cannot recreate it; {}",
            unreadable(),
            caller.sparse_recovery(repo, target)
        );
    }
    let list = git_at_admin(repo, &admin, &["sparse-checkout", "list"])?;
    if !list.status.success() {
        return Err(git_failure("git sparse-checkout list", &list)).with_context(unreadable);
    }
    let directories: Vec<String> = String::from_utf8(list.stdout)
        .with_context(unreadable)?
        .lines()
        .map(str::to_owned)
        .collect();
    // Git quotes a name it cannot print plainly; never guess at its unquoting.
    if directories.iter().any(|dir| dir.starts_with('"')) {
        bail!(
            "{}: a directory name is quoted; {}",
            unreadable(),
            caller.sparse_recovery(repo, target)
        );
    }
    Ok((ephemeral, directories))
}

/// Whether a worktree's registration holds uncommitted work of its own: an
/// operation in progress, submodule repositories, per-worktree refs, or an
/// index that differs from HEAD. It reads only the registration, so it answers
/// for a worktree whose directory is gone.
pub(super) fn registration_holds_uncommitted(repo: &RepoContext, worktree: &Path) -> Result<bool> {
    let admin = overlay::admin_dir(repo, worktree)
        .with_context(|| format!("cannot find the registration of {}", worktree.display()))?;
    admin_holds_uncommitted(repo, &admin)
}

/// `registration_holds_uncommitted` for the admin dir `admin`.
pub(super) fn admin_holds_uncommitted(repo: &RepoContext, admin: &Path) -> Result<bool> {
    const IN_PROGRESS: [&str; 6] = [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ];
    // An entry that cannot be checked is not known to be absent.
    let present = |name: &str| {
        let path = admin.join(name);
        path.try_exists()
            .with_context(|| format!("check {}", path.display()))
    };
    for name in IN_PROGRESS {
        if present(name)? {
            return Ok(true);
        }
    }
    // The worktree's submodule repositories live here, and may hold the only
    // copy of a commit.
    if holds_a_file(&admin.join("modules"))? {
        return Ok(true);
    }
    let git = |args: &[&str]| git_at_admin(repo, admin, args);
    // So do its own refs, which may be all that reaches a commit. Git lists
    // them, since under reftable they are not files of their own.
    let refs = git(&[
        "for-each-ref",
        "--count=1",
        "--format=%(refname)",
        "refs/worktree/",
        "refs/bisect/",
        "refs/rewritten/",
    ])?;
    if !refs.status.success() {
        return Err(git_failure("git for-each-ref", &refs));
    }
    if !refs.stdout.is_empty() {
        return Ok(true);
    }
    // Nothing was ever staged without an index.
    if !present("index")? {
        return Ok(false);
    }
    let head = git(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?;
    match head.status.code() {
        Some(0) => {
            let head = String::from_utf8(head.stdout)?;
            let diff = git(&["diff-index", "--cached", "--quiet", head.trim()])?;
            match diff.status.code() {
                Some(0) => Ok(false),
                Some(1) => Ok(true),
                _ => Err(git_failure("git diff-index --cached", &diff)),
            }
        }
        // An unborn HEAD has no commit to compare with: any entry is staged.
        Some(1) => {
            let files = git(&["ls-files", "-z"])?;
            if !files.status.success() {
                return Err(git_failure("git ls-files", &files));
            }
            Ok(!files.stdout.is_empty())
        }
        _ => Err(git_failure("git rev-parse HEAD", &head)),
    }
}

/// Whether `dir` holds a file at any depth. A missing `dir` holds none; one
/// that cannot be read is an error, never an answer.
fn holds_a_file(dir: &Path) -> Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("check {}", entry.path().display()))?;
        if !file_type.is_dir() || holds_a_file(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Drop the `.simgit/<repo>` scaffolding once its last worktree is gone, so
/// removing every worktree leaves no empty directories beside the repository.
/// Only empty directories are removed, and only up to the `.simgit` root.
fn prune_empty_worktree_dirs(repo: &RepoContext, target: &Path) {
    if std::env::var_os("SIMGIT_WORKTREE_ROOT").is_some() {
        // A directory the user chose is theirs to keep, empty or not.
        return;
    }
    let Some(root) = default_worktree_path(repo, "unused")
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return;
    };
    if target.parent() != Some(root.as_path())
        || !root
            .ancestors()
            .any(|dir| dir.file_name() == Some(OsStr::new(".simgit")))
    {
        return;
    }
    // Up to and including `.simgit`, stopping at the first that is not empty.
    for dir in root.ancestors() {
        if fs::remove_dir(dir).is_err() || dir.file_name() == Some(OsStr::new(".simgit")) {
            break;
        }
    }
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

pub(super) fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

pub fn remove(args: WorktreeRemove, json: bool) -> Result<()> {
    // Refuse before touching the repository: --delete-unmerged widens branch
    // deletion, so on its own it silently promises something it cannot do.
    if args.delete_unmerged && !args.delete_branch {
        bail!("--delete-unmerged only relaxes branch deletion; pass --delete-branch too");
    }
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    // One listing serves the lookup and the entry: each reads every registration.
    let entries = list_worktrees(&repo)?;
    let (target, gone) = match lookup_worktree_target_in(&repo, &entries, args.target.as_deref())? {
        TargetLookup::Found(path) => (path, false),
        // Deleted by hand: only the registration is left, and it still names
        // the branch for --delete-branch.
        TargetLookup::Gone(path) => (path, true),
        // Removing what is already gone is the state the caller asked for. An
        // agent that crashed mid-cleanup, or handed the same cleanup token
        // back twice, must not have to tell the two cases apart.
        TargetLookup::Absent(spec) => return remove_absent(&repo, &spec, &args, json),
        // A directory that is no longer registered is not a worktree, however
        // much it looks like one: finishing that teardown is its own path.
        TargetLookup::Stray(path) => return remove_stray(&path, &args, json),
    };
    let entry = entries.into_iter().find(|entry| entry.path == target);
    // Git cannot remove the main worktree, so refuse before --commit commits
    // the user's files in it and teardown then fails. First, so a main entry
    // with no lock to find (a bare repository) is still named as such.
    if entry.as_ref().is_some_and(|entry| entry.is_main) {
        bail!("{} is the main checkout, not a worktree", target.display());
    }
    let lock = ensure_unlocked(&repo, &target)?;
    // Committing an unpopulated worktree would record every file as deleted.
    if args.commit && !gone && unfinished(&lock) {
        return Err(unfinished_worktree(&target, "--discard-dirty"));
    }
    let branch = entry
        .and_then(|entry| entry.branch)
        .or_else(|| overlay::branch(&repo, &target));
    // Fail before anything is torn down: a detached worktree has no branch to
    // delete, and silently reporting success would hide the mistake.
    if args.delete_branch && branch.is_none() {
        bail!(
            "--delete-branch needs a branch: {} is detached",
            target.display()
        );
    }
    // Before --commit makes a commit; teardown rechecks in case a commit lands
    // between here and there.
    refuse_unreached_head(&repo, &target, Caller::Cli)?;
    // A detached worktree's commit would be reached by no ref, so teardown
    // would refuse it, after the commit, on every retry.
    if args.commit && !gone && branch.is_none() && worktree_dirty(&target)? {
        bail!(
            "{} is detached, so --commit would make a commit no branch reaches\n{}",
            target.display(),
            keep_detached_changes(&repo, &target, Caller::Cli)
        );
    }
    // A commit that succeeded is never rolled back when a later step fails: it
    // is the work the caller asked to keep, and an "atomic" remove that
    // un-commits it would trade a visible seam for destroyed work. The failure
    // says so instead.
    let mut commit = None;
    // A deleted worktree holds nothing left to commit.
    if args.commit && !gone {
        run_git_at(&target, ["add", "-A"]).context("stage worktree changes")?;
        let diff = git_output_at(&target, ["diff", "--cached", "--quiet"])?;
        match diff.status.code() {
            Some(0) => {
                if !json {
                    eprintln!("no changes to commit");
                }
            }
            Some(1) => {
                run_git_at(&target, ["commit", "-m", &args.message])
                    .context("commit worktree changes")?;
                commit = Some(git_path_output(&target, ["rev-parse", "HEAD"])?);
            }
            _ => return Err(git_failure("git diff --cached --quiet", &diff)),
        }
    }

    teardown_worktree(&repo, &target, args.discard_dirty, Caller::Cli)
        .map_err(|error| annotate_commit_kept(error, commit.as_deref(), branch.as_deref()))?;

    let mut branch_deleted = false;
    if let (true, Some(reference)) = (args.delete_branch, branch) {
        delete_local_branch(&repo, &reference, args.delete_unmerged).map_err(|error| {
            // The worktree is gone by now, so a retry by path can no longer
            // name this branch. Hand back the form that still works.
            let name = short_branch(&reference);
            error.context(format!(
                "the worktree is removed, but branch {name} was not deleted; \
                 retry with `simgit remove {name} --delete-branch`"
            ))
        })?;
        branch_deleted = true;
    }

    if json {
        emit(&json!({
            "removed": target.display().to_string(),
            "already_absent": gone,
            "committed": commit.is_some(),
            "commit": commit,
            "branch_deleted": branch_deleted,
        }));
    } else if gone {
        println!("{} (already absent)", target.display());
    } else {
        println!("{}", target.display());
    }
    Ok(())
}

/// Report a target that names no worktree, and still honour `--delete-branch`
/// for a branch whose worktree is already gone — the ref outliving its
/// checkout is exactly the leftover the flag exists to clean up.
fn remove_absent(repo: &RepoContext, spec: &str, args: &WorktreeRemove, json: bool) -> Result<()> {
    let mut branch_deleted = false;
    if args.delete_branch {
        // A path that is gone names no branch: the registration that mapped
        // one to the other went with it. Reporting `branch_deleted: false`
        // here reads as "there was nothing to delete", which is a guess.
        if spec_is_path(spec) {
            bail!(
                "cannot infer a branch from an already-removed path; pass the branch name\n\
                 {spec} no longer exists, so nothing records which branch it held"
            );
        }
        // `show-ref --verify` failing means the ref is gone too, and then
        // nothing of this workspace is left to delete.
        let reference = format!("refs/heads/{spec}");
        let exists = git_output_common(repo, ["show-ref", "--verify", "--quiet", &reference])?;
        if exists.status.success() {
            delete_local_branch(repo, &reference, args.delete_unmerged)?;
            branch_deleted = true;
        }
    }

    if json {
        emit(&json!({
            "removed": spec,
            "already_absent": true,
            "committed": false,
            "commit": null,
            "branch_deleted": branch_deleted,
        }));
    } else {
        println!("{spec} (already absent)");
    }
    Ok(())
}

/// Finish a teardown Git left half-done.
///
/// `git worktree remove` can delete a worktree's contents and its registration
/// and still fail to unlink the directory itself — a read-only parent is
/// enough. What survives is an empty directory that is no longer a worktree,
/// and a retried cleanup has to converge on it instead of asking Git about a
/// checkout Git has already forgotten.
fn remove_stray(path: &Path, args: &WorktreeRemove, json: bool) -> Result<()> {
    if args.delete_branch {
        bail!(
            "cannot infer a branch from a path that is no longer a worktree; pass the branch name\n\
             {} is not registered, so nothing records which branch it held",
            path.display()
        );
    }
    // Emptiness is the whole safety argument for deleting this at all.
    if !is_empty_dir(path)? {
        bail!(
            "{} is not a worktree of this repository, and it is not empty\n\
             simgit does not delete directories it does not manage; inspect it and remove it yourself",
            path.display()
        );
    }
    // Best effort: whatever stopped Git from unlinking this directory stops
    // simgit too, and the worktree the caller asked about is gone either way.
    let _ = fs::remove_dir(path);

    if json {
        emit(&json!({
            "removed": path.display().to_string(),
            "already_absent": true,
            "committed": false,
            "commit": null,
            "branch_deleted": false,
        }));
    } else {
        println!("{} (already absent)", path.display());
    }
    Ok(())
}

fn is_empty_dir(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path).with_context(|| format!("read {}", path.display()))?;
    Ok(entries.next().is_none())
}

/// Name the commit a partially completed `remove --commit` already created.
///
/// A `--json` failure emits no JSON, so this diagnostic is the only place the
/// fact can be said — and without it "removing the worktree failed" is
/// indistinguishable from a failure that committed nothing, which is exactly
/// the ambiguity that makes a script afraid to retry.
fn annotate_commit_kept(
    error: anyhow::Error,
    commit: Option<&str>,
    branch: Option<&str>,
) -> anyhow::Error {
    let Some(commit) = commit else {
        return error;
    };
    let short = commit.get(..12).unwrap_or(commit);
    let location = match branch {
        Some(reference) => format!(" on {}", short_branch(reference)),
        None => String::new(),
    };
    error.context(format!(
        "the worktree was not removed, but its changes are committed as {short}{location}; \
         that commit is kept, and repeating this remove will not create a second one"
    ))
}

/// Refuse to tear down a detached worktree whose HEAD commit no ref reaches.
/// The HEAD comes from the registry, never from the worktree's own gitfile: a
/// missing gitfile makes git walk up and measure the main checkout instead.
fn refuse_unreached_head(repo: &RepoContext, target: &Path, caller: Caller) -> Result<()> {
    // Read this one registration's HEAD rather than listing every worktree:
    // gc runs this once per worktree it reaps.
    let admin = overlay::admin_dir(repo, target)
        .with_context(|| format!("{} is not a registered worktree", target.display()))?;
    let head = match admin_head(repo, &admin).with_context(|| {
        format!(
            "cannot tell whether {} holds unreached commits",
            target.display()
        )
    })? {
        AdminHead::Branch(_) => return Ok(()),
        AdminHead::Detached(head) => head,
    };
    let entry = WorktreeEntry {
        path: target.to_path_buf(),
        head: Some(head),
        branch: None,
        is_main: false,
    };
    refuse_unreached_entry(repo, &entry, &caller.then_remove(repo, target))
}

/// `refuse_unreached_head` for a detached `entry` already read from the
/// registry; `then` is the step that follows keeping the commit.
fn refuse_unreached_entry(repo: &RepoContext, entry: &WorktreeEntry, then: &str) -> Result<()> {
    let target = &entry.path;
    let head = entry.head.as_deref();
    let unreached = head_unreached(repo, head).with_context(|| {
        format!(
            "cannot tell whether {} holds unreached commits",
            target.display()
        )
    })?;
    if unreached {
        // Without its gitfile, git run there walks up to another repository,
        // so name the commit from the main checkout instead.
        let keep = if matches!(target.join(".git").try_exists(), Ok(true)) {
            format!("git -C {} branch <name>", shell_quote(target))
        } else {
            format!(
                "git -C {} branch <name> {}",
                shell_quote(&repo.main_worktree),
                head.unwrap_or_default()
            )
        };
        bail!(
            "{} is detached at a commit no branch or tag reaches; removing it would \
             lose that work. Keep it with `{keep}`, {then}",
            target.display()
        );
    }
    Ok(())
}

/// True when the registered HEAD commit is reached by no ref and by no other
/// worktree's HEAD: removing the worktree would leave its commits dangling.
/// A missing HEAD or a git failure is an error, never an answer, so it cannot
/// lead to deletion.
pub(super) fn head_unreached(repo: &RepoContext, head: Option<&str>) -> Result<bool> {
    let head = head.context("the registry records no HEAD for the worktree")?;
    // --single-worktree limits --all to the main checkout's HEAD, so this
    // worktree's own detached HEAD does not count as a ref reaching itself.
    let output = git_output_common(
        repo,
        [
            "rev-list",
            "-n1",
            "--single-worktree",
            head,
            "--not",
            "--all",
        ],
    )?;
    if !output.status.success() {
        return Err(git_failure("git rev-list --not --all", &output));
    }
    Ok(!output.stdout.is_empty())
}

pub(super) fn remove_worktree_force(repo: &RepoContext, target: &Path) -> Result<()> {
    let mut command = git_common_command(repo);
    command.args(["worktree", "remove", "--force"]).arg(target);
    run_retrying_half_written_registration(&mut command, "git worktree remove --force")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktree::add::WorktreeKind;
    use crate::commands::worktree::repo::{discover_repo, resolve_commit, run_git_at};
    use crate::commands::worktree::test_support::{add_git_worktree, resolve_commit_at, Fixture};
    use crate::commands::worktree::{Caller, WorktreeRemove};
    use anyhow::Result;
    use std::fs;

    /// Finishing an interrupted teardown is only safe because the residue is
    /// empty. A directory with files in it is somebody's data, whatever its name.
    #[test]
    fn stray_removal_clears_an_empty_shell_and_refuses_one_holding_files() -> Result<()> {
        let fixture = Fixture::new()?;
        let args = WorktreeRemove {
            target: None,
            commit: false,
            message: "simgit remove".to_owned(),
            discard_dirty: false,
            delete_branch: false,
            delete_unmerged: false,
        };

        let occupied = fixture.root.join("occupied");
        fs::create_dir_all(&occupied)?;
        fs::write(occupied.join("work.txt"), "unexplained")?;
        let refused = remove_stray(&occupied, &args, false).expect_err("files must not be deleted");
        assert!(refused.to_string().contains("not empty"));
        assert!(occupied.join("work.txt").is_file());

        let empty = fixture.root.join("empty");
        fs::create_dir_all(&empty)?;
        remove_stray(&empty, &args, false)?;
        assert!(!empty.exists());
        Ok(())
    }

    /// A commit that lands after the caller's own check must not slip through:
    /// teardown itself refuses a clean detached worktree whose HEAD no ref reaches.
    #[test]
    fn teardown_refuses_a_clean_detached_worktree_with_unreached_commits() -> Result<()> {
        let fixture = Fixture::new()?;
        let repo = discover_repo(&fixture.repo)?;
        let base = resolve_commit(&repo, "HEAD")?;
        let detached = fixture.root.join("detached");
        add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
        teardown_worktree(&repo, &detached, false, Caller::Cli)
            .expect("a fresh detached worktree is reached");

        add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
        fs::write(detached.join("agent.txt"), "result")?;
        run_git_at(&detached, ["add", "."])?;
        run_git_at(&detached, ["commit", "-q", "-m", "agent result"])?;
        let head = resolve_commit_at(&detached)?;

        for discard_dirty in [false, true] {
            let error =
                teardown_worktree(&repo, &detached, discard_dirty, Caller::Cli).unwrap_err();
            assert!(format!("{error:#}").contains("branch <name>"), "{error:#}");
        }
        assert!(detached.is_dir());
        assert_eq!(resolve_commit_at(&detached)?, head);
        Ok(())
    }
}
