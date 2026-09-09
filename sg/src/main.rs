//! # sg — simgit CLI
//!
//! A command-line tool for native copy-on-write Git worktrees. `sg worktree`
//! creates real linked worktrees whose unchanged extents share an immutable
//! baseline via filesystem CoW (APFS `clonefile` / Linux reflink), falling back
//! to an ordinary `git checkout` when the filesystem can't clone.
//!
//! It has no daemon and no server: Git owns the refs, the filesystem owns the
//! data. Agents work in separate Git worktrees in parallel and integrate through
//! normal Git merges.
//!
//! ## Commands
//!
//! - `sg worktree add <branch>` — create a CoW linked worktree (`--ephemeral`
//!   marks it for automatic `gc`, `--json` for machine-readable output)
//! - `sg worktree list` — list worktrees
//! - `sg worktree remove <branch|path>` — remove a worktree (optionally
//!   committing first)
//! - `sg run [branch] -- <command>` — create, reuse, or pick a workspace and
//!   launch any command inside it (`sg worktree run` is also supported)
//! - `sg worktree gc` — reap idle/ephemeral worktrees and optionally branches
//! - `sg worktree repair` — remount interrupted Linux overlay worktrees
//! - `sg worktree prune` — prune stale worktree administrative entries
//!
//! ## Example
//!
//! ```bash
//! # Create a linked worktree and cd into it
//! cd "$(sg worktree add feature-1)"
//!
//! git add <files>
//! git commit -m "feature work"
//!
//! sg worktree remove --commit   # commit and remove
//! ```

mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// simgit — native copy-on-write Git worktrees.
#[derive(Parser)]
#[command(
    name = "sg",
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
    /// Run any command in a workspace; omit the branch to choose interactively.
    Run(commands::worktree::WorktreeRun),
    /// Native CoW-backed linked worktrees.
    #[command(subcommand)]
    Worktree(commands::worktree::Worktree),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run(args) => {
            commands::worktree::run(commands::worktree::Worktree::Run(args), cli.json)
        }
        Commands::Worktree(cmd) => commands::worktree::run(cmd, cli.json),
    }
}
