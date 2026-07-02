//! `sg session-set-base` — update a session's base commit.
//!
//! Called by git hooks (post-checkout, post-merge, post-rewrite, pre-push)
//! to notify the daemon when the working tree HEAD changes.

use clap::Args;
use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::Client;

#[derive(Args)]
pub struct SessionSetBase {
    /// Session ID. Defaults to $SIMGIT_SESSION.
    #[arg(long, env = "SIMGIT_SESSION")]
    pub session: Option<String>,

    /// The new base commit hash.
    #[arg(long)]
    pub commit: String,
}

pub async fn run(cmd: SessionSetBase, client: &Client) -> Result<()> {
    let session_str = cmd.session.ok_or_else(|| {
        anyhow::anyhow!("no session specified; set $SIMGIT_SESSION or pass --session <id>")
    })?;
    let session_id = session_str.parse::<Uuid>()?;

    client.session_set_base(session_id, &cmd.commit).await?;
    Ok(())
}
