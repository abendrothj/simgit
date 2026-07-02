//! Protocol-agnostic VFS operations — shared by FUSE and NFS backends.
//!
//! # Overview
//!
//! This module defines the [`SessionVfsOps`] trait, a single interface through
//! which every backend (FUSE on Linux, embedded NFSv3 on macOS, FSKit on macOS
//! 15.4+) delegates borrow-checking, copy-on-write delta storage, and git-tree
//! resolution.  A thin protocol adapter (e.g. `filesystem_impl.rs` for FUSE,
//! an `impl NFSFileSystem` adapter for NFS) unpacks the wire request, calls
//! the corresponding `SessionVfsOps` method, and packs the result back into
//! the protocol reply — it never touches `BorrowRegistry`, `DeltaStore`, or
//! the git resolver directly.
//!
//! The one exception is the `.simgit/peers/<uuid>/...` virtual peer-diff
//! subtree, which is FUSE-only UX handled directly in `SessionFs` without
//! going through this trait.
//!
//! # Design
//!
//! Everything is keyed by a plain `u64` file ID — deliberately the same
//! primitive used by both FUSE's `INodeNo` and NFSv3's `fileid3`.  This lets
//! us reuse [`git_resolver::InodeMap`] unchanged across backends.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::delta::DeltaStore;
use crate::vfs::git_resolver::{self, EntryKind, InodeMap, TreeCache};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that any VFS backend can translate into its own protocol codes.
#[derive(Debug, Clone)]
pub enum VfsOpError {
    /// No such file or directory.
    NotFound,
    /// Caller passed a file where a directory was required.
    NotADirectory,
    /// Caller passed a directory where a file was required.
    IsADirectory,
    /// Entry already exists (used for `create`/`rename` exclusivity checks).
    AlreadyExists,
    /// Invalid argument (e.g. internal inconsistency, broken invariant).
    InvalidArgument,
    /// Path is currently write-locked by another session (borrow conflict).
    Busy(String),
    /// Unclassified I/O or infrastructure failure.
    Io,
}

impl std::fmt::Display for VfsOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VfsOpError::NotFound => write!(f, "not found"),
            VfsOpError::NotADirectory => write!(f, "not a directory"),
            VfsOpError::IsADirectory => write!(f, "is a directory"),
            VfsOpError::AlreadyExists => write!(f, "already exists"),
            VfsOpError::InvalidArgument => write!(f, "invalid argument"),
            VfsOpError::Busy(ref s) => write!(f, "busy: {s}"),
            VfsOpError::Io => write!(f, "I/O error"),
        }
    }
}

impl std::error::Error for VfsOpError {}

impl From<simgit_sdk::BorrowError> for VfsOpError {
    fn from(e: simgit_sdk::BorrowError) -> Self {
        VfsOpError::Busy(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Shared attribute / entry types
// ---------------------------------------------------------------------------

/// File kind — protocol-agnostic counterpart to fuser `FileType` or NFS `ftype3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfsFileKind {
    File,
    Dir,
    Symlink,
    /// Git submodule (gitlink, mode 160000).  The VFS presents these as
    /// directories; git resolves the linked commit for actual content.
    Submodule,
}

impl From<EntryKind> for VfsFileKind {
    fn from(kind: EntryKind) -> Self {
        match kind {
            EntryKind::File => VfsFileKind::File,
            EntryKind::Dir => VfsFileKind::Dir,
            EntryKind::Symlink => VfsFileKind::Symlink,
            EntryKind::Commit => VfsFileKind::Submodule,
        }
    }
}

/// Protocol-agnostic file attributes, enough to build `fuser::FileAttr` or
/// `nfsserve::nfs::fattr3`.
#[derive(Clone, Debug)]
pub struct VfsAttr {
    pub kind: VfsFileKind,
    pub size: u64,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Default for VfsAttr {
    fn default() -> Self {
        Self {
            kind: VfsFileKind::File,
            size: 0,
            perm: 0o644,
            nlink: 1,
            uid: crate::platform::current_uid(),
            gid: crate::platform::current_gid(),
        }
    }
}

/// One entry returned by `readdir`.
#[derive(Clone, Debug)]
pub struct VfsDirEntry {
    pub name: String,
    pub id: u64,
    pub kind: VfsFileKind,
}

// ---------------------------------------------------------------------------
// Git tree resolution helpers (moved from fuse_backend/tree.rs)
// ---------------------------------------------------------------------------

pub fn resolve_tree_oid(repo_path: &Path, commitish: &str) -> Result<String> {
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

#[derive(Debug)]
pub enum DirTreeError {
    NotFound,
    NotDir,
    Io,
}

impl From<DirTreeError> for VfsOpError {
    fn from(e: DirTreeError) -> Self {
        match e {
            DirTreeError::NotFound => VfsOpError::NotFound,
            DirTreeError::NotDir => VfsOpError::NotADirectory,
            DirTreeError::Io => VfsOpError::Io,
        }
    }
}

pub fn directory_tree_oid_for_ino(
    ino: u64,
    base_commit: &str,
    repo_path: &Path,
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

    if entry.kind != EntryKind::Dir {
        if entry.kind == EntryKind::Commit {
            return Err(DirTreeError::NotDir);
        }
        return Err(DirTreeError::NotDir);
    }

    Ok(entry.oid.clone())
}

pub fn lookup_entry_for_ino(
    ino: u64,
    repo_path: &Path,
    tree_cache: &TreeCache,
    inode_map: &InodeMap,
) -> Option<git_resolver::TreeEntry> {
    let (tree_oid, entry_idx) = inode_map.lookup(ino)?;
    let entries = tree_cache.get(&tree_oid, repo_path).ok()?;
    entries.get(entry_idx).cloned()
}

pub fn full_child_path(parent_ino: u64, name: &str, inode_map: &InodeMap) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if parent_ino == 1 {
        return Some(PathBuf::from(name));
    }
    let parent_path = inode_map.path_of(parent_ino)?;
    Some(parent_path.join(name))
}

pub fn delta_path_deleted(
    deltas: &DeltaStore,
    session_id: uuid::Uuid,
    path: &Path,
) -> Result<bool> {
    let manifest = deltas.load_manifest(session_id)?;
    Ok(manifest.deletes.contains(path))
}

pub fn path_starts_with_dir(path: &Path, dir: &Path) -> bool {
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

pub fn git_entry_exists_in_parent(
    repo_path: &Path,
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

// ---------------------------------------------------------------------------
// Byte-slice helpers (moved from fuse_backend/bytes.rs)
// ---------------------------------------------------------------------------

pub fn file_slice(data: &[u8], offset: usize, size: usize) -> &[u8] {
    if offset >= data.len() {
        return &[];
    }
    let end = offset.saturating_add(size).min(data.len());
    &data[offset..end]
}

pub fn apply_write_at_offset(buf: &mut Vec<u8>, offset: usize, data: &[u8]) {
    if buf.len() < offset {
        buf.resize(offset, 0);
    }
    let end = offset.saturating_add(data.len());
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[offset..end].copy_from_slice(data);
}

// ---------------------------------------------------------------------------
// The trait itself
// ---------------------------------------------------------------------------

/// Protocol-agnostic VFS operations for a single session.
///
/// Implemented by [`SessionFs`] (FUSE) and [`NfsSession`] (embedded NFSv3).
/// All borrow-checking, CoW delta storage, and git-tree resolution lives in
/// this impl. Protocol adapters call these methods and translate the results
/// back into their own wire types.
pub trait SessionVfsOps {
    /// Resolve a name within a parent directory.  Returns the file ID of the
    /// child, or `VfsOpError::NotFound` if no such entry exists.
    fn lookup(&self, parent: u64, name: &str) -> Result<u64, VfsOpError>;

    /// Return file attributes for the given file ID.
    fn getattr(&self, id: u64) -> Result<VfsAttr, VfsOpError>;

    /// Read up to `size` bytes starting at `offset` from the file.  Returns
    /// the bytes read (may be shorter than requested at EOF).
    fn read(&self, id: u64, offset: u64, size: u64) -> Result<Vec<u8>, VfsOpError>;

    /// Write `data` at `offset` in the file (extending if necessary).
    /// Returns the number of bytes written.  Acquires a write lock via
    /// [`BorrowRegistry`] and stores the delta via [`DeltaStore`].
    fn write(&self, id: u64, offset: u64, data: &[u8]) -> Result<u64, VfsOpError>;

    /// List all entries in a directory.
    fn readdir(&self, id: u64) -> Result<Vec<VfsDirEntry>, VfsOpError>;

    /// Create a new regular file in `parent` with the given `name` and access `mode`.
    /// Returns the newly allocated file ID.
    fn create(&self, parent: u64, name: &str, mode: u32) -> Result<u64, VfsOpError>;

    /// Create a symbolic link in `parent` with `name` pointing to `target`.
    /// Returns the newly allocated file ID.
    fn create_symlink(&self, parent: u64, name: &str, target: &str) -> Result<u64, VfsOpError> {
        let _target = target;
        Err(VfsOpError::InvalidArgument)
    }

    /// Create a new directory in `parent` with the given `name` and access `mode`.
    /// Returns the newly allocated file ID.
    fn mkdir(&self, parent: u64, name: &str, _mode: u32) -> Result<u64, VfsOpError> {
        // Default: just verify parent exists.  simgit directories are virtual
        // (existence implies from child files); no delta entry is needed.
        let _parent_id = self.lookup(parent, ".")?;
        // Allocate a synthetic inode that lookup/getattr can resolve for
        // newly-created-empty directories by reserving a range.
        self.allocate_dir_ino(parent, name)
    }

    /// Remove the directory named `name` from `parent` (must be empty).
    fn rmdir(&self, parent: u64, name: &str) -> Result<(), VfsOpError> {
        // Directories in simgit are virtual; removing them just means
        // verifying the directory has no children (in git tree or delta).
        let dir_id = self.lookup(parent, name)?;
        let children = self.readdir(dir_id)?;
        if children.iter().any(|c| c.name != "." && c.name != "..") {
            return Err(VfsOpError::IsADirectory);
        }
        Ok(())
    }

    /// Allocate a synthetic inode for a newly created directory.
    /// Backends that track inodes in maps should override this.
    fn allocate_dir_ino(&self, _parent: u64, _name: &str) -> Result<u64, VfsOpError> {
        Err(VfsOpError::InvalidArgument)
    }

    /// Remove the entry named `name` from `parent`.
    fn unlink(&self, parent: u64, name: &str) -> Result<(), VfsOpError>;

    /// Rename `from_name` in `from_parent` to `to_name` in `to_parent`.
    fn rename(
        &self,
        from_parent: u64,
        from_name: &str,
        to_parent: u64,
        to_name: &str,
    ) -> Result<(), VfsOpError>;

    /// Check that a file is accessible/valid for open (same semantics as FUSE `open`).
    /// Returns an error if the file does not exist or is a directory.
    fn open(&self, id: u64) -> Result<(), VfsOpError>;

    /// Check that a directory is valid for opendir. Returns an error if the
    /// directory does not exist or is not a directory.
    fn opendir(&self, id: u64) -> Result<(), VfsOpError>;

    /// Read symlink target.  Returns `NotFound` if the ID does not refer to a
    /// symlink.
    fn read_symlink_target(&self, id: u64) -> Result<Vec<u8>, VfsOpError>;
}

// ---------------------------------------------------------------------------
// Unit tests for shared helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn path_starts_with_dir_only_direct_children() {
        assert!(path_starts_with_dir(Path::new("a.txt"), Path::new("")));
        assert!(!path_starts_with_dir(
            Path::new("src/main.rs"),
            Path::new("")
        ));
        assert!(path_starts_with_dir(
            Path::new("src/main.rs"),
            Path::new("src")
        ));
        assert!(!path_starts_with_dir(
            Path::new("src/nested/main.rs"),
            Path::new("src")
        ));
    }
}
