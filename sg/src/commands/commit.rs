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

    let result = client.session_commit(session_id, cmd.branch, cmd.message).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Committed");
        println!("  Branch: {}", result.session.branch_name.as_deref().unwrap_or("(none)"));
        println!("  Session: {}", result.session.session_id);
        println!("  Duration: {:.1} ms", result.telemetry.total_duration_ms);
        println!("  Queue Wait: {:.1} ms", result.telemetry.scheduler_queue_wait_ms);
    }
    Ok(())
}
