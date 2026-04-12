//! Background task that releases write locks whose TTL has expired.
//!
//! An agent that crashes without explicitly committing or aborting would
//! otherwise leave paths locked indefinitely. The TTL sweeper prevents that.

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use super::BorrowRegistry;

/// Spawn the sweeper. Runs every 30 seconds.
pub fn spawn(registry: Arc<BorrowRegistry>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            sweep(&registry);
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
            "TTL expired — force-releasing write lock"
        );
        registry.force_release(session_id);
    }
    info!("TTL sweep released {} stale write lock(s)", count);
}
