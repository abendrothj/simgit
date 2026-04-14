//! NFS-loopback backend for macOS (Phase 0–1 roadmap).
//!
//! # Motivation
//!
//! macOS does not include FUSE support by default. Instead of requiring users
//! to install macFUSE (Apple Silicon: 5–10 minute security dialogs + kernel extension approval),
//! simgitd can embed a lightweight NFSv3 server.
//!
//! # Phase 0: Plain Directory (Current)
//!
//! A minimal stub that creates plain directories at `/vdev/<session-id>/`.
//! Agents write directly to disk; the daemon later applies delta logic
//! (Phase 1+) via file system events (inotify/kqueue + FSEvents).
//!
//! Advantages:
//! - Zero complexity (no kernel extensions, no user interaction)
//! - Works immediately on macOS
//! - Git history still available via gitoxide (read-only overlay)
//!
//! Disadvantages:
//! - Writes go directly to disk (not captured as deltas yet)
//! - No CoW semantics (Phase 1+ addition)
//! - No borrow checking on writes (Phase 1 blocked on this)
//! - Slower on large repos (no inode caching)
//! - No idemptotent mount/unmount on crash (might leave stale mounts)
//!
//! # Phase 1+: Full NFSv3 Server
//!
//! A complete NFSv3 (RFC 1813) implementation:
//!
//! ```text
//! simgitd launches NFSv3 RPC listener on 127.0.0.1:random_port
//!       ↓
//! Exports "/session/<session-id>" → session's file tree
//!       ↓
//! Agent mounts via: mount -t nfs -o vers=3 127.0.0.1:/session/<id> /vdev/<id>
//!       ↓
//! Agent read/write → NFSv3 RPC → simgitd handler
//!       ↓
//! Handler applies same logic as FUSE backend (git tree + delta)
//! ```
//!
//! Benefits:
//! - No kernel extension needed (100% user-space)
//! - Same VFS semantics as FUSE (unified code path)
//! - Better performance on large directories (inode caching)
//! - Built-in NFS caching by macOS kernel
//!
//! Cost:
//! - NFSv3 XDR/RPC implementation (~2k lines)
//! - Testing on macOS only (Linux uses FUSE)
//!
//! # Architecture (Phase 1)
//!
//! ```text
//! ┌─ simgitd daemon ───────────────────────────┐\n//! │                                             │\n//! │  NFSv3 RPC Listener                         │\n//! │  ├─ sunrpc (port 111)                        │\n//! │  └─ nfs (port 2049, random)                │\n//! │       ↓                                       │\n//! │  SessionFs handlers (same as FUSE)          │\n//! │  ├─ getattr(ino)                           │\n//! │  ├─ lookup(parent, name)                    │\n//! │  ├─ read(ino, offset, len)                 │\n//! │  ├─ readdir(ino, cookie, count)            │\n//! │  └─ write(ino, offset, data) [Phase 2]     │\n//! │                                             │\n//! │  Caches (per session)                       │\n//! │  ├─ TreeCache (git tree objects)           │\n//! │  ├─ BlobCache (small files)                │\n//! │  └─ InodeMap (path → oid)                  │\n//! │                                             │\n//! └──────────────────────────────────────────────┘\n//!        ↑ (NFS RPC)              ↑ (NFS mount)\n//!        │ 127.0.0.1:2049         │ /vdev/<id>\n//!        macOS kernel             Agent\n//! ```
//!
//! # Implementation Schedule
//!
//! - **Phase 0** (current): Plain directory stub
//! - **Phase 1 (macOS)**: Full NFSv3 server + XDR codec
//! - **Phase 2+**: Delta interception in NFS write handler
//! - **Phase 3+**: Borrow checking integrated with NFS write
//!
//! # Integration with FUSE
//!
//! Both FUSE (Linux) and NFSv3 (macOS Phase 1+) backends share:
//! - Same `VfsBackendTrait` interface
//! - Same `SessionFs` handler logic (git traversal, delta CoW, borrow checking)
//! - Same cache structures (TreeCache, BlobCache, InodeMap)
//! - Same inode numbering scheme (1 = root, 2+ = path entries)

use std::sync::Arc;
use std::path::{Path, PathBuf};
use anyhow::Result;
use uuid::Uuid;
use tracing::warn;

use simgit_sdk::SessionInfo;
use crate::config::Config;
use crate::delta::store::ByteRange;
use crate::delta::DeltaStore;

/// NFS-loopback backend driver (macOS, Phase 0 stub).
///
/// Phase 0 creates plain directories. Phase 1 will implement full NFSv3 server.
///
/// # Phase 0 Behavior
///
/// Sessions are mounted as plain directories. Agents write directly to disk
/// (no interception). Delta logic is not yet applied.
///
/// # Phase 1 Behavior
///
/// Will spawn an NFSv3 RPC server and use the OS's `mount_nfs` systemcall
/// to attach sessions as read-only NFS mounts (with delta overlay, matching FUSE semantics).
pub struct NfsLoopbackBackend {
    cfg: Arc<Config>,
    deltas: Arc<DeltaStore>,
}

impl NfsLoopbackBackend {
    /// Create a new NFS-loopback backend.
    ///
    /// # Arguments
    ///
    /// - `cfg`: Daemon configuration (mount options, cache sizes, git repo path)
    ///
    /// In Phase 0, this is a no-op. Phase 1 will initialize NFSv3 RPC server resources.
    pub fn new(cfg: Arc<Config>, deltas: Arc<DeltaStore>) -> Self {
        Self { cfg, deltas }
    }

    fn list_mount_files(root: &Path) -> Result<Vec<PathBuf>> {
        fn walk(acc: &mut Vec<PathBuf>, root: &Path, dir: &Path) -> Result<()> {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let ty = entry.file_type()?;
                if ty.is_dir() {
                    walk(acc, root, &path)?;
                } else if ty.is_file() {
                    let rel = path
                        .strip_prefix(root)
                        .map(|p| p.to_path_buf())
                        .unwrap_or(path.clone());
                    acc.push(rel);
                }
            }
            Ok(())
        }

        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        walk(&mut out, root, root)?;
        Ok(out)
    }

    fn rel_to_git_path(rel: &Path) -> String {
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn git_show_blob(repo: &Path, base_commit: &str, rel: &Path) -> Result<Option<Vec<u8>>> {
        let spec = format!("{}:{}", base_commit, Self::rel_to_git_path(rel));
        let out = std::process::Command::new("git")
            .current_dir(repo)
            .args(["show", &spec])
            .output()?;
        if out.status.success() {
            Ok(Some(out.stdout))
        } else {
            Ok(None)
        }
    }

    fn infer_changed_ranges(current: &[u8], baseline: Option<&[u8]>) -> Vec<ByteRange> {
        let base = baseline.unwrap_or_default();
        let max_len = current.len().max(base.len());
        let mut ranges = Vec::new();
        let mut start: Option<usize> = None;

        for i in 0..max_len {
            let cur = current.get(i).copied().unwrap_or(0);
            let old = base.get(i).copied().unwrap_or(0);
            if cur != old {
                if start.is_none() {
                    start = Some(i);
                }
            } else if let Some(s) = start.take() {
                ranges.push(ByteRange {
                    offset: s as u64,
                    len: (i - s) as u64,
                });
            }
        }

        if let Some(s) = start {
            ranges.push(ByteRange {
                offset: s as u64,
                len: (max_len - s) as u64,
            });
        }

        ranges
    }

    fn record_blob_with_ranges(
        &self,
        session_id: Uuid,
        rel: &Path,
        current: &[u8],
        ranges: &[ByteRange],
    ) -> Result<()> {
        if ranges.is_empty() {
            self.deltas.write_blob(session_id, rel, current, None)?;
            return Ok(());
        }

        for range in ranges {
            self.deltas.write_blob(session_id, rel, current, Some(*range))?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl super::VfsBackendTrait for NfsLoopbackBackend {
    /// Mount a session's filesystem (**Phase 0: plain directory stub**).
    ///
    /// # Phase 0 Behavior
    ///
    /// Creates a plain directory at [`SessionInfo::mount_path`].
    /// No NFS server, no kernel mount — just a target directory for agents to write to.
    ///
    /// # Phase 1 Planned Behavior
    ///
    /// 1. Spawn NFSv3 RPC server (if not already running)
    /// 2. Export `/session/<session-id>` via NFS  
    /// 3. Call `mount -t nfs 127.0.0.1:/session/<id> /vdev/<id>`
    /// 4. Verify mount succeeded and is writable
    ///
    /// # Errors
    ///
    /// - Could not create mount_path directory
    /// - Permission denied
    async fn mount(&self, session: &SessionInfo) -> Result<()> {
        warn!(
            session = %session.session_id,
            "NFS-loopback backend is a Phase 0 stub. \
             macOS sessions will be mounted as plain directories until \
             the NFSv3 server is fully implemented in Phase 1."
        );
        // Stub: create the directory so agents can at least write to it
        // and the delta store still captures changes via inotify/kqueue later.
        std::fs::create_dir_all(&session.mount_path)?;
        Ok(())
    }

    /// Unmount a session's filesystem (**Phase 0: plain directory cleanup**).
    ///
    /// # Phase 0 Behavior
    ///
    /// Removes the plain directory. Non-recursive (fails if not empty).
    ///
    /// # Phase 1 Planned Behavior
    ///
    /// Calls `umount /vdev/<session-id>` to unmount the NFS export cleanly.
    ///
    /// # Errors
    ///
    /// - Mount path does not exist (already cleaned up — idempotent)
    /// - Mount path not empty (Phase 0 only; Phase 1 will use umount force)
    ///
    /// # Best Effort
    ///
    /// Errors are logged but not returned (errors here should not crash the daemon).
    async fn unmount(&self, session_id: Uuid) -> Result<()> {
        let mount_path = self.cfg.mnt_dir.join(session_id.to_string());
        // On macOS the real implementation will call:
        //   std::process::Command::new("umount").arg(&mount_path).status()
        let _ = std::fs::remove_dir(&mount_path);
        Ok(())
    }

    fn capture_mount_delta(&self, session: &SessionInfo) -> Result<()> {
        // Phase 0 macOS backend writes directly to a plain directory. Capture only
        // files that appear in the session mount and differ from base content.
        //
        // Important: do not infer global deletes from "missing in mount" because
        // the Phase 0 mount is not a full repo projection. Treating every absent
        // path as deleted causes false conflicts across the whole repository.
        if !session.mount_path.exists() {
            return Ok(());
        }

        let files = Self::list_mount_files(&session.mount_path)?;

        for rel in &files {
            let mount_file = session.mount_path.join(rel);
            let current = std::fs::read(&mount_file)?;
            let baseline = Self::git_show_blob(&self.cfg.repo_path, &session.base_commit, rel)?;

            if baseline.as_deref() != Some(current.as_slice()) {
                let ranges = Self::infer_changed_ranges(&current, baseline.as_deref());
                self.record_blob_with_ranges(session.session_id, rel, &current, &ranges)?;
            }
        }

        Ok(())
    }
}
