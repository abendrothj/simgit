//! Virtual filesystem layer — native copy-on-write working trees.
//!
//! # Overview
//!
//! Each session gets a **real working tree** that is a copy-on-write clone of a
//! shared read-only baseline materialized from `base_commit`:
//!
//! - **Linux**: overlayfs (`lowerdir` = shared baseline, per-session `upperdir`
//!   for writes), falling back to a reflink clone if overlay is not permitted.
//! - **macOS / other Unix**: APFS `clonefile` (`cp -c`) or `cp --reflink=auto`.
//!
//! Reads and writes are ordinary filesystem operations at native latency. The
//! disk-scaling benefit — one shared baseline plus per-session deltas instead of
//! N full checkouts — is preserved by the CoW clone.
//!
//! # Architecture
//!
//! Each session's working tree lives at `<mnt_dir>/<session-uuid>/` and contains
//! a real in-tree `.git` (bootstrapped by [`crate::git_proxy`]) so native git
//! commands work and the pre-commit hook forwards commits to the daemon.
//!
//! # Conflict detection
//!
//! Unlike a write-intercepting VFS, conflicts are detected at **commit time**:
//! [`CowBackend::capture_mount_delta`] diffs the working tree (overlayfs upperdir
//! scan on Linux, `git status` otherwise) into the per-session delta store, and
//! the commit scheduler / borrow registry resolves overlaps from there.
//!
//! # Lifecycle
//!
//! 1. **Create Session** → SessionManager.create()
//! 2. **Mount** → [`VfsManager::mount`] materializes the CoW working tree
//! 3. **Agent Works** → ordinary reads/writes against the working tree
//! 4. **Commit** → [`VfsManager::capture_mount_delta`] captures changes
//! 5. **Unmount** → [`VfsManager::unmount_session`] removes the tree + GCs baselines

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::info;
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::DeltaStore;

mod cow_backend;

use cow_backend::CowBackend;

/// VFS manager — owns the copy-on-write backend and tracks active mounts.
///
/// # Example
///
/// ```ignore
/// let cfg = Arc::new(Config::load()?);
/// let vfs = VfsManager::new(cfg, deltas, borrows, metrics);
///
/// let session = SessionInfo {
///     session_id: Uuid::new_v7(),
///     mount_path: PathBuf::from("/mnt/sess-001"),
///     ..Default::default()
/// };
///
/// vfs.mount(&session).await?;             // Materialize CoW working tree
/// // Agent reads/writes via the working tree
/// vfs.unmount_session(session.session_id).await;  // Clean up
/// ```
pub struct VfsManager {
    backend: CowBackend,
    /// Track which sessions are currently mounted.
    mounted: Mutex<HashMap<Uuid, PathBuf>>,
}

impl VfsManager {
    /// Create a new VFS manager backed by the copy-on-write backend.
    pub fn new(
        cfg: Arc<Config>,
        deltas: Arc<DeltaStore>,
        borrows: Arc<BorrowRegistry>,
        metrics: Arc<crate::metrics::Metrics>,
    ) -> Self {
        Self {
            backend: CowBackend::new(cfg, deltas, borrows, metrics),
            mounted: Mutex::new(HashMap::new()),
        }
    }

    /// Mount a session's VFS by materializing its CoW working tree.
    ///
    /// Blocks until the working tree is ready for I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the mount directory can't be created or the CoW
    /// clone / overlay mount fails.
    pub async fn mount(&self, session: &SessionInfo) -> Result<()> {
        self.backend.mount(session).await?;
        self.mounted
            .lock()
            .unwrap()
            .insert(session.session_id, session.mount_path.clone());
        info!(session = %session.session_id, path = %session.mount_path.display(), "VFS mounted");
        Ok(())
    }

    /// Unmount a single session's VFS.
    ///
    /// Best-effort cleanup: removes tracking, tears down the working tree
    /// (overlay umount + dir removal, or clone removal), and GCs baselines.
    /// Safe to call multiple times (idempotent).
    pub async fn unmount_session(&self, session_id: Uuid) {
        if self.mounted.lock().unwrap().remove(&session_id).is_some() {
            let _ = self.backend.unmount(session_id).await;
            info!(session = %session_id, "VFS unmounted");
        }
    }

    /// Unmount all active sessions.
    ///
    /// Called during daemon shutdown. Errors are logged, not returned.
    pub async fn unmount_all(&self) {
        let ids: Vec<Uuid> = self.mounted.lock().unwrap().keys().copied().collect();
        for id in ids {
            self.unmount_session(id).await;
        }
    }

    /// Capture the session's working-tree changes into the delta store.
    ///
    /// Called before commit to materialize CoW writes as delta entries.
    pub fn capture_mount_delta(&self, session: &SessionInfo) -> Result<()> {
        self.backend.capture_mount_delta(session)
    }

    /// Notify the backend that the session's base commit changed.
    ///
    /// A no-op for the CoW backend: base changes flow through native `git
    /// checkout` inside the working tree, so there is no VFS-side tree to swap.
    pub fn update_base_commit(&self, session_id: Uuid, new_base: &str) {
        self.backend.update_base_commit(session_id, new_base);
    }
}
