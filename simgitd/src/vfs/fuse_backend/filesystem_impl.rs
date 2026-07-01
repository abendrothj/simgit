//! `fuser::Filesystem` trait implementation for [`SessionFs`].

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::PathBuf;

use fuser::{
    Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, WriteFlags,
};
use tracing::debug;
use uuid::Uuid;

use crate::delta::store::ByteRange;
use crate::vfs::git_resolver;

use super::bytes::{apply_write_at_offset, file_slice};
use super::session_fs::{SessionFs, SIMGIT_META_DIR, SIMGIT_PEERS_DIR, TTL};
use super::tree::{
    delta_path_deleted, directory_tree_oid_for_ino, entry_attr, full_child_path,
    git_entry_exists_in_parent, lookup_entry_for_ino, parse_virtual_peer_path,
    path_starts_with_dir, root_attr, virtual_dir_attr, virtual_file_attr, DirTreeError,
};

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
                self.inode_map
                    .insert(ino, parent_tree_oid.clone(), idx, full_path);

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
            self.inode_map
                .insert_delta_file(ino, full_path, size, 0o100644);
            let entry = git_resolver::TreeEntry {
                name: name_str.to_owned(),
                mode: "100644".to_owned(),
                oid: hash.clone(),
                kind: git_resolver::EntryKind::File,
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
            let entry = git_resolver::TreeEntry {
                name: meta
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                mode: "100644".to_owned(),
                oid: String::new(),
                kind: git_resolver::EntryKind::File,
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

        if entry.kind == git_resolver::EntryKind::File {
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
                            if is_dir {
                                FileType::Directory
                            } else {
                                FileType::RegularFile
                            },
                            name,
                        ));
                    }
                }

                for (i, (entry_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize)
                {
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
                    git_resolver::EntryKind::File => FileType::RegularFile,
                    git_resolver::EntryKind::Dir => FileType::Directory,
                    git_resolver::EntryKind::Symlink => FileType::Symlink,
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
            self.inode_map
                .insert_delta_file(ino, path.clone(), size, 0o100644);
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

        let Some(entry) = lookup_entry_for_ino(
            ino.0,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) else {
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

        if entry.kind == git_resolver::EntryKind::Dir {
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

        let Some(entry) = lookup_entry_for_ino(
            ino.0,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) else {
            reply.error(Errno::ENOENT);
            return;
        };

        if entry.kind != git_resolver::EntryKind::Symlink {
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

        let Some(entry) = lookup_entry_for_ino(
            ino.0,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if entry.kind == git_resolver::EntryKind::Dir {
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

        if self
            .deltas
            .write_blob(self.session_id, &path, &[], None)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }

        let ino = self.inode_map.allocate();
        let perm = ((mode as u16) & 0o7777) | 0o100000;
        self.inode_map.insert_delta_file(ino, path.clone(), 0, perm);
        let entry = git_resolver::TreeEntry {
            name: name_str.to_owned(),
            mode: format!("{:o}", perm),
            oid: String::new(),
            kind: git_resolver::EntryKind::File,
            size: 0,
            perm,
        };
        reply.created(
            &TTL,
            &entry_attr(INodeNo(ino), &entry),
            Generation(0),
            FileHandle(0),
            FopenFlags::empty(),
        );
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

            if self
                .deltas
                .write_blob(
                    self.session_id,
                    &meta.path,
                    &current,
                    Some(ByteRange {
                        offset,
                        len: data.len() as u64,
                    }),
                )
                .is_err()
            {
                reply.error(Errno::EIO);
                return;
            }
            self.inode_map
                .update_delta_size(ino.0, current.len() as u64);
            reply.written(data.len().min(u32::MAX as usize) as u32);
            return;
        }

        let Some(entry) = lookup_entry_for_ino(
            ino.0,
            &self.cfg.repo_path,
            &self.tree_cache,
            &self.inode_map,
        ) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if entry.kind == git_resolver::EntryKind::Dir {
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

        if self
            .deltas
            .write_blob(
                self.session_id,
                &path,
                &current,
                Some(ByteRange {
                    offset,
                    len: data.len() as u64,
                }),
            )
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        self.inode_map
            .update_delta_size(ino.0, current.len() as u64);

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

        let mut exists_as_file =
            manifest.writes.contains_key(&path) && !manifest.deletes.contains(&path);

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
            if entry.kind == git_resolver::EntryKind::Dir {
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
        let old_parent_entries = match self
            .tree_cache
            .get(&old_parent_tree_oid, &self.cfg.repo_path)
        {
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
            if entry.kind == git_resolver::EntryKind::Dir {
                reply.error(Errno::EISDIR);
                return;
            }
        }
        let exists_in_manifest =
            manifest.writes.contains_key(&old_path) && !manifest.deletes.contains(&old_path);
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

        if self
            .deltas
            .write_blob(self.session_id, &new_path, &source_data, None)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        if self
            .deltas
            .mark_deleted(self.session_id, &old_path)
            .is_err()
        {
            reply.error(Errno::EIO);
            return;
        }
        let _ = self
            .deltas
            .record_rename(self.session_id, &old_path, &new_path);
        reply.ok();
    }
}
