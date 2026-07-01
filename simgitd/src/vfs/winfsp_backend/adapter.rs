//! WinFSP callback adapter — translates WinFSP requests into `SessionVfsOps` calls.
//!
//! This is the WinFSP equivalent of `fuse_backend/filesystem_impl.rs`.  Each
//! WinFSP callback unpacks the request, delegates to `SessionVfsOps` (which
//! handles borrow-checking, delta storage, and git-tree resolution), and packs
//! the result back into WinFSP's wire types.

use std::sync::Arc;

use winfsp_wrs::{
    file_info::FileInfo,
    types::{
        FileAttributes, FileAccessRights, OpenFileInfo, OpenResult,
        DirInfo, DirBuffer,
    },
    FileSystemContext, FileSystemInfo,
};

use crate::vfs::fuse_backend::SessionFs;
use crate::vfs::session_ops::{SessionVfsOps, VfsOpError, VfsFileKind};

// ── File context ──────────────────────────────────────────────────────────

/// Per-open-handle context.  For files this stores the VFS file ID; for
/// directories it stores the parent inode and the child name.
#[derive(Debug, Clone)]
enum WinFspContext {
    /// A regular file or directory, identified by its VFS file ID.
    Node { id: u64, is_dir: bool },
    /// Root directory (special-cased for WinFSP).
    Root,
}

impl WinFspContext {
    fn file_id(&self) -> u64 {
        match self {
            WinFspContext::Node { id, .. } => *id,
            WinFspContext::Root => 1,
        }
    }
}

// ── Session helpers ───────────────────────────────────────────────────────

/// Convert a `VfsOpError` into a WinFSP NTSTATUS error code.
fn to_ntstatus(err: VfsOpError) -> u32 {
    // Standard NTSTATUS codes used by WinFSP.
    const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
    const STATUS_OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;
    const STATUS_NOT_A_DIRECTORY: u32 = 0xC000_0103;
    const STATUS_FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
    const STATUS_OBJECT_NAME_COLLISION: u32 = 0xC000_0035;
    const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
    const STATUS_RESOURCE_IN_USE: u32 = 0xC000_0056; // EBUSY equivalent
    const STATUS_IO_DEVICE_ERROR: u32 = 0xC000_0185;

    match err {
        VfsOpError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        VfsOpError::NotADirectory => STATUS_NOT_A_DIRECTORY,
        VfsOpError::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        VfsOpError::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        VfsOpError::InvalidArgument => STATUS_INVALID_PARAMETER,
        VfsOpError::Busy(_) => STATUS_RESOURCE_IN_USE,
        VfsOpError::Io => STATUS_IO_DEVICE_ERROR,
    }
}

/// Convert a `VfsFileKind` to WinFSP `FileAttributes`.
fn kind_to_attributes(kind: VfsFileKind) -> FileAttributes {
    match kind {
        VfsFileKind::File => FileAttributes::FILE_ATTRIBUTE_ARCHIVE,
        VfsFileKind::Dir => FileAttributes::FILE_ATTRIBUTE_DIRECTORY,
        VfsFileKind::Symlink => {
            FileAttributes::FILE_ATTRIBUTE_REPARSE_POINT
                | FileAttributes::FILE_ATTRIBUTE_ARCHIVE
        }
    }
}

/// Build a `FileInfo` from a VFS attr and file ID.
fn build_file_info(id: u64, attr: &crate::vfs::session_ops::VfsAttr) -> FileInfo {
    let mut info = FileInfo::default();
    info.file_attributes = kind_to_attributes(attr.kind);
    info.file_size = attr.size;
    info.allocation_size = attr.size;
    info.creation_time = 0; // Use Windows epoch default
    info.last_access_time = 0;
    info.last_write_time = 0;
    info.change_time = 0;
    info.index_number = id;
    info
}

// ── WinFSP session ────────────────────────────────────────────────────────

/// Implements `FileSystemInfo` and `FileSystemInterface` from `winfsp_wrs`,
/// delegating all operations to the shared `SessionFs` (via `SessionVfsOps`).
pub struct WinFspSession {
    fs: Arc<SessionFs>,
}

impl WinFspSession {
    pub fn new(fs: SessionFs) -> Self {
        Self { fs: Arc::new(fs) }
    }

    /// Mount the filesystem and run the WinFSP dispatch loop.
    ///
    /// Returns a `FileSystemHost` handle that must be kept alive for the
    /// duration of the mount.
    pub fn spawn_mount(&self, mount_point: &str) -> anyhow::Result<winfsp_wrs::host::FileSystemHost> {
        let host = winfsp_wrs::host::FileSystemHost::new(
            self.clone(),
            mount_point,
        )
        .map_err(|e| anyhow::anyhow!("WinFSP mount failed: {e}"))?;

        host.start().map_err(|e| anyhow::anyhow!("WinFSP start failed: {e}"))?;
        Ok(host)
    }

    fn call_ops<F, T>(&self, f: F) -> Result<T, u32>
    where
        F: FnOnce(&SessionFs) -> Result<T, VfsOpError>,
    {
        f(&self.fs).map_err(to_ntstatus)
    }
}

impl Clone for WinFspSession {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
        }
    }
}

// ── FileSystemInfo ────────────────────────────────────────────────────────

impl FileSystemInfo for WinFspSession {
    fn get_volume_info(&self) -> winfsp_wrs::types::VolumeInfo {
        let mut info = winfsp_wrs::types::VolumeInfo::default();
        info.total_size = 1024 * 1024 * 1024 * 1024; // 1 TiB virtual
        info.free_size = info.total_size;
        info.volume_label = format!("simgit-{}", self.fs.session_id);
        info
    }

    fn get_volume_size_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(5)
    }
}

// ── FileSystemInterface ───────────────────────────────────────────────────

impl FileSystemContext for WinFspSession {
    type FileContext = WinFspContext;

    // Enable the operations we implement.
    const CAN_DELETE_DEFINED: bool = true;
    const CLEANUP_DEFINED: bool = true;

    // ── Open / Close ────────────────────────────────────────────────────

    fn get_security_by_name(
        &self,
        file_name: &std::ffi::OsStr,
    ) -> Result<(OpenFileInfo, Option<WinFspContext>), u32> {
        let name = file_name.to_string_lossy();

        // Handle root.
        if name.is_empty() || name == "\\" || name == "/" {
            return Ok((
                OpenFileInfo::new(
                    FileAttributes::FILE_ATTRIBUTE_DIRECTORY,
                    1, // root inode
                ),
                Some(WinFspContext::Root),
            ));
        }

        // Strip leading backslash.
        let clean = name.trim_start_matches('\\');
        if clean.is_empty() {
            return Ok((
                OpenFileInfo::new(
                    FileAttributes::FILE_ATTRIBUTE_DIRECTORY,
                    1,
                ),
                Some(WinFspContext::Root),
            ));
        }

        // Split into parent path and filename.
        let path = std::path::Path::new(clean);
        let (parent_ino, child_name) = if let Some(parent) = path.parent() {
            if parent.as_os_str().is_empty() {
                // Direct child of root.
                let child = path.file_name().unwrap().to_string_lossy().to_string();
                (1u64, child)
            } else {
                // Nested — resolve parent via lookup chain.
                let parent_str = parent.to_string_lossy().to_string();
                match self.call_ops(|fs| fs.lookup(1, &parent_str)) {
                    Ok(parent_id) => {
                        let child = path.file_name().unwrap().to_string_lossy().to_string();
                        (parent_id, child)
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            let child = clean.to_string();
            (1u64, child)
        };

        // Look up the child.
        let child_id = self.call_ops(|fs| fs.lookup(parent_ino, &child_name))?;
        let attr = self.call_ops(|fs| fs.getattr(child_id))?;

        let is_dir = matches!(attr.kind, VfsFileKind::Dir);

        Ok((
            OpenFileInfo::new(
                kind_to_attributes(attr.kind),
                child_id,
            ),
            Some(WinFspContext::Node { id: child_id, is_dir }),
        ))
    }

    fn open(
        &self,
        _file_name: &std::ffi::OsStr,
        _context: &WinFspContext,
        _access_rights: FileAccessRights,
        _open_result: OpenResult,
    ) -> Result<(), u32> {
        // Open is a no-op for simgit; per-write borrow-checking happens
        // in the write() callback.  We validate existence via
        // get_security_by_name already.
        Ok(())
    }

    fn close(&self, _context: &WinFspContext) {
        // No per-handle state to clean up.
    }

    // ── Read / Write ─────────────────────────────────────────────────────

    fn read(
        &self,
        _file_name: &std::ffi::OsStr,
        context: &WinFspContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<u32, u32> {
        let id = context.file_id();
        let data = self.call_ops(|fs| fs.read(id, offset, buffer.len() as u64))?;
        let len = data.len().min(buffer.len());
        buffer[..len].copy_from_slice(&data[..len]);
        Ok(len as u32)
    }

    fn write(
        &self,
        _file_name: &std::ffi::OsStr,
        context: &WinFspContext,
        buffer: &[u8],
        offset: u64,
    ) -> Result<u32, u32> {
        let id = context.file_id();
        let written = self.call_ops(|fs| fs.write(id, offset, buffer))?;
        Ok(written as u32)
    }

    // ── File attributes ──────────────────────────────────────────────────

    fn get_file_info(
        &self,
        context: &WinFspContext,
    ) -> Result<FileInfo, u32> {
        let id = context.file_id();
        let attr = self.call_ops(|fs| fs.getattr(id))?;
        Ok(build_file_info(id, &attr))
    }

    fn set_basic_info(
        &self,
        _file_name: &std::ffi::OsStr,
        _context: &WinFspContext,
        _file_attributes: FileAttributes,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _change_time: u64,
    ) -> Result<FileInfo, u32> {
        // simgit does not track Windows file attributes or timestamps.
        // Return the current file info unchanged.
        self.get_file_info(_context)
    }

    fn set_file_size(
        &self,
        _file_name: &std::ffi::OsStr,
        context: &WinFspContext,
        new_size: u64,
    ) -> Result<(), u32> {
        let id = context.file_id();

        // Truncate: write zero bytes at `new_size` which resizes the delta
        // buffer to exactly `new_size` bytes.
        if new_size == 0 {
            // Write zero-length at offset 0 to truncate.
            self.call_ops(|fs| fs.write(id, 0, &[]))?;
        } else {
            // For non-zero truncation, we write zero at the truncation point.
            // The delta store handles this via apply_write_at_offset which
            // resizes the buffer if needed.
            self.call_ops(|fs| fs.write(id, new_size, &[]))?;
        }

        Ok(())
    }

    // ── Directory listing ────────────────────────────────────────────────

    fn read_directory(
        &self,
        _file_name: &std::ffi::OsStr,
        context: &WinFspContext,
        _pattern: &str,
        _marker: &Option<String>,
        buffer: &mut DirBuffer,
    ) -> Result<Option<String>, u32> {
        let id = context.file_id();
        let entries = self.call_ops(|fs| fs.readdir(id))?;

        for entry in entries {
            let file_attr = if matches!(entry.kind, VfsFileKind::Dir) {
                FileAttributes::FILE_ATTRIBUTE_DIRECTORY
            } else {
                FileAttributes::FILE_ATTRIBUTE_ARCHIVE
            };

            let entry_info = DirInfo::new(
                file_attr,
                entry.id,
                entry.name,
            );
            buffer.push(&entry_info);
        }

        Ok(None) // No pagination — return all entries at once.
    }

    // ── Create ───────────────────────────────────────────────────────────

    fn create(
        &self,
        file_name: &std::ffi::OsStr,
        _security_descriptor: Option<&[u8]>,
        _create_options: u32,
        _granted_access: FileAccessRights,
        _file_attributes: FileAttributes,
        _allocation_size: u64,
    ) -> Result<(OpenFileInfo, Option<WinFspContext>), u32> {
        let name = file_name.to_string_lossy();
        let clean = name.trim_start_matches('\\');
        let path = std::path::Path::new(clean);

        let (parent_ino, child_name) = if let Some(parent) = path.parent() {
            if parent.as_os_str().is_empty() {
                let child = path.file_name().unwrap().to_string_lossy().to_string();
                (1u64, child)
            } else {
                let parent_str = parent.to_string_lossy().to_string();
                let parent_id = self.call_ops(|fs| fs.lookup(1, &parent_str))?;
                let child = path.file_name().unwrap().to_string_lossy().to_string();
                (parent_id, child)
            }
        } else {
            let child = clean.to_string();
            (1u64, child)
        };

        let child_id = self.call_ops(|fs| fs.create(parent_ino, &child_name, 0o644))?;

        Ok((
            OpenFileInfo::new(FileAttributes::FILE_ATTRIBUTE_ARCHIVE, child_id),
            Some(WinFspContext::Node { id: child_id, is_dir: false }),
        ))
    }

    // ── Delete / Rename ──────────────────────────────────────────────────

    fn can_delete(
        &self,
        _file_name: &std::ffi::OsStr,
        _context: &WinFspContext,
    ) -> Result<(), u32> {
        // simgit allows deletion of any file in the session's delta;
        // borrow-checking prevents concurrent writers from corrupting state.
        Ok(())
    }

    fn cleanup(
        &self,
        _file_name: &std::ffi::OsStr,
        _context: &WinFspContext,
        _flags: u32,
    ) {
        // Per-handle cleanup.  simgit doesn't need per-handle state.
    }

    fn rename(
        &self,
        _file_name: &std::ffi::OsStr,
        _context: &WinFspContext,
        new_file_name: &std::ffi::OsStr,
    ) -> Result<(), u32> {
        // WinFSP provides rename as from-current-file to new-path.
        // We delegate to SessionVfsOps::rename which handles the delta manifest.
        let old_name = _file_name.to_string_lossy();
        let old_clean = old_name.trim_start_matches('\\');
        let old_path = std::path::Path::new(old_clean);

        let new_name = new_file_name.to_string_lossy();
        let new_clean = new_name.trim_start_matches('\\');
        let new_path = std::path::Path::new(new_clean);

        let (from_parent, from_name) = if let Some(parent) = old_path.parent() {
            if parent.as_os_str().is_empty() {
                let child = old_path.file_name().unwrap().to_string_lossy().to_string();
                (1u64, child)
            } else {
                let parent_str = parent.to_string_lossy().to_string();
                let parent_id = self.call_ops(|fs| fs.lookup(1, &parent_str))?;
                let child = old_path.file_name().unwrap().to_string_lossy().to_string();
                (parent_id, child)
            }
        } else {
            let child = old_clean.to_string();
            (1u64, child)
        };

        let (to_parent, to_name) = if let Some(parent) = new_path.parent() {
            if parent.as_os_str().is_empty() {
                let child = new_path.file_name().unwrap().to_string_lossy().to_string();
                (1u64, child)
            } else {
                let parent_str = parent.to_string_lossy().to_string();
                let parent_id = self.call_ops(|fs| fs.lookup(1, &parent_str))?;
                let child = new_path.file_name().unwrap().to_string_lossy().to_string();
                (parent_id, child)
            }
        } else {
            let child = new_clean.to_string();
            (1u64, child)
        };

        self.call_ops(|fs| fs.rename(from_parent, &from_name, to_parent, &to_name))
    }
}
