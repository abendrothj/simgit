use clap::{Args, Subcommand};
use anyhow::Result;
use simgit_sdk::Client;

#[derive(Subcommand)]
pub enum Peer {
    /// List peer sessions (other active sessions in the same repo).
    Ls(PeerLs),
}

#[derive(Args)]
pub struct PeerLs {}

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
    }
    Ok(())
}
