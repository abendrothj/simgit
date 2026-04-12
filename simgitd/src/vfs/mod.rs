//! VFS manager — abstracts FUSE (Linux) and NFS-loopback (macOS) backends.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::info;
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::config::{Config, VfsBackend};

mod fuse_backend;
mod git_resolver;
mod nfs_backend;

/// Trait implemented by both the FUSE and NFS-loopback backends.
#[async_trait::async_trait]
pub trait VfsBackendTrait: Send + Sync {
    async fn mount(&self, session: &SessionInfo) -> Result<()>;
    async fn unmount(&self, session_id: Uuid) -> Result<()>;
}

pub struct VfsManager {
    backend:  Box<dyn VfsBackendTrait>,
    /// Track which sessions are currently mounted.
    mounted: Mutex<HashMap<Uuid, PathBuf>>,
}

impl VfsManager {
    pub fn new(cfg: Arc<Config>) -> Self {
        let backend: Box<dyn VfsBackendTrait> = match cfg.vfs_backend {
            VfsBackend::Fuse        => Box::new(fuse_backend::FuseBackend::new(cfg)),
            VfsBackend::NfsLoopback => Box::new(nfs_backend::NfsLoopbackBackend::new(cfg)),
        };
        Self { backend, mounted: Mutex::new(HashMap::new()) }
    }

    pub async fn mount(&self, session: &SessionInfo) -> Result<()> {
        self.backend.mount(session).await?;
        self.mounted.lock().unwrap().insert(session.session_id, session.mount_path.clone());
        info!(session = %session.session_id, path = %session.mount_path.display(), "VFS mounted");
        Ok(())
    }

    pub async fn unmount_session(&self, session_id: Uuid) {
        if self.mounted.lock().unwrap().remove(&session_id).is_some() {
            let _ = self.backend.unmount(session_id).await;
            info!(session = %session_id, "VFS unmounted");
        }
    }

    pub async fn unmount_all(&self) {
        let ids: Vec<Uuid> = self.mounted.lock().unwrap().keys().copied().collect();
        for id in ids {
            self.unmount_session(id).await;
        }
    }
}
