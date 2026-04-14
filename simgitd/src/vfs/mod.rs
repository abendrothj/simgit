//! Virtual filesystem layer — abstracts FUSE (Linux) and NFS-loopback (macOS) backends.
//!
//! # Overview
//!
//! The VFS layer presents a unified interface for mounting read-only git trees + delta CoW overlays.
//! It abstracts platform-specific details (FUSE XDR, NFS RPC) behind a simple trait:
//!
//! ```ignore
//! pub trait VfsBackendTrait {
//!     async fn mount(&self, session: &SessionInfo) -> Result<()>;
//!     async fn unmount(&self, session_id: Uuid) -> Result<()>;
//! }
//! ```
//!
//! # Supported Platforms
//!
//! - **Linux**: FUSE (via `fuser` crate, all distros)
//! - **macOS**: NFS-loopback stub (Phase 0); full NFSv3 server (Phase 1+)
//!
//! # Architecture
//!
//! Each session gets a VFS mount at `/vdev/<session-uuid>/` containing:
//!
//! ```text
//! /vdev/<session-uuid>/
//!   ├── (read-only git tree from base_commit)
//!   ├── (delta writes from this session, CoW)
//!   └── (merged view: git tree + alterations)
//! ```
//!
//! Reads and directory listings merge:
//! - **Git tree** (baseline): Files from HEAD commit
//! - **Delta additions**: Files written by this session
//! - **Deletions**: Paths marked as deleted in delta manifest
//!
//! # Read Flow
//!
//! ```text
//! Agent reads /src/main.rs
//!       ↓
//! VFS receives read request → SessionFs::read()
//!       ↓
//! Check delta layer (has this session written it?)
//!       ├─ YES → return delta version
//!       └─ NO → read from git blob
//! ```
//!
//! # Write Flow
//!
//! ```text
//! Agent writes /src/main.rs
//!       ↓
//! VFS receives write request → SessionFs::write()
//!       ↓
//! BorrowRegistry: acquire_write(session_id, path)
//!       ├─ SUCCESS → proceed to delta store
//!       └─ CONFLICT → return BorrowError immediately
//!       ↓
//! DeltaStore::write(content) → hash, save to blob store
//!       ↓
//! Update delta manifest
//!       ↓
//! Return success to agent
//! ```
//!
//! # Lifecycle
//!
//! 1. **Create Session** → SessionManager.create()
//! 2. **Mount VFS** → VfsManager.mount() (blocks until FUSE/NFS is ready)
//! 3. **Agent Works** → reads/writes via `/vdev/...` (VFS intercepts)
//! 4. **Unmount** → VfsManager.unmount_session() (clean up kernel resources)
//! 5. **Flatten** → Apply delta to git branch
//!
//! # Configuration
//!
//! Configured in `simgitd.toml`:
//!
//! ```toml
//! [vfs]
//! backend = "fuse"      # or "nfs-loopback"
//! mount_dir = "/vdev"
//! ```
//!
//! On macOS, the backend is selected automatically (always NFS-loopback for now).
//!
//! # Phase Roadmap
//!
//! - **Phase 0**: Backend abstraction, FUSE skeleton on Linux, NFS stub on macOS
//! - **Phase 1**: Git tree traversal + inode caching, read-only serving
//! - **Phase 2**: Delta write interception, manifest tracking
//! - **Phase 3**: Borrow checking integrated into write path
//! - **Phase 4**: Three-way merge on flatten
//! - **Phase 1+**: Full NFSv3 XDR server for macOS (no kernel extensions)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::info;
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::config::{Config, VfsBackend};
use crate::borrow::BorrowRegistry;
use crate::delta::DeltaStore;

mod fuse_backend;
mod git_resolver;
mod nfs_backend;

/// Trait implemented by both the FUSE and NFS-loopback backends.
///
/// Each backend must implement mounting and unmounting of a session's filesystem.
/// The VfsManager delegates to the appropriate backend based on platform/config.
///
/// # Implementation Notes
///
/// - **FUSE** (Linux): Uses fuser crate to negotiate with kernel FUSE subsystem.
///   Registration happens via init → getattr → lookup → read/write.
/// - **NFS-loopback** (macOS): Creates a plain directory tree at mount_path;
///   no kernel involvement (Phase 0 stub).
///
/// # Atomicity
///
/// Mount operations are NOT atomic across the filesystem. If a mount fails partway
/// (e.g., kernel permission denied), cleanup is backend-specific. Backends should
/// be idempotent: calling mount twice with the same session should succeed on
/// the second attempt or return the existing mount point.
#[async_trait::async_trait]
pub trait VfsBackendTrait: Send + Sync {
    /// Mount a session's VFS at [`SessionInfo::mount_path`].
    ///
    /// Blocks until the mount is ready (FUSE negotiated with kernel, NFS RPC listening).
    /// Returns immediately; the mount persists across multiple file operations.
    ///
    /// # Errors
    ///
    /// - Permission denied (session mount_path not writable)
    /// - Mount point already exists (race condition)
    /// - Backend resource exhaustion (too many mounts)
    async fn mount(&self, session: &SessionInfo) -> Result<()>;

    /// Unmount a session's VFS by its session ID.
    ///
    /// Best-effort; errors are logged but don't fail the unmount operation.
    /// Cleans up kernel resources (FUSE fd, inode cache) or directory tree.
    async fn unmount(&self, session_id: Uuid) -> Result<()>;

    /// Synchronize backend-native mount writes into the delta store prior to commit.
    ///
    /// Backends with in-kernel/write-intercept paths (FUSE) can keep the default no-op.
    /// Backends that currently mount plain directories (macOS NFS-loopback stub) can
    /// materialize changed files into the per-session delta manifest here.
    fn capture_mount_delta(&self, _session: &SessionInfo) -> Result<()> {
        Ok(())
    }
}

/// VFS manager — dispatches mount/unmount to the appropriate backend.
///
/// Tracks active mounts and coordinates lifecycle across sessions.
///
/// # Example
///
/// ```ignore
/// let cfg = Arc::new(Config::load("simgitd.toml")?);
/// let vfs = VfsManager::new(cfg);
///
/// let session = SessionInfo {
///     session_id: Uuid::new_v7(),
///     mount_path: PathBuf::from("/vdev/sess-001"),
///     ..Default::default()
/// };
///
/// vfs.mount(&session).await?;  // Blocks until VFS is ready
///
/// // Agent reads/writes via /vdev/sess-001/
///
/// vfs.unmount_session(session.session_id).await;  // Clean up
/// ```

pub struct VfsManager {
    backend:  Box<dyn VfsBackendTrait>,
    /// Track which sessions are currently mounted.
    mounted: Mutex<HashMap<Uuid, PathBuf>>,
}

impl VfsManager {
    /// Create a new VFS manager with the appropriate backend.
    ///
    /// Backend selection:
    /// - Linux: Always FUSE (or explicitly configured)
    /// - macOS: Always NFS-loopback (kernel extension disabled)
    ///
    /// # Panics
    ///
    /// None. Backend instantiation is always fallible via async mount calls.
    pub fn new(cfg: Arc<Config>, deltas: Arc<DeltaStore>, borrows: Arc<BorrowRegistry>, metrics: Arc<crate::metrics::Metrics>) -> Self {
        let backend: Box<dyn VfsBackendTrait> = match cfg.vfs_backend {
            VfsBackend::Fuse        => Box::new(fuse_backend::FuseBackend::new(cfg, deltas, borrows)),
            VfsBackend::NfsLoopback => Box::new(nfs_backend::NfsLoopbackBackend::new(cfg, deltas, metrics)),
        };
        Self { backend, mounted: Mutex::new(HashMap::new()) }
    }

    /// Mount a session's VFS.
    ///
    /// Delegates to the backend (FUSE or NFS-loopback) and tracks the mount in the manager.
    /// Blocks until the mount is ready for I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Mount directory doesn't exist or is not writable
    /// - Backend resource exhaustion
    /// - Platform-specific setup failed (e.g., FUSE module not loaded)
    pub async fn mount(&self, session: &SessionInfo) -> Result<()> {
        self.backend.mount(session).await?;
        self.mounted.lock().unwrap().insert(session.session_id, session.mount_path.clone());
        info!(session = %session.session_id, path = %session.mount_path.display(), "VFS mounted");
        Ok(())
    }

    /// Unmount a single session's VFS.
    ///
    /// Best-effort cleanup:
    /// - Removes tracking from manager
    /// - Delegates unmount to backend (FUSE fd close, directory cleanup)
    /// - Logs errors instead of propagating (don't want daemon crashes on unmount)
    ///
    /// Safe to call multiple times (idempotent).
    pub async fn unmount_session(&self, session_id: Uuid) {
        if self.mounted.lock().unwrap().remove(&session_id).is_some() {
            let _ = self.backend.unmount(session_id).await;
            info!(session = %session_id, "VFS unmounted");
        }
    }

    /// Unmount all active sessions.
    ///
    /// Called during daemon shutdown. Ensures all kernel resources (FUSE fds, etc.)
    /// are cleaned up before the daemon exits.
    ///
    /// Errors are logged, not returned (best-effort).
    pub async fn unmount_all(&self) {
        let ids: Vec<Uuid> = self.mounted.lock().unwrap().keys().copied().collect();
        for id in ids {
            self.unmount_session(id).await;
        }
    }

    /// Ask the backend to synchronize mount-side writes into the session delta store.
    pub fn capture_mount_delta(&self, session: &SessionInfo) -> Result<()> {
        self.backend.capture_mount_delta(session)
    }
}
