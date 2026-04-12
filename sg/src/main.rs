//! # sg — simgit Command-Line Interface
//!
//! A command-line tool for managing simgit agent sessions with borrow-checked filesystem semantics.
//!
//! ## Overview
//!
//! `sg` is the primary user-facing interface to simgitd. It provides clients (AI agents, human users)
//! with commands to:
//! - **Create sessions** with borrow-checked write semantics
//! - **Commit/abort** changes (flatten delta to git branch or discard)
//! - **Monitor locks** (which session locked which file?)
//! - **Observe peers** (collaborate with other agents)
//! - **Manage daemon** (start/stop simgitd)
//!
//! ## Architecture
//!
//! ```text
//! Agent/User
//!    ↑↓
//! sg CLI ← invokes subcommands (init, new, commit, abort, status, lock, peer, gc, daemon)
//!    ↓
//! RPC Client ← connects to simgitd via Unix socket
//!    ↓
//! simgitd ← processes requests, manages sessions/locks/VFS
//! ```
//!
//! ## Socket Path
//!
//! By default, `sg` connects to the daemon socket at:
//! - Linux: `$XDG_RUNTIME_DIR/simgitd.sock`
//! - macOS: `$HOME/.local/state/simgit/simgitd.sock`
//!
//! Override with:
//! ```bash
//! sg --socket /custom/path/sock <command>
//! export SIMGIT_SOCKET=/custom/path/sock  # env var
//! ```
//!
//! ## Output Formats
//!
//! By default, output is human-readable (tables, lists).
//! For machine-readable output:
//! ```bash
//! sg --json status  # JSON output
//! ```
//!
//! ## Command Reference
//!
//! - **init** — Initialize simgit + start daemon (one-time setup)
//! - **new** — Create new session (`--branch <name>` for target branch)
//! - **commit** — Flatten delta to git branch + close session
//! - **abort** — Discard delta + close session
//! - **status** — Show all sessions, locks, peer visibility
//! - **diff** — Show unified diff of session changes (read-only)
//! - **lock status** — List all write locks (by session/path/TTL)
//! - **lock release** — Forcefully release a lock (admin only)
//! - **peer ls** — List active peer sessions
//! - **peer diff** — Show in-flight diff for a peer session
//! - **peer events** — Poll recent peer/global events (`--stream` for long-poll stream)
//! - **gc** — Delete STALE sessions older than 24h
//! - **daemon start/stop/status** — Manage simgitd lifecycle
//!
//! ## Example Workflow
//!
//! ```bash
//! # One-time: initialize daemon
//! sg init
//!
//! # Agent 1: Create session on 'feature-1' branch
//! SG_SESSION=$(sg new --branch feature-1 | jq .session_id)
//! mount | grep /vdev/$SG_SESSION    # Agent now sees /vdev/$SG_SESSION mounted
//! # Agent writes to /vdev/$SG_SESSION/...
//! sg diff $SG_SESSION                 # Preview changes
//! sg commit $SG_SESSION               # Flatten to git 'feature-1' branch
//!
//! # Agent 2: Check lock status
//! sg lock status
//! # → (no locks shown if agent 1 has committed)
//! ```
//!
//! ## Error Codes
//!
//! - 0: Success
//! - 1: Daemon connection failed (simgitd not running)
//! - 2: RPC error (session not found, borrow conflict, etc.)
//! - 3: File I/O error (disk, permissions)
//! - 4: Git error (invalid branch, merge conflict, etc.)
//!
//! ## Design Notes
//!
//! - All commands are stateless: connection made per invocation
//! - No local session state: all state queried from simgitd
//! - Subcommands map 1:1 to RPC methods
//! - JSON output is exact RPC response (for integration)
//!
//! ## Future Enhancements (Phase 2+)
//!
//! - Interactive REPL mode (persistent connection)
//! - Streaming output (large diffs)
//! - Authentication/authorization (Entra ID)
//! - Custom output formats (CSV, YAML)

mod commands;

use clap::{Parser, Subcommand};
use anyhow::Result;

/// Command-line arguments for the sg CLI.
///
/// # Global Options
///
/// - `--socket <PATH>`: Custom daemon socket path (defaults to `$XDG_RUNTIME_DIR/simgitd.sock`)
/// - `--json`: Output in JSON format (for machine parsing)
///
/// # Subcommands
///
/// See [`Commands`] enum for available subcommands.
#[derive(Parser)]
#[command(name = "sg", about = "simgit — borrow-checked filesystem sessions for AI agents")]
struct Cli {
    /// Override the daemon socket path.
    #[arg(long, env = "SIMGIT_SOCKET", global = true)]
    socket: Option<String>,

    /// Output machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

/// All available sg subcommands.
///
/// Each subcommand corresponds to an RPC method in simgitd:
///
/// - **Init** (`session.create` future; startup handshake)
/// - **New** (`session.create`)
/// - **Commit** (`session.commit` with `flatten=true`)
/// - **Abort** (`session.abort`)
/// - **Diff** (`session.diff`)
/// - **Status** (`session.list` + `lock.list`)
/// - **Lock** (nested: `lock.list`, `lock.release`)
/// - **Peer** (nested: `session.list` with `peer_filter`)
/// - **Gc** (custom: delete stale sessions)
/// - **Daemon** (nested: `daemon.start`, `daemon.stop`, `daemon.status`)
///
/// # Example
///
/// ```bash
/// sg new --branch feature-x         # Dispatch to Commands::New
/// sg lock release <lock_id>         # Dispatch to Commands::Lock(Release)
/// sg daemon start                   # Dispatch to Commands::Daemon(Start)
/// ```
#[derive(Subcommand)]
enum Commands {
    /// Initialize simgit for a repository and start the daemon.
    Init(commands::init::Init),
    /// Create a new agent session.
    New(commands::new::New),
    /// Flatten the session's delta to a git branch.
    Commit(commands::commit::Commit),
    /// Discard the session's delta layer.
    Abort(commands::abort::Abort),
    /// Show the unified diff of an in-flight session.
    Diff(commands::diff::Diff),
    /// Show all active sessions and lock table.
    Status(commands::status::Status),
    /// Lock table operations.
    #[command(subcommand)]
    Lock(commands::lock::Lock),
    /// Peer session operations.
    #[command(subcommand)]
    Peer(commands::peer::Peer),
    /// Garbage-collect STALE sessions older than 24h.
    Gc(commands::gc::Gc),
    /// Daemon management.
    #[command(subcommand)]
    Daemon(commands::daemon::Daemon),
}

/// Entry point for the sg CLI.
///
/// # Execution Flow
///
/// 1. Parse command-line arguments via clap
/// 2. Resolve daemon socket path (flag > env var > default)
/// 3. Connect RPC client to socket
/// 4. Dispatch to subcommand handler
/// 5. Return exit code (0 = success, non-zero = error)
///
/// # Error Propagation
///
/// All errors (connection, RPC, I/O) are returned as [`anyhow::Result`].
/// The Rust runtime automatically converts Result to exit codes.
///
/// # Async Runtime
///
/// Uses tokio for async/await (required for RPC client).
/// No background tasks; all execution is in main() context.
///
/// # Example
///
/// ```bash
/// sg --socket /tmp/test.sock new --branch feature-x
/// # Returns exit code 0 if successful, non-zero if failed
/// ```
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let socket = cli.socket.unwrap_or_else(|| {
        simgit_sdk::client::default_socket_path()
            .to_string_lossy()
            .into_owned()
    });
    let client = simgit_sdk::Client::new(&socket);

    match cli.command {
        Commands::Init(cmd)   => commands::init::run(cmd).await,
        Commands::New(cmd)    => commands::new::run(cmd, &client, cli.json).await,
        Commands::Commit(cmd) => commands::commit::run(cmd, &client, cli.json).await,
        Commands::Abort(cmd)  => commands::abort::run(cmd, &client).await,
        Commands::Diff(cmd)   => commands::diff::run(cmd, &client, cli.json).await,
        Commands::Status(cmd) => commands::status::run(cmd, &client, cli.json).await,
        Commands::Lock(cmd)   => commands::lock::run(cmd, &client, cli.json).await,
        Commands::Peer(cmd)   => commands::peer::run(cmd, &client, cli.json).await,
        Commands::Gc(cmd)     => commands::gc::run(cmd, &client).await,
        Commands::Daemon(cmd) => commands::daemon::run(cmd).await,
    }
}
