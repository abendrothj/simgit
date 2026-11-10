//! Native Git linked worktrees populated with filesystem copy-on-write clones.
//!
//! `simgit` deliberately has no daemon dependency. Git owns the refs,
//! index, commits, and lifecycle; simgit only avoids repeatedly inflating the
//! same checkout by cloning an immutable cached baseline when the filesystem
//! supports it.

use clap::Args;
use std::path::{Path, PathBuf};

mod add;
mod cow;
mod doctor;
mod hook;
mod index;
mod launch;
mod locks;
mod maintenance;
mod overlay;
mod registry;
mod remove;
mod repo;
#[cfg(test)]
mod test_support;

pub use add::add;
pub use doctor::doctor;
pub use hook::{check_requires_before_parsing, hook, hook_arguments, WorktreeHook};
pub use launch::{run_in_worktree, WorktreeRun};
pub use locks::{unlock, WorktreeUnlock};
pub use maintenance::{gc, prune, repair, WorktreeGc, WorktreePrune};
pub use registry::list;
pub use remove::remove;

#[derive(Args)]
pub struct WorktreeAdd {
    /// Branch name to create (for example, feat/my-feature). When omitted,
    /// creates a unique `agent/<uuid>` branch unless --detach is used. With
    /// --attach, an existing branch to check out instead.
    pub branch: Option<String>,

    /// Create a detached worktree without creating or deleting a branch.
    #[arg(long, conflicts_with = "branch")]
    pub detach: bool,

    /// Check out an existing branch at its current commit instead of creating
    /// one. Refused when the branch is missing or already checked out.
    #[arg(long, requires = "branch", conflicts_with = "base")]
    pub attach: bool,

    /// Worktree path. Defaults to a slug of the branch under
    /// `../.simgit/<repo>/`.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Check out only these directories (Git cone-mode sparse checkout).
    /// Repeatable. Cuts the worktree's metadata cost proportionally.
    #[arg(long = "sparse", value_name = "DIR")]
    pub sparse: Vec<String>,

    /// Commit-ish to start from. Defaults to HEAD.
    #[arg(long)]
    pub base: Option<String>,

    /// Fail instead of using a normal Git checkout when CoW is unavailable.
    #[arg(long)]
    pub require_cow: bool,

    /// Mark the worktree as ephemeral so `gc` can reap it automatically.
    #[arg(long)]
    pub ephemeral: bool,
}

#[derive(Args)]
pub struct WorktreeRemove {
    /// Worktree path or branch name. Defaults to the worktree containing the
    /// current directory.
    pub target: Option<String>,

    /// Commit all changes before removing the worktree.
    #[arg(long)]
    pub commit: bool,

    /// Commit message for --commit.
    #[arg(short, long, default_value = "simgit remove")]
    pub message: String,

    /// Discard uncommitted changes. Without this flag, removal refuses a dirty
    /// worktree.
    #[arg(long, conflicts_with = "commit")]
    pub discard_dirty: bool,

    /// Delete the worktree branch too.
    #[arg(long)]
    pub delete_branch: bool,

    /// Permit deleting a branch that is not merged. Requires --delete-branch.
    #[arg(long)]
    pub delete_unmerged: bool,
}

#[derive(Debug)]
pub(super) struct RepoContext {
    pub(super) top_level: PathBuf,
    pub(super) common_git_dir: PathBuf,
    /// The main working tree; the parent of a bare repository named `.git`;
    /// else the common git dir. See `find_main_worktree`.
    pub(super) main_worktree: PathBuf,
    /// The outermost superproject when this is a submodule.
    pub(super) superproject: Option<Superproject>,
}

/// Where a submodule's default worktrees go; see `find_superproject`.
#[derive(Debug)]
pub(super) struct Superproject {
    /// The outermost working tree enclosing the submodule's checkout.
    working_tree: PathBuf,
    /// That superproject's main working tree, which the worktrees go beside.
    main_worktree: PathBuf,
    /// The submodule's directory under `.modules`.
    module_dir: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PopulateMode {
    /// Per-file reflink clone (APFS `clonefile`, Linux reflink).
    CowClone,
    /// `fuse-overlayfs` mount — CoW on any Linux filesystem (incl. ext4).
    Overlay,
    /// Ordinary `git checkout` — a full copy, no CoW benefit.
    GitCheckout,
}

impl PopulateMode {
    fn label(self) -> &'static str {
        match self {
            Self::CowClone => "cow-clone",
            Self::Overlay => "overlay",
            Self::GitCheckout => "git-checkout",
        }
    }
}

/// Who tears a worktree down, so that its refusals name a fix that caller's
/// user can run: `remove` takes a flag, a hook's user a whole command.
#[derive(Clone, Copy)]
pub(super) enum Caller {
    Cli,
    Hook,
}

impl Caller {
    /// Adding `flag` to the removal of `target`, in this caller's terms.
    fn flag(self, repo: &RepoContext, target: &Path, flag: &str) -> String {
        match self {
            Caller::Cli => flag.to_owned(),
            Caller::Hook => format!(
                "`{}`",
                simgit_anywhere(repo, &format!("remove {} {flag}", shell_quote(target)))
            ),
        }
    }

    /// How to recreate a deleted worktree whose sparse checkout cannot be read
    /// back: the CLI names the cone to pass, a hook's user drops the registration.
    fn sparse_recovery(self, repo: &RepoContext, target: &Path) -> String {
        match self {
            Caller::Cli => "pass --sparse with the directories to check out".to_owned(),
            Caller::Hook => format!(
                "drop its registration with `{}`, then create the worktree again",
                simgit_anywhere(repo, &format!("remove {}", shell_quote(target)))
            ),
        }
    }

    /// The step after keeping a detached worktree's commit.
    fn then_remove(self, repo: &RepoContext, target: &Path) -> String {
        match self {
            Caller::Cli => "then remove it".to_owned(),
            Caller::Hook => format!(
                "then remove it with `{}`",
                simgit_anywhere(repo, &format!("remove {}", shell_quote(target)))
            ),
        }
    }
}

/// `simgit <arguments>` as a command that works from any directory: simgit
/// finds its repository from the one it runs in, and the hook's worktrees lie
/// outside it.
pub(super) fn simgit_anywhere(repo: &RepoContext, arguments: &str) -> String {
    format!(
        "cd {} && simgit {arguments}",
        shell_quote(&repo.main_worktree)
    )
}

/// `text` as one shell word, for a command a person pastes: bare when it has
/// only characters no shell treats specially, else single-quoted.
pub(super) fn shell_quote(text: impl AsRef<std::ffi::OsStr>) -> String {
    let text = text.as_ref().to_string_lossy();
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

pub(super) fn emit(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}
