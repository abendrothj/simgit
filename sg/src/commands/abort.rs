use clap::Args;
use anyhow::Result;
use uuid::Uuid;
use simgit_sdk::Client;

#[derive(Args)]
pub struct Abort {
    #[arg(long, env = "SIMGIT_SESSION")]
    pub session: Option<String>,
}

pub async fn run(cmd: Abort, client: &Client) -> Result<()> {
    let session_str = cmd.session.ok_or_else(|| {
        anyhow::anyhow!("no session specified; set $SIMGIT_SESSION or pass --session <id>")
    })?;
    let session_id = session_str.parse::<Uuid>()?;
    client.session_abort(session_id).await?;
    println!("Session {session_id} aborted — delta discarded, locks released.");
    Ok(())
}
