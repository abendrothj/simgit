//! Content-addressed delta store: one directory per session.
//!
//! Layout:
//!   <state_dir>/deltas/<session-id>/
//!     objects/<aa>/<bbbbb…>   SHA-256 blob files (first 2 hex chars as bucket)
//!     manifest.json           { writes: {path: hash}, deletes: [path], renames: [[from,to]] }

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tracing::warn;
use uuid::Uuid;

use serde::{Deserialize, Serialize};

/// A half-open byte interval `[offset, offset + len)` written in a single VFS call.
///
/// Stored per-path in `DeltaManifest::ranges` when the write was intercepted by the
/// FUSE layer. Absent for full-file NFS snapshot writes; in that case the conflict
/// scan falls back to whole-file path comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeltaManifest {
    /// base commit OID (hex) this delta was forked from.
    pub base_commit: String,
    /// path → SHA-256 hex of new content
    pub writes:  HashMap<PathBuf, String>,
    /// paths deleted in this session
    pub deletes: HashSet<PathBuf>,
    /// renames: (from, to)
    pub renames: Vec<(PathBuf, PathBuf)>,
    /// Byte ranges written per path. Absent for full-file (NFS) writes; conflict
    /// scan treats absent entries as whole-file overlap.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ranges: HashMap<PathBuf, Vec<ByteRange>>,
}

pub struct DeltaStore {
    root:           PathBuf,
    /// Maximum bytes of delta blobs per session. `u64::MAX` = unlimited.
    max_delta_bytes: u64,
}

impl DeltaStore {
    /// Create a store with no quota limit.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), max_delta_bytes: u64::MAX }
    }

    /// Create a store that enforces a per-session delta-size quota.
    ///
    /// `write_blob` will return an error when adding a new blob would push the
    /// cumulative unique-blob size for the session over `max_delta_bytes`.
    pub fn new_with_quota(root: impl Into<PathBuf>, max_delta_bytes: u64) -> Self {
        Self { root: root.into(), max_delta_bytes }
    }

    /// Sum of all unique blob file sizes currently stored for `session_id`.
    ///
    /// On I/O errors (e.g. missing directory, permission denied), logs a warning
    /// and treats the unreadable subtree as zero bytes so quota checks remain
    /// conservative (they may allow a write that should be blocked) rather than
    /// hard-failing an otherwise valid write operation.
    fn session_bytes_used(&self, session_id: Uuid) -> u64 {
        let objects_dir = self.objects_dir(session_id);
        let buckets = match std::fs::read_dir(&objects_dir) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    session = %session_id,
                    path = %objects_dir.display(),
                    err = %e,
                    "delta quota: could not read objects dir — treating used bytes as 0"
                );
                return 0;
            }
        };
        buckets
            .filter_map(|e| {
                e.map_err(|err| {
                    warn!(session = %session_id, err = %err, "delta quota: error reading bucket entry");
                })
                .ok()
            })
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .flat_map(|bucket| std::fs::read_dir(bucket.path()).into_iter().flatten())
            .filter_map(|e| {
                e.map_err(|err| {
                    warn!(session = %session_id, err = %err, "delta quota: error reading blob entry");
                })
                .ok()
            })
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    fn session_dir(&self, session_id: Uuid) -> PathBuf {
        self.root.join(session_id.to_string())
    }

    fn objects_dir(&self, session_id: Uuid) -> PathBuf {
        self.session_dir(session_id).join("objects")
    }

    fn manifest_path(&self, session_id: Uuid) -> PathBuf {
        self.session_dir(session_id).join("manifest.json")
    }

    /// Initialise the directory structure for a new session.
    pub fn init_session(&self, session_id: Uuid, base_commit: &str) -> Result<()> {
        std::fs::create_dir_all(self.objects_dir(session_id))
            .with_context(|| format!("create objects dir for session {session_id}"))?;

        let manifest = DeltaManifest { base_commit: base_commit.to_owned(), ..Default::default() };
        self.write_manifest(session_id, &manifest)
    }

    // ── Blob write ────────────────────────────────────────────────────────

    /// Write `content` into the delta store for `session_id` at `path`.
    /// Returns the SHA-256 hex of the content.
    ///
    /// `range` is the half-open byte interval of the write if known (FUSE write path).
    /// Pass `None` for full-file writes (NFS snapshot, renames, creates).
    ///
    /// Returns an error if adding this blob would push the session's total
    /// unique-blob size over `max_delta_bytes`.
    pub fn write_blob(&self, session_id: Uuid, path: &Path, content: &[u8], range: Option<ByteRange>) -> Result<String> {
        // Compute content hash for deduplication and integrity.
        let hash = hex::encode(Sha256::digest(content));

        // Bucket by first 2 hex chars.
        let bucket = &hash[..2];
        let blob_dir = self.objects_dir(session_id).join(bucket);
        std::fs::create_dir_all(&blob_dir)?;

        let blob_path = blob_dir.join(&hash[2..]);
        if !blob_path.exists() {
            // Quota check: only new (non-deduplicated) blobs consume space.
            //
            // This is a *soft* limit. There is a narrow TOCTOU window between the
            // `session_bytes_used()` call and the actual write, so two concurrent
            // writes to the *same* session could each pass the check and collectively
            // slightly exceed the quota. In practice, the VFS write path is
            // single-threaded per session, and any overshoot is bounded by one
            // additional blob. Full serialization would require a per-session Mutex
            // and is deferred to a future hardening pass.
            if self.max_delta_bytes != u64::MAX {
                let used = self.session_bytes_used(session_id);
                let new_size = content.len() as u64;
                if used.saturating_add(new_size) > self.max_delta_bytes {
                    anyhow::bail!(
                        "delta quota exceeded for session {session_id}: \
                         {} bytes used + {} bytes new > {} bytes limit",
                        used, new_size, self.max_delta_bytes
                    );
                }
            }

            // Write-then-rename for atomicity.
            let tmp = blob_path.with_extension("tmp");
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(content)?;
                f.sync_data()?;
            }
            std::fs::rename(&tmp, &blob_path)?;
        }

        // Update manifest.
        let mut manifest = self.load_manifest(session_id)?;
        // Remove from deletes if it was previously deleted.
        manifest.deletes.remove(path);
        manifest.writes.insert(path.to_owned(), hash.clone());
        match range {
            Some(r) => manifest.ranges.entry(path.to_owned()).or_default().push(r),
            None    => { manifest.ranges.remove(path); }
        }
        self.write_manifest(session_id, &manifest)?;

        Ok(hash)
    }

    // ── Blob read ─────────────────────────────────────────────────────────

    /// Read the delta blob for `path` in `session_id`, if any.
    pub fn read_blob(&self, session_id: Uuid, path: &Path) -> Result<Option<Vec<u8>>> {
        let manifest = self.load_manifest(session_id)?;

        if manifest.deletes.contains(path) {
            // File was deleted in this session — signal that to the caller.
            return Ok(None);
        }

        let hash = match manifest.writes.get(path) {
            Some(h) => h.clone(),
            None    => return Ok(None), // not in this delta; fall through to git
        };

        let blob_path = self.objects_dir(session_id).join(&hash[..2]).join(&hash[2..]);
        let data = std::fs::read(&blob_path)
            .with_context(|| format!("read blob {hash} for session {session_id}"))?;

        // Integrity check: reject blobs whose content doesn't match filename.
        let actual = hex::encode(Sha256::digest(&data));
        if actual != hash {
            anyhow::bail!(
                "blob integrity failure for session {session_id}, path {}: expected {hash}, got {actual}",
                path.display()
            );
        }

        Ok(Some(data))
    }

    // ── Delete marker ─────────────────────────────────────────────────────

    pub fn mark_deleted(&self, session_id: Uuid, path: &Path) -> Result<()> {
        let mut manifest = self.load_manifest(session_id)?;
        manifest.writes.remove(path);
        manifest.deletes.insert(path.to_owned());
        self.write_manifest(session_id, &manifest)
    }

    // ── Rename ────────────────────────────────────────────────────────────

    pub fn record_rename(&self, session_id: Uuid, from: &Path, to: &Path) -> Result<()> {
        let mut manifest = self.load_manifest(session_id)?;
        // Move write entry from old path to new path.
        if let Some(hash) = manifest.writes.remove(from) {
            manifest.writes.insert(to.to_owned(), hash);
        }
        manifest.renames.push((from.to_owned(), to.to_owned()));
        self.write_manifest(session_id, &manifest)
    }

    // ── Manifest helpers ──────────────────────────────────────────────────

    pub fn load_manifest(&self, session_id: Uuid) -> Result<DeltaManifest> {
        let p = self.manifest_path(session_id);
        if !p.exists() {
            return Ok(DeltaManifest::default());
        }
        let data = std::fs::read(&p)?;
        serde_json::from_slice(&data).with_context(|| format!("parse manifest for {session_id}"))
    }

    fn write_manifest(&self, session_id: Uuid, manifest: &DeltaManifest) -> Result<()> {
        let p = self.manifest_path(session_id);
        let tmp = p.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            serde_json::to_writer(&mut f, manifest)?;
            f.sync_data()?;
        }
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Remove all delta data for a session (post-commit cleanup).
    pub fn purge_session(&self, session_id: Uuid) -> Result<()> {
        let dir = self.session_dir(session_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// List all sessions that have a delta directory (used for crash recovery).
    pub fn list_sessions(&self) -> Result<Vec<Uuid>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if let Ok(id) = entry.file_name().to_string_lossy().parse::<Uuid>() {
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

// Need hex encoding — add a tiny helper rather than pulling a new crate.
// (hex crate is a transitive dep of sha2/digest anyway.)
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().fold(String::new(), |mut s, b| {
            s.push_str(&format!("{b:02x}"));
            s
        })
    }
}

#[cfg(test)]
mod tests {
    use super::DeltaStore;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    static TEST_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_delta_root() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let seq = TEST_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "simgit-delta-test-{}-{}-{}",
            std::process::id(),
            nanos,
            seq
        ))
    }

    #[test]
    fn delta_store_write_delete_and_rename_flow() {
        let root = temp_delta_root();
        let store = DeltaStore::new(&root);
        let session_id = Uuid::now_v7();
        let old_path = Path::new("src/old.rs");
        let new_path = Path::new("src/new.rs");

        store.init_session(session_id, "HEAD").expect("init session");
        store
            .write_blob(session_id, old_path, b"hello", None)
            .expect("write old path");
        store
            .record_rename(session_id, old_path, new_path)
            .expect("rename path");
        store
            .mark_deleted(session_id, old_path)
            .expect("delete old path");

        let manifest = store.load_manifest(session_id).expect("load manifest");
        assert!(manifest.writes.contains_key(new_path));
        assert!(manifest.deletes.contains(old_path));
        assert!(
            manifest
                .renames
                .iter()
                .any(|(from, to)| from == old_path && to == new_path)
        );

        let bytes = store
            .read_blob(session_id, new_path)
            .expect("read renamed path")
            .expect("renamed path has bytes");
        assert_eq!(bytes, b"hello");

        let old = store.read_blob(session_id, old_path).expect("read old path");
        assert!(old.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_sessions_reflects_delta_dirs_for_recovery() {
        let root = temp_delta_root();
        let store = DeltaStore::new(&root);
        let s1 = Uuid::now_v7();
        let s2 = Uuid::now_v7();

        store.init_session(s1, "HEAD").expect("init s1");
        store.init_session(s2, "HEAD").expect("init s2");

        let sessions = store.list_sessions().expect("list sessions");
        assert!(sessions.contains(&s1));
        assert!(sessions.contains(&s2));

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Quota enforcement ─────────────────────────────────────────────────

    #[test]
    fn write_blob_succeeds_within_quota() {
        let root = temp_delta_root();
        let store = DeltaStore::new_with_quota(&root, 1024);
        let session_id = Uuid::now_v7();

        store.init_session(session_id, "HEAD").expect("init");
        store
            .write_blob(session_id, Path::new("file.txt"), b"small content", None)
            .expect("write should succeed within quota");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_blob_fails_when_quota_exceeded() {
        let root = temp_delta_root();
        // 10-byte quota; writing 20 bytes should fail.
        let store = DeltaStore::new_with_quota(&root, 10);
        let session_id = Uuid::now_v7();
        let content = b"this is twenty bytes!";

        store.init_session(session_id, "HEAD").expect("init");
        let err = store
            .write_blob(session_id, Path::new("big.txt"), content, None)
            .expect_err("write should fail when quota exceeded");

        assert!(err.to_string().contains("quota exceeded"), "error must mention quota exceeded");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_blob_dedup_does_not_recount_existing_blob() {
        let root = temp_delta_root();
        let content = b"dedup content";
        // quota is slightly above content size — just enough for one blob.
        let quota = content.len() as u64 + 5;
        let store = DeltaStore::new_with_quota(&root, quota);
        let session_id = Uuid::now_v7();

        store.init_session(session_id, "HEAD").expect("init");
        // First write — should succeed.
        store
            .write_blob(session_id, Path::new("a.txt"), content, None)
            .expect("first write should succeed");
        // Second write of same bytes (different path) — dedup: no new bytes on disk.
        store
            .write_blob(session_id, Path::new("b.txt"), content, None)
            .expect("dedup write of same content should not be rejected by quota");

        let _ = std::fs::remove_dir_all(&root);
    }
}
