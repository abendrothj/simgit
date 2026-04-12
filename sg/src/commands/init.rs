use clap::Args;
use anyhow::Result;

#[derive(Args)]
pub struct Init {
    /// Path to the git repository to manage. Defaults to $PWD.
    pub repo: Option<String>,
}

pub async fn run(cmd: Init) -> Result<()> {
    let repo = cmd.repo.unwrap_or_else(|| ".".to_owned());
    println!("Initializing simgit for repo: {repo}");
    println!("Run `simgitd` separately (or use `sg daemon start`) to start the daemon.");
    // Phase 2: will write a simgit.toml and optionally exec simgitd.
    Ok(())
}
