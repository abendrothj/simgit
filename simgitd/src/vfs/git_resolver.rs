//! Git tree traversal and inode resolution for VFS.
//!
//! # Overview
//!
//! The git resolver converts git tree objects into inode entries that the VFS can serve.
//! It abstracts the complexity of reading git objects, traversing directory trees, and
//! caching frequently accessed data to avoid repeated subprocess calls.
//!
//! # Architecture
//!
//! Two independent caches operate in parallel:
//!
//! - **TreeCache**: Maps git tree OID → entries (filenames, modes, sub-tree OIDs)
//! - **BlobCache**: Maps git blob OID → file content (small files only)
//!
//! Both use **git CLI** subprocess calls (not gix) to avoid threading constraints
//! in the fuser FUSE handler context.
//!
//! # Tree Lookup Flow
//!
//! ```text
//! Agent does readdir("/src/")
//!       ↓
//! VFS asks git_resolver: getEntries("src", base_commit)
//!       ↓
//! Resolve "src" OID from base_commit tree → tree123
//!       ↓
//! TreeCache.get("tree123", repo_path)
//!       ├─ HIT (< 60s old) → return entries
//!       └─ MISS → exec: git ls-tree tree123
//!              ↓ Parse output (mode, oid, name)
//!              ↓ Cache result
//!              ↓ Evict oldest if over capacity
//!              ↓ Return entries
//! ```
//!
//! # Blob Read Flow
//!
//! ```text
//! Agent reads /README.md (small file)
//!       ↓
//! VFS asks git_resolver: getBlob("abc123", repo_path)
//!       ↓
//! BlobCache.get("abc123", repo_path)
//!       ├─ HIT (< 60s old AND < max_bytes) → return content
//!       └─ MISS → exec: git cat-file -p abc123
//!              ↓ Read stdout (file content)
//!              ↓ Cache if size <= max_bytes (skip large files)
//!              ↓ Evict oldest if over capacity
//!              ↓ Return content
//! ```
//!
//! # Caching Strategy
//!
//! - **TTL**: 60 seconds per entry (configurable in future)
//! - **Capacity**: TreeCache holds up to 1024 trees; BlobCache holds up to 512 blobs
//! - **Size Limit**: BlobCache only caches files < 10 MiB (avoids OOM)
//! - **Eviction**: LRU when capacity exceeded (oldest entry removed)
//!
//! # Git CLI Approach
//!
//! Used here instead of gix because:
//! - fuser trait handlers run in thread pool under `spawn_mount`
//! - gix requires careful repo lifetime management across threads
//! - git CLI is subprocess-based (isolated, GC'd automatically)
//! - Trades subprocess overhead for robustness (Phase 1 tradeoff)
//!
//! Phase 2+ may switch to gix if thread safety can be solved.
//!
//! # Example
//!
//! ```ignore
//! let tree_cache = TreeCache::new(1024);
//! let blob_cache = BlobCache::new(512, 10 * 1024 * 1024); // 10 MiB limit
//!
//! let entries = tree_cache.get("ae3f123", repo_path)?;
//! for entry in entries {
//!     println!("{} ({})", entry.name, entry.mode);
//! }
//!
//! let content = blob_cache.get("def456", repo_path)?;
//! println!("File size: {} bytes", content.len());
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use anyhow::{bail, Context, Result};
use std::time::Instant;
use std::path::Path;

/// Maps git tree OID → file entries.
///
/// Represents a single entry from `git ls-tree` output.
///
/// # Fields
///
/// - `name`: Filename (may contain spaces; UTF-8 only, non-UTF-8 filenames cause parse errors)
/// - `mode`: Git file mode as string ("100644" = regular file, "100755" = executable,
///   "040000" = directory, "120000" = symlink)
/// - `oid`: Object hash of the blob or tree (SHA-1 or SHA-256 depending on git config)
///
/// # Example
///
/// ```text
/// TreeEntry {
///   name: "main.rs",
///   mode: "100644",
///   oid:  "abc123def456..."
/// }
/// ```
#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub name: String,
    pub mode: String, // "100644", "100755", "040000", "120000"
    pub oid:  String,
}

/// LRU tree cache — holds parsed tree objects indexed by OID.
///
/// Reduces subprocess calls by caching recently accessed git tree objects.
/// Each cache entry has a 60-second TTL and is evicted when cache exceeds capacity.
pub struct TreeCache {
    trees:    Mutex<HashMap<String, CachedTree>>,
    max_size: usize,
}

struct CachedTree {
    entries: Vec<TreeEntry>,
    added:   Instant,
}

impl TreeCache {
    /// Create a new tree cache with a maximum capacity.
    ///
    /// # Arguments
    ///
    /// - `max_size`: Maximum number of tree objects to hold before LRU eviction
    ///
    /// # Example
    ///
    /// ```ignore
    /// let cache = TreeCache::new(1024);
    /// ```
    pub fn new(max_size: usize) -> Self {
        Self {
            trees:    Mutex::new(HashMap::new()),
            max_size,
        }
    }

    /// Get entries from a git tree object.
    ///
    /// Caches the result for 60 seconds. On cache miss or expiry, reads from git
    /// via `git ls-tree` subprocess call and caches the result.
    ///
    /// # Arguments
    ///
    /// - `tree_oid`: Git tree OID (e.g., "ae3f1234..." or full 40-char SHA-1)
    /// - `repo_path`: Path to git repository
    ///
    /// # Returns
    ///
    /// Ordered list of entries (files and subdirectories) in the tree.
    ///
    /// # Errors
    ///
    /// - Git subprocess failed (not a valid tree)
    /// - UTF-8 decode error (unlikely; git output is UTF-8)
    ///
    /// # Performance
    ///
    /// - **Cache hit**: O(1) HashMap lookup
    /// - **Cache miss**: subprocess call (100–500ms depending on tree size)
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
///
/// Caches file contents to avoid repeated `git cat-file` calls. Only caches files
/// smaller than `max_bytes` (default 10 MiB) to avoid memory bloat.
///
/// Each cache entry has a 60-second TTL and is evicted when cache exceeds capacity.
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
    /// Create a new blob cache.
    ///
    /// # Arguments
    ///
    /// - `max_size`: Maximum number of blobs to hold (e.g., 512)
    /// - `max_bytes`: Maximum size of a single blob to cache (e.g., 10 MiB = 10 * 1024 * 1024)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let cache = BlobCache::new(512, 10 * 1024 * 1024); // 512 blobs, 10 MiB each
    /// ```
    pub fn new(max_size: usize, max_bytes: u64) -> Self {
        Self {
            blobs:    Mutex::new(HashMap::new()),
            max_size,
            max_bytes,
        }
    }

    /// Get blob content from a git object.
    ///
    /// Caches the result for 60 seconds. On cache miss or expiry, reads from git
    /// via `git cat-file -p` subprocess call. Large blobs are NOT cached.
    ///
    /// # Arguments
    ///
    /// - `blob_oid`: Git blob OID (e.g., "abc123..." or full SHA-1)
    /// - `repo_path`: Path to git repository
    ///
    /// # Returns
    ///
    /// Raw file content as bytes. Never cached if > `max_bytes`.
    ///
    /// # Errors
    ///
    /// - Git subprocess failed (not a valid blob)
    /// - Cannot read from git object store
    ///
    /// # Caching Bypass
    ///
    /// If the blob is larger than `max_bytes`, it is returned immediately and NOT cached.
    /// This prevents large files from evicting smaller cached entries.
    ///
    /// # Performance
    ///
    /// - **Cache hit**: O(1) HashMap lookup + clone
    /// - **Cache miss (small)**: subprocess call + cache insert
    /// - **Cache miss (large)**: subprocess call only (no caching)
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

