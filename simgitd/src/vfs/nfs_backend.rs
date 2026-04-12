//! NFS-loopback backend for macOS.
//!
//! Instead of requiring the macFUSE kernel extension (which demands user
//! approval in System Settings on Apple Silicon), simgitd embeds a minimal
//! NFSv3 server and uses the OS's built-in `mount_nfs(8)` to attach each
//! session as a regular NFS mount.
//!
//! Architecture:
//!  1. simgitd binds an NFSv3 listener on a random TCP port on 127.0.0.1.
//!  2. Each session is a separate NFS export path "/session/<id>".
//!  3. `mount_nfs` is called to attach the export to /vdev/<session-id>.
//!  4. All VFS logic (delta CoW, borrow checking) runs inside the NFS
//!     request handlers, mirroring the FUSE backend.
//!
//! Status: Phase 0 stub — skeleton only. Full NFSv3 XDR/RPC implementation
//! is scoped to Phase 1 (macOS track).

use std::sync::Arc;
use anyhow::Result;
use uuid::Uuid;
use tracing::warn;

use simgit_sdk::SessionInfo;
use crate::config::Config;

pub struct NfsLoopbackBackend {
    cfg: Arc<Config>,
}

impl NfsLoopbackBackend {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

#[async_trait::async_trait]
impl super::VfsBackendTrait for NfsLoopbackBackend {
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

    async fn unmount(&self, session_id: Uuid) -> Result<()> {
        let mount_path = self.cfg.mnt_dir.join(session_id.to_string());
        // On macOS the real implementation will call:
        //   std::process::Command::new("umount").arg(&mount_path).status()
        let _ = std::fs::remove_dir(&mount_path);
        Ok(())
    }
}
