use clap::Args;
use anyhow::Result;
use simgit_sdk::Client;

#[derive(Args)]
pub struct Status {}

pub async fn run(_cmd: Status, client: &Client, json: bool) -> Result<()> {
    let sessions = client.session_list(None).await?;
    let locks    = client.lock_list(None).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "sessions": sessions,
            "locks":    locks,
        }))?);
        return Ok(());
    }

    println!("Sessions ({}):", sessions.len());
    for s in &sessions {
        println!(
            "  [{:?}] {} | task={} | mount={}",
            s.status,
            s.session_id,
            s.task_id,
            s.mount_path.display(),
        );
    }

    println!("\nLocks ({}):", locks.len());
    for l in &locks {
        println!(
            "  {} | writer={} | readers={}",
            l.path.display(),
            l.writer_session.map(|u| u.to_string()).unwrap_or_else(|| "(none)".into()),
            l.reader_sessions.len(),
        );
    }
    Ok(())
}
