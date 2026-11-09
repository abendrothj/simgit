//! `simgit hook <provider>`: adapt an agent harness's hook payload to simgit.

use super::launch::{resolve_branch_worktree, Resolved};
use super::{
    canonical_path, discover_repo, ensure_unlocked, git_common_command, git_failure,
    git_output_common, list_worktrees, output_retrying_half_written_registration,
    prune_git_worktrees, teardown_worktree, worktree_dirty, worktree_path_for_branch, BranchClaim,
    RepoContext, WorktreeAdd, WorktreeEntry,
};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use std::io::Read;
use std::path::Path;

#[derive(Args)]
pub struct WorktreeHook {
    /// Harness whose hook payload arrives on stdin.
    provider: HookProvider,

    /// Refuse to run on a simgit older than this X.Y.Z version.
    #[arg(long)]
    requires: String,

    /// Permit removing a worktree with uncommitted changes.
    #[arg(long)]
    discard_dirty: bool,
}

#[derive(ValueEnum, Clone)]
enum HookProvider {
    ClaudeCode,
}

pub fn hook(args: WorktreeHook, json: bool) -> Result<()> {
    if json {
        bail!("--json is not supported with `hook` because the harness defines its output");
    }
    let running = env!("CARGO_PKG_VERSION");
    if !version_at_least(running, &args.requires)? {
        bail!(
            "simgit hook requires simgit >= {}, found {running}",
            args.requires
        );
    }
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read hook payload from stdin")?;
    let payload: serde_json::Value =
        // Folded into one line: the harness shows stderr to the user.
        serde_json::from_str(&input).map_err(|error| anyhow!("hook payload is not valid JSON: {error}"))?;
    let HookProvider::ClaudeCode = args.provider;
    match payload["hook_event_name"].as_str() {
        Some("SessionStart") => Ok(()),
        Some("WorktreeCreate") => on_worktree_create(&payload),
        Some("WorktreeRemove") => on_worktree_remove(&payload, args.discard_dirty),
        Some("SubagentStop") => on_subagent_stop(&payload),
        _ => Ok(()),
    }
}

/// Compare numeric `X.Y.Z` versions; a malformed `required` is an error.
fn version_at_least(running: &str, required: &str) -> Result<bool> {
    fn parse(version: &str) -> Option<[u64; 3]> {
        let mut parts = version.split('.');
        let parsed = [
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ];
        parts.next().is_none().then_some(parsed)
    }
    let required = parse(required)
        .with_context(|| format!("--requires must be a version like 1.2.3, got `{required}`"))?;
    let running =
        parse(running).with_context(|| format!("unparsable simgit version `{running}`"))?;
    Ok(running >= required)
}

/// The project repository: `$CLAUDE_PROJECT_DIR`, else the payload's `cwd`.
fn project_repo(payload: &serde_json::Value) -> Result<RepoContext> {
    let dir = match std::env::var_os("CLAUDE_PROJECT_DIR") {
        Some(dir) if !dir.is_empty() => dir.into(),
        _ => payload["cwd"]
            .as_str()
            .map(|cwd| Path::new(cwd).to_path_buf())
            .context("hook payload has no cwd and CLAUDE_PROJECT_DIR is unset")?,
    };
    discover_repo(&dir)
}

/// The branch simgit keeps for a Claude Code worktree name.
fn branch_for_name(repo: &RepoContext, name: &str) -> Result<String> {
    let branch = format!("simgit/{name}");
    let output = git_output_common(repo, ["check-ref-format", "--branch", &branch])?;
    if !output.status.success() {
        bail!("`{name}` is not a valid worktree name: {branch} is not a valid branch name");
    }
    Ok(branch)
}

/// Where new worktrees start: the remote's default branch, else `HEAD`.
fn default_base(repo: &RepoContext) -> Result<String> {
    let remote = "refs/remotes/origin/HEAD";
    let output = git_output_common(repo, ["rev-parse", "--verify", "--quiet", remote])?;
    Ok(if output.status.success() {
        remote
    } else {
        "HEAD"
    }
    .to_owned())
}

/// The change number in a `pr-<n>` name: `pr-` plus ASCII digits only, kept as
/// text so it reaches the refspec verbatim whatever its size.
fn change_number(name: &str) -> Option<&str> {
    let digits = name.strip_prefix("pr-")?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())).then_some(digits)
}

/// Fetch a change's head from origin as Claude Code does natively, trying the
/// GitHub ref and then the GitLab one, and return its commit.
///
/// Each fetch lands in its own temporary ref: the shared `FETCH_HEAD` would let
/// two parallel creates for different changes read each other's commit.
fn fetch_change(repo: &RepoContext, number: &str) -> Result<String> {
    for remote_ref in [
        format!("pull/{number}/head"),
        format!("merge-requests/{number}/head"),
    ] {
        let temporary = format!("refs/simgit/fetch/{}", uuid::Uuid::new_v4());
        let fetched = fetch_ref(repo, &format!("+{remote_ref}:{temporary}"))?;
        if !fetched.status.success() {
            let complaint = String::from_utf8_lossy(&fetched.stderr);
            // Only a ref the remote lacks means "try the other forge's ref".
            if complaint.contains("couldn't find remote ref") {
                continue;
            }
            bail!(
                "pull request {number}: fetching {remote_ref} from origin failed: {}",
                fetch_complaint(&complaint)
            );
        }
        let commit = git_output_common(
            repo,
            ["rev-parse", "--verify", &format!("{temporary}^{{commit}}")],
        )?;
        let deleted = git_output_common(repo, ["update-ref", "-d", &temporary])?;
        if !commit.status.success() {
            return Err(git_failure("read the fetched commit", &commit));
        }
        if !deleted.status.success() {
            return Err(git_failure("delete the temporary fetch ref", &deleted));
        }
        return Ok(String::from_utf8_lossy(&commit.stdout).trim().to_owned());
    }
    bail!(
        "pull request {number}: neither pull/{number}/head nor merge-requests/{number}/head could be fetched from origin"
    )
}

/// `git fetch origin <refspec>`, retried when its connectivity check died on a
/// worktree registration another allocation was half-way through writing.
fn fetch_ref(repo: &RepoContext, refspec: &str) -> Result<std::process::Output> {
    // No terminal prompt: a credential request must fail, not hang the hook.
    let mut command = git_common_command(repo);
    command.env("GIT_TERMINAL_PROMPT", "0").args([
        "fetch",
        "--quiet",
        "--no-write-fetch-head",
        "origin",
        refspec,
    ]);
    output_retrying_half_written_registration(&mut command)
}

/// The line of a failed fetch's stderr that says what went wrong: Git's first
/// `fatal:` line, since the last is often advice such as "and the repository
/// exists.".
fn fetch_complaint(stderr: &str) -> &str {
    let mut lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    lines
        .clone()
        .find(|line| line.starts_with("fatal:"))
        .or_else(|| lines.next_back())
        .unwrap_or("no error output")
}

/// Create or reuse the worktree for the payload's `name` and print its path,
/// which the harness reads from stdout, so nothing else may go there.
fn on_worktree_create(payload: &serde_json::Value) -> Result<()> {
    let name = payload["name"]
        .as_str()
        .context("WorktreeCreate payload has no name")?;
    let repo = project_repo(payload)?;
    let branch = branch_for_name(&repo, name)?;
    let claim = BranchClaim::acquire_blocking(&repo, &branch)?;
    // A worktree deleted by hand leaves its registration, which keeps the
    // branch checked out at a missing path; prune it so the branch reattaches.
    if worktree_path_for_branch(&repo, &branch)?.is_some_and(|path| !path.exists()) {
        prune_git_worktrees(&repo)?;
    }
    let main = repo.main_worktree.clone();
    // The base applies only to a new branch; reuse and attach reject it.
    let reference = format!("refs/heads/{branch}");
    let exists = git_output_common(&repo, ["show-ref", "--verify", "--quiet", &reference])?;
    let base = match exists.status.code() {
        Some(0) => None,
        Some(1) => Some(match change_number(name) {
            Some(number) => fetch_change(&repo, number)?,
            None => default_base(&repo)?,
        }),
        _ => return Err(git_failure("look up branch", &exists)),
    };
    let template = WorktreeAdd {
        branch: Some(branch),
        detach: false,
        attach: false,
        path: None,
        sparse: Vec::new(),
        base,
        require_cow: false,
        ephemeral: false,
    };
    let path = match resolve_branch_worktree(&repo, &main, template, claim)? {
        Resolved::Existing(path) => {
            // Still registered after the prune above: Git keeps a locked one.
            let resolved = path
                .canonicalize()
                .with_context(|| format!("{} is registered but missing", path.display()))?;
            if resolved == main.canonicalize()? {
                bail!("{} is the main checkout, not a worktree", path.display());
            }
            ensure_unlocked(&repo, &path)?;
            path
        }
        Resolved::Created(created) => {
            eprintln!("mode: {}", created.mode.label());
            created.target
        }
    };
    println!("{}", path.display());
    Ok(())
}

/// Whether `branch` holds a commit that no other ref reaches.
///
/// `--single-worktree` keeps `--all` from counting every worktree's `HEAD`:
/// the worktree being removed has the branch checked out, so its own `HEAD`
/// would make every commit look reached. The main checkout is the working
/// directory, so its `HEAD` still counts.
fn holds_unreached_commits(repo: &RepoContext, branch: &str) -> Result<bool> {
    let branch_ref = format!("refs/heads/{branch}");
    let exclude = format!("--exclude={branch_ref}");
    let output = git_output_common(
        repo,
        [
            "rev-list",
            "--single-worktree",
            &branch_ref,
            "--not",
            &exclude,
            "--all",
        ],
    )?;
    if !output.status.success() {
        return Err(git_failure("look for unreached commits", &output));
    }
    Ok(!output.stdout.is_empty())
}

/// The commit `branch` points at, or `None` when there is no such branch.
fn branch_tip(repo: &RepoContext, branch: &str) -> Result<Option<String>> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output_common(repo, ["rev-parse", "--verify", "--quiet", &reference])?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Some(1) => Ok(None),
        _ => Err(git_failure("look up branch", &output)),
    }
}

/// Delete `branch` when deleting it loses no commit; returns whether it did.
/// The delete names the tip read before the check, so a commit that lands in
/// between makes Git refuse it and the branch stays.
fn drop_branch_if_reached(repo: &RepoContext, branch: &str) -> Result<bool> {
    if branch == "main" || branch == "master" {
        bail!("refusing to delete primary branch '{branch}'");
    }
    let Some(tip) = branch_tip(repo, branch)? else {
        return Ok(false);
    };
    // `update-ref` does not check worktrees as `branch -d` does, so look here.
    if holds_unreached_commits(repo, branch)? || worktree_path_for_branch(repo, branch)?.is_some() {
        return Ok(false);
    }
    let reference = format!("refs/heads/{branch}");
    let deleted = git_output_common(repo, ["update-ref", "-d", &reference, &tip])?;
    if deleted.status.success() {
        return Ok(true);
    }
    if branch_tip(repo, branch)?.as_deref() != Some(tip.as_str()) {
        return Ok(false);
    }
    Err(git_failure("delete worktree branch", &deleted))
}

/// The worktree registered at `path`, compared in canonical form so a path
/// that no longer exists still matches.
fn registered_at(repo: &RepoContext, path: &Path) -> Result<Option<WorktreeEntry>> {
    Ok(list_worktrees(repo)?
        .into_iter()
        .find(|entry| canonical_path(&entry.path) == path))
}

/// Remove the payload's `worktree_path` if it is a worktree on a `simgit/*`
/// branch, then drop the branch unless it holds unreached commits. A path that
/// no longer exists is already removed; a registration it left is pruned.
fn on_worktree_remove(payload: &serde_json::Value, discard_dirty: bool) -> Result<()> {
    let given = Path::new(
        payload["worktree_path"]
            .as_str()
            .context("WorktreeRemove payload has no worktree_path")?,
    );
    let resolved = canonical_path(given);
    let repo = match discover_repo(&resolved) {
        Ok(repo) => repo,
        // Already gone, perhaps deleted by hand: look for what it left in the
        // project. Without a project there is nothing left to tidy.
        Err(_) if !given.exists() => match project_repo(payload) {
            Ok(repo) => repo,
            Err(_) => return Ok(()),
        },
        Err(error) => return Err(error),
    };
    let Some(entry) = registered_at(&repo, &resolved)? else {
        if given.exists() {
            bail!("{} is not a registered worktree", given.display());
        }
        return Ok(());
    };
    if entry.is_main {
        bail!("{} is the main checkout, not a worktree", given.display());
    }
    let branch_ref = entry.branch.clone();
    let Some(branch) = branch_ref
        .as_deref()
        .and_then(|branch| branch.strip_prefix("refs/heads/"))
        .filter(|branch| branch.starts_with("simgit/"))
    else {
        if !entry.path.exists() {
            return Ok(());
        }
        bail!(
            "{} is not on a simgit/ branch; simgit leaves it alone",
            given.display()
        );
    };
    // A parallel duplicate of this hook takes the same claim, and finds the
    // work done once it has it.
    let _claim = BranchClaim::acquire_blocking(&repo, branch)?;
    let Some(entry) = registered_at(&repo, &resolved)?.filter(|entry| entry.branch == branch_ref)
    else {
        return Ok(());
    };
    if entry.path.exists() {
        teardown_worktree(&repo, &entry.path, discard_dirty)?;
    } else {
        prune_git_worktrees(&repo)?;
    }
    drop_branch_if_reached(&repo, branch)?;
    Ok(())
}

/// Mirror Claude Code's own cleanup for a finished subagent: remove its
/// `simgit/agent-<agent_id>` worktree and branch when it is clean, unlocked and
/// holds no unreached commit; keep everything otherwise, never discarding.
/// Exit 2 would stop the subagent from finishing, so every problem is one
/// stderr line and the result is always `Ok`.
fn on_subagent_stop(payload: &serde_json::Value) -> Result<()> {
    if let Err(error) = remove_finished_subagent(payload) {
        eprintln!("{}", subagent_stop_note(&error));
    }
    Ok(())
}

fn subagent_stop_note(error: &anyhow::Error) -> String {
    format!(
        "simgit: SubagentStop: {}",
        crate::one_line(&format!("{error:#}"))
    )
}

fn remove_finished_subagent(payload: &serde_json::Value) -> Result<()> {
    let id = payload["agent_id"]
        .as_str()
        .context("SubagentStop payload has no agent_id")?;
    let repo = project_repo(payload)?;
    let branch = branch_for_name(&repo, &format!("agent-{id}"))?;
    let reference = format!("refs/heads/{branch}");
    // A plain subagent has no branch: leave without listing or claiming, so
    // its stop writes nothing to the repository.
    let exists = git_output_common(&repo, ["show-ref", "--verify", "--quiet", &reference])?;
    match exists.status.code() {
        Some(0) => {}
        Some(1) => return Ok(()),
        _ => return Err(git_failure("look up branch", &exists)),
    }
    let checked_out = |repo: &RepoContext| -> Result<Option<WorktreeEntry>> {
        Ok(list_worktrees(repo)?
            .into_iter()
            .find(|entry| !entry.is_main && entry.branch.as_deref() == Some(reference.as_str())))
    };
    // No worktree on the branch (switched away, or already removed): leave all.
    if checked_out(&repo)?.is_none() {
        return Ok(());
    }
    // Held to the end, and the lookup repeated under it: two concurrent
    // `git worktree remove` calls on one worktree both fail and leave its
    // branch behind; WorktreeCreate and WorktreeRemove take the same claim.
    let _claim = BranchClaim::acquire_blocking(&repo, &branch)?;
    let Some(entry) = checked_out(&repo)? else {
        return Ok(());
    };
    let present = entry.path.exists();
    if present {
        if worktree_dirty(&entry.path)? {
            bail!(
                "{} has uncommitted changes; keeping it",
                entry.path.display()
            );
        }
        ensure_unlocked(&repo, &entry.path)?;
    } else {
        // Deleted by hand: only its registration is left to clean up.
        prune_git_worktrees(&repo)?;
    }
    if holds_unreached_commits(&repo, &branch)? {
        bail!("{branch} holds unpushed commits; keeping it");
    }
    if present {
        teardown_worktree(&repo, &entry.path, false)?;
    }
    drop_branch_if_reached(&repo, &branch)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // Git's own messages can span lines (Linux adds "Stopping at filesystem
    // boundary ..."); the stop note must still be one stderr line.
    #[test]
    fn subagent_stop_note_is_one_line_for_a_multi_line_git_error() {
        let error = anyhow!("fatal: not a git repository\nStopping at filesystem boundary.")
            .context("not in a Git working tree");
        let note = subagent_stop_note(&error);
        assert_eq!(note.lines().count(), 1, "{note}");
        assert!(note.contains("Stopping at filesystem boundary."), "{note}");
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn version_at_least_accepts_equal_and_newer_and_refuses_older() {
        let at_least = |running, required| version_at_least(running, required).expect("parse");
        assert!(at_least("1.2.3", "1.2.3"));
        for newer in ["1.2.4", "1.3.0", "2.0.0"] {
            assert!(at_least(newer, "1.2.3"), "{newer}");
        }
        for older in ["1.2.2", "1.1.9", "0.9.9"] {
            assert!(!at_least(older, "1.2.3"), "{older}");
        }
        // Numeric, not lexical.
        assert!(at_least("0.10.0", "0.9.0"));
        assert!(!at_least("0.9.0", "0.10.0"));
        for malformed in ["", "1.2", "1.2.3.4", "v1.2.3", "1.x.3", "1.2.-3"] {
            assert!(version_at_least("1.2.3", malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn unreached_check_counts_the_branch_while_it_is_checked_out() {
        let root = std::env::temp_dir().join(format!("simgit-hook-unit-{}", uuid::Uuid::new_v4()));
        let main = root.join("main");
        let linked = root.join("linked");
        std::fs::create_dir_all(&main).expect("mkdir");
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "T"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "base"]);
        let linked_arg = linked.to_str().expect("utf8");
        git(
            &main,
            &["worktree", "add", "-q", "-b", "simgit/u", linked_arg],
        );
        let repo = discover_repo(&main).expect("repo");
        assert!(!holds_unreached_commits(&repo, "simgit/u").expect("check"));
        git(&linked, &["commit", "-q", "--allow-empty", "-m", "work"]);
        assert!(holds_unreached_commits(&repo, "simgit/u").expect("check"));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
