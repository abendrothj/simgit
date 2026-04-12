//! Top-level daemon bootstrap: initializes subsystems and runs the main loop.
//!
//! # Overview
//!
//! The `run()` function:
//! 1. Loads configuration from environment/defaults
//! 2. Creates state directories
//! 3. Initializes all subsystems (database, caches, RPC server, VFS)
//! 4. Recovers any sessions that were active before a crash
//! 5. Waits for signals (SIGINT/SIGTERM) and performs graceful shutdown
//!
//! # Application State
//!
//! All subsystems share a single `AppState` instance threaded through RPC/signal handlers.
//! This enables coordination across:
//! - Session lifecycle (creation, persistence, recovery)
//! - Borrow checking (write lock acquisition/release)
//! - Delta storage (capture agent writes)
//! - Event broadcasting (multi-agent coordination)
//! - VFS mounts (kernel filesystem integration)
//!
//! # Startup Sequence
//!
//! ```text
//! simgitd main()
//!       ↓
//! run(config)
//!       ├─ mkdir state_dir, mnt_dir
//!       ├─ SessionManager::open(db_path) ← SQLite startup
//!       │   └─ recover_active_sessions() ← Crash recovery
//!       ├─ BorrowRegistry::new() ← TTL-based lock tracking
//!       ├─ DeltaStore::new() ← CoW blob storage
//!       ├─ EventBroker::new() ← Pub/sub for agents
//!       ├─ VfsManager::new() ← Backend selection (FUSE/NFS)
//!       ├─ RpcServer::new() ← Unix socket listener
//!       ├─ spawn(ttl_sweeper) ← Background TTL cleanup task
//!       ├─ spawn(rpc_server) ← Background RPC listener task
//!       ├─ signal::ctrl_c() or signal::unix::SIGTERM
//!       │   ↓
//!       ├─ vfs::unmount_all() ← Unmount all active sessions
//!       ├─ rpc_handle.abort() ← Kill RPC listener
//!       └─ return Ok(()) ← Daemon exits
//! ```
//!
//! # Crash Recovery
//!
//! On startup, the daemon queries the SQLite database for sessions in ACTIVE state.
//! These sessions' mounts are restored in the VFS layer, allowing agents to
//! reconnect without data loss.
//!
//! # Signal Handling
//!
//! - **SIGINT** (Ctrl+C): Initiate graceful shutdown
//! - **SIGTERM**: Same as SIGINT
//! - **SIGTERM/SIGKILL** (without handler): Dirty shutdown (sessions persist for recovery)
//!
//! # Thread Model
//!
//! - **Main task**: Waits for signals (blocks until shutdown)
//! - **RPC task**: Listens on Unix socket, spawns handler tasks per request
//! - **TTL sweeper task**: Background cleanup of expired locks (1s interval)
//! - **FUSE/NFS tasks** (per session): Spawned by VFS backend, die when mount unmounts
//!
//! # Graceful Shutdown
//!
//! On signal:
//! 1. All active sessions are unmounted (agents disconnected)
//! 2. SQLite database is closed properly (COMMIT any pending transactions)
//! 3. RPC listener is killed (blocks new session.create requests)
//! 4. Delta blobs are finalized and persisted
//! 5. Daemon process exits cleanly

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

/// Shared application state distributed to all subsystems.
///
/// All major components (sessions, borrows, deltas, events, VFS) maintain
/// references to a single `AppState` instance for coordinated operation.
/// This enables:
/// - Session queries from RPC handlers
/// - Lock validation across Borrow and RPC
/// - Event publishing when state changes
/// - VFS integration with session metadata
///
/// # Fields
///
/// - **config**: Daemon configuration (repo path, state dir, quotas)
/// - **sessions**: Session database + lifecycle manager
/// - **borrows**: Write lock registry (Borrow checker)
/// - **deltas**: Delta CoW storage + manifest tracker
/// - **events**: Pub/sub event broker for agents
/// - **vfs**: VFS mount manager (FUSE/NFS backend)
///
/// # Cloning
///
/// AppState is cheap to clone (all fields are Arc-wrapped).
/// Clone and share AppState with every spawned task.
///
/// # Example
///
/// ```ignore
/// let state = AppState { /* initialized */ };
/// let session = state.sessions.get(session_id)?;
/// state.events.publish(Event::PeerCommit { ... })?;
/// state.vfs.mount(&session).await?;
/// ```
#[derive(Clone)]
pub struct AppState {
    pub config:   Arc<Config>,
    pub sessions: Arc<SessionManager>,
    pub borrows:  Arc<BorrowRegistry>,
    pub deltas:   Arc<DeltaStore>,
    pub events:   Arc<EventBroker>,
    pub vfs:      Arc<VfsManager>,
}

/// Run the simgitd daemon until shutdown signal.
///
/// # Arguments
///
/// - `cfg`: Daemon configuration (repository, state directory, quotas)
///
/// # Initialization Steps
///
/// 1. Create state directories (`state_dir`, `mnt_dir`) with mode 0700
/// 2. Open SQLite database (`state.db`) in WAL mode
/// 3. Recover sessions that were ACTIVE before a crash
/// 4. Initialize subsystems (Borrow, Delta, Events, VFS)
/// 5. Spawn background tasks (RPC server, TTL sweeper)
/// 6. Wait for SIGINT/SIGTERM
/// 7. Graceful shutdown (unmount sessions, close socket, exit)
///
/// # Returns
///
/// `Ok(())` on clean shutdown; `Err(...) ` if initialization fails.
///
/// # Errors
///
/// - Cannot create state directories (permissions)
/// - Cannot open SQLite database (corruption, disk full)
/// - Cannot bind RPC socket (already in use)
/// - Backend initialization failed (FUSE kernel, NFS resources)
///
/// # Panics
///
/// None expected post-initialization; all errors are propagated as Result.
///
/// # Signals
///
/// - **SIGINT** (Ctrl+C): Initiates graceful shutdown
/// - **SIGTERM**: Initiates graceful shutdown
/// - Unhandled **SIGKILL**: Process dies; sessions persist in DB for recovery on restart
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
