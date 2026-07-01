//! FUSE filesystem handler for a single session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use uuid::Uuid;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::DeltaStore;
use crate::vfs::git_resolver::{BlobCache, InodeMap, TreeCache};

pub(super) const TTL: Duration = Duration::from_secs(1);
pub(super) const SIMGIT_META_DIR: &str = ".simgit";
pub(super) const SIMGIT_PEERS_DIR: &str = ".simgit/peers";

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

pub(super) struct SessionFs {
    pub(super) session_id: Uuid,
    pub(super) peers_enabled: bool,
    pub(super) cfg: Arc<Config>,
    pub(super) base_commit: String,
    pub(super) deltas: Arc<DeltaStore>,
    pub(super) borrows: Arc<BorrowRegistry>,
    pub(super) tree_cache: Arc<TreeCache>,
    pub(super) blob_cache: Arc<BlobCache>,
    pub(super) inode_map: Arc<InodeMap>,
    pub(super) virtual_inos: Arc<Mutex<HashMap<u64, PathBuf>>>,
    pub(super) virtual_paths: Arc<Mutex<HashMap<PathBuf, u64>>>,
    pub(super) next_virtual_ino: Arc<AtomicU64>,
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
    pub(super) fn new(
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
            tree_cache: Arc::new(TreeCache::new(100)),
            blob_cache: Arc::new(BlobCache::new(50, 10 * 1024 * 1024)), // 50 blobs, 10MB cap
            inode_map: Arc::new(InodeMap::new()),
            virtual_inos: Arc::new(Mutex::new(HashMap::new())),
            virtual_paths: Arc::new(Mutex::new(HashMap::new())),
            next_virtual_ino: Arc::new(AtomicU64::new(1u64 << 62)),
        }
    }

    pub(super) fn ensure_virtual_ino(&self, path: &std::path::Path) -> u64 {
        if let Some(ino) = self.virtual_paths.lock().unwrap().get(path).copied() {
            return ino;
        }
        let ino = self.next_virtual_ino.fetch_add(1, Ordering::Relaxed);
        self.virtual_inos
            .lock()
            .unwrap()
            .insert(ino, path.to_path_buf());
        self.virtual_paths
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), ino);
        ino
    }

    pub(super) fn virtual_path_of(&self, ino: u64) -> Option<PathBuf> {
        self.virtual_inos.lock().unwrap().get(&ino).cloned()
    }

    pub(super) fn active_peer_ids(&self) -> Vec<Uuid> {
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

    pub(super) fn peer_children(
        &self,
        peer_session: Uuid,
        dir: &std::path::Path,
    ) -> Vec<(String, bool)> {
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

    pub(super) fn peer_file_bytes(
        &self,
        peer_session: Uuid,
        rel: &std::path::Path,
    ) -> Option<Vec<u8>> {
        match self.deltas.read_blob(peer_session, rel) {
            Ok(Some(b)) => Some(b),
            _ => None,
        }
    }
}
