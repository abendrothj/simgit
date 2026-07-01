//! FUSE backend using the `fuser` crate (Phase 1 implementation).
//!
//! # Overview
//!
//! Implements a copy-on-write (CoW) filesystem overlay mounted at `/vdev/<session-uuid>/`
//! that merges:
//! - **Read-only git tree** (from base_commit)
//! - **Delta overlay** (writes from the current session)
//!
//! The filesystem behaves from the agent's perspective as a complete filesystem,
//! but internally bridges two data sources (git + delta store).
//!
//! # FUSE Handler Traits
//!
//! The implementation follows the `fuser::Filesystem` trait:
//! - `init()`: Daemon initialization (currently a no-op)
//! - `lookup()`: Resolve filename → inode number
//! - `getattr()`: Return file metadata (size, mode, mtime)
//! - `read()`: Serve file contents
//! - `readdir()`: List directory entries
//! - `write()` (Phase 2): Intercept writes to delta store
//! - `unlink()` / `rename()`: Track tombstones and renames in delta manifest (Phase 2)
//!
//! # Request Flow
//!
//! 1. **Agent opens file** → kernel → fuser thread pool → SessionFs handler
//! 2. **Handler acquires read reference** on inode
//! 3. **Delta check**: Is file in delta store? (written by this session)
//!    - YES → serve from delta
//!    - NO → resolve path in git tree, serve blob
//! 4. **Handler releases reference**
//! 5. **Kernel returns data to agent**
//!
//! # Architecture
//!
//! Each session has a **SessionFs** instance with three caches:
//! - **TreeCache** (1024 entries): git tree objects
//! - **BlobCache** (50 entries, 10 MiB cap): file contents (small files only)
//! - **InodeMap**: inode number → (path, git OID) mapping
//!
//! # Current Scope
//!
//! Phase 1 (read-only serving) is complete, with Phase 2 now partially implemented:
//! - ✅ Traversal of git tree structure
//! - ✅ Inode caching for fast lookups
//! - ✅ Blob serving (small files from cache, large files streamed)
//! - ✅ Directory listing (merged view)
//! - ✅ Existing-file write interception into delta store
//! - ✅ Tombstone-aware lookup/readdir/getattr/read/open
//! - ✅ File unlink/rename capture into delta manifest
//! - ⬜ New file creation (`create`) interception
//! - ⬜ Borrow checking (Phase 3)
//!
//! # Performance Characteristics
//!
//! - **Read-only tree traversal**: O(log n) per lookup (binary search in git tree)
//! - **Cache hit**: O(1) (HashMap lookup)
//! - **Cache miss**: ~100–500ms (git subprocess call)
//! - **readdir() without cache**: O(n) in tree size (unavoidable)
//! - **Memory**: ~10–50 MiB per session (configurable)
//!
//! # Linux-Only
//!
//! FUSE is Linux-native. macOS support uses NFS-loopback (see [nfs_backend]).

use std::sync::Arc;

use anyhow::Result;
use fuser::MountOption;
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::DeltaStore;

mod filesystem_impl;
mod session_fs;
mod tree;

#[cfg(all(test, target_os = "linux"))]
mod linux_integration_tests;

use session_fs::SessionFs;

/// FUSE backend driver (Linux).
///
/// Responsible for mounting and unmounting FUSE filesystems via the fuser crate.
/// One instance per daemon; spawns a SessionFs handler per session.
pub struct FuseBackend {
    cfg: Arc<Config>,
    deltas: Arc<DeltaStore>,
    borrows: Arc<BorrowRegistry>,
}

impl FuseBackend {
    /// Create a new FUSE backend with daemon configuration.
    ///
    /// # Arguments
    ///
    /// - `cfg`: Daemon configuration (mount options, cache sizes, git repo path)
    pub fn new(cfg: Arc<Config>, deltas: Arc<DeltaStore>, borrows: Arc<BorrowRegistry>) -> Self {
        Self {
            cfg,
            deltas,
            borrows,
        }
    }
}

#[async_trait::async_trait]
impl super::VfsBackendTrait for FuseBackend {
    async fn mount(&self, session: &SessionInfo) -> Result<()> {
        let mount_path = session.mount_path.clone();
        std::fs::create_dir_all(&mount_path)?;

        let fs = SessionFs::new(
            session.session_id,
            session.peers_enabled,
            Arc::clone(&self.cfg),
            session.base_commit.clone(),
            Arc::clone(&self.deltas),
            Arc::clone(&self.borrows),
        );

        let mut config = fuser::Config::default();
        config.mount_options = vec![MountOption::FSName(format!(
            "simgit-{}",
            session.session_id
        ))];

        // Spawn mount in a background thread.
        let _guard = fuser::spawn_mount2(fs, &mount_path, &config)?;
        std::mem::forget(_guard); // intentional: Keep mount alive

        Ok(())
    }

    async fn unmount(&self, session_id: Uuid) -> Result<()> {
        let mount_path = self.cfg.mnt_dir.join(session_id.to_string());
        let _ = std::process::Command::new("fusermount")
            .args(["-u", &mount_path.to_string_lossy()])
            .status();
        let _ = std::fs::remove_dir(&mount_path);
        Ok(())
    }
}
