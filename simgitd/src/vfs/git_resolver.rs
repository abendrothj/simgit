//! Git tree traversal and inode resolution for VFS.
//!
//! Phase 1: Convert git tree objects into inode entries.
//! Uses git CLI subprocess calls to read tree objects (avoiding gix threading issues).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use anyhow::{bail, Context, Result};
use std::time::Instant;
use std::path::Path;

/// Maps git tree OID → file entries.
#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub name: String,
    pub mode: String, // "100644", "100755", "040000", "120000"
    pub oid:  String,
}

/// LRU tree cache — holds parsed tree objects indexed by OID.
pub struct TreeCache {
    trees:    Mutex<HashMap<String, CachedTree>>,
    max_size: usize,
}

struct CachedTree {
    entries: Vec<TreeEntry>,
    added:   Instant,
}

impl TreeCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            trees:    Mutex::new(HashMap::new()),
            max_size,
        }
    }

    pub fn get(&self, tree_oid: &str, repo_path: &Path) -> Result<Vec<TreeEntry>> {
        // Check cache.
        if let Some(cached) = self.trees.lock().unwrap().get(tree_oid) {
            // Simple TTL: 60 seconds
            if cached.added.elapsed().as_secs() < 60 {
                return Ok(cached.entries.clone());
            }
        }

        // Cache miss or expired — read from git via CLI.
        let output = std::process::Command::new("git")
            .current_dir(repo_path)
            .args(&["ls-tree", tree_oid])
            .output()
            .with_context(|| format!("exec git ls-tree {tree_oid}"))?;

        if !output.status.success() {
            bail!("git ls-tree failed");
        }

        let lines = String::from_utf8(output.stdout)?;
        let mut entries = Vec::new();

        for line in lines.lines() {
            // Format: "100644 blob abc123def456...  filename"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 4 {
                continue;
            }
            let mode = parts[0];
            let oid = parts[2];
            let name = parts[3..].join(" "); // Filename might have spaces

            entries.push(TreeEntry {
                name,
                mode: mode.to_owned(),
                oid:  oid.to_owned(),
            });
        }

        // Cache it.
        self.trees.lock().unwrap().insert(tree_oid.to_owned(), CachedTree {
            entries: entries.clone(),
            added:   Instant::now(),
        });

        // Evict oldest if over capacity.
        let mut trees = self.trees.lock().unwrap();
        if trees.len() > self.max_size {
            if let Some(oldest_oid) = trees
                .iter()
                .min_by_key(|(_, t)| t.added)
                .map(|(oid, _)| oid.clone())
            {
                trees.remove(&oldest_oid);
            }
        }

        Ok(entries)
    }
}

/// Blob content cache — for small frequently-read files.
pub struct BlobCache {
    blobs:    Mutex<HashMap<String, CachedBlob>>,
    max_size: usize,
    max_bytes: u64,
}

struct CachedBlob {
    content: Vec<u8>,
    added:   Instant,
}

impl BlobCache {
    pub fn new(max_size: usize, max_bytes: u64) -> Self {
        Self {
            blobs:    Mutex::new(HashMap::new()),
            max_size,
            max_bytes,
        }
    }

    pub fn get(&self, blob_oid: &str, repo_path: &Path) -> Result<Vec<u8>> {
        // Check cache.
        if let Some(cached) = self.blobs.lock().unwrap().get(blob_oid) {
            if cached.added.elapsed().as_secs() < 60 {
                return Ok(cached.content.clone());
            }
        }

        // Cache miss — read from git.
        let output = std::process::Command::new("git")
            .current_dir(repo_path)
            .args(&["cat-file", "-p", blob_oid])
            .output()
            .with_context(|| format!("exec git cat-file {blob_oid}"))?;

        if !output.status.success() {
            bail!("git cat-file failed");
        }

        let content = output.stdout;

        // Only cache if small enough.
        if content.len() as u64 <= self.max_bytes {
            self.blobs.lock().unwrap().insert(blob_oid.to_owned(), CachedBlob {
                content: content.clone(),
                added:   Instant::now(),
            });

            // Evict oldest if over capacity.
            let mut blobs = self.blobs.lock().unwrap();
            if blobs.len() > self.max_size {
                if let Some(oldest_oid) = blobs
                    .iter()
                    .min_by_key(|(_, b)| b.added)
                    .map(|(oid, _)| oid.clone())
                {
                    blobs.remove(&oldest_oid);
                }
            }
        }

        Ok(content)
    }
}

/// Inode to {tree_oid, entry_index} mapping for path resolution.
pub struct InodeMap {
    // inode → (tree_oid, entry_index_in_tree)
    map: Mutex<HashMap<u64, (String, usize)>>,
    next_ino: Mutex<u64>,
}

impl InodeMap {
    pub fn new() -> Self {
        let mut m = HashMap::new();
        // inode 1 is always root
        m.insert(1, (String::new(), 0));
        Self {
            map: Mutex::new(m),
            next_ino: Mutex::new(2),
        }
    }

    pub fn allocate(&self) -> u64 {
        let mut next = self.next_ino.lock().unwrap();
        let ino = *next;
        *next += 1;
        ino
    }

    pub fn insert(&self, ino: u64, tree_oid: String, entry_idx: usize) {
        self.map.lock().unwrap().insert(ino, (tree_oid, entry_idx));
    }

    pub fn lookup(&self, ino: u64) -> Option<(String, usize)> {
        self.map.lock().unwrap().get(&ino).cloned()
    }
}

