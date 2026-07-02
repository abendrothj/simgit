//! Embedded NFSv3-server backend for macOS (Phase 1+).
//!
//! # Overview
//!
//! Replaces the Phase 0 plain-directory stub with a real, kext-less VFS mount.
//! An embedded NFSv3 server (via the [`nfsserve`] crate) binds on 127.0.0.1,
//! exposes a per-session git-tree + delta-Cow overlay, and is mounted by the
//! system's built-in NFS client (`mount_nfs`).  This gives macOS agents the
//! same write-time borrow-checking (`BorrowRegistry`) and CoW delta semantics
//! (`DeltaStore`) that Linux gets via FUSE — with zero user install friction.
//!
//! # Architecture
//!
//! ```text
//! Agent writes  →  macOS NFS client  →  NFSv3 RPC over TCP loopback
//!                                      →  NfsSession (implements NFSFileSystem)
//!                                      →  SessionVfsOps::write()
//!                                      →  BorrowRegistry + DeltaStore
//! ```
//!
//! # Cache consistency
//!
//! The mount uses `actimeo=0` (no attribute caching) so that writes from one
//! session are visible immediately.  NFSv3 data caching is disabled by default
//! when the client sees `actimeo=0`.  This is conservative (higher RPC volume)
//! but correct for the borrow-checker promise.
//!
//! # Security
//!
//! The NFS server binds exclusively to `127.0.0.1` (loopback).  NFSv3 has no
//! strong client authentication, so this backend is NOT safe to expose to
//! non-local processes.  On shared build machines, the per-session mount
//! directory should be owned by the calling user (`0700`) — this is already
//! enforced by `VfsManager`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use nfsserve::{
    nfs::{fattr3, fileid3, filename3, ftype3, nfsstat3, nfstime3, sattr3},
    tcp::{NFSTcp, NFSTcpListener},
    vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities},
};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::DeltaStore;
use crate::metrics::Metrics;
use crate::vfs::git_resolver::{BlobCache, EntryKind, InodeMap, TreeCache};
use crate::vfs::session_ops::{SessionVfsOps, VfsAttr, VfsFileKind, VfsOpError};

// ---------------------------------------------------------------------------
// Per-session NFS server
// ---------------------------------------------------------------------------

/// An NFSv3 filesystem for a single simgit session.
///
/// Implements [`NFSFileSystem`] (the `nfsserve` trait) by delegating every
/// operation to the shared [`SessionVfsOps`] trait — the same borrow-checking
/// and CoW logic as the FUSE backend.
pub struct NfsSession {
    session_id: Uuid,
    base_commit: String,
    cfg: Arc<Config>,
    deltas: Arc<DeltaStore>,
    borrows: Arc<BorrowRegistry>,
    tree_cache: Arc<TreeCache>,
    blob_cache: Arc<BlobCache>,
    inode_map: Arc<InodeMap>,
    root_fileid: fileid3,
    git_proxy: Option<crate::git_proxy::GitProxy>,
}

impl NfsSession {
    pub fn new(
        session_id: Uuid,
        cfg: Arc<Config>,
        base_commit: String,
        deltas: Arc<DeltaStore>,
        borrows: Arc<BorrowRegistry>,
        git_proxy: Option<crate::git_proxy::GitProxy>,
    ) -> Self {
        Self {
            session_id,
            base_commit,
            cfg,
            deltas,
            borrows,
            tree_cache: Arc::new(TreeCache::new(100)),
            blob_cache: Arc::new(BlobCache::new(50, 10 * 1024 * 1024)),
            inode_map: Arc::new(InodeMap::new()),
            root_fileid: 1,
            git_proxy,
        }
    }

    fn to_nfsstat(e: &VfsOpError) -> nfsstat3 {
        match e {
            VfsOpError::NotFound => nfsstat3::NFS3ERR_NOENT,
            VfsOpError::NotADirectory => nfsstat3::NFS3ERR_NOTDIR,
            VfsOpError::IsADirectory => nfsstat3::NFS3ERR_ISDIR,
            VfsOpError::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
            VfsOpError::InvalidArgument => nfsstat3::NFS3ERR_INVAL,
            VfsOpError::Busy(_) => nfsstat3::NFS3ERR_IO,
            VfsOpError::Io => nfsstat3::NFS3ERR_IO,
        }
    }

    fn kind_to_ftype(kind: VfsFileKind) -> ftype3 {
        match kind {
            VfsFileKind::File => ftype3::NF3REG,
            VfsFileKind::Dir => ftype3::NF3DIR,
            VfsFileKind::Symlink => ftype3::NF3LNK,
        }
    }

    fn attr_to_fattr(id: u64, attr: &VfsAttr) -> fattr3 {
        fattr3 {
            ftype: Self::kind_to_ftype(attr.kind),
            mode: attr.perm as u32,
            nlink: attr.nlink,
            uid: attr.uid,
            gid: attr.gid,
            size: attr.size,
            used: attr.size,
            rdev: Default::default(),
            fsid: 0,
            fileid: id,
            atime: nfstime3::default(),
            mtime: nfstime3::default(),
            ctime: nfstime3::default(),
        }
    }
}

// NOTE: The SessionVfsOps impl for NfsSession is the EXACT same code as in
// fuse_backend/session_fs.rs's impl SessionVfsOps for SessionFs, just swapped
// into this struct's fields.  If Copy-on-Write exists: they share nothing but
// the trait contract.  If we later find a way to share the impl without
// introducing a dependency between fuse_backend and nfs_backend, great.
// For now, this duplication is the deliberate "two independent impls proving
// the trait is correct" pattern.
impl SessionVfsOps for NfsSession {
    fn lookup(&self, parent: u64, name: &str) -> Result<u64, VfsOpError> {
        let parent_tree_oid = match crate::vfs::session_ops::directory_tree_oid_for_ino(
            parent,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(crate::vfs::session_ops::DirTreeError::NotDir) => {
                return Err(VfsOpError::NotADirectory)
            }
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
            if crate::vfs::session_ops::delta_path_deleted(
                &self.deltas,
                self.session_id,
                &meta.path,
            )
            .unwrap_or(false)
            {
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
        if crate::vfs::session_ops::delta_path_deleted(&self.deltas, self.session_id, &path)
            .unwrap_or(false)
        {
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
            if crate::vfs::session_ops::delta_path_deleted(
                &self.deltas,
                self.session_id,
                &meta.path,
            )
            .unwrap_or(false)
            {
                return Err(VfsOpError::NotFound);
            }
            let data = match self.deltas.read_blob(self.session_id, &meta.path) {
                Ok(Some(b)) => b,
                Ok(None) => return Err(VfsOpError::NotFound),
                Err(_) => return Err(VfsOpError::Io),
            };
            return Ok(
                crate::vfs::session_ops::file_slice(&data, offset as usize, size as usize).to_vec(),
            );
        }

        let Some(entry) = crate::vfs::session_ops::lookup_entry_for_ino(
            id,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) else {
            return Err(VfsOpError::NotFound);
        };

        let Some(path) = self.inode_map.path_of(id) else {
            return Err(VfsOpError::NotFound);
        };
        if crate::vfs::session_ops::delta_path_deleted(&self.deltas, self.session_id, &path)
            .unwrap_or(false)
        {
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

        Ok(crate::vfs::session_ops::file_slice(&data, offset as usize, size as usize).to_vec())
    }

    fn write(&self, id: u64, offset: u64, data: &[u8]) -> Result<u64, VfsOpError> {
        if id == 1 {
            return Err(VfsOpError::IsADirectory);
        }

        let meta = self.inode_map.delta_file_of(id);
        let path = if let Some(ref meta) = meta {
            meta.path.clone()
        } else {
            let Some(entry) = crate::vfs::session_ops::lookup_entry_for_ino(
                id,
                &self.cfg.repo_path,
                &self.tree_cache,
                &self.inode_map,
            ) else {
                return Err(VfsOpError::NotFound);
            };
            if entry.kind == EntryKind::Dir {
                return Err(VfsOpError::IsADirectory);
            }
            self.inode_map.path_of(id).ok_or(VfsOpError::NotFound)?
        };

        self.borrows
            .acquire_write(self.session_id, &path, Some(self.cfg.lock_ttl_seconds))
            .map_err(VfsOpError::from)?;

        let mut current = match self.deltas.read_blob(self.session_id, &path) {
            Ok(Some(delta_bytes)) => delta_bytes,
            Ok(None) => {
                if let Some(entry) = crate::vfs::session_ops::lookup_entry_for_ino(
                    id,
                    &self.cfg.repo_path,
                    &self.tree_cache,
                    &self.inode_map,
                ) {
                    self.blob_cache
                        .get(&entry.oid, &self.cfg.repo_path)
                        .map_err(|_| VfsOpError::Io)?
                } else {
                    Vec::new()
                }
            }
            Err(_) => return Err(VfsOpError::Io),
        };

        crate::vfs::session_ops::apply_write_at_offset(&mut current, offset as usize, data);

        self.deltas
            .write_blob(
                self.session_id,
                &path,
                &current,
                Some(crate::delta::store::ByteRange {
                    offset,
                    len: data.len() as u64,
                }),
            )
            .map_err(|_| VfsOpError::Io)?;


        if meta.is_some() {
            self.inode_map.update_delta_size(id, current.len() as u64);
        }

        Ok(data.len() as u64)
    }

    fn readdir(&self, id: u64) -> Result<Vec<crate::vfs::session_ops::VfsDirEntry>, VfsOpError> {
        use crate::vfs::session_ops::path_starts_with_dir;
        use std::collections::HashSet;

        let tree_oid = match crate::vfs::session_ops::directory_tree_oid_for_ino(
            id,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(oid) => oid,
            Err(crate::vfs::session_ops::DirTreeError::NotDir) => {
                return Err(VfsOpError::NotADirectory)
            }
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
            entries.push(crate::vfs::session_ops::VfsDirEntry {
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
            entries.push(crate::vfs::session_ops::VfsDirEntry {
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
            entries.push(crate::vfs::session_ops::VfsDirEntry {
                name: dir_name.to_owned(),
                id: ino,
                kind: VfsFileKind::Dir,
            });
            seen_names.insert(dir_name.to_owned());
        }

        Ok(entries)
    }

    fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64, VfsOpError> {
        use crate::delta::store::ByteRange;
        use crate::vfs::session_ops::{
            directory_tree_oid_for_ino, full_child_path, git_entry_exists_in_parent, DirTreeError,
        };

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
        use crate::vfs::session_ops::{directory_tree_oid_for_ino, full_child_path, DirTreeError};

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
        use crate::vfs::session_ops::{directory_tree_oid_for_ino, full_child_path, DirTreeError};

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
            if crate::vfs::session_ops::delta_path_deleted(
                &self.deltas,
                self.session_id,
                &meta.path,
            )
            .unwrap_or(false)
            {
                return Err(VfsOpError::NotFound);
            }
            return Ok(());
        }
        let Some(entry) = crate::vfs::session_ops::lookup_entry_for_ino(
            id,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) else {
            return Err(VfsOpError::NotFound);
        };
        if entry.kind == EntryKind::Dir {
            return Err(VfsOpError::IsADirectory);
        }
        let Some(path) = self.inode_map.path_of(id) else {
            return Err(VfsOpError::NotFound);
        };
        if crate::vfs::session_ops::delta_path_deleted(&self.deltas, self.session_id, &path)
            .unwrap_or(false)
        {
            return Err(VfsOpError::NotFound);
        }
        Ok(())
    }

    fn opendir(&self, id: u64) -> Result<(), VfsOpError> {
        match crate::vfs::session_ops::directory_tree_oid_for_ino(
            id,
            &self.base_commit,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) {
            Ok(_) => Ok(()),
            Err(crate::vfs::session_ops::DirTreeError::NotDir) => Err(VfsOpError::NotADirectory),
            Err(_) => Err(VfsOpError::Io),
        }
    }

    fn read_symlink_target(&self, id: u64) -> Result<Vec<u8>, VfsOpError> {
        if id == 1 {
            return Err(VfsOpError::InvalidArgument);
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
        let _ = <Self as SessionVfsOps>::lookup(self, parent, ".")?; // Validate parent exists
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
        // Check for children in delta (tree children handled by rmdir default).
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
        // Remove the inode mapping so getattr won't find it.
        self.inode_map.remove_delta_file(dir_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// nfsserve::vfs::NFSFileSystem adapter — thin NFSv3 ↔ SessionVfsOps bridge
// ---------------------------------------------------------------------------

#[async_trait]
impl NFSFileSystem for NfsSession {
    fn root_dir(&self) -> fileid3 {
        self.root_fileid
    }

    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    async fn lookup(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> std::result::Result<fileid3, nfsstat3> {
        let name = String::from_utf8_lossy(filename.as_ref());
        SessionVfsOps::lookup(self, dirid, &name).map_err(|e| Self::to_nfsstat(&e))
    }

    async fn getattr(&self, id: fileid3) -> std::result::Result<fattr3, nfsstat3> {
        SessionVfsOps::getattr(self, id)
            .map(|a| Self::attr_to_fattr(id, &a))
            .map_err(|e| Self::to_nfsstat(&e))
    }

    async fn setattr(
        &self,
        id: fileid3,
        _setattr: sattr3,
    ) -> std::result::Result<fattr3, nfsstat3> {
        // Accept all attribute changes (mode, owner, size, times) silently.
        // Truncation via O_TRUNC is handled by the write path (overwriting
        // files with empty content). Full setattr semantics deferred.
        let attr = SessionVfsOps::getattr(self, id).map_err(|e| Self::to_nfsstat(&e))?;
        Ok(Self::attr_to_fattr(id, &attr))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> std::result::Result<(Vec<u8>, bool), nfsstat3> {
        let data = SessionVfsOps::read(self, id, offset, count as u64)
            .map_err(|e| Self::to_nfsstat(&e))?;
        let eof = data.len() < count as usize;
        Ok((data, eof))
    }

    async fn write(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<fattr3, nfsstat3> {
        SessionVfsOps::write(self, id, offset, data).map_err(|e| Self::to_nfsstat(&e))?;
        // Re-fetch attrs so the NFS client gets the updated size.
        NFSFileSystem::getattr(self, id).await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        let name = String::from_utf8_lossy(filename.as_ref());
        let new_id =
            SessionVfsOps::create(self, dirid, &name, 0o644).map_err(|e| Self::to_nfsstat(&e))?;
        let attr = NFSFileSystem::getattr(self, new_id).await?;
        Ok((new_id, attr))
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> std::result::Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        let name = String::from_utf8_lossy(dirname.as_ref());
        let child = SessionVfsOps::mkdir(self, dirid, &name, 0o755)
            .map_err(|e| Self::to_nfsstat(&e))?;
        let attr = SessionVfsOps::getattr(self, child).map_err(|e| Self::to_nfsstat(&e))?;
        Ok((child, Self::attr_to_fattr(child, &attr)))
    }

    async fn remove(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> std::result::Result<(), nfsstat3> {
        let name = String::from_utf8_lossy(filename.as_ref());
        SessionVfsOps::unlink(self, dirid, &name).map_err(|e| Self::to_nfsstat(&e))
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> std::result::Result<(), nfsstat3> {
        let from_name = String::from_utf8_lossy(from_filename.as_ref());
        let to_name = String::from_utf8_lossy(to_filename.as_ref());
        SessionVfsOps::rename(self, from_dirid, &from_name, to_dirid, &to_name)
            .map_err(|e| Self::to_nfsstat(&e))
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> std::result::Result<ReadDirResult, nfsstat3> {
        let all = SessionVfsOps::readdir(self, dirid).map_err(|e| Self::to_nfsstat(&e))?;

        let start_idx = if start_after == 0 {
            0
        } else {
            all.iter()
                .position(|e| e.id == start_after)
                .map(|p| p + 1)
                .unwrap_or(0)
        };

        let slice = &all[start_idx..];
        let end = slice.len() <= max_entries;
        let entries: Vec<DirEntry> = slice
            .iter()
            .take(max_entries)
            .map(|e| {
                // quick getattr for readdir; NFS client uses it for stat
                let attr = SessionVfsOps::getattr(self, e.id).ok();
                DirEntry {
                    fileid: e.id,
                    name: e.name.as_bytes().into(),
                    attr: match attr {
                        Some(a) => Self::attr_to_fattr(e.id, &a),
                        None => Self::attr_to_fattr(e.id, &VfsAttr::default()),
                    },
                }
            })
            .collect();

        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfsserve::nfs::nfspath3,
        _attr: &sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(
        &self,
        id: fileid3,
    ) -> std::result::Result<nfsserve::nfs::nfspath3, nfsstat3> {
        let data =
            SessionVfsOps::read_symlink_target(self, id).map_err(|e| Self::to_nfsstat(&e))?;
        Ok(nfsserve::nfs::nfspath3::from(data))
    }
}

// ---------------------------------------------------------------------------
// Backend driver — replaces the old Phase 0 stub
// ---------------------------------------------------------------------------

struct ActiveNfsMount {
    _handle: JoinHandle<()>,
    port: u16,
}

/// Embedded NFSv3 backend driver (macOS Phase 1+, also available on Linux).
///
/// On `mount`, this backend:
/// 1. Creates an [`NfsSession`] sharing the git-tree + delta + borrow state.
/// 2. Binds an [`NFSTcpListener`] on a random loopback port.
/// 3. Spawns the server's `handle_forever()` loop on a tokio task.
/// 4. Shells out to `mount_nfs` to attach the macOS NFS client.
///
/// On `unmount`, it shells `umount` and aborts the tokio task.
pub struct NfsLoopbackBackend {
    cfg: Arc<Config>,
    deltas: Arc<DeltaStore>,
    borrows: Arc<BorrowRegistry>,
    metrics: Arc<Metrics>,
    mounts: Mutex<HashMap<Uuid, ActiveNfsMount>>,
}

impl NfsLoopbackBackend {
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
            mounts: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl super::VfsBackendTrait for NfsLoopbackBackend {
    async fn mount(&self, session: &SessionInfo) -> Result<()> {
        let mount_path = session.mount_path.clone();
        std::fs::create_dir_all(&mount_path)
            .with_context(|| format!("create mount dir {}", mount_path.display()))?;

        let fs = NfsSession::new(
            session.session_id,
            Arc::clone(&self.cfg),
            session.base_commit.clone(),
            Arc::clone(&self.deltas),
            Arc::clone(&self.borrows),
            if session.git_proxy_enabled {
                crate::git_proxy::GitProxy::bootstrap(
                    &mount_path,
                    &session.base_commit,
                    &self.cfg.repo_path,
                    session.initial_branch.as_deref(),
                    session.session_id,
                )
                .ok()
            } else {
                None
            },
        );

        // Bind on random loopback port.
        let listener = NFSTcpListener::bind("127.0.0.1:0", fs)
            .await
            .context("bind NFSv3 TCP listener")?;
        let port = listener.get_listen_port();

        // Mount via macOS built-in NFS client BEFORE spawning the server
        // task so a mount_nfs failure doesn't leave an orphaned task.
        let mount_output = tokio::task::spawn_blocking({
            let mp = mount_path.clone();
            move || {
                std::process::Command::new("mount_nfs")
                    .args([
                        "-o",
                        &format!("nolocks,vers=3,tcp,actimeo=0,port={port},mountport={port}"),
                        &format!("localhost:/"),
                        &mp.to_string_lossy().as_ref(),
                    ])
                    .output()
                    .context("mount_nfs")
            }
        })
        .await
        .context("spawn_blocking mount_nfs")??;

        if !mount_output.status.success() {
            let stderr = String::from_utf8_lossy(&mount_output.stderr);
            // listener is dropped here — the bound port is released.
            anyhow::bail!(
                "mount_nfs failed (exit {:?}): {stderr}",
                mount_output.status.code()
            );
        }

        // Spawn the server task after mount_nfs succeeds — no orphaned task
        // on mount failure.
        let handle = tokio::spawn(async move {
            if let Err(e) = listener.handle_forever().await {
                warn!(port, err = %e, "NFSv3 server task exited with error");
            }
        });

        let mut mounts = self.mounts.lock().unwrap();
        mounts.insert(
            session.session_id,
            ActiveNfsMount {
                _handle: handle,
                port,
            },
        );

        info!(
            session = %session.session_id,
            port,
            path = %mount_path.display(),
            "NFSv3 session mounted"
        );
        Ok(())
    }

    async fn unmount(&self, session_id: Uuid) -> Result<()> {
        let mount_path = self.cfg.mnt_dir.join(session_id.to_string());

        // Force-unmount if possible.
        let _ = std::process::Command::new("umount")
            .args([&mount_path.to_string_lossy().to_string()])
            .status();

        // Give macOS NFS client a moment, then force.
        let _ = std::process::Command::new("umount")
            .args(["-f", &mount_path.to_string_lossy().to_string()])
            .status();

        let _ = std::fs::remove_dir(&mount_path);

        if let Ok(mut mounts) = self.mounts.lock() {
            if let Some(mount) = mounts.remove(&session_id) {
                mount._handle.abort();
                info!(session = %session_id, port = mount.port, "NFSv3 session unmounted");
            }
        }

        Ok(())
    }

    fn capture_mount_delta(&self, _session: &SessionInfo) -> Result<()> {
        // No-op: all writes flow through the NFSv3 RPC handler →
        // SessionVfsOps → DeltaStore synchronously, so there's nothing
        // to "capture" at commit time.  This is the same no-op that
        // FuseBackend uses.
        Ok(())
    }
}
