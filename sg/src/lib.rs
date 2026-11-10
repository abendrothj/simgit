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
//!   generates a unique `agent/<uuid>` branch; `--attach` checks out an
//!   existing branch instead; `--detach` creates a detached
//!   worktree and no ref at all (`--ephemeral` marks it for automatic `gc`,
//!   `--json` reports `path`/`worktree`/`cleanup_token` and `branch`, which is
//!   null when detached)
//! - `simgit list` — list worktrees
//! - `simgit remove <branch|path>` — remove a worktree (optionally committing
//!   first); removing a target that is already gone succeeds
//! - `simgit run [branch] -- <command>` — create, reuse, or pick a workspace
//!   and launch any command inside it
//! - `simgit unlock [branch|path]` — clear simgit's own `run` or
//!   allocation lock left behind by a killed process, unless the process holding
//!   it is still alive; a lock set with `git worktree lock` is refused
//! - `simgit hook claude-code --requires X.Y.Z` — handle one Claude Code hook
//!   payload read from stdin, refusing to run on an older simgit
//! - `simgit gc` — reap idle/ephemeral worktrees and optionally branches
//! - `simgit repair` — remount interrupted Linux overlay worktrees
//! - `simgit prune` — prune stale worktree registrations and cached baselines
//!   unused for seven days or whose commit nothing reaches, never one an
//!   allocation is using
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
    /// Clear a stranded `run` or allocation lock, by path or by branch name.
    Unlock(commands::worktree::WorktreeUnlock),
    /// Handle an agent harness's hook payload read from stdin.
    Hook(commands::worktree::WorktreeHook),
    /// Reap idle/ephemeral worktrees (e.g. abandoned agent sandboxes).
    Gc(commands::worktree::WorktreeGc),
    /// Prune stale Git registrations and old or unreachable cached baselines.
    Prune(commands::worktree::WorktreePrune),
    /// Remount overlay-backed worktrees after a reboot or interrupted mount.
    Repair,
}

// The libc signal and session calls used across the crate, declared once. A
// handler is passed as its address, so `SIG_DFL` and `SIG_IGN` fit the same
// parameter.
extern "C" {
    pub(crate) fn signal(signum: std::ffi::c_int, handler: usize) -> usize;
    pub(crate) fn kill(pid: i32, signal: std::ffi::c_int) -> std::ffi::c_int;
    pub(crate) fn setsid() -> i32;
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
    // SAFETY: `SIG_DFL` is the reserved default-disposition value, not a
    // handler this process owns, so the call dereferences nothing. Resetting a
    // signal to its default is the documented way to undo the runtime's
    // `SIG_IGN`, and its previous-handler return value is of no use here.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

/// Preserve error context while keeping machine-facing diagnostics on one line.
///
/// Only line breaks are replaced, each by one space, and blank lines dropped;
/// all other whitespace is kept, because it can be part of a path the
/// diagnostic names, and a path is only useful verbatim.
fn one_line(diagnostic: &str) -> String {
    diagnostic
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse this process's arguments, execute the requested command and return the
/// process exit code. Both the canonical `simgit` binary and its `sg` alias are
/// one-line wrappers around this, so the CLI is compiled and tested once.
///
/// Errors are printed here, in the format `main` returning `Err` would use,
/// because that path appends a backtrace whenever `RUST_BACKTRACE` or
/// `RUST_LIB_BACKTRACE` is set, which breaks the one-line machine contract.
pub fn run_cli() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", format_error(&error));
            std::process::ExitCode::FAILURE
        }
    }
}

/// `Error: <message>`, then the causes under `Caused by:`, as anyhow's debug
/// output lays them out, without the backtrace it would add.
fn format_error(error: &anyhow::Error) -> String {
    let mut text = format!("Error: {error}");
    let mut causes = error.chain().skip(1).enumerate().peekable();
    if causes.peek().is_some() {
        text.push_str("\n\nCaused by:");
        let numbered = error.chain().count() > 2;
        for (index, cause) in causes {
            if numbered {
                text.push_str(&format!("\n{index:>5}: {cause}"));
            } else {
                text.push_str(&format!("\n    {cause}"));
            }
        }
    }
    text
}

fn run() -> Result<()> {
    restore_default_sigpipe();
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    commands::worktree::check_requires_before_parsing(&arguments)?;
    let cli = Cli::try_parse().unwrap_or_else(|error| {
        // Clap prints usage and hints on several lines. In JSON mode, and for
        // a hook, which the harness shows the person, a failed invocation has
        // no stdout and exactly one stderr diagnostic instead.
        let json = arguments
            .iter()
            .take_while(|argument| *argument != "--")
            .any(|argument| argument == "--json");
        let one_line_usage = json || commands::worktree::hook_arguments(&arguments).is_some();
        if one_line_usage && error.use_stderr() {
            let diagnostic = error.to_string();
            let primary = diagnostic.split("\n\n").next().unwrap_or(&diagnostic);
            eprintln!("{}", one_line(primary));
            std::process::exit(error.exit_code());
        }
        error.exit()
    });
    let json = cli.json;
    // The harness shows a failed hook's stderr to the person: one line, too.
    let one_line_errors = json || matches!(cli.command, Commands::Hook(_));
    let result = match cli.command {
        Commands::Doctor => commands::worktree::doctor(json),
        Commands::Add(args) => commands::worktree::add(args, json),
        Commands::List => commands::worktree::list(json),
        Commands::Remove(args) => commands::worktree::remove(args, json),
        Commands::Run(args) => commands::worktree::run_in_worktree(args, json),
        Commands::Unlock(args) => commands::worktree::unlock(args, json),
        Commands::Hook(args) => commands::worktree::hook(args, json),
        Commands::Gc(args) => commands::worktree::gc(args, json),
        Commands::Prune(args) => commands::worktree::prune(args, json),
        Commands::Repair => commands::worktree::repair(json),
    };
    if one_line_errors {
        result.map_err(|error| anyhow::anyhow!(one_line(&format!("{error:#}"))))
    } else {
        result
    }
}
