use clap::{Args, Subcommand};
use anyhow::Result;

#[derive(Subcommand)]
pub enum Daemon {
    /// Start simgitd in the background.
    Start(DaemonStart),
    /// Gracefully stop simgitd.
    Stop(DaemonStop),
}

#[derive(Args)]
pub struct DaemonStart {
    /// Repository path (default: $PWD).
    pub repo: Option<String>,
}

#[derive(Args)]
pub struct DaemonStop {}

pub async fn run(cmd: Daemon) -> Result<()> {
    match cmd {
        Daemon::Start(s) => {
            let repo = s.repo.unwrap_or_else(|| ".".to_owned());
            // Exec simgitd with SIMGIT_REPO set. A platform-specific daemonise
            // wrapper will be added in Phase 5; for now we just exec.
            let status = std::process::Command::new("simgitd")
                .env("SIMGIT_REPO", repo)
                .spawn()?;
            println!("Started simgitd (pid {})", status.id());
        }
        Daemon::Stop(_) => {
            // Phase 5: send a shutdown RPC.
            println!("Send SIGTERM to the simgitd process to stop it.");
        }
    }
    Ok(())
}
