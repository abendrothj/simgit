//! Command launch and workspace selection, independent of the chosen agent.

use super::{
    create_worktree, discover_repo, git_failure, git_output_common, list_worktrees, mark_ephemeral,
    overlay, remove_file_if_present, worktree_admin_dir, worktree_description,
    worktree_path_for_branch, RepoContext, WorktreeAdd, WorktreeEntry, WorktreeLock,
};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;

#[derive(Args)]
pub struct WorktreeRun {
    /// Branch to create or reuse. Omit to choose an existing workspace.
    pub branch: Option<String>,

    /// Worktree path. Defaults to `../.simgit/<repo>/<branch>`.
    #[arg(long)]
    pub path: Option<PathBuf>,

    /// Commit-ish to start from. Defaults to HEAD.
    #[arg(long)]
    pub base: Option<String>,

    /// Fail instead of using a normal Git checkout when CoW is unavailable.
    #[arg(long)]
    pub require_cow: bool,

    /// Explicitly keep the workspace persistent (the default for new worktrees).
    #[arg(long, conflicts_with = "ephemeral")]
    pub persistent: bool,

    /// Mark the workspace as disposable for GC.
    #[arg(long)]
    pub ephemeral: bool,

    /// Command and arguments to execute in the worktree.
    #[arg(required = true, last = true)]
    pub command: Vec<OsString>,
}

pub(super) fn run_in_worktree(args: WorktreeRun, json: bool) -> Result<()> {
    if json {
        bail!("--json is not supported with `run` because command output is streamed");
    }
    let repo = discover_repo(&std::env::current_dir()?)?;
    let (branch, existing) = match args.branch {
        Some(branch) => {
            let path = worktree_path_for_branch(&repo, &branch)?
                .or_else(|| overlay::worktree_for_branch(&repo, &branch));
            (Some(branch), path)
        }
        None => {
            if args.base.is_some() || args.require_cow {
                bail!("--base and --require-cow require an explicit branch to create a worktree");
            }
            let entry = pick_worktree(&repo)?;
            let branch = entry.branch.map(|reference| {
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&reference)
                    .to_owned()
            });
            (branch, Some(entry.path))
        }
    };
    let target = if let Some(target) = existing {
        if args.base.is_some() || args.require_cow {
            bail!("--base and --require-cow apply only when creating a worktree");
        }
        if let Some(path) = &args.path {
            if path.canonicalize()? != target.canonicalize()? {
                bail!(
                    "branch '{}' already has a worktree at {}",
                    branch.as_deref().unwrap_or("(detached)"),
                    target.display()
                );
            }
        }
        target
    } else {
        let branch = branch
            .as_ref()
            .context("branch is required to create a workspace")?;
        let reference = format!("refs/heads/{branch}");
        let exists = git_output_common(&repo, ["show-ref", "--verify", "--quiet", &reference])?;
        let attach = match exists.status.code() {
            Some(0) => true,
            Some(1) => false,
            _ => return Err(git_failure("look up branch", &exists)),
        };
        let created = create_worktree(
            &WorktreeAdd {
                branch: branch.clone(),
                path: args.path,
                base: args.base,
                require_cow: args.require_cow,
                ephemeral: args.ephemeral,
                json: false,
            },
            attach,
        )?;
        eprintln!("mode: {}", created.mode.label());
        created.target
    };
    // Git's lock also protects against native `git worktree remove/prune`.
    // A killed launcher leaves the lock in place; unlock manually after checking
    // the child has stopped. This conservatively avoids PID-reuse heuristics.
    let lock = WorktreeLock::acquire(&repo, &target)?;
    overlay::repair(&repo, &target)?;
    let actual = discover_repo(&target)?;
    if actual.common_git_dir != repo.common_git_dir
        || actual.top_level.canonicalize()? != target.canonicalize()?
    {
        bail!("registered worktree is unavailable: {}", target.display());
    }
    if args.ephemeral {
        mark_ephemeral(&target)?;
    } else if args.persistent {
        remove_file_if_present(&worktree_admin_dir(&target)?.join("simgit-ephemeral"))?;
    }
    eprintln!(
        "worktree: {} (branch: {})",
        target.display(),
        branch.as_deref().unwrap_or("(detached)")
    );
    let (program, command_args) = args.command.split_first().context("command is required")?;
    let status = Command::new(program)
        .args(command_args)
        .current_dir(&target)
        .status()
        .with_context(|| format!("run command in {}", target.display()))?;
    lock.release()?;
    if !status.success() {
        bail!(
            "command exited with {status}; worktree retained at {}",
            target.display()
        );
    }
    Ok(())
}

fn pick_worktree(repo: &RepoContext) -> Result<WorktreeEntry> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!(
            "workspace selection requires a terminal; pass a branch: sg run <branch> -- <command>"
        );
    }
    let mut entries = list_worktrees(repo)?;
    if entries.is_empty() {
        bail!("no workspaces found; create one with sg run <branch> -- <command>");
    }
    eprintln!("Choose a workspace:");
    for (index, entry) in entries.iter().enumerate() {
        eprintln!("  {}. {}", index + 1, worktree_description(repo, entry));
    }
    loop {
        eprint!("Workspace [1-{}], or q to cancel: ", entries.len());
        io::stderr().flush()?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 || answer.trim().eq_ignore_ascii_case("q") {
            bail!("workspace selection cancelled");
        }
        if let Ok(number) = answer.trim().parse::<usize>() {
            if (1..=entries.len()).contains(&number) {
                return Ok(entries.remove(number - 1));
            }
        }
        eprintln!(
            "Enter a number from 1 to {}, or q to cancel.",
            entries.len()
        );
    }
}
