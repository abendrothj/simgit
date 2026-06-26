//! Background task that releases write locks whose TTL has expired.
//!
//! An agent that crashes without explicitly committing or aborting would
//! otherwise leave paths locked indefinitely. The TTL sweeper prevents that.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use super::BorrowRegistry;

/// Spawn the sweeper. Runs every 30 seconds until `shutdown` is signalled.
///
/// When the daemon receives SIGINT/SIGTERM, callers should signal the sender
/// so the sweeper exits cleanly instead of running during VFS teardown.
///
/// # Example
///
/// ```ignore
/// let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
/// ttl_sweeper::spawn(registry, shutdown_rx);
/// // … on shutdown …
/// let _ = shutdown_tx.send(true);
/// ```
pub fn spawn(registry: Arc<BorrowRegistry>, shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        // Missed ticks should not pile up after a long sweep or pause.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut shutdown = shutdown;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    sweep(&registry);
                }
                _ = shutdown.changed() => {
                    // Shutdown signal received (sender was dropped or value changed).
                    info!("TTL sweeper shutting down");
                    break;
                }
            }
        }
    });
}

fn sweep(registry: &BorrowRegistry) {
    let stale = registry.stale_writers();
    if stale.is_empty() {
        return;
    }
    let count = stale.len();
    for (path, session_id, acquired_at, ttl) in stale {
        warn!(
            path = %path.display(),
            session = %session_id,
            acquired_at = %acquired_at,
            ttl_seconds = ttl,
            "TTL expired — force-releasing write lock and marking session STALE"
        );
        registry.force_release_and_mark_stale(session_id);
    }
    info!("TTL sweep released {} stale write lock(s)", count);
}
