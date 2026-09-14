//! # simgit — native copy-on-write Git worktrees
//!
//! A command-line tool for native copy-on-write Git worktrees. `simgit add`
//! creates real linked worktrees whose unchanged extents share an immutable
//! baseline via filesystem CoW (APFS `clonefile` / Linux reflink), falling
//! back to an ordinary `git checkout` when the filesystem can't clone.
//!
//! It has no daemon and no server: Git owns the refs, the filesystem owns the
//! data. Agents work in separate Git worktrees in parallel and integrate through
//! normal Git merges.
//!
//! The canonical binary is `simgit`; `sg` is installed as an alias and accepts
//! exactly the same arguments.
//!
//! ## Commands
//!
//! - `simgit doctor` — report identity, repository, filesystem, populate mode,
//!   default worktree root, Git worktree support, stale registrations, and the
//!   baseline cache (`--json` for machine-readable output). Outside a Git
//!   worktree it still succeeds, reporting the repository-dependent fields as
//!   null
//! - `simgit add [branch]` — create a CoW linked worktree. Omitting the branch
//!   generates a unique `agent/<uuid>` branch; `--detach` creates a detached
//!   worktree and no ref at all (`--ephemeral` marks it for automatic `gc`,
//!   `--json` reports `path`/`worktree`/`cleanup_token` and `branch`, which is
//!   null when detached)
//! - `simgit list` — list worktrees
//! - `simgit remove <branch|path>` — remove a worktree (optionally committing
//!   first); removing a target that is already gone succeeds
//! - `simgit run [branch] -- <command>` — create, reuse, or pick a workspace
//!   and launch any command inside it
//! - `simgit unlock [branch|path]` — clear a `run` lock left behind by a
//!   launcher that was killed, unless the process holding it is still alive
//! - `simgit gc` — reap idle/ephemeral worktrees and optionally branches
//! - `simgit repair` — remount interrupted Linux overlay worktrees
//! - `simgit prune` — prune stale worktree administrative entries
//!
//! `--json` is a global flag: it is accepted before or after the command name.
//!
//! ## Example
//!
//! ```bash
//! # Create a linked worktree on a generated agent branch and cd into it
//! cd "$(simgit add --json --ephemeral | jq -r .path)"
//!
//! git add <files>
//! git commit -m "feature work"
//!
//! simgit remove --commit   # commit and remove
//! ```

mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// simgit — native copy-on-write Git worktrees.
#[derive(Parser)]
#[command(
    name = "simgit",
    version,
    about = "simgit — native copy-on-write Git worktrees"
)]
struct Cli {
    /// Output machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Diagnose repository, filesystem, and worktree-provider readiness.
    Doctor,
    /// Create a real Git linked worktree, using CoW clones when supported.
    Add(commands::worktree::WorktreeAdd),
    /// List linked worktrees using Git's native registry.
    List,
    /// Remove a linked worktree, by path or by branch name.
    Remove(commands::worktree::WorktreeRemove),
    /// Run any command in a workspace; omit the branch to choose interactively.
    Run(commands::worktree::WorktreeRun),
    /// Clear a stranded `run` lock, by path or by branch name.
    Unlock(commands::worktree::WorktreeUnlock),
    /// Reap idle/ephemeral worktrees (e.g. abandoned agent sandboxes).
    Gc(commands::worktree::WorktreeGc),
    /// Prune stale Git registrations and old cached baselines.
    Prune(commands::worktree::WorktreePrune),
    /// Remount overlay-backed worktrees after a reboot or interrupted mount.
    Repair,
}

/// Restore the default disposition for `SIGPIPE`.
///
/// Rust's runtime sets `SIGPIPE` to `SIG_IGN` before `main`, so writing to a
/// closed pipe returns `EPIPE` and the printing macros escalate that into a
/// panic and exit 101. `list` writes one line per worktree, so `simgit list |
/// head -1` would panic where every other line-oriented tool simply stops.
fn restore_default_sigpipe() {
    // SIGPIPE and SIG_DFL as libc defines them on the supported targets:
    // signal number 13, and the reserved null value of the pointer-sized
    // handler type.
    const SIGPIPE: std::ffi::c_int = 13;
    const SIG_DFL: usize = 0;
    extern "C" {
        fn signal(signum: std::ffi::c_int, handler: usize) -> usize;
    }
    // SAFETY: `SIG_DFL` is the reserved default-disposition value, not a
    // handler this process owns, so the call dereferences nothing. Resetting a
    // signal to its default is the documented way to undo the runtime's
    // `SIG_IGN`, and its previous-handler return value is of no use here.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

/// Parse this process's arguments and execute the requested command. Both the
/// canonical `simgit` binary and its `sg` alias are one-line wrappers around
/// this, so the CLI is compiled and tested once.
pub fn run_cli() -> Result<()> {
    restore_default_sigpipe();
    let cli = Cli::parse();
    let json = cli.json;
    match cli.command {
        Commands::Doctor => commands::worktree::doctor(json),
        Commands::Add(args) => commands::worktree::add(args, json),
        Commands::List => commands::worktree::list(json),
        Commands::Remove(args) => commands::worktree::remove(args, json),
        Commands::Run(args) => commands::worktree::run_in_worktree(args, json),
        Commands::Unlock(args) => commands::worktree::unlock(args, json),
        Commands::Gc(args) => commands::worktree::gc(args, json),
        Commands::Prune(args) => commands::worktree::prune(args, json),
        Commands::Repair => commands::worktree::repair(json),
    }
}
