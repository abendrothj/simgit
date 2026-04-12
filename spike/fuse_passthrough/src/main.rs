//! Spike: FUSE passthrough filesystem.
//!
//! Mounts a source directory as a FUSE filesystem.
//! Validates that `fuser` works on this platform.
//!
//! Usage:  fuse_passthrough <source-dir> <mount-point>

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Result};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, Request, INodeNo, Errno, Generation,
};
use tracing::info;

struct PassthroughFs {
    source: PathBuf,
}

impl PassthroughFs {
    fn real_path(&self, name: &OsStr) -> PathBuf {
        self.source.join(name)
    }
}

const TTL: Duration = Duration::from_secs(1);

fn file_attr(meta: &std::fs::Metadata, ino: u64) -> FileAttr {
    use std::os::unix::fs::MetadataExt;
    let kind = if meta.is_dir() { FileType::Directory } else { FileType::RegularFile };
    FileAttr {
        ino:     fuser::INodeNo(ino),
        size:    meta.len(),
        blocks:  meta.blocks(),
        atime:   UNIX_EPOCH + Duration::from_secs(meta.atime().max(0) as u64),
        mtime:   UNIX_EPOCH + Duration::from_secs(meta.mtime().max(0) as u64),
        ctime:   UNIX_EPOCH + Duration::from_secs(meta.ctime().max(0) as u64),
        crtime:  UNIX_EPOCH,
        kind,
        perm:    meta.mode() as u16,
        nlink:   meta.nlink() as u32,
        uid:     meta.uid(),
        gid:     meta.gid(),
        rdev:    meta.rdev() as u32,
        flags:   0,
        blksize: meta.blksize() as u32,
    }
}

impl Filesystem for PassthroughFs {
    fn lookup(&self, _req: &Request, _parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let path = self.real_path(name);
        match std::fs::metadata(&path) {
            Ok(meta) => reply.entry(&TTL, &file_attr(&meta, 2), Generation(0)),
            Err(_)   => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<fuser::FileHandle>, reply: ReplyAttr) {
        if ino.0 == 1 {
            match std::fs::metadata(&self.source) {
                Ok(meta) => reply.attr(&TTL, &file_attr(&meta, 1)),
                Err(_)   => reply.error(Errno::EIO),
            }
            return;
        }
        reply.error(Errno::ENOENT);
    }

    fn readdir(
        &self, _req: &Request, ino: INodeNo, _fh: fuser::FileHandle, offset: u64, mut reply: ReplyDirectory,
    ) {
        if ino.0 != 1 {
            reply.error(Errno::ENOTDIR);
            return;
        }
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (1, FileType::Directory, ".".to_owned()),
            (1, FileType::Directory, "..".to_owned()),
        ];
        if let Ok(dir) = std::fs::read_dir(&self.source) {
            for (i, entry) in dir.filter_map(|e| e.ok()).enumerate() {
                let kind = if entry.path().is_dir() {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                entries.push(((i + 2) as u64, kind, entry.file_name().to_string_lossy().into_owned()));
            }
        }
        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*ino), (i + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &self, _req: &Request, _ino: INodeNo, _fh: fuser::FileHandle, _offset: u64, _size: u32,
        _flags: fuser::OpenFlags, _lock: Option<fuser::LockOwner>, reply: ReplyData,
    ) {
        reply.error(Errno::EIO);
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        bail!("Usage: fuse_passthrough <source-dir> <mount-point>");
    }

    let source = PathBuf::from(&args[1]).canonicalize()?;
    let mount  = PathBuf::from(&args[2]);
    std::fs::create_dir_all(&mount)?;

    info!(source = %source.display(), mount = %mount.display(), "Mounting passthrough FS");

    let fs = PassthroughFs { source };
    let options = vec![
        MountOption::RO,
        MountOption::FSName("passthrough".into()),
    ];
    let mut config = fuser::Config::default();
    config.mount_options = options;
    fuser::mount2(fs, &mount, &config)?;
    Ok(())
}
