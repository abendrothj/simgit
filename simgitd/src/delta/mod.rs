//! Delta store — captures agent writes in a Copy-on-Write overlay.
//!
//! # Overview
//!
//! When an agent writes to a file, the VFS intercepts the write and stores the new content
//! in the session's **delta layer** instead of touching the real files. This achieves:
//!
//! - **Isolation**: Agent writes are private until flatten
//! - **Efficiency**: Only changed files consume disk space (not full repo copy)
//! - **Atomicity**: Write-then-rename ensures no corruption on crash
//! - **Content Integrity**: SHA-256 hashing validates all blobs
//!
//! # Architecture
//!
//! Each session has a delta directory:
//! ```text
//! /delta/<session-uuid>/
//!   ├── objects/
//!   │   ├── ab/
//!   │   │   └── cdef0123456... (blob content, named by hash)
//!   │   └── ...
//!   └── manifest.json (list of writes, deletes, renames)
//! ```
//!
//! The manifest tracks:
//! - **writes**: { path → blob_hash }
//! - **deletes**: [ path ]
//! - **renames**: [ (old_path, new_path) ]
//!
//! # Workflow
//!
//! 1. Agent writes `/src/main.rs` → VFS captures new content
//! 2. `DeltaStore::write()` hashes content → `ab/cdef0123...`
//! 3. Atomic write-then-rename: `/delta/.../ab/cdef...tmp` → `/delta/.../ab/cdef...`
//! 4. Manifest updated: `manifest.json` records `"/src/main.rs" → "abcdef0123..."`
//! 5. On commit: `flatten()` applies delta to git branch
//!
//! # Integrity
//!
//! - **Write Integrity**: SHA-256 hash computed before write; re-verified on read
//! - **Atomicity**: Write to temp file, atomic rename ensures no partial writes
//! - **Recovery**: On restart, manifest is re-validated; corrupted blobs are detected
//!
//! # Example
//!
//! ```ignore
//! let store = DeltaStore::new(session_id, delta_root)?;
//!
//! // Agent writes new content
//! let content = b"fn main() { println!(\"Hello!\"); }";
//! let blob_hash = store.write(content)?;
//! // Returns: "a1b2c3d4..."
//!
//! // Manifest tracks this write
//! let manifest = store.manifest()?;
//! assert_eq!(manifest.writes[Path::new("/src/main.rs")], "a1b2c3d4...");
//!
//! // On session end, flatten stores it in git
//! let result = flatten(&repo, base_commit, &manifest, ...)?;
//! // Returns new commit SHA
//! ```

pub mod store;
pub mod flatten;

pub use store::DeltaStore;
