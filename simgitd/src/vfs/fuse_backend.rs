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

use std::ffi::OsStr;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyCreate, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, WriteFlags,
};
use tracing::debug;
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::config::Config;
use crate::borrow::BorrowRegistry;
use crate::delta::DeltaStore;
use crate::delta::store::ByteRange;
use super::git_resolver::{TreeCache, BlobCache, InodeMap};

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
        Self { cfg, deltas, borrows }
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
        config.mount_options = vec![
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
    peers_enabled: bool,
    cfg:          Arc<Config>,
    base_commit:  String,
    deltas:       Arc<DeltaStore>,
    borrows:      Arc<BorrowRegistry>,
    tree_cache:   Arc<TreeCache>,
    blob_cache:   Arc<BlobCache>,
    inode_map:    Arc<InodeMap>,
    virtual_inos: Arc<Mutex<HashMap<u64, PathBuf>>>,
    virtual_paths: Arc<Mutex<HashMap<PathBuf, u64>>>,
    next_virtual_ino: Arc<AtomicU64>,
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
    fn new(
        session_id: Uuid,
        peers_enabled: bool,
        cfg: Arc<Config>,
        base_commit: String,
        deltas: Arc<DeltaStore>,
        borrows: Arc<BorrowRegistry>,
    ) -> Self {
        Self {
            session_id,
            peers_enabled,
            cfg,
            base_commit,
            deltas,
            borrows,
            tree_cache:   Arc::new(TreeCache::new(100)),
            blob_cache:   Arc::new(BlobCache::new(50, 10 * 1024 * 1024)), // 50 blobs, 10MB cap
            inode_map:    Arc::new(InodeMap::new()),
            virtual_inos: Arc::new(Mutex::new(HashMap::new())),
            virtual_paths: Arc::new(Mutex::new(HashMap::new())),
            next_virtual_ino: Arc::new(AtomicU64::new(1u64 << 62)),
        }
    }

    fn ensure_virtual_ino(&self, path: &std::path::Path) -> u64 {
        if let Some(ino) = self.virtual_paths.lock().unwrap().get(path).copied() {
            return ino;
        }
        let ino = self.next_virtual_ino.fetch_add(1, Ordering::Relaxed);
        self.virtual_inos.lock().unwrap().insert(ino, path.to_path_buf());
        self.virtual_paths.lock().unwrap().insert(path.to_path_buf(), ino);
        ino
    }

    fn virtual_path_of(&self, ino: u64) -> Option<PathBuf> {
        self.virtual_inos.lock().unwrap().get(&ino).cloned()
    }

    fn active_peer_ids(&self) -> Vec<Uuid> {
        if !self.peers_enabled {
            return Vec::new();
        }
        let mut peers: Vec<Uuid> = self
            .borrows
            .active_sessions()
            .into_iter()
            .filter(|s| s.session_id != self.session_id)
            .map(|s| s.session_id)
            .collect();
        peers.sort();
        peers
    }

    fn peer_children(&self, peer_session: Uuid, dir: &std::path::Path) -> Vec<(String, bool)> {
        let manifest = match self.deltas.load_manifest(peer_session) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        let mut entries: HashMap<String, bool> = HashMap::new();
        for path in manifest.writes.keys() {
            if !dir.as_os_str().is_empty() && !path.starts_with(dir) {
                continue;
            }
            let rel = if dir.as_os_str().is_empty() {
                path.as_path()
            } else {
                match path.strip_prefix(dir) {
                    Ok(r) => r,
                    Err(_) => continue,
                }
            };
            let mut comps = rel.components();
            let Some(first) = comps.next() else {
                continue;
            };
            let name = first.as_os_str().to_string_lossy().to_string();
            let is_dir = comps.next().is_some();
            entries
                .entry(name)
                .and_modify(|v| *v = *v || is_dir)
                .or_insert(is_dir);
        }
        let mut out: Vec<(String, bool)> = entries.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn peer_file_bytes(&self, peer_session: Uuid, rel: &std::path::Path) -> Option<Vec<u8>> {
        match self.deltas.read_blob(peer_session, rel) {
            Ok(Some(b)) => Some(b),
            _ => None,
        }
    }
}

const TTL: Duration = Duration::from_secs(1);
const SIMGIT_META_DIR: &str = ".simgit";
const SIMGIT_PEERS_DIR: &str = ".simgit/peers";

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

        if parent.0 == 1 && name_str == SIMGIT_META_DIR {
            let ino = self.ensure_virtual_ino(std::path::Path::new(SIMGIT_META_DIR));
            reply.entry(&TTL, &virtual_dir_attr(INodeNo(ino)), Generation(0));
            return;
        }

        if let Some(parent_path) = self.virtual_path_of(parent.0) {
            if parent_path == std::path::Path::new(SIMGIT_META_DIR) {
                if name_str == "peers" {
                    let ino = self.ensure_virtual_ino(std::path::Path::new(SIMGIT_PEERS_DIR));
                    reply.entry(&TTL, &virtual_dir_attr(INodeNo(ino)), Generation(0));
                    return;
                }
                reply.error(Errno::ENOENT);
                return;
            }
            if parent_path == std::path::Path::new(SIMGIT_PEERS_DIR) {
                if !self.peers_enabled {
                    reply.error(Errno::ENOENT);
                    return;
                }
                let Ok(peer_id) = Uuid::parse_str(name_str) else {
                    reply.error(Errno::ENOENT);
                    return;
                };
                if !self.active_peer_ids().contains(&peer_id) {
                    reply.error(Errno::ENOENT);
                    return;
                }
                let child = std::path::Path::new(SIMGIT_PEERS_DIR).join(name_str);
                let ino = self.ensure_virtual_ino(&child);
                reply.entry(&TTL, &virtual_dir_attr(INodeNo(ino)), Generation(0));
                return;
            }
            if let Some((peer_id, rel_dir)) = parse_virtual_peer_path(&parent_path) {
                let children = self.peer_children(peer_id, &rel_dir);
                if let Some((_, is_dir)) = children.iter().find(|(n, _)| n == name_str) {
                    let child_rel = rel_dir.join(name_str);
                    let full = std::path::Path::new(SIMGIT_PEERS_DIR)
                        .join(peer_id.to_string())
                        .join(&child_rel);
                    let ino = self.ensure_virtual_ino(&full);
                    if *is_dir {
                        reply.entry(&TTL, &virtual_dir_attr(INodeNo(ino)), Generation(0));
                    } else {
                        let size = self
                            .peer_file_bytes(peer_id, &child_rel)
                            .map(|b| b.len() as u64)
                            .unwrap_or(0);
                        reply.entry(&TTL, &virtual_file_attr(INodeNo(ino), size), Generation(0));
                    }
                    return;
                }
                reply.error(Errno::ENOENT);
                return;
            }

            reply.error(Errno::ENOENT);
            return;
        }

        let parent_tree_oid = match directory_tree_oid_for_ino(
            parent.0,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let tree_entries = match self.tree_cache.get(&parent_tree_oid, &self.cfg.repo_path) {
            Ok(e) => e,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        // Search for the named entry.
        let parent_path = if parent.0 == 1 {
            PathBuf::new()
        } else {
            self.inode_map.path_of(parent.0).unwrap_or_default()
        };
        let manifest = match self.deltas.load_manifest(self.session_id) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        for (idx, entry) in tree_entries.iter().enumerate() {
            if entry.name == name_str {
                let full_path = parent_path.join(&entry.name);
                if manifest.deletes.contains(&full_path) {
                    reply.error(Errno::ENOENT);
                    return;
                }
                let ino = self.inode_map.allocate();
                self.inode_map.insert(ino, parent_tree_oid.clone(), idx, full_path);

                reply.entry(&TTL, &entry_attr(INodeNo(ino), entry), Generation(0));
                return;
            }
        }

        let full_path = parent_path.join(name_str);
        if let Some(hash) = manifest.writes.get(&full_path) {
            let size = self
                .deltas
                .read_blob(self.session_id, &full_path)
                .ok()
                .flatten()
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            let ino = self.inode_map.allocate();
            self.inode_map.insert_delta_file(ino, full_path, size, 0o100644);
            let entry = super::git_resolver::TreeEntry {
                name: name_str.to_owned(),
                mode: "100644".to_owned(),
                oid: hash.clone(),
                kind: super::git_resolver::EntryKind::File,
                size,
                perm: 0o100644,
            };
            reply.entry(&TTL, &entry_attr(INodeNo(ino), &entry), Generation(0));
            return;
        }

        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        if ino.0 == 1 {
            // Root directory.
            reply.attr(&TTL, &root_attr());
            return;
        }

        if let Some(path) = self.virtual_path_of(ino.0) {
            if path == std::path::Path::new(SIMGIT_META_DIR)
                || path == std::path::Path::new(SIMGIT_PEERS_DIR)
            {
                reply.attr(&TTL, &virtual_dir_attr(ino));
                return;
            }
            if let Some((peer_id, rel)) = parse_virtual_peer_path(&path) {
                if rel.as_os_str().is_empty() {
                    reply.attr(&TTL, &virtual_dir_attr(ino));
                    return;
                }
                if let Some(bytes) = self.peer_file_bytes(peer_id, &rel) {
                    reply.attr(&TTL, &virtual_file_attr(ino, bytes.len() as u64));
                    return;
                }
                if !self.peer_children(peer_id, &rel).is_empty() {
                    reply.attr(&TTL, &virtual_dir_attr(ino));
                    return;
                }
            }
            reply.error(Errno::ENOENT);
            return;
        }

        if let Some(meta) = self.inode_map.delta_file_of(ino.0) {
            if delta_path_deleted(&self.deltas, self.session_id, &meta.path).unwrap_or(true) {
                reply.error(Errno::ENOENT);
                return;
            }
            let entry = super::git_resolver::TreeEntry {
                name: meta
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                mode: "100644".to_owned(),
                oid: String::new(),
                kind: super::git_resolver::EntryKind::File,
                size: meta.size,
                perm: meta.perm,
            };
            reply.attr(&TTL, &entry_attr(ino, &entry));
            return;
        }

        let Some((tree_oid, entry_idx)) = self.inode_map.lookup(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let tree_entries = match self.tree_cache.get(&tree_oid, &self.cfg.repo_path) {
            Ok(e) => e,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let Some(entry) = tree_entries.get(entry_idx) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let Some(path) = self.inode_map.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if delta_path_deleted(&self.deltas, self.session_id, &path).unwrap_or(true) {
            reply.error(Errno::ENOENT);
            return;
        }

        if entry.kind == super::git_resolver::EntryKind::File {
            match self.deltas.read_blob(self.session_id, &path) {
                Ok(Some(delta_bytes)) => {
                    let mut delta_entry = entry.clone();
                    delta_entry.size = delta_bytes.len() as u64;
                    reply.attr(&TTL, &entry_attr(ino, &delta_entry));
                    return;
                }
                Ok(None) => {}
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            }
        }

        reply.attr(&TTL, &entry_attr(ino, entry));
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
            if let Some(path) = self.virtual_path_of(ino.0) {
                let mut entries: Vec<(u64, FileType, String)> = vec![
                    (ino.0, FileType::Directory, ".".to_owned()),
                    (1, FileType::Directory, "..".to_owned()),
                ];

                if path == std::path::Path::new(SIMGIT_META_DIR) {
                    let peers_ino = self.ensure_virtual_ino(std::path::Path::new(SIMGIT_PEERS_DIR));
                    entries.push((peers_ino, FileType::Directory, "peers".to_owned()));
                } else if path == std::path::Path::new(SIMGIT_PEERS_DIR) {
                    if self.peers_enabled {
                        for peer in self.active_peer_ids() {
                            let p = std::path::Path::new(SIMGIT_PEERS_DIR).join(peer.to_string());
                            let child_ino = self.ensure_virtual_ino(&p);
                            entries.push((child_ino, FileType::Directory, peer.to_string()));
                        }
                    }
                } else if let Some((peer_id, rel)) = parse_virtual_peer_path(&path) {
                    for (name, is_dir) in self.peer_children(peer_id, &rel) {
                        let child_rel = rel.join(&name);
                        let full = std::path::Path::new(SIMGIT_PEERS_DIR)
                            .join(peer_id.to_string())
                            .join(child_rel);
                        let child_ino = self.ensure_virtual_ino(&full);
                        entries.push((
                            child_ino,
                            if is_dir { FileType::Directory } else { FileType::RegularFile },
                            name,
                        ));
                    }
                }

                for (i, (entry_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
                    if reply.add(INodeNo(*entry_ino), (i + 1) as u64, *kind, name) {
                        break;
                    }
                }
                reply.ok();
                return;
            }
        }

        let tree_oid = match directory_tree_oid_for_ino(
            ino.0,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let tree_entries = match self.tree_cache.get(&tree_oid, &self.cfg.repo_path) {
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
        if ino.0 == 1 {
            let sm_ino = self.ensure_virtual_ino(std::path::Path::new(SIMGIT_META_DIR));
            entries.push((sm_ino, FileType::Directory, SIMGIT_META_DIR.to_owned()));
        }

        // Add git tree entries.
        let parent_path = if ino.0 == 1 {
            PathBuf::new()
        } else {
            self.inode_map.path_of(ino.0).unwrap_or_default()
        };
        let manifest = match self.deltas.load_manifest(self.session_id) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        for (idx, entry) in tree_entries.iter().enumerate() {
            let full_path = parent_path.join(&entry.name);
            if manifest.deletes.contains(&full_path) {
                continue;
            }
            let ino = self.inode_map.allocate();
            self.inode_map.insert(ino, tree_oid.clone(), idx, full_path);

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

        let mut seen_names: HashSet<String> = tree_entries.iter().map(|e| e.name.clone()).collect();
        for path in manifest.writes.keys() {
            if !path_starts_with_dir(path, &parent_path) {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if seen_names.contains(file_name) || manifest.deletes.contains(path) {
                continue;
            }
            let size = self
                .deltas
                .read_blob(self.session_id, path)
                .ok()
                .flatten()
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            let ino = self.inode_map.allocate();
            self.inode_map.insert_delta_file(ino, path.clone(), size, 0o100644);
            entries.push((ino, FileType::RegularFile, file_name.to_owned()));
            seen_names.insert(file_name.to_owned());
        }

        // Yield entries starting from offset.
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        if ino.0 == 1 {
            reply.error(Errno::EISDIR);
            return;
        }

        if let Some(path) = self.virtual_path_of(ino.0) {
            if let Some((peer_id, rel)) = parse_virtual_peer_path(&path) {
                if let Some(bytes) = self.peer_file_bytes(peer_id, &rel) {
                    let slice = file_slice(&bytes, offset as usize, size as usize);
                    reply.data(slice);
                    return;
                }
                reply.error(Errno::ENOENT);
                return;
            }
            reply.error(Errno::EISDIR);
            return;
        }

        if let Some(meta) = self.inode_map.delta_file_of(ino.0) {
            if delta_path_deleted(&self.deltas, self.session_id, &meta.path).unwrap_or(true) {
                reply.error(Errno::ENOENT);
                return;
            }
            let data = match self.deltas.read_blob(self.session_id, &meta.path) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    reply.error(Errno::ENOENT);
                    return;
                }
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            };
            let slice = file_slice(&data, offset as usize, size as usize);
            reply.data(slice);
            return;
        }

        let Some(entry) = lookup_entry_for_ino(ino.0, &self.cfg.repo_path, &self.tree_cache, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let Some(path) = self.inode_map.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if delta_path_deleted(&self.deltas, self.session_id, &path).unwrap_or(true) {
            reply.error(Errno::ENOENT);
            return;
        }

        if entry.kind == super::git_resolver::EntryKind::Dir {
            reply.error(Errno::EISDIR);
            return;
        }

        let offset = offset as usize;
        let data = match self.deltas.read_blob(self.session_id, &path) {
            Ok(Some(delta_bytes)) => delta_bytes,
            Ok(None) => match self.blob_cache.get(&entry.oid, &self.cfg.repo_path) {
                Ok(d) => d,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            },
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let slice = file_slice(&data, offset, size as usize);
        reply.data(slice);
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        if ino.0 == 1 {
            reply.error(Errno::EINVAL);
            return;
        }

        let Some(entry) = lookup_entry_for_ino(ino.0, &self.cfg.repo_path, &self.tree_cache, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };

        if entry.kind != super::git_resolver::EntryKind::Symlink {
            reply.error(Errno::EINVAL);
            return;
        }

        let target = match self.blob_cache.get(&entry.oid, &self.cfg.repo_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        reply.data(&target);
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if ino.0 == 1 {
            reply.error(Errno::EISDIR);
            return;
        }

        if let Some(path) = self.virtual_path_of(ino.0) {
            if let Some((peer_id, rel)) = parse_virtual_peer_path(&path) {
                if self.peer_file_bytes(peer_id, &rel).is_some() {
                    reply.opened(FileHandle(0), FopenFlags::empty());
                    return;
                }
            }
            reply.error(Errno::EISDIR);
            return;
        }

        if let Some(meta) = self.inode_map.delta_file_of(ino.0) {
            if delta_path_deleted(&self.deltas, self.session_id, &meta.path).unwrap_or(true) {
                reply.error(Errno::ENOENT);
                return;
            }
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }

        let Some(entry) = lookup_entry_for_ino(ino.0, &self.cfg.repo_path, &self.tree_cache, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if entry.kind == super::git_resolver::EntryKind::Dir {
            reply.error(Errno::EISDIR);
            return;
        }
        let Some(path) = self.inode_map.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if delta_path_deleted(&self.deltas, self.session_id, &path).unwrap_or(true) {
            reply.error(Errno::ENOENT);
            return;
        }
        // Read-only handle, direct I/O disabled.
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.virtual_path_of(ino.0).is_some() {
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }

        match directory_tree_oid_for_ino(
            ino.0,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(_) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(DirTreeError::NotDir) => reply.error(Errno::ENOTDIR),
            Err(DirTreeError::NotFound) => reply.error(Errno::ENOENT),
            Err(DirTreeError::Io) => reply.error(Errno::EIO),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };

        match directory_tree_oid_for_ino(
            parent.0,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(_) => {}
            Err(DirTreeError::NotDir) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(DirTreeError::NotFound) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(DirTreeError::Io) => {
                reply.error(Errno::EIO);
                return;
            }
        }

        let Some(path) = full_child_path(parent.0, name_str, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let manifest = match self.deltas.load_manifest(self.session_id) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if !manifest.deletes.contains(&path) && manifest.writes.contains_key(&path) {
            reply.error(Errno::EEXIST);
            return;
        }

        if !manifest.deletes.contains(&path)
            && git_entry_exists_in_parent(
                &self.cfg.repo_path,
                &self.tree_cache,
                &self.base_commit,
                &self.inode_map,
                parent.0,
                name_str,
            )
            .unwrap_or(false)
        {
            reply.error(Errno::EEXIST);
            return;
        }

        if self
            .borrows
            .acquire_write(self.session_id, &path, Some(self.cfg.lock_ttl_seconds))
            .is_err()
        {
            reply.error(Errno::EBUSY);
            return;
        }

        if self.deltas.write_blob(self.session_id, &path, &[], None).is_err() {
            reply.error(Errno::EIO);
            return;
        }

        let ino = self.inode_map.allocate();
        let perm = ((mode as u16) & 0o7777) | 0o100000;
        self.inode_map.insert_delta_file(ino, path.clone(), 0, perm);
        let entry = super::git_resolver::TreeEntry {
            name: name_str.to_owned(),
            mode: format!("{:o}", perm),
            oid: String::new(),
            kind: super::git_resolver::EntryKind::File,
            size: 0,
            perm,
        };
        reply.created(&TTL, &entry_attr(INodeNo(ino), &entry), Generation(0), FileHandle(0), FopenFlags::empty());
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if ino.0 == 1 {
            reply.error(Errno::EISDIR);
            return;
        }

        if let Some(meta) = self.inode_map.delta_file_of(ino.0) {
            if self
                .borrows
                .acquire_write(self.session_id, &meta.path, Some(self.cfg.lock_ttl_seconds))
                .is_err()
            {
                reply.error(Errno::EBUSY);
                return;
            }

            let mut current = match self.deltas.read_blob(self.session_id, &meta.path) {
                Ok(Some(delta_bytes)) => delta_bytes,
                Ok(None) => Vec::new(),
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            };

            apply_write_at_offset(&mut current, offset as usize, data);

            if self.deltas.write_blob(self.session_id, &meta.path, &current, Some(ByteRange { offset, len: data.len() as u64 })).is_err() {
                reply.error(Errno::EIO);
                return;
            }
            self.inode_map.update_delta_size(ino.0, current.len() as u64);
            reply.written(data.len().min(u32::MAX as usize) as u32);
            return;
        }

        let Some(entry) = lookup_entry_for_ino(ino.0, &self.cfg.repo_path, &self.tree_cache, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if entry.kind == super::git_resolver::EntryKind::Dir {
            reply.error(Errno::EISDIR);
            return;
        }

        let Some(path) = self.inode_map.path_of(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        if self
            .borrows
            .acquire_write(self.session_id, &path, Some(self.cfg.lock_ttl_seconds))
            .is_err()
        {
            reply.error(Errno::EBUSY);
            return;
        }

        let mut current = match self.deltas.read_blob(self.session_id, &path) {
            Ok(Some(delta_bytes)) => delta_bytes,
            Ok(None) => match self.blob_cache.get(&entry.oid, &self.cfg.repo_path) {
                Ok(b) => b,
                Err(_) => {
                    reply.error(Errno::EIO);
                    return;
                }
            },
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        apply_write_at_offset(&mut current, offset as usize, data);

        if self.deltas.write_blob(self.session_id, &path, &current, Some(ByteRange { offset, len: data.len() as u64 })).is_err() {
            reply.error(Errno::EIO);
            return;
        }
        self.inode_map.update_delta_size(ino.0, current.len() as u64);

        reply.written(data.len().min(u32::MAX as usize) as u32);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };

        let Some(path) = full_child_path(parent.0, name_str, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let manifest = match self.deltas.load_manifest(self.session_id) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let mut exists_as_file = manifest.writes.contains_key(&path) && !manifest.deletes.contains(&path);

        let parent_tree_oid = match directory_tree_oid_for_ino(
            parent.0,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(DirTreeError::NotFound) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(DirTreeError::Io) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let tree_entries = match self.tree_cache.get(&parent_tree_oid, &self.cfg.repo_path) {
            Ok(e) => e,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if let Some(entry) = tree_entries.iter().find(|e| e.name == name_str) {
            if entry.kind == super::git_resolver::EntryKind::Dir {
                reply.error(Errno::EISDIR);
                return;
            }
            if !manifest.deletes.contains(&path) {
                exists_as_file = true;
            }
        }

        if !exists_as_file {
            reply.error(Errno::ENOENT);
            return;
        }

        if self
            .borrows
            .acquire_write(self.session_id, &path, Some(self.cfg.lock_ttl_seconds))
            .is_err()
        {
            reply.error(Errno::EBUSY);
            return;
        }

        if self.deltas.mark_deleted(self.session_id, &path).is_err() {
            reply.error(Errno::EIO);
            return;
        }
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }

        let Some(old_name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let Some(new_name) = newname.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };

        let Some(old_path) = full_child_path(parent.0, old_name, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(new_path) = full_child_path(newparent.0, new_name, &self.inode_map) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let old_parent_tree_oid = match directory_tree_oid_for_ino(
            parent.0,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(DirTreeError::NotFound) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(DirTreeError::Io) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let old_parent_entries = match self.tree_cache.get(&old_parent_tree_oid, &self.cfg.repo_path) {
            Ok(e) => e,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let manifest = match self.deltas.load_manifest(self.session_id) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let git_entry = old_parent_entries.iter().find(|e| e.name == old_name);
        if let Some(entry) = git_entry {
            if entry.kind == super::git_resolver::EntryKind::Dir {
                reply.error(Errno::EISDIR);
                return;
            }
        }
        let exists_in_manifest = manifest.writes.contains_key(&old_path) && !manifest.deletes.contains(&old_path);
        let exists_in_git = git_entry.is_some() && !manifest.deletes.contains(&old_path);
        if !exists_in_manifest && !exists_in_git {
            reply.error(Errno::ENOENT);
            return;
        }

        match directory_tree_oid_for_ino(
            newparent.0,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(_) => {}
            Err(DirTreeError::NotDir) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(DirTreeError::NotFound) => {
                reply.error(Errno::ENOENT);
                return;
            }
            Err(DirTreeError::Io) => {
                reply.error(Errno::EIO);
                return;
            }
        }

        if self
            .borrows
            .acquire_write(self.session_id, &old_path, Some(self.cfg.lock_ttl_seconds))
            .is_err()
        {
            reply.error(Errno::EBUSY);
            return;
        }
        if self
            .borrows
            .acquire_write(self.session_id, &new_path, Some(self.cfg.lock_ttl_seconds))
            .is_err()
        {
            reply.error(Errno::EBUSY);
            return;
        }

        let source_data = match self.deltas.read_blob(self.session_id, &old_path) {
            Ok(Some(delta_bytes)) => delta_bytes,
            Ok(None) => match git_entry {
                Some(entry) => match self.blob_cache.get(&entry.oid, &self.cfg.repo_path) {
                    Ok(d) => d,
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                },
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            },
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        if self.deltas.write_blob(self.session_id, &new_path, &source_data, None).is_err() {
            reply.error(Errno::EIO);
            return;
        }
        if self.deltas.mark_deleted(self.session_id, &old_path).is_err() {
            reply.error(Errno::EIO);
            return;
        }
        let _ = self.deltas.record_rename(self.session_id, &old_path, &new_path);
        reply.ok();
    }
}

fn resolve_tree_oid(repo_path: &std::path::Path, commitish: &str) -> Result<String> {
    let rev = format!("{commitish}^{{tree}}");
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", &rev])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("git rev-parse failed for {rev}");
    }
    let oid = String::from_utf8(output.stdout)?.trim().to_owned();
    if oid.is_empty() {
        anyhow::bail!("empty tree oid for {rev}");
    }
    Ok(oid)
}

enum DirTreeError {
    NotFound,
    NotDir,
    Io,
}

fn directory_tree_oid_for_ino(
    ino: u64,
    base_commit: &str,
    repo_path: &std::path::Path,
    tree_cache: &TreeCache,
    inode_map: &InodeMap,
) -> std::result::Result<String, DirTreeError> {
    if ino == 1 {
        return resolve_tree_oid(repo_path, base_commit).map_err(|_| DirTreeError::Io);
    }

    let Some((parent_tree_oid, entry_idx)) = inode_map.lookup(ino) else {
        return Err(DirTreeError::NotFound);
    };
    let entries = tree_cache.get(&parent_tree_oid, repo_path).map_err(|_| DirTreeError::Io)?;
    let Some(entry) = entries.get(entry_idx) else {
        return Err(DirTreeError::NotFound);
    };

    if entry.kind != super::git_resolver::EntryKind::Dir {
        return Err(DirTreeError::NotDir);
    }

    Ok(entry.oid.clone())
}

fn lookup_entry_for_ino(
    ino: u64,
    repo_path: &std::path::Path,
    tree_cache: &TreeCache,
    inode_map: &InodeMap,
) -> Option<super::git_resolver::TreeEntry> {
    let (tree_oid, entry_idx) = inode_map.lookup(ino)?;
    let entries = tree_cache.get(&tree_oid, repo_path).ok()?;
    entries.get(entry_idx).cloned()
}

fn file_slice(data: &[u8], offset: usize, size: usize) -> &[u8] {
    if offset >= data.len() {
        return &[];
    }
    let end = offset.saturating_add(size).min(data.len());
    &data[offset..end]
}

fn apply_write_at_offset(buf: &mut Vec<u8>, offset: usize, data: &[u8]) {
    if buf.len() < offset {
        buf.resize(offset, 0);
    }
    let end = offset.saturating_add(data.len());
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[offset..end].copy_from_slice(data);
}

fn full_child_path(parent_ino: u64, name: &str, inode_map: &InodeMap) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if parent_ino == 1 {
        return Some(PathBuf::from(name));
    }
    let parent_path = inode_map.path_of(parent_ino)?;
    Some(parent_path.join(name))
}

fn delta_path_deleted(deltas: &DeltaStore, session_id: Uuid, path: &std::path::Path) -> Result<bool> {
    let manifest = deltas.load_manifest(session_id)?;
    Ok(manifest.deletes.contains(path))
}

fn path_starts_with_dir(path: &std::path::Path, dir: &std::path::Path) -> bool {
    if dir.as_os_str().is_empty() {
        return path.components().count() == 1;
    }
    if !path.starts_with(dir) {
        return false;
    }
    let Ok(rest) = path.strip_prefix(dir) else {
        return false;
    };
    rest.components().count() == 1
}

fn git_entry_exists_in_parent(
    repo_path: &std::path::Path,
    tree_cache: &TreeCache,
    base_commit: &str,
    inode_map: &InodeMap,
    parent_ino: u64,
    name: &str,
) -> std::result::Result<bool, DirTreeError> {
    let parent_tree_oid = directory_tree_oid_for_ino(parent_ino, base_commit, repo_path, tree_cache, inode_map)?;
    let entries = tree_cache.get(&parent_tree_oid, repo_path).map_err(|_| DirTreeError::Io)?;
    Ok(entries.iter().any(|e| e.name == name))
}

fn entry_kind_to_file_type(kind: super::git_resolver::EntryKind) -> FileType {
    match kind {
        super::git_resolver::EntryKind::File => FileType::RegularFile,
        super::git_resolver::EntryKind::Dir => FileType::Directory,
        super::git_resolver::EntryKind::Symlink => FileType::Symlink,
    }
}

fn entry_attr(ino: INodeNo, entry: &super::git_resolver::TreeEntry) -> FileAttr {
    FileAttr {
        ino,
        size: entry.size,
        blocks: (entry.size + 511) / 512,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: entry_kind_to_file_type(entry.kind),
        perm: entry.perm,
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        flags: 0,
        blksize: 4096,
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

fn virtual_dir_attr(ino: INodeNo) -> FileAttr {
    FileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o555,
        nlink: 2,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

fn virtual_file_attr(ino: INodeNo, size: u64) -> FileAttr {
    FileAttr {
        ino,
        size,
        blocks: (size + 511) / 512,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0o444,
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

fn parse_virtual_peer_path(path: &std::path::Path) -> Option<(Uuid, PathBuf)> {
    let mut comps = path.components();
    let c0 = comps.next()?.as_os_str().to_str()?;
    let c1 = comps.next()?.as_os_str().to_str()?;
    let c2 = comps.next()?.as_os_str().to_str()?;
    if c0 != ".simgit" || c1 != "peers" {
        return None;
    }
    let peer = Uuid::parse_str(c2).ok()?;
    let mut rel = PathBuf::new();
    for c in comps {
        rel.push(c.as_os_str());
    }
    Some((peer, rel))
}

#[cfg(test)]
mod tests {
    use super::{
        apply_write_at_offset, file_slice, full_child_path, parse_virtual_peer_path,
        path_starts_with_dir, InodeMap,
    };
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn file_slice_within_bounds() {
        let data = b"abcdef";
        assert_eq!(file_slice(data, 1, 3), b"bcd");
    }

    #[test]
    fn file_slice_offset_past_end() {
        let data = b"abcdef";
        assert_eq!(file_slice(data, 99, 5), b"");
    }

    #[test]
    fn file_slice_truncates_to_end() {
        let data = b"abcdef";
        assert_eq!(file_slice(data, 4, 10), b"ef");
    }

    #[test]
    fn apply_write_in_middle() {
        let mut data = b"abcdef".to_vec();
        apply_write_at_offset(&mut data, 2, b"ZZ");
        assert_eq!(data, b"abZZef");
    }

    #[test]
    fn apply_write_extends_with_zeros() {
        let mut data = b"abc".to_vec();
        apply_write_at_offset(&mut data, 5, b"xy");
        assert_eq!(data, b"abc\0\0xy");
    }

    #[test]
    fn full_child_path_from_root() {
        let map = InodeMap::new();
        let p = full_child_path(1, "a.txt", &map).expect("root child path");
        assert_eq!(p, PathBuf::from("a.txt"));
    }

    #[test]
    fn path_starts_with_dir_only_direct_children() {
        assert!(path_starts_with_dir(std::path::Path::new("a.txt"), std::path::Path::new("")));
        assert!(!path_starts_with_dir(std::path::Path::new("src/main.rs"), std::path::Path::new("")));
        assert!(path_starts_with_dir(std::path::Path::new("src/main.rs"), std::path::Path::new("src")));
        assert!(!path_starts_with_dir(std::path::Path::new("src/nested/main.rs"), std::path::Path::new("src")));
    }

    #[test]
    fn parse_virtual_peer_path_extracts_peer_and_relative_path() {
        let peer = Uuid::now_v7();
        let path = PathBuf::from(format!(".simgit/peers/{}/src/main.rs", peer));
        let parsed = parse_virtual_peer_path(&path).expect("should parse");
        assert_eq!(parsed.0, peer);
        assert_eq!(parsed.1, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn parse_virtual_peer_path_rejects_non_virtual_prefix() {
        let path = PathBuf::from("src/main.rs");
        assert!(parse_virtual_peer_path(&path).is_none());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_integration_tests {
    use super::FuseBackend;
    use crate::borrow::BorrowRegistry;
    use crate::config::{Config, VfsBackend};
    use crate::delta::DeltaStore;
    use crate::session::SessionManager;
    use crate::vfs::VfsBackendTrait;
    use chrono::Utc;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    use simgit_sdk::{SessionInfo, SessionStatus};

    static TEST_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let seq = TEST_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "simgit-fuse-int-{}-{}-{}",
            std::process::id(),
            nanos,
            seq
        ))
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .expect("git command should execute");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn init_repo(root: &std::path::Path) -> std::path::PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");

        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.email", "tests@simgit.local"]);
        run_git(&repo, &["config", "user.name", "simgit-tests"]);
        std::fs::write(repo.join("old.txt"), b"old-content\n").expect("write file");
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        repo
    }

    fn wait_for_mount_ready(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if std::fs::read_dir(path).is_ok() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("mount path did not become ready: {}", path.display());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[tokio::test]
    #[ignore = "requires Linux FUSE kernel + fusermount available"]
    async fn fuse_mount_roundtrip_create_unlink_rename() {
        let root = temp_root();
        let repo = init_repo(&root);
        let state_dir = root.join("state");
        let mnt_dir = state_dir.join("mnt");
        std::fs::create_dir_all(&mnt_dir).expect("create mnt dir");

        let cfg = Arc::new(Config {
            repo_path: repo.clone(),
            state_dir: state_dir.clone(),
            mnt_dir: mnt_dir.clone(),
            max_sessions: 8,
            max_delta_bytes: 2 * 1024 * 1024,
            lock_ttl_seconds: 3600,
            session_recovery_ttl_seconds: 86400,
            vfs_backend: VfsBackend::Fuse,
            metrics_enabled: false,
            metrics_addr: "127.0.0.1:0".to_owned(),
            commit_peer_capture_concurrency: 4,
        });

        let db_path = state_dir.join("state.db");
        let sessions = Arc::new(SessionManager::open(&db_path).await.expect("open sessions"));
        let borrows = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
        let deltas = Arc::new(DeltaStore::new(state_dir.join("deltas")));
        let backend = FuseBackend::new(Arc::clone(&cfg), Arc::clone(&deltas), Arc::clone(&borrows));

        let session_id = Uuid::now_v7();
        let mount_path = mnt_dir.join(session_id.to_string());
        let session = SessionInfo {
            session_id,
            task_id: "fuse-int".to_owned(),
            agent_label: Some("linux-int".to_owned()),
            base_commit: "HEAD".to_owned(),
            created_at: Utc::now(),
            status: SessionStatus::Active,
            mount_path: mount_path.clone(),
            branch_name: None,
            peers_enabled: false,
        };

        backend.mount(&session).await.expect("mount");
        wait_for_mount_ready(&mount_path);

        let new_path = mount_path.join("new.txt");
        std::fs::write(&new_path, b"hello-from-create\n").expect("create/write new file");
        let created = std::fs::read(&new_path).expect("read created file");
        assert_eq!(created, b"hello-from-create\n");

        let old_path = mount_path.join("old.txt");
        let renamed_path = mount_path.join("renamed.txt");
        std::fs::rename(&old_path, &renamed_path).expect("rename baseline file");
        assert!(!old_path.exists(), "old path should be gone after rename");
        let renamed = std::fs::read(&renamed_path).expect("read renamed file");
        assert_eq!(renamed, b"old-content\n");

        std::fs::remove_file(&renamed_path).expect("unlink renamed file");
        assert!(!renamed_path.exists(), "renamed path should be removed");

        let _ = backend.unmount(session_id).await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[ignore = "requires Linux FUSE kernel + fusermount available"]
    async fn fuse_mount_can_remount_same_session_path() {
        let root = temp_root();
        let repo = init_repo(&root);
        let state_dir = root.join("state");
        let mnt_dir = state_dir.join("mnt");
        std::fs::create_dir_all(&mnt_dir).expect("create mnt dir");

        let cfg = Arc::new(Config {
            repo_path: repo.clone(),
            state_dir: state_dir.clone(),
            mnt_dir: mnt_dir.clone(),
            max_sessions: 8,
            max_delta_bytes: 2 * 1024 * 1024,
            lock_ttl_seconds: 3600,
            session_recovery_ttl_seconds: 86400,
            vfs_backend: VfsBackend::Fuse,
            metrics_enabled: false,
            metrics_addr: "127.0.0.1:0".to_owned(),
            commit_peer_capture_concurrency: 4,
        });

        let db_path = state_dir.join("state.db");
        let sessions = Arc::new(SessionManager::open(&db_path).await.expect("open sessions"));
        let borrows = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
        let deltas = Arc::new(DeltaStore::new(state_dir.join("deltas")));
        let backend = FuseBackend::new(Arc::clone(&cfg), Arc::clone(&deltas), Arc::clone(&borrows));

        let session_id = Uuid::now_v7();
        let mount_path = mnt_dir.join(session_id.to_string());
        let session = SessionInfo {
            session_id,
            task_id: "fuse-remount".to_owned(),
            agent_label: Some("linux-int".to_owned()),
            base_commit: "HEAD".to_owned(),
            created_at: Utc::now(),
            status: SessionStatus::Active,
            mount_path: mount_path.clone(),
            branch_name: None,
            peers_enabled: false,
        };

        backend.mount(&session).await.expect("first mount");
        wait_for_mount_ready(&mount_path);
        let bytes = std::fs::read(mount_path.join("old.txt")).expect("read baseline file");
        assert_eq!(bytes, b"old-content\n");

        let _ = backend.unmount(session_id).await;

        backend.mount(&session).await.expect("second mount");
        wait_for_mount_ready(&mount_path);
        let bytes2 = std::fs::read(mount_path.join("old.txt")).expect("read baseline after remount");
        assert_eq!(bytes2, b"old-content\n");

        let _ = backend.unmount(session_id).await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
