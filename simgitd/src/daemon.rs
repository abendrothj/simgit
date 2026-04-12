//! Top-level daemon bootstrap: initialises subsystems and runs the main loop.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::signal;
use tracing::info;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::DeltaStore;
use crate::events::EventBroker;
use crate::rpc::RpcServer;
use crate::session::SessionManager;
use crate::vfs::VfsManager;

/// Shared application state threaded through every subsystem.
#[derive(Clone)]
pub struct AppState {
    pub config:   Arc<Config>,
    pub sessions: Arc<SessionManager>,
    pub borrows:  Arc<BorrowRegistry>,
    pub deltas:   Arc<DeltaStore>,
    pub events:   Arc<EventBroker>,
    pub vfs:      Arc<VfsManager>,
}

pub async fn run(cfg: Config) -> Result<()> {
    // Ensure state directories exist.
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("create state_dir: {}", cfg.state_dir.display()))?;
    std::fs::create_dir_all(&cfg.mnt_dir)
        .with_context(|| format!("create mnt_dir: {}", cfg.mnt_dir.display()))?;

    let db_path = cfg.state_dir.join("state.db");
    let cfg = Arc::new(cfg);

    // Initialise subsystems.
    let sessions = Arc::new(SessionManager::open(&db_path).await?);
    let borrows  = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
    let deltas   = Arc::new(DeltaStore::new(cfg.state_dir.join("deltas")));
    let events   = Arc::new(EventBroker::new());
    let vfs      = Arc::new(VfsManager::new(Arc::clone(&cfg)));

    let state = AppState { config: Arc::clone(&cfg), sessions, borrows, deltas, events, vfs };

    // Recover any sessions that were ACTIVE before a previous crash.
    state.sessions.recover_active_sessions(&state).await?;

    // Start TTL sweeper.
    crate::borrow::ttl_sweeper::spawn(Arc::clone(&state.borrows));

    // Start RPC server.
    let rpc = RpcServer::new(state.clone());
    let socket_path = cfg.state_dir.join("control.sock");
    let rpc_handle = tokio::spawn(async move { rpc.serve(&socket_path).await });

    info!("simgitd ready — socket at {}", cfg.state_dir.join("control.sock").display());

    // Wait for SIGINT or SIGTERM.
    tokio::select! {
        _ = signal::ctrl_c() => { info!("received SIGINT, shutting down"); }
        Ok(()) = async {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
            sigterm.recv().await;
            Ok::<(), std::io::Error>(())
        } => { info!("received SIGTERM, shutting down"); }
    }

    // Graceful shutdown: unmount all active sessions.
    state.vfs.unmount_all().await;
    rpc_handle.abort();

    info!("simgitd stopped");
    Ok(())
}
