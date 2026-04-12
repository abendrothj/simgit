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
    /// List recent peer/global events.
    Events(PeerEvents),
}

#[derive(Args)]
pub struct PeerLs {}

#[derive(Args)]
pub struct PeerDiff {
    /// Session ID of the peer to inspect.
    pub session_id: Uuid,
}

#[derive(Args)]
pub struct PeerEvents {
    /// Optional session filter for emitted events.
    #[arg(long)]
    pub session_id: Option<Uuid>,

    /// Max events to return (default 50, server max 500).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Continuously stream events using `event.subscribe`.
    #[arg(long)]
    pub stream: bool,

    /// Long-poll timeout in milliseconds for stream mode.
    #[arg(long)]
    pub timeout_ms: Option<u64>,
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
        Peer::Events(cmd) => {
            if cmd.stream {
                loop {
                    let event = client
                        .event_subscribe(cmd.session_id, cmd.timeout_ms)
                        .await?;
                    let Some(e) = event else {
                        continue;
                    };
                    if json {
                        println!("{}", serde_json::to_string(&e)?);
                    } else {
                        println!(
                            "{} | {} | {} | {}",
                            e.emitted_at.to_rfc3339(),
                            e.kind,
                            e.source_session,
                            e.payload
                        );
                    }
                }
            }

            let events = client.event_list(cmd.session_id, cmd.limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else {
                println!("events ({})", events.len());
                for e in events {
                    println!(
                        "{} | {} | {} | {}",
                        e.emitted_at.to_rfc3339(),
                        e.kind,
                        e.source_session,
                        e.payload
                    );
                }
            }
        }
    }
    Ok(())
}
