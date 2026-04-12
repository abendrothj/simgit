use clap::{Args, Subcommand};
use anyhow::Result;
use std::path::PathBuf;
use simgit_sdk::Client;

#[derive(Subcommand)]
pub enum Lock {
    /// List all current locks.
    Ls(LockLs),
    /// Block until a path's write lock is free.
    Wait(LockWait),
}

#[derive(Args)]
pub struct LockLs {
    /// Filter to a specific path prefix.
    pub path: Option<PathBuf>,
}

#[derive(Args)]
pub struct LockWait {
    pub path: PathBuf,
    #[arg(long, default_value = "5000")]
    pub timeout_ms: u64,
}

pub async fn run(cmd: Lock, client: &Client, json: bool) -> Result<()> {
    match cmd {
        Lock::Ls(ls) => {
            let locks = client.lock_list(ls.path.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&locks)?);
            } else {
                if locks.is_empty() {
                    println!("(no locks held)");
                }
                for l in locks {
                    println!(
                        "{} writer={} readers={}",
                        l.path.display(),
                        l.writer_session.map(|u| u.to_string()).unwrap_or_else(|| "(none)".into()),
                        l.reader_sessions.len(),
                    );
                }
            }
        }
        Lock::Wait(w) => {
            let acquired = client.lock_wait(&w.path, Some(w.timeout_ms)).await?;
            if acquired {
                println!("Lock on {} is free.", w.path.display());
            } else {
                eprintln!("Timed out waiting for lock on {}.", w.path.display());
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
