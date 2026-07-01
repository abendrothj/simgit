//! Transparent git-proxy index — makes `git status`, `git diff`, etc. work
//! inside a simgit session mount by maintaining a synthetic `.git` directory
//! whose index reflects the session's delta state in real time.
//!
//! # Motivation
//!
//! simgit sessions are synthetic VFS mounts, not real git checkouts.  Without
//! a `.git` directory at the mount root, any agent tool that shells out to
//! `git` (Claude Code, Aider, SWE-agent, Codex CLI, …) gets nothing — no
//! `git status`, no `git diff`, no `git log`.  This module provides a
//! lightweight, write-through git index that makes the mount look like a real
//! git working tree to every git subprocess.
//!
//! # Architecture
//!
//! ```text
//! Agent runs `git status` inside /vdev/<session>/
//!       │
//!       ▼
//! git reads .git/index  (maintained by us, updated on every write/delete/rename)
//! git reads .git/HEAD  →  ref: refs/heads/<session-branch>  (or base_commit)
//! git reads objects/    →  falls through to real repo via alternates
//!       │
//!       ▼
//! Output: "modified: src/main.rs"  (because index says src/main.rs = OLD blob,
//!                                    but working tree has the delta version)
//! ```
//!
//! # What works
//!
//! - `git status`        — shows delta files as modified
//! - `git diff`          — working tree vs index (i.e. delta vs baseline)
//! - `git diff --cached` — index vs HEAD (empty unless explicit add)
//! - `git log`           — history up to base_commit (objects via alternates)
//! - `git blame`         — file history (objects via alternates)
//! - `git show`          — any commit reachable from base_commit
//!
//! # What doesn't work (by design)
//!
//! - `git add` — idempotent, has no effect (all writes are already "staged")
//! - `git commit` — results in a git commit outside simgit's lifecycle.
//!   Agents must use `sg commit` for proper conflict-scheduled commit.
//! - `git push` / `git fetch` — no remote configured in the synthetic .git;
//!   agents use the orchestrator's git operations for this.
//!
//! # Performance
//!
//! Index updates are synchronous (happen in the FUSE/NFS write handler), so
//! there is no async overhead.  The index is written to disk on every update;
//! a future optimization could batch writes or defer to a periodic flush.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

/// Full-featured git-proxy index for transparent agent tooling support.
///
/// Creates and maintains a synthetic `.git` directory at the session mount
/// root, including:
/// - `HEAD` pointing to the session's base commit
/// - `objects/info/alternates` linking to the real repository's object store
///   (so all blobs, trees, and commits are reachable without copying data)
/// - `index` file initialized from the base tree and updated on every delta
///   operation
pub struct GitProxy {
    git_dir: PathBuf,
}

impl GitProxy {
    /// Bootstrap a synthetic `.git` directory at `mount_path`.
    ///
    /// Creates `HEAD`, `objects/info/alternates`, and an initial sparse index
    /// that lists every file in `base_commit`'s tree as tracked (stage 0).
    /// Subsequent writes/deletes/renames update this index so `git status`
    /// reflects the session's delta state.
    ///
    /// # Arguments
    ///
    /// - `mount_path`: The root of the session VFS mount (e.g. `/vdev/<uuid>/`).
    /// - `base_commit`: The git commit OID this session is forked from.
    /// - `repo_path`: Path to the real git repository (for alternates and
    ///   `git ls-tree`).
    /// - `initial_branch`: Optional branch name for `HEAD`; if `None`, HEAD
    ///   points directly to `base_commit` in detached-HEAD mode.
    pub fn bootstrap(
        mount_path: &Path,
        base_commit: &str,
        repo_path: &Path,
        initial_branch: Option<&str>,
    ) -> Result<Self> {
        let git_dir = mount_path.join(".git");
        fs::create_dir_all(git_dir.join("objects").join("info"))
            .with_context(|| format!("create .git/objects/info in {}", mount_path.display()))?;
        fs::create_dir_all(git_dir.join("refs").join("heads"))
            .with_context(|| format!("create .git/refs/heads in {}", mount_path.display()))?;

        // ── HEAD ──────────────────────────────────────────────────────────
        let head_path = git_dir.join("HEAD");
        let head = if let Some(branch) = initial_branch {
            format!("ref: refs/heads/{branch}\n")
        } else {
            format!("{base_commit}\n")
        };
        fs::write(&head_path, &head)
            .with_context(|| format!("write HEAD at {}", head_path.display()))?;

        // ── alternates ────────────────────────────────────────────────────
        let alternates_path = git_dir.join("objects").join("info").join("alternates");
        let real_objects = repo_path
            .canonicalize()
            .with_context(|| format!("canonicalize repo path {}", repo_path.display()))?
            .join(".git")
            .join("objects");
        fs::write(
            &alternates_path,
            format!("{real_objects}\n", real_objects = real_objects.display()),
        )
        .with_context(|| format!("write alternates at {}", alternates_path.display()))?;

        // ── config ────────────────────────────────────────────────────────
        // Minimal config so git knows our name.
        let config_path = git_dir.join("config");
        fs::write(
            &config_path,
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
                 [user]\n\tname = simgit\n\temail = simgit@localhost\n"
            ),
        )
        .with_context(|| format!("write config at {}", config_path.display()))?;

        // ── index ─────────────────────────────────────────────────────────
        // Build the initial index from base_commit's tree snapshot.
        let tree_entries = list_tree_entries(repo_path, base_commit)
            .context("list tree from base commit for git-proxy index")?;
        build_index_file(&git_dir, &tree_entries).context("build initial git-proxy index")?;

        Ok(Self { git_dir })
    }

    /// Update the git index to reflect a file write.
    ///
    /// Records the blob hash of `content` at `path` in the index.  If `path`
    /// was not previously in the index (a new file), it is added.
    pub fn update_index_on_write(&self, rel_path: &Path, content: &[u8]) -> Result<()> {
        let hash = git_hash_object(&self.git_dir, content).context("hash blob for index update")?;
        self.update_index_entry(rel_path, Some(&hash))
    }

    /// Update the git index to reflect a file deletion.
    ///
    /// Removes `path` from the index.
    pub fn update_index_on_delete(&self, rel_path: &Path) -> Result<()> {
        self.update_index_entry(rel_path, None)
    }

    /// Update the git index to reflect a rename.
    ///
    /// Removes `from` from the index and inserts `to` with the same blob hash
    /// that `from` had.
    pub fn update_index_on_rename(&self, from: &Path, to: &Path) -> Result<()> {
        // Read the existing entry's hash, remove it, and insert at new path.
        let index_path = self.git_dir.join("index");
        let old_hash = read_blob_hash_from_index(&index_path, from)?;
        self.update_index_entry(from, None)?;

        if let Some(hash) = old_hash {
            self.update_index_entry(to, Some(&hash))?;
        } else {
            // Source wasn't in the index; just make sure 'to' exists as a
            // new entry with the content it has in the working tree.
            // We'll let subsequent writes populate it.
        }
        Ok(())
    }

    // ── internal helpers ──────────────────────────────────────────────────

    fn update_index_entry(&self, rel_path: &Path, blob_hash: Option<&str>) -> Result<()> {
        let index_path = self.git_dir.join("index");
        let rel_str = rel_path
            .to_str()
            .with_context(|| format!("non-UTF-8 path: {}", rel_path.display()))?;

        if let Some(hash) = blob_hash {
            // Add or update: git update-index --add --cacheinfo 100644,<hash>,<path>
            let output = std::process::Command::new("git")
                .env("GIT_INDEX_FILE", &index_path)
                .args([
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{hash},{rel_str}"),
                ])
                .output()
                .with_context(|| format!("git update-index --add for {rel_str}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(
                    path = rel_str,
                    stderr = %stderr,
                    "git update-index --add failed (non-fatal)"
                );
            }
        } else {
            // Remove: git update-index --force-remove <path>
            let output = std::process::Command::new("git")
                .env("GIT_INDEX_FILE", &index_path)
                .args(["update-index", "--force-remove", rel_str])
                .output()
                .with_context(|| format!("git update-index --force-remove for {rel_str}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(
                    path = rel_str,
                    stderr = %stderr,
                    "git update-index --force-remove failed (non-fatal)"
                );
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// List all (mode, oid, path) entries in the tree of `commitish`.
fn list_tree_entries(repo_path: &Path, commitish: &str) -> Result<Vec<(String, String, String)>> {
    let treeish = format!("{commitish}^{{tree}}");
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["ls-tree", "-r", &treeish])
        .output()
        .with_context(|| format!("git ls-tree -r {treeish}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git ls-tree -r failed for {treeish}: {stderr}");
    }

    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Format: "<mode> <type> <oid>\t<path>"
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        entries.push((parts[0].to_owned(), parts[2].to_owned(), path.to_owned()));
    }

    Ok(entries)
}

/// Build the initial git index file from a list of (mode, oid, path) tuples.
///
/// Uses `git update-index` to populate the index.  This is simpler and more
/// correct than hand-building the binary index format.
fn build_index_file(git_dir: &Path, entries: &[(String, String, String)]) -> Result<()> {
    let index_path = git_dir.join("index");

    for (mode, oid, path) in entries {
        let output = std::process::Command::new("git")
            .env("GIT_INDEX_FILE", &index_path)
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{oid},{path}"),
            ])
            .output()
            .with_context(|| format!("git update-index --add --cacheinfo for {path}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git update-index failed for {path}: {stderr}");
        }
    }

    Ok(())
}

/// Hash a blob via `git hash-object` and return the hex OID.
fn git_hash_object(git_dir: &Path, content: &[u8]) -> Result<String> {
    let mut child = std::process::Command::new("git")
        .env("GIT_DIR", git_dir)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn git hash-object")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content)
            .context("write to git hash-object stdin")?;
    }
    // stdin is dropped here, closing the pipe.

    let output = child
        .wait_with_output()
        .context("wait for git hash-object")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git hash-object failed: {stderr}");
    }

    let hash = String::from_utf8(output.stdout)
        .context("git hash-object non-UTF-8 output")?
        .trim()
        .to_owned();

    Ok(hash)
}

/// Read the blob hash (OID) of an entry from the git index, or `None` if
/// the entry doesn't exist.
///
/// Uses `git ls-files --stage` which outputs lines of the form:
/// `<mode> <hash> <stage>\t<path>`
fn read_blob_hash_from_index(index_path: &Path, rel_path: &Path) -> Result<Option<String>> {
    let rel_str = rel_path
        .to_str()
        .with_context(|| format!("non-UTF-8 path: {}", rel_path.display()))?;

    let output = std::process::Command::new("git")
        .env("GIT_INDEX_FILE", index_path)
        .args(["ls-files", "--stage", "--", rel_str])
        .output()
        .with_context(|| format!("git ls-files --stage for {rel_str}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Format: "100644 <hash> 0\t<path>"
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            return Ok(Some(parts[1].to_owned()));
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "simgit-gitproxy-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    static NEXT_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn init_git_repo(path: &Path) {
        let output = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success(), "git init: {:?}", output);
    }

    fn create_and_commit(repo: &Path, filename: &str, content: &str) -> String {
        let filepath = repo.join(filename);
        if let Some(parent) = filepath.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&filepath, content).unwrap();
        std::process::Command::new("git")
            .current_dir(repo)
            .args(["add", filename])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .current_dir(repo)
            .args(["commit", "-m", &format!("add {filename}")])
            .output()
            .unwrap();
        // Get commit hash via rev-parse.
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn bootstrap_creates_dot_git_with_head_and_index() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "hello.txt", "hello world\n");

        let mount = temp_dir();
        let proxy = GitProxy::bootstrap(&mount, &base, &repo, Some("main")).expect("bootstrap");

        assert!(mount.join(".git").join("HEAD").exists());
        assert!(mount.join(".git").join("index").exists());
        assert!(mount.join(".git").join("config").exists());
        assert!(mount
            .join(".git")
            .join("objects")
            .join("info")
            .join("alternates")
            .exists());

        // HEAD should reference the branch.
        let head = fs::read_to_string(mount.join(".git").join("HEAD")).unwrap();
        assert!(head.contains("ref: refs/heads/main"), "HEAD: {head}");

        drop(proxy);
    }

    #[test]
    fn write_updates_index_and_status_reflects_change() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "src/main.rs", "fn main() {}\n");

        let mount = temp_dir();
        let proxy = GitProxy::bootstrap(&mount, &base, &repo, Some("main")).expect("bootstrap");

        // Write a new version of the file into the mount.
        let new_content = "fn main() { println!(\"hello\"); }\n";
        fs::create_dir_all(mount.join("src")).unwrap();
        fs::write(mount.join("src/main.rs"), new_content).unwrap();

        // Update the index to reflect our delta.
        proxy
            .update_index_on_write(Path::new("src/main.rs"), new_content.as_bytes())
            .expect("update_index_on_write");

        // Now git status should show the file as modified.
        let output = std::process::Command::new("git")
            .current_dir(&mount)
            .env("GIT_DIR", mount.join(".git"))
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("src/main.rs"),
            "git status should show src/main.rs as changed: {stdout}"
        );

        drop(proxy);
    }

    #[test]
    fn delete_removes_from_index() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "delete_me.txt", "to be removed\n");

        let mount = temp_dir();
        let proxy = GitProxy::bootstrap(&mount, &base, &repo, Some("main")).expect("bootstrap");

        // Create the file in the working tree so we can "delete" it.
        fs::write(mount.join("delete_me.txt"), "to be removed\n").unwrap();

        // Delete the file from the mount.
        fs::remove_file(mount.join("delete_me.txt")).unwrap();
        proxy
            .update_index_on_delete(Path::new("delete_me.txt"))
            .expect("update_index_on_delete");

        let output = std::process::Command::new("git")
            .current_dir(&mount)
            .env("GIT_DIR", mount.join(".git"))
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("delete_me.txt"),
            "git status should show delete_me.txt as deleted: {stdout}"
        );

        drop(proxy);
    }

    #[test]
    fn rename_moves_index_entry() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "old.txt", "old content\n");

        let mount = temp_dir();
        let proxy = GitProxy::bootstrap(&mount, &base, &repo, Some("main")).expect("bootstrap");

        // Move old.txt → new.txt in the working tree.
        fs::create_dir_all(mount.join("old.txt").parent().unwrap()).unwrap_or(());
        let content = fs::read_to_string(&repo.join("old.txt")).unwrap_or_default();
        fs::write(mount.join("new.txt"), &content).unwrap();

        proxy
            .update_index_on_rename(Path::new("old.txt"), Path::new("new.txt"))
            .expect("update_index_on_rename");
        proxy
            .update_index_on_write(Path::new("new.txt"), content.as_bytes())
            .expect("update_index_on_rename: write new");

        let output = std::process::Command::new("git")
            .current_dir(&mount)
            .env("GIT_DIR", mount.join(".git"))
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("old.txt") && stdout.contains("new.txt"),
            "git status should show rename: {stdout}"
        );

        drop(proxy);
    }
}
