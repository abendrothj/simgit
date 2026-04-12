use clap::Args;
use anyhow::Result;
use simgit_sdk::{Client, SessionStatus};

#[derive(Args)]
pub struct Gc {}

pub async fn run(_cmd: Gc, client: &Client) -> Result<()> {
    let all = client.session_list(Some(SessionStatus::Stale)).await?;
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(24);
    let mut n = 0;
    for s in all {
        if s.created_at < cutoff {
            client.session_abort(s.session_id).await?;
            println!("GC: purged stale session {}", s.session_id);
            n += 1;
        }
    }
    println!("GC complete: {n} session(s) removed.");
    Ok(())
}
