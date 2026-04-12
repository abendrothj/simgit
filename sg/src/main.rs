mod commands;

use clap::{Parser, Subcommand};
use anyhow::Result;

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
