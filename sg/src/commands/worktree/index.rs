//! Adopt a cloned worktree's stat data into a copied Git index.
//!
//! A CoW clone carries the baseline's content, mode, size and mtime, but has
//! its own inode and ctime — precisely the fields Git compares before it
//! trusts an index entry. So a copied baseline index is worthless as-is: Git
//! rehashes every file on the first `git status` (3.0 s for 18.7k entries).
//!
//! Re-recording those fields from the clone produces a byte-for-byte match
//! with `git update-index --really-refresh` in 0.16 s instead of 3.34 s,
//! because no file content is read. Git's normal, strict staleness checks
//! keep working afterwards; nothing is relaxed.

use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const HEADER_LEN: usize = 12;
const TRAILER_LEN: usize = 20;
/// Offset of the object id inside a cache entry: 10 stat fields of 4 bytes.
const STAT_LEN: usize = 40;
const OID_LEN: usize = 20;
/// Offset of the 16-bit flags field: stat data plus object id.
const FLAGS_OFFSET: usize = STAT_LEN + OID_LEN;
const EXTENDED_FLAG: u16 = 0x4000;
const NAME_MASK: u16 = 0x0fff;
/// A name length of 0xfff means "longer than the field can hold; scan to NUL".
const NAME_OVERFLOW: u16 = 0x0fff;

/// Rewrite every entry's stat data in `index` to describe the files in
/// `worktree`. Entries whose size no longer matches, or whose file is missing,
/// are left stale so Git still inspects them.
pub(super) fn adopt_stat_data(index: &Path, worktree: &Path) -> Result<()> {
    let mut buffer = fs::read(index).with_context(|| format!("read {}", index.display()))?;
    let entries = parse_header(&buffer)?;

    let mut offset = HEADER_LEN;
    for _ in 0..entries {
        offset = adopt_entry(&mut buffer, offset, worktree)?;
    }

    let digest = Sha1::digest(&buffer[..buffer.len() - TRAILER_LEN]);
    let trailer = buffer.len() - TRAILER_LEN;
    buffer[trailer..].copy_from_slice(&digest);

    let staging = index.with_extension("simgit-adopt");
    fs::write(&staging, &buffer).with_context(|| format!("write {}", staging.display()))?;
    fs::rename(&staging, index).with_context(|| format!("install {}", index.display()))?;
    Ok(())
}

/// Validate the index well enough to walk it, returning the entry count.
///
/// The trailer check doubles as a format guard: a SHA-256 repository's index
/// carries a 32-byte trailer and longer object ids, so it fails here and the
/// caller falls back instead of corrupting the file.
fn parse_header(buffer: &[u8]) -> Result<u32> {
    if buffer.len() < HEADER_LEN + TRAILER_LEN || &buffer[..4] != b"DIRC" {
        bail!("not a Git index file");
    }
    let version = read_u32(buffer, 4)?;
    if !(2..=3).contains(&version) {
        bail!("unsupported Git index version {version}");
    }
    let body = &buffer[..buffer.len() - TRAILER_LEN];
    if Sha1::digest(body).as_slice() != &buffer[buffer.len() - TRAILER_LEN..] {
        bail!("Git index checksum mismatch");
    }
    read_u32(buffer, 8)
}

/// Patch one cache entry and return the offset of the next one.
fn adopt_entry(buffer: &mut [u8], offset: usize, worktree: &Path) -> Result<usize> {
    let flags = read_u16(buffer, offset + FLAGS_OFFSET)?;
    let name_offset = offset + FLAGS_OFFSET + if flags & EXTENDED_FLAG != 0 { 4 } else { 2 };
    let name_len = match flags & NAME_MASK {
        NAME_OVERFLOW => buffer[name_offset..]
            .iter()
            .position(|byte| *byte == 0)
            .context("unterminated index path")?,
        len => usize::from(len),
    };
    let name = buffer
        .get(name_offset..name_offset + name_len)
        .context("index path runs past end of file")?;
    // Entries are NUL-padded to a multiple of eight bytes from the entry start.
    let next = offset + (name_offset - offset + name_len + 1).div_ceil(8) * 8;

    let path = worktree.join(Path::new(std::ffi::OsStr::from_bytes(name)));
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Ok(next);
    };
    if read_u32(buffer, offset + 36)? != metadata.size() as u32 {
        return Ok(next);
    }

    // Field order matches Git's `struct cache_entry`: ctime, mtime, dev, ino,
    // mode, uid, gid, size. Mode and size are left alone — the clone shares
    // them with the baseline, and Git normalizes mode itself.
    write_u32(buffer, offset, metadata.ctime() as u32);
    write_u32(buffer, offset + 4, metadata.ctime_nsec() as u32);
    write_u32(buffer, offset + 8, metadata.mtime() as u32);
    write_u32(buffer, offset + 12, metadata.mtime_nsec() as u32);
    write_u32(buffer, offset + 16, metadata.dev() as u32);
    write_u32(buffer, offset + 20, metadata.ino() as u32);
    write_u32(buffer, offset + 28, metadata.uid());
    write_u32(buffer, offset + 32, metadata.gid());
    Ok(next)
}

fn read_u16(buffer: &[u8], offset: usize) -> Result<u16> {
    let bytes = buffer
        .get(offset..offset + 2)
        .context("Git index ended mid-entry")?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(buffer: &[u8], offset: usize) -> Result<u32> {
    let bytes = buffer
        .get(offset..offset + 4)
        .context("Git index ended mid-entry")?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn write_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
