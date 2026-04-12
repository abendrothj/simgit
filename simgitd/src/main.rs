mod daemon;
mod vfs;
mod borrow;
mod delta;
mod session;
mod rpc;
mod events;
mod config;
mod metrics;

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("SIMGIT_LOG")
                .add_directive("simgitd=info".parse()?),
        )
        .init();

    let cfg = config::Config::load()?;
    info!(repo = %cfg.repo_path.display(), state = %cfg.state_dir.display(), "simgitd starting");

    daemon::run(cfg).await
}
