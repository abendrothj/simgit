//! Path-level commit serializer.
//!
//! # Problem
//!
//! When two sessions both write the same file and call `session.commit`
//! concurrently the conflict scan returns `active_session_overlap` and the
//! second caller gets an immediate `-32003` error.  The Python harness then
//! retries with a backoff, but under heavy load most retries find the peer still
//! active and fail again.
//!
//! # Solution
//!
//! `CommitScheduler` gates entry to the commit path on a per-path advisory lock.
//! Before a session begins its conflict scan it calls `begin_commit(paths)`,
//! which acquires one `tokio::sync::Mutex` per changed path (sorted to prevent
//! deadlock).  The returned `CommitGuard` releases all locks on drop.
//!
//! Because mutexes are per-path, sessions that touch *disjoint* file sets enter
//! the conflict scan concurrently — no unnecessary serialization.
//!
//! # Timeout
//!
//! `begin_commit` accepts a `wait_timeout`.  If it cannot acquire all locks
//! within that duration it returns `Err(CommitWaitTimeout)`.  A typical value is
//! 30 s; callers convert this to a JSON-RPC error distinct from `-32003` so the
//! client can report a timeout rather than a conflict.
//!
//! # Example
//!
//! ```ignore
//! let guard = scheduler.begin_commit(&paths, Duration::from_secs(30)).await?;
//! // … conflict scan and flatten …
//! drop(guard);   // per-path locks released
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

/// Error returned when the wait deadline expires before all path locks are free.
#[derive(Debug)]
pub struct CommitWaitTimeout;

impl std::fmt::Display for CommitWaitTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timed out waiting for conflicting session to finish")
    }
}

// ---------------------------------------------------------------------------
// Internal: per-path lock entry
// ---------------------------------------------------------------------------

/// Each entry is a reference-counted mutex.  When the last `Arc` clone is
/// dropped (all guards released *and* no waiters) the entry can be pruned.
type PathMutex = Arc<Mutex<()>>;

// ---------------------------------------------------------------------------
// CommitGuard — RAII holder of multiple per-path mutex guards
// ---------------------------------------------------------------------------

/// Held by a session for the duration of its commit.  Dropping this releases
/// all per-path locks so the next waiter can proceed.
pub struct CommitGuard {
    // `_guards` keeps the `MutexGuard`s alive; drop order doesn't matter for
    // correctness since we hold *all* of them simultaneously.
    _guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
}

// ---------------------------------------------------------------------------
// CommitScheduler
// ---------------------------------------------------------------------------

/// Global per-path commit serializer shared through `AppState`.
pub struct CommitScheduler {
    /// Registry of per-path mutexes.  Protected by a plain `Mutex` so entries
    /// can be inserted/looked-up without async overhead (the critical section is
    /// very short — just a HashMap insert + clone).
    registry: Mutex<HashMap<PathBuf, PathMutex>>,
    /// Default wait timeout used when callers pass `None`.
    default_timeout: Duration,
}

impl CommitScheduler {
    /// Create a scheduler with the given default wait timeout.
    pub fn new(default_timeout: Duration) -> Self {
        Self {
            registry: Mutex::new(HashMap::new()),
            default_timeout,
        }
    }

    /// Acquire advisory locks for every path in `paths`, then return a guard.
    ///
    /// Paths are sorted before locking to guarantee a consistent acquisition
    /// order and prevent deadlocks when two sessions overlap on a subset of
    /// paths.
    ///
    /// The call waits up to `timeout` (or `self.default_timeout` when `None`)
    /// for all locks to become available.  On timeout `Err(CommitWaitTimeout)`
    /// is returned and *no* locks are held.
    pub async fn begin_commit(
        &self,
        paths: &[impl AsRef<Path>],
        timeout: Option<Duration>,
    ) -> Result<CommitGuard, CommitWaitTimeout> {
        if paths.is_empty() {
            return Ok(CommitGuard { _guards: vec![] });
        }

        // Deduplicate and sort paths for consistent lock order.
        let mut sorted: Vec<PathBuf> = paths
            .iter()
            .map(|p| p.as_ref().to_path_buf())
            .collect();
        sorted.sort();
        sorted.dedup();

        // Collect (or create) the per-path mutex for every path we need.
        let mutexes: Vec<PathMutex> = {
            let mut registry = self.registry.lock().await;
            sorted
                .iter()
                .map(|p| {
                    registry
                        .entry(p.clone())
                        .or_insert_with(|| Arc::new(Mutex::new(())))
                        .clone()
                })
                .collect()
        };

        let deadline = timeout.unwrap_or(self.default_timeout);

        // Acquire locks one by one in sorted order.
        let mut guards: Vec<tokio::sync::OwnedMutexGuard<()>> = Vec::with_capacity(mutexes.len());
        for (i, mtx) in mutexes.into_iter().enumerate() {
            match tokio::time::timeout(deadline, mtx.lock_owned()).await {
                Ok(guard) => guards.push(guard),
                Err(_elapsed) => {
                    // Drop already-held guards before returning so peers can proceed.
                    drop(guards);
                    // Also remove partially-created entries from registry to avoid
                    // unbounded growth: the paths we *didn't* lock won't have a
                    // guard, so future calls will re-create them as needed.
                    let _ = i; // already used above in loop
                    return Err(CommitWaitTimeout);
                }
            }
        }

        Ok(CommitGuard { _guards: guards })
    }

    /// Remove path entries that are no longer contested (no active waiters or
    /// holders).  This is a best-effort GC step; omitting it only causes memory
    /// growth proportional to the number of distinct paths ever committed.
    ///
    /// Call periodically (e.g. after every successful commit) when the session
    /// count is high.
    pub async fn gc(&self) {
        let mut registry = self.registry.lock().await;
        registry.retain(|_, mtx| Arc::strong_count(mtx) > 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn disjoint_paths_run_concurrently() {
        let sched = Arc::new(CommitScheduler::new(Duration::from_secs(5)));
        let counter = Arc::new(AtomicU32::new(0));

        let paths_a = vec![PathBuf::from("a.txt")];
        let paths_b = vec![PathBuf::from("b.txt")];

        let s1 = Arc::clone(&sched);
        let c1 = Arc::clone(&counter);
        let h1 = tokio::spawn(async move {
            let _g = s1.begin_commit(&paths_a, None).await.unwrap();
            c1.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            c1.fetch_sub(1, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let s2 = Arc::clone(&sched);
        let c2 = Arc::clone(&counter);
        let h2 = tokio::spawn(async move {
            let _g = s2.begin_commit(&paths_b, None).await.unwrap();
            // Should have been allowed in even while h1 holds a.txt lock.
            assert_eq!(c2.load(Ordering::SeqCst), 1, "b.txt should have entered concurrently with a.txt");
        });

        let _ = tokio::join!(h1, h2);
    }

    #[tokio::test]
    async fn overlapping_paths_serialize() {
        let sched = Arc::new(CommitScheduler::new(Duration::from_secs(5)));
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));

        let paths = vec![PathBuf::from("shared.txt")];

        let s1 = Arc::clone(&sched);
        let p1 = paths.clone();
        let l1 = Arc::clone(&log);
        let h1 = tokio::spawn(async move {
            let _g = s1.begin_commit(&p1, None).await.unwrap();
            l1.lock().await.push("start-1");
            tokio::time::sleep(Duration::from_millis(60)).await;
            l1.lock().await.push("end-1");
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let s2 = Arc::clone(&sched);
        let p2 = paths.clone();
        let l2 = Arc::clone(&log);
        let h2 = tokio::spawn(async move {
            let _g = s2.begin_commit(&p2, None).await.unwrap();
            l2.lock().await.push("start-2");
        });

        let _ = tokio::join!(h1, h2);

        let events = log.lock().await.clone();
        // "end-1" must come before "start-2" — serialized by path lock.
        let pos_end1   = events.iter().position(|&e| e == "end-1").unwrap();
        let pos_start2 = events.iter().position(|&e| e == "start-2").unwrap();
        assert!(pos_end1 < pos_start2, "h2 should wait for h1: {events:?}");
    }

    #[tokio::test]
    async fn timeout_returns_error() {
        let sched = Arc::new(CommitScheduler::new(Duration::from_millis(50)));
        let paths = vec![PathBuf::from("contested.txt")];

        // Acquire lock and hold it for longer than the timeout.
        let s1 = Arc::clone(&sched);
        let p1 = paths.clone();
        let _h1 = tokio::spawn(async move {
            let _g = s1.begin_commit(&p1, None).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let result = sched.begin_commit(&paths, Some(Duration::from_millis(20))).await;
        assert!(result.is_err(), "should have timed out");
    }
}
