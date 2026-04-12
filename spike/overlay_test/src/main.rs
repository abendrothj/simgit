//! Spike: in-process Copy-on-Write overlay.
//!
//! Simulates the core CoW logic without FUSE:
//!   - A "base layer" (read-only directory)
//!   - A "delta layer" (writable directory, per-session)
//!   - read(path)  → delta first, then base
//!   - write(path) → always to delta
//!
//! Run: overlay_test <base-dir> [<delta-dir>]

use std::path::{Path, PathBuf};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

struct Overlay {
    base:  PathBuf,
    delta: PathBuf,
}

impl Overlay {
    fn new(base: impl Into<PathBuf>, delta: impl Into<PathBuf>) -> Self {
        Self { base: base.into(), delta: delta.into() }
    }

    fn read(&self, rel: &str) -> Result<Vec<u8>> {
        let delta_path = self.delta.join(rel);
        if delta_path.exists() {
            return std::fs::read(&delta_path)
                .with_context(|| format!("read from delta: {rel}"));
        }
        let base_path = self.base.join(rel);
        std::fs::read(&base_path)
            .with_context(|| format!("read from base: {rel}"))
    }

    fn write(&self, rel: &str, content: &[u8]) -> Result<()> {
        let dest = self.delta.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write-then-rename for atomicity.
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &dest)?;
        println!("  WRITE delta/{rel} ({} bytes, sha256={})",
            content.len(), hex_sha256(content));
        Ok(())
    }

    fn list_changed(&self) -> Result<Vec<String>> {
        let mut changed = Vec::new();
        if !self.delta.exists() {
            return Ok(changed);
        }
        for entry in walkdir(&self.delta)? {
            let rel = entry
                .strip_prefix(&self.delta)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if !rel.is_empty() {
                changed.push(rel);
            }
        }
        Ok(changed)
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        bail!("Usage: overlay_test <base-dir> [<delta-dir>]");
    }

    let base  = PathBuf::from(&args[1]).canonicalize()?;
    let delta = if args.len() > 2 {
        PathBuf::from(&args[2])
    } else {
        std::env::temp_dir().join("simgit_overlay_spike")
    };
    std::fs::create_dir_all(&delta)?;

    println!("Base:  {}", base.display());
    println!("Delta: {}", delta.display());
    println!();

    let ov = Overlay::new(&base, &delta);

    // --- Read a file from base (fall-through) ---
    let base_files: Vec<_> = std::fs::read_dir(&base)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .collect();

    if base_files.is_empty() {
        println!("(base dir has no files — add some files to {base:?} to test)");
        return Ok(());
    }

    let first = base_files[0].file_name();
    let name = first.to_string_lossy();
    println!("Reading '{name}' from overlay (expects base)...");
    let content = ov.read(&name)?;
    println!("  READ  base/{name} ({} bytes, sha256={})", content.len(), hex_sha256(&content));

    // --- Write a mutation ---
    let mutated = format!("MUTATED BY OVERLAY SPIKE\n{}", String::from_utf8_lossy(&content));
    println!("Writing mutated version to delta...");
    ov.write(&name, mutated.as_bytes())?;

    // --- Read again — should now return delta version ---
    println!("Reading '{name}' again (expects delta)...");
    let readback = ov.read(&name)?;
    assert_eq!(readback.as_slice(), mutated.as_bytes(), "CoW readback mismatch");
    println!("  OK — delta version returned correctly");

    // --- List changed files ---
    println!("\nChanged files in this overlay session:");
    for f in ov.list_changed()? {
        println!("  M {f}");
    }

    println!("\nSpike passed ✓");
    Ok(())
}

fn hex_sha256(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_owned()
}

fn walkdir(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            out.extend(walkdir(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}
