use clap::Args;
use anyhow::Result;

use simgit_sdk::Client;

#[derive(Args)]
pub struct New {
    /// Logical task identifier for this agent session.
    #[arg(short, long)]
    pub task: String,

    /// Human-readable agent description.
    #[arg(short, long)]
    pub label: Option<String>,

    /// Base commit to fork from (default: HEAD).
    #[arg(long)]
    pub base: Option<String>,

    /// Enable real-time peer visibility for this session.
    #[arg(long)]
    pub peers: bool,
}

pub async fn run(cmd: New, client: &Client, json: bool) -> Result<()> {
    let info = client
        .session_create(cmd.task, cmd.label, cmd.base, cmd.peers)
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("Session created");
        println!("  ID:    {}", info.session_id);
        println!("  Mount: {}", info.mount_path.display());
        println!("  Base:  {}", info.base_commit);
        if info.peers_enabled {
            println!("  Peers: enabled");
        }
        println!();
        println!("export SIMGIT_SESSION={}", info.session_id);
        println!("cd {}", info.mount_path.display());
    }
    Ok(())
}
