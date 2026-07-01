//! Free functions for resolving git tree/inode relationships and building FUSE attrs.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use fuser::{FileAttr, FileType, INodeNo};
use uuid::Uuid;

use crate::delta::DeltaStore;
use crate::vfs::git_resolver::{self, InodeMap, TreeCache, TreeEntry};

pub(super) fn resolve_tree_oid(repo_path: &std::path::Path, commitish: &str) -> Result<String> {
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

pub(super) enum DirTreeError {
    NotFound,
    NotDir,
    Io,
}

pub(super) fn directory_tree_oid_for_ino(
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
    let entries = tree_cache
        .get(&parent_tree_oid, repo_path)
        .map_err(|_| DirTreeError::Io)?;
    let Some(entry) = entries.get(entry_idx) else {
        return Err(DirTreeError::NotFound);
    };

    if entry.kind != git_resolver::EntryKind::Dir {
        return Err(DirTreeError::NotDir);
    }

    Ok(entry.oid.clone())
}

pub(super) fn lookup_entry_for_ino(
    ino: u64,
    repo_path: &std::path::Path,
    tree_cache: &TreeCache,
    inode_map: &InodeMap,
) -> Option<TreeEntry> {
    let (tree_oid, entry_idx) = inode_map.lookup(ino)?;
    let entries = tree_cache.get(&tree_oid, repo_path).ok()?;
    entries.get(entry_idx).cloned()
}

pub(super) fn full_child_path(
    parent_ino: u64,
    name: &str,
    inode_map: &InodeMap,
) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if parent_ino == 1 {
        return Some(PathBuf::from(name));
    }
    let parent_path = inode_map.path_of(parent_ino)?;
    Some(parent_path.join(name))
}

pub(super) fn delta_path_deleted(
    deltas: &DeltaStore,
    session_id: Uuid,
    path: &std::path::Path,
) -> Result<bool> {
    let manifest = deltas.load_manifest(session_id)?;
    Ok(manifest.deletes.contains(path))
}

pub(super) fn path_starts_with_dir(path: &std::path::Path, dir: &std::path::Path) -> bool {
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

pub(super) fn git_entry_exists_in_parent(
    repo_path: &std::path::Path,
    tree_cache: &TreeCache,
    base_commit: &str,
    inode_map: &InodeMap,
    parent_ino: u64,
    name: &str,
) -> std::result::Result<bool, DirTreeError> {
    let parent_tree_oid =
        directory_tree_oid_for_ino(parent_ino, base_commit, repo_path, tree_cache, inode_map)?;
    let entries = tree_cache
        .get(&parent_tree_oid, repo_path)
        .map_err(|_| DirTreeError::Io)?;
    Ok(entries.iter().any(|e| e.name == name))
}

pub(super) fn entry_kind_to_file_type(kind: git_resolver::EntryKind) -> FileType {
    match kind {
        git_resolver::EntryKind::File => FileType::RegularFile,
        git_resolver::EntryKind::Dir => FileType::Directory,
        git_resolver::EntryKind::Symlink => FileType::Symlink,
    }
}

pub(super) fn entry_attr(ino: INodeNo, entry: &TreeEntry) -> FileAttr {
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
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
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
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
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
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
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
    use super::{full_child_path, parse_virtual_peer_path, path_starts_with_dir, InodeMap};
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn full_child_path_from_root() {
        let map = InodeMap::new();
        let p = full_child_path(1, "a.txt", &map).expect("root child path");
        assert_eq!(p, PathBuf::from("a.txt"));
    }

    #[test]
    fn path_starts_with_dir_only_direct_children() {
        assert!(path_starts_with_dir(
            std::path::Path::new("a.txt"),
            std::path::Path::new("")
        ));
        assert!(!path_starts_with_dir(
            std::path::Path::new("src/main.rs"),
            std::path::Path::new("")
        ));
        assert!(path_starts_with_dir(
            std::path::Path::new("src/main.rs"),
            std::path::Path::new("src")
        ));
        assert!(!path_starts_with_dir(
            std::path::Path::new("src/nested/main.rs"),
            std::path::Path::new("src")
        ));
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
