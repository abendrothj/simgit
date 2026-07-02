//! FUSE filesystem handler for a single session.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use uuid::Uuid;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::store::ByteRange;
use crate::delta::DeltaStore;
use crate::vfs::git_resolver::{BlobCache, EntryKind, InodeMap, TreeCache};
use crate::vfs::session_ops::{
    apply_write_at_offset, delta_path_deleted, directory_tree_oid_for_ino, file_slice,
    full_child_path, git_entry_exists_in_parent, lookup_entry_for_ino, path_starts_with_dir,
    DirTreeError, SessionVfsOps, VfsAttr, VfsDirEntry, VfsFileKind, VfsOpError,
};

pub const TTL: Duration = Duration::from_secs(1);
pub const SIMGIT_META_DIR: &str = ".simgit";
pub const SIMGIT_PEERS_DIR: &str = ".simgit/peers";

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

pub struct SessionFs {
    pub session_id: Uuid,
    pub peers_enabled: bool,
    pub cfg: Arc<Config>,
    pub base_commit: String,
    pub deltas: Arc<DeltaStore>,
    pub borrows: Arc<BorrowRegistry>,
    pub tree_cache: Arc<TreeCache>,
    pub blob_cache: Arc<BlobCache>,
    pub inode_map: Arc<InodeMap>,
    pub virtual_inos: Arc<Mutex<HashMap<u64, PathBuf>>>,
    pub virtual_paths: Arc<Mutex<HashMap<PathBuf, u64>>>,
    pub next_virtual_ino: Arc<AtomicU64>,
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
    pub fn new(
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

    pub fn ensure_virtual_ino(&self, path: &std::path::Path) -> u64 {
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

    pub fn virtual_path_of(&self, ino: u64) -> Option<PathBuf> {
        self.virtual_inos.lock().unwrap().get(&ino).cloned()
    }

    pub fn active_peer_ids(&self) -> Vec<Uuid> {
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

    pub fn peer_children(
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

    pub fn peer_file_bytes(
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

/// The single place borrow-checking, CoW delta storage, and git-tree
/// resolution are wired together. This impl handles only real (non-virtual)
/// paths; the `.simgit/peers/<uuid>/...` virtual peer-diff feature is
/// FUSE-only UX and is handled directly by `filesystem_impl` before falling
/// through to these methods.
impl SessionVfsOps for SessionFs {
    fn lookup(&self, parent: u64, name: &str) -> Result<u64, VfsOpError> {
        let parent_tree_oid = match directory_tree_oid_for_ino(
            parent,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => return Err(VfsOpError::NotADirectory),
            Err(_) => return Err(VfsOpError::Io),
        };

        let tree_entries = self
            .tree_cache
            .get(&parent_tree_oid, &self.cfg.repo_path)
            .map_err(|_| VfsOpError::Io)?;

        let parent_path = if parent == 1 {
            PathBuf::new()
        } else {
            self.inode_map.path_of(parent).unwrap_or_default()
        };
        let manifest = self
            .deltas
            .load_manifest(self.session_id)
            .map_err(|_| VfsOpError::Io)?;

        for (idx, entry) in tree_entries.iter().enumerate() {
            if entry.name == name {
                let full_path = parent_path.join(&entry.name);
                if manifest.deletes.contains(&full_path) {
                    return Err(VfsOpError::NotFound);
                }
                let ino = self.inode_map.allocate();
                self.inode_map
                    .insert(ino, parent_tree_oid.clone(), idx, full_path);
                return Ok(ino);
            }
        }

        let full_path = parent_path.join(name);
        if manifest.writes.contains_key(&full_path) {
            let size = self
                .deltas
                .read_blob(self.session_id, &full_path)
                .ok()
                .flatten()
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            let ino = self.inode_map.allocate();
            self.inode_map
                .insert_delta_file(ino, full_path, size, 0o100644);
            return Ok(ino);
        }

        // Check created directories (mkdir in delta manifest).
        if manifest.dirs.contains(&full_path) {
            let ino = self.inode_map.allocate();
            self.inode_map
                .insert_delta_file(ino, full_path, 0, 0o40755);
            return Ok(ino);
        }

        Err(VfsOpError::NotFound)
    }

    fn getattr(&self, id: u64) -> Result<VfsAttr, VfsOpError> {
        if id == 1 {
            return Ok(VfsAttr {
                kind: VfsFileKind::Dir,
                size: 0,
                perm: 0o755,
                nlink: 2,
                uid: crate::platform::current_uid(),
                gid: crate::platform::current_gid(),
            });
        }

        if let Some(meta) = self.inode_map.delta_file_of(id) {
            if delta_path_deleted(&self.deltas, self.session_id, &meta.path).unwrap_or(false) {
                return Err(VfsOpError::NotFound);
            }
            let is_dir = meta.perm & 0o40000 != 0;
            return Ok(VfsAttr {
                kind: if is_dir { VfsFileKind::Dir } else { VfsFileKind::File },
                size: meta.size,
                perm: meta.perm,
                nlink: 1,
                uid: crate::platform::current_uid(),
                gid: crate::platform::current_gid(),
            });
        }

        let Some((tree_oid, entry_idx)) = self.inode_map.lookup(id) else {
            return Err(VfsOpError::NotFound);
        };

        let tree_entries = self
            .tree_cache
            .get(&tree_oid, &self.cfg.repo_path)
            .map_err(|_| VfsOpError::Io)?;

        let Some(entry) = tree_entries.get(entry_idx) else {
            return Err(VfsOpError::NotFound);
        };

        let Some(path) = self.inode_map.path_of(id) else {
            return Err(VfsOpError::NotFound);
        };
        if delta_path_deleted(&self.deltas, self.session_id, &path).unwrap_or(false) {
            return Err(VfsOpError::NotFound);
        }

        if entry.kind == EntryKind::File {
            match self.deltas.read_blob(self.session_id, &path) {
                Ok(Some(delta_bytes)) => {
                    return Ok(VfsAttr {
                        kind: VfsFileKind::File,
                        size: delta_bytes.len() as u64,
                        perm: entry.perm,
                        nlink: 1,
                        uid: crate::platform::current_uid(),
                        gid: crate::platform::current_gid(),
                    });
                }
                Ok(None) => {}
                Err(_) => return Err(VfsOpError::Io),
            }
        }

        Ok(VfsAttr {
            kind: entry.kind.into(),
            size: entry.size,
            perm: entry.perm,
            nlink: 1,
            uid: crate::platform::current_uid(),
            gid: crate::platform::current_gid(),
        })
    }

    fn read(&self, id: u64, offset: u64, size: u64) -> Result<Vec<u8>, VfsOpError> {
        if id == 1 {
            return Err(VfsOpError::IsADirectory);
        }

        if let Some(meta) = self.inode_map.delta_file_of(id) {
            if delta_path_deleted(&self.deltas, self.session_id, &meta.path).unwrap_or(false) {
                return Err(VfsOpError::NotFound);
            }
            let data = match self.deltas.read_blob(self.session_id, &meta.path) {
                Ok(Some(b)) => b,
                Ok(None) => return Err(VfsOpError::NotFound),
                Err(_) => return Err(VfsOpError::Io),
            };
            return Ok(file_slice(&data, offset as usize, size as usize).to_vec());
        }

        let Some(entry) =
            lookup_entry_for_ino(id, &self.cfg.repo_path, &self.tree_cache, &self.inode_map)
        else {
            return Err(VfsOpError::NotFound);
        };

        let Some(path) = self.inode_map.path_of(id) else {
            return Err(VfsOpError::NotFound);
        };
        if delta_path_deleted(&self.deltas, self.session_id, &path).unwrap_or(false) {
            return Err(VfsOpError::NotFound);
        }

        if entry.kind == EntryKind::Dir {
            return Err(VfsOpError::IsADirectory);
        }

        let data = match self.deltas.read_blob(self.session_id, &path) {
            Ok(Some(delta_bytes)) => delta_bytes,
            Ok(None) => self
                .blob_cache
                .get(&entry.oid, &self.cfg.repo_path)
                .map_err(|_| VfsOpError::Io)?,
            Err(_) => return Err(VfsOpError::Io),
        };

        Ok(file_slice(&data, offset as usize, size as usize).to_vec())
    }

    fn write(&self, id: u64, offset: u64, data: &[u8]) -> Result<u64, VfsOpError> {
        if id == 1 {
            return Err(VfsOpError::IsADirectory);
        }

        if let Some(meta) = self.inode_map.delta_file_of(id) {
            self.borrows
                .acquire_write(self.session_id, &meta.path, Some(self.cfg.lock_ttl_seconds))
                .map_err(VfsOpError::from)?;

            let mut current = match self.deltas.read_blob(self.session_id, &meta.path) {
                Ok(Some(delta_bytes)) => delta_bytes,
                Ok(None) => Vec::new(),
                Err(_) => return Err(VfsOpError::Io),
            };

            apply_write_at_offset(&mut current, offset as usize, data);

            self.deltas
                .write_blob(
                    self.session_id,
                    &meta.path,
                    &current,
                    Some(ByteRange {
                        offset,
                        len: data.len() as u64,
                    }),
                )
                .map_err(|_| VfsOpError::Io)?;

            self.inode_map.update_delta_size(id, current.len() as u64);
            return Ok(data.len() as u64);
        }

        let Some(entry) =
            lookup_entry_for_ino(id, &self.cfg.repo_path, &self.tree_cache, &self.inode_map)
        else {
            return Err(VfsOpError::NotFound);
        };
        if entry.kind == EntryKind::Dir {
            return Err(VfsOpError::IsADirectory);
        }

        let Some(path) = self.inode_map.path_of(id) else {
            return Err(VfsOpError::NotFound);
        };

        self.borrows
            .acquire_write(self.session_id, &path, Some(self.cfg.lock_ttl_seconds))
            .map_err(VfsOpError::from)?;

        let mut current = match self.deltas.read_blob(self.session_id, &path) {
            Ok(Some(delta_bytes)) => delta_bytes,
            Ok(None) => self
                .blob_cache
                .get(&entry.oid, &self.cfg.repo_path)
                .map_err(|_| VfsOpError::Io)?,
            Err(_) => return Err(VfsOpError::Io),
        };

        apply_write_at_offset(&mut current, offset as usize, data);

        self.deltas
            .write_blob(
                self.session_id,
                &path,
                &current,
                Some(ByteRange {
                    offset,
                    len: data.len() as u64,
                }),
            )
            .map_err(|_| VfsOpError::Io)?;

        self.inode_map.update_delta_size(id, current.len() as u64);

        Ok(data.len() as u64)
    }

    fn readdir(&self, id: u64) -> Result<Vec<VfsDirEntry>, VfsOpError> {
        let tree_oid = match directory_tree_oid_for_ino(
            id,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => return Err(VfsOpError::NotADirectory),
            Err(_) => return Err(VfsOpError::Io),
        };

        let tree_entries = self
            .tree_cache
            .get(&tree_oid, &self.cfg.repo_path)
            .map_err(|_| VfsOpError::Io)?;

        let parent_path = if id == 1 {
            PathBuf::new()
        } else {
            self.inode_map.path_of(id).unwrap_or_default()
        };
        let manifest = self
            .deltas
            .load_manifest(self.session_id)
            .map_err(|_| VfsOpError::Io)?;

        let mut entries = Vec::new();
        for (idx, entry) in tree_entries.iter().enumerate() {
            let full_path = parent_path.join(&entry.name);
            if manifest.deletes.contains(&full_path) {
                continue;
            }
            let ino = self.inode_map.allocate();
            self.inode_map.insert(ino, tree_oid.clone(), idx, full_path);
            entries.push(VfsDirEntry {
                name: entry.name.clone(),
                id: ino,
                kind: entry.kind.into(),
            });
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
            self.inode_map
                .insert_delta_file(ino, path.clone(), size, 0o100644);
            entries.push(VfsDirEntry {
                name: file_name.to_owned(),
                id: ino,
                kind: VfsFileKind::File,
            });
            seen_names.insert(file_name.to_owned());
        }

        // Include mkdir-created directories.
        for dir_path in &manifest.dirs {
            if !path_starts_with_dir(dir_path, &parent_path) {
                continue;
            }
            let Some(dir_name) = dir_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if seen_names.contains(dir_name) || manifest.deletes.contains(dir_path) {
                continue;
            }
            let ino = self.inode_map.allocate();
            self.inode_map
                .insert_delta_file(ino, dir_path.clone(), 0, 0o40755);
            entries.push(VfsDirEntry {
                name: dir_name.to_owned(),
                id: ino,
                kind: VfsFileKind::Dir,
            });
            seen_names.insert(dir_name.to_owned());
        }

        Ok(entries)
    }

    fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64, VfsOpError> {
        match directory_tree_oid_for_ino(
            parent,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(_) => {}
            Err(DirTreeError::NotDir) => return Err(VfsOpError::NotADirectory),
            Err(DirTreeError::NotFound) => return Err(VfsOpError::NotFound),
            Err(DirTreeError::Io) => return Err(VfsOpError::Io),
        }

        let Some(path) = full_child_path(parent, name, &self.inode_map) else {
            return Err(VfsOpError::NotFound);
        };

        let manifest = self
            .deltas
            .load_manifest(self.session_id)
            .map_err(|_| VfsOpError::Io)?;
        if !manifest.deletes.contains(&path) && manifest.writes.contains_key(&path) {
            return Err(VfsOpError::AlreadyExists);
        }

        if !manifest.deletes.contains(&path)
            && git_entry_exists_in_parent(
                &self.cfg.repo_path,
                &self.tree_cache,
                &self.base_commit,
                &self.inode_map,
                parent,
                name,
            )
            .unwrap_or(false)
        {
            return Err(VfsOpError::AlreadyExists);
        }

        self.borrows
            .acquire_write(self.session_id, &path, Some(self.cfg.lock_ttl_seconds))
            .map_err(VfsOpError::from)?;

        self.deltas
            .write_blob(self.session_id, &path, &[], None)
            .map_err(|_| VfsOpError::Io)?;

        let ino = self.inode_map.allocate();
        let perm = ((mode as u16) & 0o7777) | 0o100000;
        self.inode_map.insert_delta_file(ino, path, 0, perm);
        Ok(ino)
    }

    fn unlink(&self, parent: u64, name: &str) -> Result<(), VfsOpError> {
        let Some(path) = full_child_path(parent, name, &self.inode_map) else {
            return Err(VfsOpError::NotFound);
        };

        let manifest = self
            .deltas
            .load_manifest(self.session_id)
            .map_err(|_| VfsOpError::Io)?;

        let mut exists_as_file =
            manifest.writes.contains_key(&path) && !manifest.deletes.contains(&path);

        let parent_tree_oid = match directory_tree_oid_for_ino(
            parent,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => return Err(VfsOpError::NotADirectory),
            Err(DirTreeError::NotFound) => return Err(VfsOpError::NotFound),
            Err(DirTreeError::Io) => return Err(VfsOpError::Io),
        };

        let tree_entries = self
            .tree_cache
            .get(&parent_tree_oid, &self.cfg.repo_path)
            .map_err(|_| VfsOpError::Io)?;
        if let Some(entry) = tree_entries.iter().find(|e| e.name == name) {
            if entry.kind == EntryKind::Dir {
                return Err(VfsOpError::IsADirectory);
            }
            if !manifest.deletes.contains(&path) {
                exists_as_file = true;
            }
        }

        if !exists_as_file {
            return Err(VfsOpError::NotFound);
        }

        self.borrows
            .acquire_write(self.session_id, &path, Some(self.cfg.lock_ttl_seconds))
            .map_err(VfsOpError::from)?;

        self.deltas
            .mark_deleted(self.session_id, &path)
            .map_err(|_| VfsOpError::Io)?;
        Ok(())
    }

    fn rename(
        &self,
        from_parent: u64,
        from_name: &str,
        to_parent: u64,
        to_name: &str,
    ) -> Result<(), VfsOpError> {
        let Some(old_path) = full_child_path(from_parent, from_name, &self.inode_map) else {
            return Err(VfsOpError::NotFound);
        };
        let Some(new_path) = full_child_path(to_parent, to_name, &self.inode_map) else {
            return Err(VfsOpError::NotFound);
        };

        let old_parent_tree_oid = match directory_tree_oid_for_ino(
            from_parent,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(DirTreeError::NotDir) => return Err(VfsOpError::NotADirectory),
            Err(DirTreeError::NotFound) => return Err(VfsOpError::NotFound),
            Err(DirTreeError::Io) => return Err(VfsOpError::Io),
        };
        let old_parent_entries = self
            .tree_cache
            .get(&old_parent_tree_oid, &self.cfg.repo_path)
            .map_err(|_| VfsOpError::Io)?;
        let manifest = self
            .deltas
            .load_manifest(self.session_id)
            .map_err(|_| VfsOpError::Io)?;

        let git_entry = old_parent_entries.iter().find(|e| e.name == from_name);
        if let Some(entry) = git_entry {
            if entry.kind == EntryKind::Dir {
                return Err(VfsOpError::IsADirectory);
            }
        }
        let exists_in_manifest =
            manifest.writes.contains_key(&old_path) && !manifest.deletes.contains(&old_path);
        let exists_in_git = git_entry.is_some() && !manifest.deletes.contains(&old_path);
        if !exists_in_manifest && !exists_in_git {
            return Err(VfsOpError::NotFound);
        }

        match directory_tree_oid_for_ino(
            to_parent,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(_) => {}
            Err(DirTreeError::NotDir) => return Err(VfsOpError::NotADirectory),
            Err(DirTreeError::NotFound) => return Err(VfsOpError::NotFound),
            Err(DirTreeError::Io) => return Err(VfsOpError::Io),
        }

        self.borrows
            .acquire_write(self.session_id, &old_path, Some(self.cfg.lock_ttl_seconds))
            .map_err(VfsOpError::from)?;
        self.borrows
            .acquire_write(self.session_id, &new_path, Some(self.cfg.lock_ttl_seconds))
            .map_err(VfsOpError::from)?;

        let source_data = match self.deltas.read_blob(self.session_id, &old_path) {
            Ok(Some(delta_bytes)) => delta_bytes,
            Ok(None) => match git_entry {
                Some(entry) => self
                    .blob_cache
                    .get(&entry.oid, &self.cfg.repo_path)
                    .map_err(|_| VfsOpError::Io)?,
                None => return Err(VfsOpError::NotFound),
            },
            Err(_) => return Err(VfsOpError::Io),
        };

        self.deltas
            .write_blob(self.session_id, &new_path, &source_data, None)
            .map_err(|_| VfsOpError::Io)?;
        self.deltas
            .mark_deleted(self.session_id, &old_path)
            .map_err(|_| VfsOpError::Io)?;
        let _ = self
            .deltas
            .record_rename(self.session_id, &old_path, &new_path);
        Ok(())
    }

    fn open(&self, id: u64) -> Result<(), VfsOpError> {
        if id == 1 {
            return Err(VfsOpError::IsADirectory);
        }
        if let Some(meta) = self.inode_map.delta_file_of(id) {
            if delta_path_deleted(&self.deltas, self.session_id, &meta.path).unwrap_or(false) {
                return Err(VfsOpError::NotFound);
            }
            return Ok(());
        }
        let Some(entry) =
            lookup_entry_for_ino(id, &self.cfg.repo_path, &self.tree_cache, &self.inode_map)
        else {
            return Err(VfsOpError::NotFound);
        };
        if entry.kind == EntryKind::Dir {
            return Err(VfsOpError::IsADirectory);
        }
        let Some(path) = self.inode_map.path_of(id) else {
            return Err(VfsOpError::NotFound);
        };
        if delta_path_deleted(&self.deltas, self.session_id, &path).unwrap_or(false) {
            return Err(VfsOpError::NotFound);
        }
        Ok(())
    }

    fn opendir(&self, id: u64) -> Result<(), VfsOpError> {
        match directory_tree_oid_for_ino(
            id,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(_) => Ok(()),
            Err(DirTreeError::NotDir) => Err(VfsOpError::NotADirectory),
            Err(DirTreeError::NotFound) => Err(VfsOpError::NotFound),
            Err(DirTreeError::Io) => Err(VfsOpError::Io),
        }
    }

    fn read_symlink_target(&self, id: u64) -> Result<Vec<u8>, VfsOpError> {
        if id == 1 {
            return Err(VfsOpError::InvalidArgument);
        }
        let Some(entry) =
            lookup_entry_for_ino(id, &self.cfg.repo_path, &self.tree_cache, &self.inode_map)
        else {
            return Err(VfsOpError::NotFound);
        };
        if entry.kind != EntryKind::Symlink {
            return Err(VfsOpError::InvalidArgument);
        }
        self.blob_cache
            .get(&entry.oid, &self.cfg.repo_path)
            .map_err(|_| VfsOpError::Io)
    }

    fn allocate_dir_ino(&self, parent: u64, name: &str) -> Result<u64, VfsOpError> {
        let parent_path = if parent == 1 {
            PathBuf::new()
        } else {
            self.inode_map.path_of(parent).unwrap_or_default()
        };
        let dir_path = parent_path.join(name);
        let ino = self.inode_map.allocate();
        self.inode_map.insert_delta_file(ino, dir_path, 0, 0o40755);
        Ok(ino)
    }

    fn mkdir(&self, parent: u64, name: &str, _mode: u32) -> Result<u64, VfsOpError> {
        let _ = <Self as SessionVfsOps>::lookup(self, parent, ".")?;
        let parent_path = if parent == 1 {
            PathBuf::new()
        } else {
            self.inode_map.path_of(parent).unwrap_or_default()
        };
        let dir_path = parent_path.join(name);
        self.deltas.record_mkdir(self.session_id, &dir_path).map_err(|_| VfsOpError::Io)?;
        self.allocate_dir_ino(parent, name)
    }

    fn rmdir(&self, parent: u64, name: &str) -> Result<(), VfsOpError> {
        let dir_id = <Self as SessionVfsOps>::lookup(self, parent, name)?;
        let parent_path = if parent == 1 {
            PathBuf::new()
        } else {
            self.inode_map.path_of(parent).unwrap_or_default()
        };
        let dir_path = parent_path.join(name);
        let manifest = self.deltas.load_manifest(self.session_id).map_err(|_| VfsOpError::Io)?;
        let has_children = manifest
            .writes
            .keys()
            .chain(manifest.dirs.iter())
            .any(|p| p.starts_with(&dir_path) && p != &dir_path);
        if has_children {
            return Err(VfsOpError::IsADirectory);
        }
        self.deltas.record_rmdir(self.session_id, &dir_path).map_err(|_| VfsOpError::Io)?;
        self.inode_map.remove_delta_file(dir_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VfsBackend;
    use crate::session::SessionManager;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64 as TestAtomicU64, Ordering as TestOrdering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DIR_COUNTER: TestAtomicU64 = TestAtomicU64::new(0);

    fn temp_repo_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let seq = TEST_DIR_COUNTER.fetch_add(1, TestOrdering::Relaxed);
        std::env::temp_dir().join(format!(
            "simgit-session-fs-test-{}-{}-{}",
            std::process::id(),
            nanos,
            seq
        ))
    }

    fn run_git(repo: &PathBuf, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .expect("git command should execute");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn init_repo_with_file() -> PathBuf {
        let repo = temp_repo_dir();
        fs::create_dir_all(&repo).expect("create temp repo");

        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.email", "tests@simgit.local"]);
        run_git(&repo, &["config", "user.name", "simgit-tests"]);

        fs::write(repo.join("hello.txt"), b"hello\n").expect("write file");
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);

        repo
    }

    fn test_config(repo: &std::path::Path) -> Arc<Config> {
        Arc::new(Config {
            repo_path: repo.to_path_buf(),
            state_dir: repo.join("state"),
            mnt_dir: repo.join("state").join("mnt"),
            max_sessions: 8,
            max_delta_bytes: u64::MAX,
            lock_ttl_seconds: 3600,
            session_recovery_ttl_seconds: 86400,
            vfs_backend: VfsBackend::Fuse,
            metrics_enabled: false,
            metrics_addr: "127.0.0.1:0".to_owned(),
            commit_peer_capture_concurrency: 4,
            commit_wait_secs: 30,
        })
    }

    fn new_session_fs(repo: &std::path::Path) -> SessionFs {
        let cfg = test_config(repo);
        let sessions = SessionManager::for_testing();
        let borrows = Arc::new(BorrowRegistry::new(sessions));
        let deltas = Arc::new(DeltaStore::new(repo.join("state").join("deltas")));
        let session_id = Uuid::now_v7();
        deltas
            .init_session(session_id, "HEAD")
            .expect("init_session should succeed");
        SessionFs::new(session_id, false, cfg, "HEAD".to_owned(), deltas, borrows)
    }

    #[test]
    fn lookup_getattr_and_read_serve_baseline_git_file() {
        let repo = init_repo_with_file();
        let fs = new_session_fs(&repo);

        let id = SessionVfsOps::lookup(&fs, 1, "hello.txt").expect("lookup should succeed");
        let attr = SessionVfsOps::getattr(&fs, id).expect("getattr should succeed");
        assert_eq!(attr.kind, VfsFileKind::File);
        assert_eq!(attr.size, 6);

        let data = SessionVfsOps::read(&fs, id, 0, 100).expect("read should succeed");
        assert_eq!(data, b"hello\n");

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn lookup_missing_file_returns_not_found() {
        let repo = init_repo_with_file();
        let fs = new_session_fs(&repo);

        let err = SessionVfsOps::lookup(&fs, 1, "missing.txt").expect_err("should not exist");
        assert!(matches!(err, VfsOpError::NotFound));

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_write_and_read_roundtrip_through_delta_store() {
        let repo = init_repo_with_file();
        let fs = new_session_fs(&repo);

        let id = SessionVfsOps::create(&fs, 1, "new.txt", 0o100644).expect("create should succeed");
        let written = SessionVfsOps::write(&fs, id, 0, b"hi there").expect("write should succeed");
        assert_eq!(written, 8);

        let data = SessionVfsOps::read(&fs, id, 0, 100).expect("read should succeed");
        assert_eq!(data, b"hi there");

        let attr = SessionVfsOps::getattr(&fs, id).expect("getattr should succeed");
        assert_eq!(attr.size, 8);

        // Newly created file should now be visible via lookup + readdir too.
        // Note: `lookup` allocates a fresh id per call (matching the existing
        // FUSE lookup/readdir behavior of not memoizing ids by path), so we
        // only assert that the *content* is reachable through the new id.
        let looked_up_id =
            SessionVfsOps::lookup(&fs, 1, "new.txt").expect("lookup should find delta file");
        let looked_up_data =
            SessionVfsOps::read(&fs, looked_up_id, 0, 100).expect("read via lookup should succeed");
        assert_eq!(looked_up_data, b"hi there");

        let entries = SessionVfsOps::readdir(&fs, 1).expect("readdir should succeed");
        assert!(entries.iter().any(|e| e.name == "new.txt"));

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_existing_name_fails_with_already_exists() {
        let repo = init_repo_with_file();
        let fs = new_session_fs(&repo);

        let err = SessionVfsOps::create(&fs, 1, "hello.txt", 0o100644)
            .expect_err("creating an existing git-tracked path should fail");
        assert!(matches!(err, VfsOpError::AlreadyExists));

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn unlink_marks_file_deleted() {
        let repo = init_repo_with_file();
        let fs = new_session_fs(&repo);

        SessionVfsOps::unlink(&fs, 1, "hello.txt").expect("unlink should succeed");

        let err = SessionVfsOps::lookup(&fs, 1, "hello.txt")
            .expect_err("deleted file should no longer resolve");
        assert!(matches!(err, VfsOpError::NotFound));

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn rename_moves_delta_content_and_clears_old_name() {
        let repo = init_repo_with_file();
        let fs = new_session_fs(&repo);

        let id = SessionVfsOps::create(&fs, 1, "new.txt", 0o100644).expect("create should succeed");
        SessionVfsOps::write(&fs, id, 0, b"payload").expect("write should succeed");

        SessionVfsOps::rename(&fs, 1, "new.txt", 1, "renamed.txt").expect("rename should succeed");

        let err = SessionVfsOps::lookup(&fs, 1, "new.txt")
            .expect_err("old name should no longer resolve");
        assert!(matches!(err, VfsOpError::NotFound));

        let renamed_id =
            SessionVfsOps::lookup(&fs, 1, "renamed.txt").expect("new name should resolve");
        let data = SessionVfsOps::read(&fs, renamed_id, 0, 100).expect("read should succeed");
        assert_eq!(data, b"payload");

        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn write_conflicts_with_another_sessions_write_lock() {
        let repo = init_repo_with_file();
        let fs = new_session_fs(&repo);

        let other_session = Uuid::now_v7();
        fs.borrows
            .acquire_write(other_session, &PathBuf::from("hello.txt"), None)
            .expect("other session should acquire the lock first");

        let id = SessionVfsOps::lookup(&fs, 1, "hello.txt").expect("lookup should succeed");
        let err = SessionVfsOps::write(&fs, id, 0, b"conflict")
            .expect_err("write should conflict with the other session's lock");
        assert!(matches!(err, VfsOpError::Busy(_)));

        let _ = fs::remove_dir_all(&repo);
    }
}
