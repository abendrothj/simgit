//! FUSE-specific attribute builders and virtual-peer-path parsing.
//!
//! The underlying git-tree/inode resolution helpers these used to contain
//! (`resolve_tree_oid`, `directory_tree_oid_for_ino`, `lookup_entry_for_ino`,
//! etc.) now live in [`crate::vfs::session_ops`], shared by every VFS backend.
//! What remains here is purely about turning protocol-agnostic
//! [`crate::vfs::session_ops::VfsAttr`] values into `fuser::FileAttr`, plus
//! the FUSE-only virtual `.simgit/peers/<uuid>/...` path parsing.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use fuser::{FileAttr, FileType, INodeNo};
use uuid::Uuid;

use crate::vfs::session_ops::{VfsAttr, VfsFileKind};

pub(super) fn vfs_kind_to_file_type(kind: VfsFileKind) -> FileType {
    match kind {
        VfsFileKind::File => FileType::RegularFile,
        VfsFileKind::Dir => FileType::Directory,
        VfsFileKind::Symlink => FileType::Symlink,
        VfsFileKind::Submodule => FileType::Directory,
    }
}

pub(super) fn entry_attr(ino: INodeNo, attr: &VfsAttr) -> FileAttr {
    FileAttr {
        ino,
        size: attr.size,
        blocks: (attr.size + 511) / 512,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: vfs_kind_to_file_type(attr.kind),
        perm: attr.perm,
        nlink: 1,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

pub(super) fn root_attr() -> FileAttr {
    FileAttr {
        ino: INodeNo(1),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: crate::platform::current_uid(),
        gid: crate::platform::current_gid(),
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

pub(super) fn virtual_dir_attr(ino: INodeNo) -> FileAttr {
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
        uid: crate::platform::current_uid(),
        gid: crate::platform::current_gid(),
        rdev: 0,
        flags: 0,
        blksize: 512,
    }
}

pub(super) fn virtual_file_attr(ino: INodeNo, size: u64) -> FileAttr {
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
        uid: crate::platform::current_uid(),
        gid: crate::platform::current_gid(),
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

pub(super) fn parse_virtual_peer_path(path: &std::path::Path) -> Option<(Uuid, PathBuf)> {
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
    use super::parse_virtual_peer_path;
    use std::path::PathBuf;
    use uuid::Uuid;

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
