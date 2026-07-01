//! WinFSP backend for Windows (Phase 8).
//!
//! # Overview
//!
//! WinFSP (Windows File System Proxy) is a user-mode filesystem driver for
//! Windows — the Windows equivalent of FUSE.  This backend wraps the
//! `winfsp_wrs` crate to provide a kernel-level VFS mount with real-time
//! write-time borrow-checking via `SessionVfsOps` → `BorrowRegistry`.
//!
//! # Architecture
//!
//! ```text
//! Agent writes  →  WinFSP kernel driver  →  WinFspSession (FileSystemInterface)
//!                                        →  SessionVfsOps::write()
//!                                        →  BorrowRegistry + DeltaStore
//! ```
//!
//! # Dependencies
//!
//! Requires the WinFSP runtime installed on the target machine.
//! Installation options:
//!   - MSI installer from https://github.com/winfsp/winfsp/releases
//!   - `choco install winfsp`
//!   - Bundled with the application

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::DeltaStore;
use crate::metrics::Metrics;
use crate::vfs::fuse_backend::SessionFs;

mod adapter;

/// WinFSP backend driver for Windows.
pub struct WinFspBackend {
    cfg: Arc<Config>,
    deltas: Arc<DeltaStore>,
    borrows: Arc<BorrowRegistry>,
    #[allow(dead_code)]
    metrics: Arc<Metrics>,
}

impl WinFspBackend {
    pub fn new(
        cfg: Arc<Config>,
        deltas: Arc<DeltaStore>,
        borrows: Arc<BorrowRegistry>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            cfg,
            deltas,
            borrows,
            metrics,
        }
    }
}

#[async_trait::async_trait]
impl crate::vfs::VfsBackendTrait for WinFspBackend {
    async fn mount(&self, session: &SessionInfo) -> Result<()> {
        let mount_path = session.mount_path.clone();
        std::fs::create_dir_all(&mount_path)?;

        // Bootstrap synthetic .git if enabled.
        if session.git_proxy_enabled {
            crate::git_proxy::GitProxy::bootstrap(
                &mount_path,
                &session.base_commit,
                &self.cfg.repo_path,
                session.initial_branch.as_deref(),
                session.session_id,
            )
            .ok();
        }

        let fs = SessionFs::new(
            session.session_id,
            session.peers_enabled,
            Arc::clone(&self.cfg),
            session.base_commit.clone(),
            Arc::clone(&self.deltas),
            Arc::clone(&self.borrows),
        );

        // Create the WinFSP filesystem instance and mount it.
        let session_fs = adapter::WinFspSession::new(fs);

        // WinFSP mounts to a drive letter or NTFS directory path.
        // We mount to the session's mount_path as a directory.
        let mount_point = mount_path.to_string_lossy().to_string();

        // Spawn the WinFSP filesystem in a background thread.
        // `FileSystemHost` runs the dispatch loop synchronously.
        let _host = session_fs.spawn_mount(&mount_point)?;

        // Intentionally leak the host handle to keep the mount alive.
        // The mount is torn down when the daemon calls unmount().
        // NOTE: Production code should store the host handle in a registry
        // so it can be cleanly stopped on unmount().

        Ok(())
    }

    async fn unmount(&self, session_id: Uuid) -> Result<()> {
        let mount_path = self.cfg.mnt_dir.join(session_id.to_string());

        // WinFSP volumes are unmounted by deleting the mount point directory
        // when no open handles exist.  For a drive-letter mount, we'd call
        // FspFileSystemStopDispatcher.  For directory mounts, removing the
        // directory (after all handles are closed) is sufficient.
        //
        // TODO: implement proper WinFSP stop via a stored host handle.
        let _ = std::fs::remove_dir_all(&mount_path);
        Ok(())
    }
}
