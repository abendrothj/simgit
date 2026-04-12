use clap::{Args, Subcommand};
use anyhow::Result;
use simgit_sdk::Client;
use uuid::Uuid;

#[derive(Subcommand)]
pub enum Peer {
    /// List peer sessions (other active sessions in the same repo).
    Ls(PeerLs),
    /// Show diff for another active session.
    Diff(PeerDiff),
}

#[derive(Args)]
pub struct PeerLs {}

#[derive(Args)]
pub struct PeerDiff {
    /// Session ID of the peer to inspect.
    pub session_id: Uuid,
}

pub async fn run(cmd: Peer, client: &Client, json: bool) -> Result<()> {
    match cmd {
        Peer::Ls(_) => {
            let sessions = client.session_list(Some(simgit_sdk::SessionStatus::Active)).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                if sessions.is_empty() {
                    println!("(no other active sessions)");
                }
                for s in sessions {
                    println!(
                        "{} task={} peers={} mount={}",
                        s.session_id,
                        s.task_id,
                        if s.peers_enabled { "on" } else { "off" },
                        s.mount_path.display(),
                    );
                }
            }
        }
        Peer::Diff(cmd) => {
            let diff = client.session_diff(cmd.session_id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&diff)?);
            } else {
                println!("session: {}", diff.session_id);
                println!("changed paths ({}):", diff.changed_paths.len());
                for p in &diff.changed_paths {
                    println!("  {}", p.display());
                }
                if diff.unified_diff.trim().is_empty() {
                    println!("\n(no diff output)");
                } else {
                    println!("\n{}", diff.unified_diff);
                }
            }
        }
    }
    Ok(())
}
