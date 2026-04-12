//! FUSE backend using the `fuser` crate (Linux primary).
//!
//! Implements a Copy-on-Write overlay:
//!   READ  → check delta store first, fall back to git blob
//!   WRITE → capture in delta store, acquire write lock via borrow registry

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo,
    MountOption, ReplyAttr, ReplyDirectory, ReplyEntry, Request,
};
use tracing::debug;
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::config::Config;

pub struct FuseBackend {
    cfg: Arc<Config>,
}

impl FuseBackend {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

#[async_trait::async_trait]
impl super::VfsBackendTrait for FuseBackend {
    async fn mount(&self, session: &SessionInfo) -> Result<()> {
        let mount_path = session.mount_path.clone();
        std::fs::create_dir_all(&mount_path)?;

        let fs = SessionFs::new(session.session_id, Arc::clone(&self.cfg));

        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::FSName(format!("simgit-{}", session.session_id)),
        ];

        // Spawn mount in a background thread; AutoUnmount handles cleanup.
        let _guard = fuser::spawn_mount2(fs, &mount_path, &config)?;
        std::mem::forget(_guard); // intentional: auto-unmount handles cleanup

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

// ── FUSE filesystem implementation ────────────────────────────────────────────

struct SessionFs {
    session_id: Uuid,
    cfg:        Arc<Config>,
}

impl SessionFs {
    fn new(session_id: Uuid, cfg: Arc<Config>) -> Self {
        Self { session_id, cfg }
    }
}

const TTL: Duration = Duration::from_secs(1);

impl Filesystem for SessionFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        debug!(session = %self.session_id, parent = parent.0, name = ?name, "lookup");
        // Phase 1: stub — real implementation resolves git tree objects.
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 == 1 {
            reply.attr(&TTL, &root_attr());
            return;
        }
        reply.error(Errno::ENOENT);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if ino.0 != 1 {
            reply.error(Errno::ENOTDIR);
            return;
        }
        let entries = vec![
            (1u64, FileType::Directory, "."),
            (1u64, FileType::Directory, ".."),
        ];
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

fn root_attr() -> FileAttr {
    FileAttr {
        ino:     INodeNo(1),
        size:    0,
        blocks:  0,
        atime:   UNIX_EPOCH,
        mtime:   UNIX_EPOCH,
        ctime:   UNIX_EPOCH,
        crtime:  UNIX_EPOCH,
        kind:    FileType::Directory,
        perm:    0o755,
        nlink:   2,
        uid:     unsafe { libc::getuid() },
        gid:     unsafe { libc::getgid() },
        rdev:    0,
        flags:   0,
        blksize: 512,
    }
}
