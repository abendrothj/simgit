//! # sg — simgit CLI
//!
//! A command-line tool for native copy-on-write Git worktrees. `sg worktree`
//! creates real linked worktrees whose unchanged extents share an immutable
//! baseline via filesystem CoW (APFS `clonefile` / Linux reflink), falling back
//! to an ordinary `git checkout` when the filesystem can't clone.
//!
//! It has no daemon and no server: Git owns the refs, the filesystem owns the
//! data. Agents work in isolated worktrees in parallel and integrate through
//! normal Git merges.
//!
//! ## Commands
//!
//! - `sg worktree add <branch>` — create a CoW linked worktree (`--ephemeral`
//!   marks it for automatic `gc`, `--json` for machine-readable output)
//! - `sg worktree list` — list worktrees
//! - `sg worktree remove <branch|path>` — remove a worktree (optionally
//!   committing first)
//! - `sg worktree gc` — reap idle/ephemeral worktrees (abandoned agent sandboxes)
//! - `sg worktree prune` — prune stale worktree administrative entries
//!
//! ## Example
//!
//! ```bash
//! # Create a linked worktree and cd into it
//! cd $(sg worktree add feature-1)
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
#[command(name = "sg", about = "simgit — native copy-on-write Git worktrees")]
struct Cli {
    /// Output machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Native CoW-backed linked worktrees.
    #[command(subcommand)]
    Worktree(commands::worktree::Worktree),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Worktree(cmd) => commands::worktree::run(cmd, cli.json),
    }
}
