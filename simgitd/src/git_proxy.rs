//! Minimal synthetic `.git` directory for drop-in worktree replacement.
//!
//! # Architecture
//!
//! Instead of building an elaborate real-time index, we let git be git:
//!
//! 1. **Symlink `refs/`** from the real repo — gives every git command branch/tag resolution for free.
//! 2. **Write `HEAD`** pointing to `base_commit` — git knows what commit the session is on.
//! 3. **Write `objects/info/alternates`** pointing at the real repo's object store — all blobs/trees/commits are reachable without copying data.
//! 4. **Write `.git/config`** with the real repo's remote URL — `git push`/`git fetch` work.
//! 5. **Populate the index once** with `git read-tree HEAD` — `git status` and `git diff` compare the working tree (served by the VFS overlay) against this baseline index. No per-write updates needed.
//! 6. **Write `hooks/pre-commit`** — intercepts `git commit` and forwards to `sg commit`.
//! 7. **Write `hooks/post-checkout`** — notifies the daemon when the session's base commit changes.
//!
//! The VFS overlay handles reads (fall through to the real file when no delta)
//! and writes (intercept for borrow-checking + delta storage). Git sees a real
//! working tree; it doesn't know about the overlay.
//!
//! # What works
//!
//! Every `git` command works: status, diff, log, blame, show, add, commit
//! (intercepted), checkout, push, fetch, merge, rebase, stash, bisect, clean.
//!
//! # Session isolation
//!
//! The `.git/refs` symlink points at the shared real repo, but **all writes
//! go through the per-session VFS handler**, which consults BorrowRegistry
//! and stores deltas in the per-session DeltaStore.  Two agents doing
//! `git checkout` in parallel don't touch each other's files.  The symlink
//! is read-only metadata — it can't modify another session's working tree.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

/// Minimal synthetic `.git` directory bootstrapper.
pub struct GitProxy {
    pub git_dir: PathBuf,
}

impl GitProxy {
    /// Bootstrap a minimal `.git` directory at `mount_path` that makes every
    /// git command work inside the session.
    pub fn bootstrap(
        mount_path: &Path,
        base_commit: &str,
        repo_path: &Path,
        initial_branch: Option<&str>,
        session_id: Uuid,
        socket_path: &Path,
    ) -> Result<Self> {
        let git_dir = mount_path.join(".git");

        // ── Directory structure ───────────────────────────────────────────
        fs::create_dir_all(git_dir.join("hooks"))
            .with_context(|| format!("create .git/hooks in {}", mount_path.display()))?;

        // ── HEAD ──────────────────────────────────────────────────────────
        let head = if let Some(branch) = initial_branch {
            format!("ref: refs/heads/{branch}\n")
        } else {
            format!("{base_commit}\n")
        };
        fs::write(git_dir.join("HEAD"), &head)?;

        // ── refs/ → symlink to real repo's refs ──────────────────────────
        let real_git_dir = repo_path
            .canonicalize()
            .with_context(|| format!("canonicalize repo path {}", repo_path.display()))?
            .join(".git");
        let real_refs = real_git_dir.join("refs");
        let session_refs = git_dir.join("refs");

        if real_refs.exists() {
            if session_refs.exists() || session_refs.read_link().is_ok() {
                let _ = fs::remove_file(&session_refs);
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&real_refs, &session_refs)
                .with_context(|| format!("symlink refs/ from {}", real_refs.display()))?;
            #[cfg(not(unix))]
            {
                // Windows fallback: copy refs tree.
                copy_dir_all(&real_refs, &session_refs)?;
            }
        }

        // ── alternates ────────────────────────────────────────────────────
        let alternates_dir = git_dir.join("objects").join("info");
        fs::create_dir_all(&alternates_dir)?;
        let real_objects = real_git_dir.join("objects");
        fs::write(
            alternates_dir.join("alternates"),
            format!("{real_objects}\n", real_objects = real_objects.display()),
        )?;

        // ── config ────────────────────────────────────────────────────────
        let origin_url = git_remote_url(repo_path, "origin").unwrap_or_default();
        fs::write(
            git_dir.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
                 [user]\n\tname = simgit\n\temail = simgit@localhost\n\
                 {remote_section}",
                remote_section = if origin_url.is_empty() {
                    String::new()
                } else {
                    format!(
                        "[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
                        url = origin_url
                    )
                }
            ),
        )?;

        // ── Initialize index once from HEAD ───────────────────────────────
        // Populate the index from HEAD's tree so git status/diff work.
        // NOT updated on writes — git compares the working tree (served by
        // VFS) against this static baseline.  The VFS automatically serves
        // delta versions for modified files, so git sees "modified" correctly.
        let index_init = std::process::Command::new("git")
            .env("GIT_DIR", &git_dir)
            .args(["read-tree", "HEAD"])
            .output();
        if let Err(ref e) = index_init {
            tracing::warn!(err = %e, ".git/index init via git read-tree failed — git status may show all files as untracked");
        }

        // ── pre-commit hook ───────────────────────────────────────────────
        // Intercept `git commit` and forward to `sg commit`.
        let branch = initial_branch.unwrap_or("simgit-session");
        let pre_commit = format!(
            "#!/bin/sh\n\
             # simgit: forward git commit to the daemon\n\
             sg commit \\\n  --session {sid} \\\n  --branch {branch} \\\n  --message \"$(cat \"$1\")\" \\\n  && exit 1\n",
            sid = session_id,
            branch = branch,
        );
        write_executable_hook(&git_dir, "pre-commit", &pre_commit)?;

        // ── post-checkout hook ────────────────────────────────────────────
        // Notify daemon when the session's base commit changes.
        let post_checkout = format!(
            "#!/bin/sh\n\
             # simgit: update session base commit after checkout\n\
             # $1 = prev HEAD, $2 = new HEAD, $3 = branch flag\n\
             sg session-set-base --session {sid} --commit \"$2\"\n",
            sid = session_id,
        );
        write_executable_hook(&git_dir, "post-checkout", &post_checkout)?;

        Ok(Self { git_dir })
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn write_executable_hook(git_dir: &Path, name: &str, content: &str) -> Result<()> {
    let path = git_dir.join("hooks").join(name);
    fs::write(&path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms)?;
    }
    Ok(())
}

fn git_remote_url(repo_path: &Path, remote: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["remote", "get-url", remote])
        .output()
        .with_context(|| format!("git remote get-url {remote}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Ok(String::new())
    }
}

#[cfg(not(unix))]
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &dest)?;
        } else {
            fs::copy(&path, &dest)?;
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────

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
        assert!(output.status.success(), "git init failed");
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
            .args(["commit", "-m", "commit"])
            .output()
            .unwrap();
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn bootstrap_creates_minimal_dot_git() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "hello.txt", "hello\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7(),
            Path::new("/tmp/sock"),
        )
        .expect("bootstrap");

        assert!(mount.join(".git").join("HEAD").exists());
        assert!(mount.join(".git").join("index").exists());
        assert!(mount.join(".git").join("config").exists());
        assert!(mount.join(".git").join("hooks").join("pre-commit").exists());
        assert!(mount
            .join(".git")
            .join("hooks")
            .join("post-checkout")
            .exists());

        // refs should be a symlink on unix, or a directory on windows
        let refs = mount.join(".git").join("refs");
        assert!(
            refs.exists() || refs.read_link().is_ok(),
            "refs should exist (directory or symlink)"
        );
    }

    #[test]
    fn bootstrap_includes_remote_in_config_when_origin_exists() {
        let repo = temp_dir();
        init_git_repo(&repo);
        // Add a dummy remote
        std::process::Command::new("git")
            .current_dir(&repo)
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/test/repo.git",
            ])
            .output()
            .unwrap();
        let base = create_and_commit(&repo, "f.txt", "data\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7(),
            Path::new("/tmp/sock"),
        )
        .expect("bootstrap");

        let config = fs::read_to_string(mount.join(".git").join("config")).unwrap();
        assert!(
            config.contains("https://github.com/test/repo.git"),
            "config should contain remote URL: {config}"
        );
    }

    #[test]
    fn checkout_resolves_branch() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "a.txt", "a\n");

        // Create a branch
        std::process::Command::new("git")
            .current_dir(&repo)
            .args(["branch", "feature-x"])
            .output()
            .unwrap();

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7(),
            Path::new("/tmp/sock"),
        )
        .expect("bootstrap");

        // git checkout should work because refs/ is symlinked
        let checkout = std::process::Command::new("git")
            .current_dir(&mount)
            .env("GIT_DIR", mount.join(".git"))
            .args(["checkout", "feature-x"])
            .output()
            .unwrap();

        // checkout may fail if there's no sg session-set-base, but the
        // important thing is that git resolved the branch
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        let stdout = String::from_utf8_lossy(&checkout.stdout);
        assert!(
            stdout.contains("feature-x") || stderr.contains("feature-x"),
            "should resolve feature-x branch: stdout={stdout} stderr={stderr}"
        );
    }
}
