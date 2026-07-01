//! `fuser::Filesystem` trait implementation for [`SessionFs`].
//!
//! This is a thin protocol adapter: each handler unpacks the `fuser`
//! request, delegates the actual borrow-checking/CoW/git-tree logic to
//! [`SessionVfsOps`] (implemented by [`SessionFs`] in `session_fs.rs`), and
//! packs the result back into the appropriate `fuser::Reply*`, translating
//! [`VfsOpError`] into the same `Errno` values this handler has always
//! returned.
//!
//! The one exception is the `.simgit/peers/<uuid>/...` virtual peer-diff
//! subtree: that's FUSE-specific UX with no borrow/delta/git-tree logic of
//! its own, so it's still handled directly here (and in `SessionFs`'s
//! `ensure_virtual_ino`/`peer_children`/`peer_file_bytes` helpers) rather
//! than through `SessionVfsOps`.

use std::ffi::OsStr;

use fuser::{
    Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags,
    RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyWrite, Request, WriteFlags,
};
use tracing::debug;
use uuid::Uuid;

use crate::vfs::session_ops::{file_slice, SessionVfsOps, VfsOpError};

use super::session_fs::{SessionFs, SIMGIT_META_DIR, SIMGIT_PEERS_DIR, TTL};
use super::tree::{
    entry_attr, parse_virtual_peer_path, root_attr, vfs_kind_to_file_type, virtual_dir_attr,
    virtual_file_attr,
};

fn to_errno(err: VfsOpError) -> Errno {
    match err {
        VfsOpError::NotFound => Errno::ENOENT,
        VfsOpError::NotADirectory => Errno::ENOTDIR,
        VfsOpError::IsADirectory => Errno::EISDIR,
        VfsOpError::AlreadyExists => Errno::EEXIST,
        VfsOpError::InvalidArgument => Errno::EINVAL,
        VfsOpError::Busy(_) => Errno::EBUSY,
        VfsOpError::Io => Errno::EIO,
    }
}

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

        match SessionVfsOps::lookup(self, parent.0, name_str) {
            Ok(id) => match SessionVfsOps::getattr(self, id) {
                Ok(attr) => reply.entry(&TTL, &entry_attr(INodeNo(id), &attr), Generation(0)),
                Err(e) => reply.error(to_errno(e)),
            },
            Err(e) => reply.error(to_errno(e)),
        }
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

        match SessionVfsOps::getattr(self, ino.0) {
            Ok(attr) => reply.attr(&TTL, &entry_attr(ino, &attr)),
            Err(e) => reply.error(to_errno(e)),
        }
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

        // Always start with . and ..
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (1u64, FileType::Directory, ".".to_owned()),
            (1u64, FileType::Directory, "..".to_owned()),
        ];
        if ino.0 == 1 {
            let sm_ino = self.ensure_virtual_ino(std::path::Path::new(SIMGIT_META_DIR));
            entries.push((sm_ino, FileType::Directory, SIMGIT_META_DIR.to_owned()));
        }

        match SessionVfsOps::readdir(self, ino.0) {
            Ok(real_entries) => {
                for entry in real_entries {
                    entries.push((entry.id, vfs_kind_to_file_type(entry.kind), entry.name));
                }
            }
            Err(e) => {
                reply.error(to_errno(e));
                return;
            }
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

        match SessionVfsOps::read(self, ino.0, offset, size as u64) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match SessionVfsOps::read_symlink_target(self, ino.0) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(to_errno(e)),
        }
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

        match SessionVfsOps::open(self, ino.0) {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.virtual_path_of(ino.0).is_some() {
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }

        match SessionVfsOps::opendir(self, ino.0) {
            Ok(()) => reply.opened(FileHandle(0), FopenFlags::empty()),
            Err(e) => reply.error(to_errno(e)),
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

        match SessionVfsOps::create(self, parent.0, name_str, mode) {
            Ok(id) => match SessionVfsOps::getattr(self, id) {
                Ok(attr) => reply.created(
                    &TTL,
                    &entry_attr(INodeNo(id), &attr),
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::empty(),
                ),
                Err(e) => reply.error(to_errno(e)),
            },
            Err(e) => reply.error(to_errno(e)),
        }
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
        match SessionVfsOps::write(self, ino.0, offset, data) {
            Ok(written) => reply.written(written.min(u32::MAX as u64) as u32),
            Err(e) => reply.error(to_errno(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };

        match SessionVfsOps::unlink(self, parent.0, name_str) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(to_errno(e)),
        }
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

        match SessionVfsOps::rename(self, parent.0, old_name, newparent.0, new_name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(to_errno(e)),
        }
    }
}
