use clap::Args;
use anyhow::Result;
use uuid::Uuid;
use simgit_sdk::Client;

#[derive(Args)]
pub struct Diff {
    #[arg(long, env = "SIMGIT_SESSION")]
    pub session: Option<String>,
}

pub async fn run(cmd: Diff, client: &Client, json: bool) -> Result<()> {
    let session_str = cmd.session.ok_or_else(|| {
        anyhow::anyhow!("no session specified; set $SIMGIT_SESSION or pass --session <id>")
    })?;
    let session_id = session_str.parse::<Uuid>()?;
    let diff = client.session_diff(session_id).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&diff)?);
    } else {
        if diff.changed_paths.is_empty() {
            println!("(no changes in session {session_id})");
        } else {
            println!("{}", diff.unified_diff);
        }
    }
    Ok(())
}
