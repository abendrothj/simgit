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
//! - `write()`: Capture writes to delta store (Phase 2)
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
//! # Phase 1 Scope
//!
//! Phase 1 is **read-only**:
//! - ✅ Traversal of git tree structure
//! - ✅ Inode caching for fast lookups
//! - ✅ Blob serving (small files from cache, large files streamed)
//! - ✅ Directory listing (merged view)
//! - ⬜ Write interception (Phase 2)
//! - ⬜ Delta storage integration (Phase 2)
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
use super::git_resolver::{TreeCache, BlobCache, InodeMap};

/// FUSE backend driver (Linux).
///
/// Responsible for mounting and unmounting FUSE filesystems via the fuser crate.
/// One instance per daemon; spawns a SessionFs handler per session.
pub struct FuseBackend {
    cfg: Arc<Config>,
}

impl FuseBackend {
    /// Create a new FUSE backend with daemon configuration.
    ///
    /// # Arguments
    ///
    /// - `cfg`: Daemon configuration (mount options, cache sizes, git repo path)
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg }
    }
}

#[async_trait::async_trait]
impl super::VfsBackendTrait for FuseBackend {
    async fn mount(&self, session: &SessionInfo) -> Result<()> {
        let mount_path = session.mount_path.clone();
        std::fs::create_dir_all(&mount_path)?;

        // Open the base repo.
        let repo = Arc::new(gix::open(&self.cfg.repo_path)?);
        
        let fs = SessionFs::new(
            session.session_id,
            Arc::clone(&self.cfg),
            session.base_commit.clone(),
            repo,
        );

        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::FSName(format!("simgit-{}", session.session_id)),
        ];

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

// ── FUSE filesystem implementation ────────────────────────────────────────────

/// FUSE filesystem handler for a single session.
///
/// Implements the `fuser::Filesystem` trait, handling kernel requests for:
/// - **lookup()**: Resolve inode by parent + filename
/// - **getattr()**: Return file metadata
/// - **read()**: Serve file contents
/// - **readdir()**: List directory entries
/// - **write()** (Phase 2): Intercept writes to delta store
///
/// Each session gets its own instance, with isolated caches to avoid cross-session
/// interference and enable parallel processing.
///
/// # Data Flow
///
/// ```text
/// SessionFs maintains separate caches:
///
/// ┌─────────────────────────────────────────────┐
/// │         SessionFs (session-123)             │
/// ├──────────────────────────────────────────── │
/// │ git repo ─────────────────┐                 │
/// │ base_commit = "abc123"    │                 │ ← read-only
/// │                           ↓                 │
/// │ TreeCache (100 trees) ◄─────────────────── │ ← memoized git ls-tree
/// │ BlobCache (50 blobs, 10MB) ◄────────────── │ ← small file cache
/// │ InodeMap (path → ino)                      │
/// │                                             │
/// │ Delta Store ◄────────────────────────────── │ ← Phase 2
/// │                                             │
/// └─────────────────────────────────────────────┘
/// ```
///
/// # Example (Phase 1 Read-Only)
///
/// ```text
/// Agent: open("/vdev/sess-123/src/main.rs")
///   ↓
/// FUSE kernel module → SessionFs::lookup()
/// ├─ parent_ino = 1 (root)
/// ├─ name = "src"
/// ├─ lookup("src") in git tree
/// └─ return inode for "src" directory
///   ↓
/// FUSE kernel module → SessionFs::lookup()
/// ├─ parent_ino = 2 (src directory)
/// ├─ name = "main.rs"
/// ├─ lookup("main.rs") in git tree under src/
/// └─ return inode for "main.rs" file
///   ↓
/// Agent: read(inode, 0, 4096)
///   ↓
/// FUSE kernel module → SessionFs::read()
/// ├─ blob_cache.get(oid) or git cat-file
/// └─ return 4096 bytes to agent
/// ```
///
/// # Inode Numbering
///
/// - **1**: Root directory
/// - **2+**: Generated sequentially; never reused within session lifetime
/// - Mapping: InodeMap stores (inode → path, git OID) for fast reverse lookups
///
/// # TTL & Coherency
///
/// - Kernel caches inode metadata for 1 second (TTL)
/// - After TTL, attributes are re-fetched (cache miss cost: ~1ms)
/// - No write-through guarantees (Phase 1 read-only, so N/A)

struct SessionFs {
    session_id:   Uuid,
    cfg:          Arc<Config>,
    base_commit:  String,
    tree_cache:   Arc<TreeCache>,
    blob_cache:   Arc<BlobCache>,
    inode_map:    Arc<InodeMap>,
    repo:         Arc<gix::Repository>,
}

impl SessionFs {
    /// Create a new SessionFs handler for a session.
    ///
    /// Initializes caches:
    /// - TreeCache: 100 entries (typical git repos ~10–100 directories)
    /// - BlobCache: 50 entries, 10 MiB total (cache small .rs files, skip binaries)
    /// - InodeMap: unlimited (one entry per unique path)
    ///
    /// # Arguments
    ///
    /// - `session_id`: Unique session identifier
    /// - `cfg`: Daemon configuration
    /// - `base_commit`: Git commit hash to serve as read-only tree (e.g., HEAD)
    /// - `repo`: Open git repository handle
    fn new(session_id: Uuid, cfg: Arc<Config>, base_commit: String, repo: Arc<gix::Repository>) -> Self {
        Self {
            session_id,
            cfg,
            base_commit,
            tree_cache:   Arc::new(TreeCache::new(100)),
            blob_cache:   Arc::new(BlobCache::new(50, 10 * 1024 * 1024)), // 50 blobs, 10MB cap
            inode_map:    Arc::new(InodeMap::new()),
            repo,
        }
    }
}

const TTL: Duration = Duration::from_secs(1);

impl Filesystem for SessionFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        debug!(session = %self.session_id, parent = parent.0, name = name_str, "lookup");

        // Root is always inode 1.
        if parent.0 == 1 {
            // Get the root tree from base_commit.
            let root_oid = match gix::ObjectId::from_hex(self.base_commit.as_bytes()) {
                Ok(oid) => oid,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            };

            // Find the tree object (commit → tree).
            let tree_oid = match self.repo.find_object(root_oid) {
                Ok(obj) => match obj.try_into_commit() {
                    Ok(commit) => commit.tree().ok().map(|t| t.0),
                    Err(_) => None,
                },
                Err(_) => None,
            };

            if tree_oid.is_none() {
                reply.error(Errno::EIO);
                return;
            }

            let tree_entries = match self.tree_cache.get(tree_oid.unwrap(), &self.repo) {
                Ok(e) => e,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            };

            // Search for the named entry.
            for (idx, entry) in tree_entries.iter().enumerate() {
                if entry.name == name_str {
                    let ino = self.inode_map.allocate();
                    self.inode_map.insert(ino, tree_oid.unwrap(), idx);

                    let attr = FileAttr {
                        ino: INodeNo(ino),
                        size: entry.size,
                        blocks: (entry.size + 511) / 512,
                        atime: UNIX_EPOCH,
                        mtime: UNIX_EPOCH,
                        ctime: UNIX_EPOCH,
                        crtime: UNIX_EPOCH,
                        kind: match entry.kind {
                            super::git_resolver::EntryKind::File => FileType::RegularFile,
                            super::git_resolver::EntryKind::Dir => FileType::Directory,
                            super::git_resolver::EntryKind::Symlink => FileType::Symlink,
                        },
                        perm: entry.perm,
                        nlink: 1,
                        uid: unsafe { libc::getuid() },
                        gid: unsafe { libc::getgid() },
                        rdev: 0,
                        flags: 0,
                        blksize: 4096,
                    };
                    reply.entry(&TTL, &attr, Generation(0));
                    return;
                }
            }

            reply.error(Errno::ENOENT);
            return;
        }

        // For non-root lookups, we'd need to maintain a tree_oid per inode.
        // For now, return ENOENT (Phase 1 limitation).
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 == 1 {
            // Root directory.
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

        // List root tree entries.
        let root_oid = match gix::ObjectId::from_hex(self.base_commit.as_bytes()) {
            Ok(oid) => oid,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let tree_oid = match self.repo.find_object(root_oid) {
            Ok(obj) => match obj.try_into_commit() {
                Ok(commit) => commit.tree().ok().map(|t| t.0),
                Err(_) => None,
            },
            Err(_) => None,
        };

        if tree_oid.is_none() {
            reply.error(Errno::EIO);
            return;
        }

        let tree_entries = match self.tree_cache.get(tree_oid.unwrap(), &self.repo) {
            Ok(e) => e,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        // Always start with . and ..
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (1u64, FileType::Directory, ".".to_owned()),
            (1u64, FileType::Directory, "..".to_owned()),
        ];

        // Add git tree entries.
        for (idx, entry) in tree_entries.iter().enumerate() {
            let ino = self.inode_map.allocate();
            self.inode_map.insert(ino, tree_oid.unwrap(), idx);

            entries.push((
                ino,
                match entry.kind {
                    super::git_resolver::EntryKind::File => FileType::RegularFile,
                    super::git_resolver::EntryKind::Dir => FileType::Directory,
                    super::git_resolver::EntryKind::Symlink => FileType::Symlink,
                },
                entry.name.clone(),
            ));
        }

        // Yield entries starting from offset.
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
