use clap::Args;
use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::Client;

#[derive(Args)]
pub struct Commit {
    /// Session ID to commit. Defaults to $SIMGIT_SESSION.
    #[arg(long, env = "SIMGIT_SESSION")]
    pub session: Option<String>,

    /// Branch name to create. Defaults to `simgit/<session-id>`.
    #[arg(short, long)]
    pub branch: Option<String>,

    /// Commit message.
    #[arg(short, long)]
    pub message: Option<String>,
}

pub async fn run(cmd: Commit, client: &Client, json: bool) -> Result<()> {
    let session_str = cmd.session.ok_or_else(|| {
        anyhow::anyhow!("no session specified; set $SIMGIT_SESSION or pass --session <id>")
    })?;
    let session_id = session_str.parse::<Uuid>()?;

    let info = client.session_commit(session_id, cmd.branch, cmd.message).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("Committed");
        println!("  Branch: {}", info.branch_name.as_deref().unwrap_or("(none)"));
        println!("  Session: {}", info.session_id);
    }
    Ok(())
}
