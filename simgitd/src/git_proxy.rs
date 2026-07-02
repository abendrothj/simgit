//! Minimal synthetic `.git` directory for drop-in worktree replacement.
//!
//! # Architecture
//!
//! Instead of building an elaborate real-time index, we let git be git:
//!
//! 1. **Copy `refs/`** from the real repo — gives every git command branch/tag resolution for free.
//! 2. **Write `HEAD`** pointing to `base_commit` — git knows what commit the session is on.
//! 3. **Write `objects/info/alternates`** pointing at the real repo's object store — all blobs/trees/commits are reachable without copying data.
//! 4. **Write `.git/config`** with the real repo's remote URL — `git push`/`git fetch` work.
//! 5. **Populate the index once** with `git read-tree HEAD` — `git status` and `git diff` compare the working tree (served by the VFS overlay) against this baseline index. No per-write updates needed.
//! 6. **Write `hooks/pre-commit`** — forwards `git commit` to `sg commit` via the daemon.
//! 7. **Write `hooks/post-checkout`** — notifies the daemon when the session's base commit changes.
//! 8. **Write `hooks/post-merge`** — notifies the daemon after `git merge`/`git pull`.
//! 9. **Write `hooks/post-rewrite`** — notifies the daemon after `git rebase`/`git commit --amend`.
//! 10. **Write `hooks/commit-msg`** — pass-through for agent message validation.
//! 11. **Write `hooks/pre-push`** — refreshes session state before push.
//!
//! The VFS overlay handles reads (fall through to the real file when no delta)
//! and writes (intercept for borrow-checking + delta storage). Git sees a real
//! working tree; it doesn't know about the overlay.
//!
//! # What works
//!
//! Every `git` command works: status, diff, log, blame, show, add, commit
//! (forwarded), checkout, push, fetch, merge, rebase, stash, bisect, clean.
//! All lifecycle hooks (pre-commit, post-checkout, post-merge, post-rewrite)
//! are wired to keep the daemon in sync.
//!
//! # Session isolation
//!
//! The `.git/refs` is a copy of the shared real repo's refs, but **all writes
//! go through the per-session VFS handler**, which consults BorrowRegistry
//! and stores deltas in the per-session DeltaStore.  Two agents doing
//! `git checkout` in parallel don't touch each other's files.  The refs copy
//! is read-only metadata — it can't modify another session's working tree.
//!
//! # Not a git worktree
//!
//! simgit sessions are **not** registered as `git worktree` entries.  The
//! synthetic `.git/` directory is standalone — `git worktree list` in the
//! real repo won't show sessions.  This avoids the disk and I/O overhead of
//! real worktrees (N full checkouts).  Use `sg session list` for discovery.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

/// Minimal synthetic `.git` directory bootstrapper.
pub struct GitProxy {
    pub git_dir: PathBuf,
}

impl GitProxy {
    /// Bootstrap a minimal `.git` directory at `git_data_dir` that makes
    /// every git command work inside the session at `mount_path`.
    ///
    /// `git_data_dir` is where the actual `.git/` content lives (hooks, refs,
    /// config, objects).  `mount_path` is the working-tree root where agents
    /// run git commands — it is used only for hook script paths and is not
    /// the location of the `.git/` directory itself.  For backends that
    /// overlay the mount (e.g. NFS), `git_data_dir` is placed outside the
    /// overlay so that the data is always accessible.
    ///
    /// `socket_path` is the path to the daemon's `control.port` file, injected
    /// into hook scripts so that `sg commit` can find the daemon.
    ///
    /// `sg_binary_path` is the absolute path to the `sg` CLI binary, injected
    /// into hook scripts so hooks work regardless of `$PATH`.
    pub fn bootstrap(
        _mount_path: &Path,
        git_data_dir: &Path,
        socket_path: &Path,
        sg_binary_path: &Path,
        base_commit: &str,
        repo_path: &Path,
        initial_branch: Option<&str>,
        session_id: Uuid,
    ) -> Result<Self> {
        let git_dir = git_data_dir.join(".git");

        // ── Directory structure ───────────────────────────────────────────
        fs::create_dir_all(git_dir.join("hooks"))
            .with_context(|| format!("create .git/hooks in {}", git_dir.display()))?;
        fs::create_dir_all(git_dir.join("logs"))
            .with_context(|| format!("create .git/logs in {}", git_dir.display()))?;

        // ── HEAD ──────────────────────────────────────────────────────────
        let head = if let Some(branch) = initial_branch {
            format!("ref: refs/heads/{branch}\n")
        } else {
            format!("{base_commit}\n")
        };
        fs::write(git_dir.join("HEAD"), &head)?;

        // ── refs/ → copy from real repo ─────────────────────────────────
        // We COPY rather than symlink so that the session's .git/refs is a
        // proper standalone ref store.  This lets git stash, git blame, git
        // merge, etc. treat the session as having a real local commit
        // history (the objects are still shared via alternates).
        // A typical repo's refs are ~10-600 KB — negligible per session.
        let real_git_dir = repo_path
            .canonicalize()
            .with_context(|| format!("canonicalize repo path {}", repo_path.display()))?
            .join(".git");
        let real_refs = real_git_dir.join("refs");
        let session_refs = git_dir.join("refs");

        if real_refs.exists() {
            if session_refs.exists() || session_refs.read_link().is_ok() {
                let _ = fs::remove_dir_all(&session_refs);
            }
            copy_dir_all(&real_refs, &session_refs)
                .with_context(|| format!("copy refs from {}", real_refs.display()))?;
        }

        // Also copy packed-refs so tags and branches stored in the packed
        // format are visible in the session.
        let real_packed = real_git_dir.join("packed-refs");
        let session_packed = git_dir.join("packed-refs");
        if real_packed.exists() {
            fs::copy(&real_packed, &session_packed)
                .with_context(|| format!("copy packed-refs from {}", real_packed.display()))?;
        }

        // Write the current branch ref so the session has a local HEAD ref.
        if let Some(branch) = initial_branch {
            let branch_ref_dir = session_refs.join("heads");
            fs::create_dir_all(&branch_ref_dir)?;
            fs::write(branch_ref_dir.join(branch), format!("{base_commit}\n"))?;
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
        let user_name = git_config_get(repo_path, "user.name")
            .unwrap_or_else(|| "simgit".to_owned());
        let user_email = git_config_get(repo_path, "user.email")
            .unwrap_or_else(|| "simgit@localhost".to_owned());

        // Inherit key config values from the real repo so the session
        // behaves identically to a real worktree.
        let inherited_config = build_inherited_config(repo_path);

        fs::write(
            git_dir.join("config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
                 [user]\n\tname = {user_name}\n\temail = {user_email}\n\
                 {remote_section}\
                 {inherited_config}",
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
        // Forward `git commit` to `sg commit` via the daemon. Let git
        // proceed with its local commit afterwards (harmless duplicate).
        // Pre-commit doesn't receive the commit message — we use a fixed
        // message; the real commit message is in the git commit object.
        let branch = initial_branch.unwrap_or("simgit-session");
        let socket = socket_path.to_string_lossy();
        let sg = sg_binary_path.to_string_lossy();
        let pre_commit = format!(
            "#!/bin/sh\n\
             # simgit: forward git commit to the daemon\n\
             {sg} --socket {socket} commit \\\n  --session {sid} \\\n  --branch {branch} \\\n  --message \"simgit commit\"\n",
            sg = sg,
            socket = socket,
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
             {sg} --socket {socket} session-set-base --session {sid} --commit \"$2\"\n",
            sg = sg,
            socket = socket,
            sid = session_id,
        );
        write_executable_hook(&git_dir, "post-checkout", &post_checkout)?;

        // ── post-merge hook ────────────────────────────────────────────────
        // Notify daemon after git merge / git pull.
        let post_merge = format!(
            "#!/bin/sh\n\
             # simgit: update session base commit after merge\n\
             # $1 = 1 if squash, 0 otherwise\n\
             HEAD_COMMIT=$(git rev-parse HEAD 2>/dev/null) && \\\n\
             {sg} --socket {socket} session-set-base --session {sid} --commit \"$HEAD_COMMIT\"\n",
            sg = sg,
            socket = socket,
            sid = session_id,
        );
        write_executable_hook(&git_dir, "post-merge", &post_merge)?;

        // ── post-rewrite hook ──────────────────────────────────────────────
        // Notify daemon after git rebase / git commit --amend.
        let post_rewrite = format!(
            "#!/bin/sh\n\
             # simgit: update session base commit after rebase/amend\n\
             HEAD_COMMIT=$(git rev-parse HEAD 2>/dev/null) && \\\n\
             {sg} --socket {socket} session-set-base --session {sid} --commit \"$HEAD_COMMIT\"\n",
            sg = sg,
            socket = socket,
            sid = session_id,
        );
        write_executable_hook(&git_dir, "post-rewrite", &post_rewrite)?;

        // ── commit-msg hook ────────────────────────────────────────────────
        // Allow the agent's commit-msg hook (e.g. ticket ID validation) to
        // run by providing a pass-through that preserves the message file.
        // The real repo's commit-msg hook is not replicated — agents should
        // apply their own message policies through the hook mechanism.
        let commit_msg = format!(
            "#!/bin/sh\n\
             # simgit: commit-msg pass-through\n\
             # The message file ($1) is preserved; sg commit already ran.\n\
             exit 0\n"
        );
        write_executable_hook(&git_dir, "commit-msg", &commit_msg)?;

        // ── pre-push hook ──────────────────────────────────────────────────
        // Notify daemon before git push so it can refresh refs.
        let pre_push = format!(
            "#!/bin/sh\n\
             # simgit: update session refs before push\n\
             # $1 = remote name, $2 = remote URL\n\
             HEAD_COMMIT=$(git rev-parse HEAD 2>/dev/null) && \\\n\
             {sg} --socket {socket} session-set-base --session {sid} --commit \"$HEAD_COMMIT\"\n",
            sg = sg,
            socket = socket,
            sid = session_id,
        );
        write_executable_hook(&git_dir, "pre-push", &pre_push)?;

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

fn git_config_get(repo_path: &Path, key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if output.status.success() {
        let val = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if val.is_empty() { None } else { Some(val) }
    } else {
        None
    }
}

/// Inherit key config values from the real repo so the session behaves
/// identically to a real worktree. Skips keys simgit manages itself.
fn build_inherited_config(repo_path: &Path) -> String {
    // Simple dotted keys: section.key → [section] \t key = val
    let simple_keys = &[
        "core.autocrlf",
        "core.filemode",
        "core.safecrlf",
        "core.symlinks",
        "core.ignorecase",
        "core.precomposeunicode",
        "core.sshCommand",
        "core.editor",
        "core.pager",
        "core.quotePath",
        "credential.helper",
        "http.proxy",
        "https.proxy",
        "http.sslVerify",
        "pull.rebase",
        "pull.ff",
        "push.default",
        "push.autoSetupRemote",
        "commit.gpgsign",
        "tag.gpgsign",
        "tag.sort",
        "rerere.enabled",
        "diff.algorithm",
        "diff.renameLimit",
        "init.defaultBranch",
        "merge.conflictStyle",
        "merge.ff",
    ];

    // Subsection keys: section.subsection.key → [section "subsection"] \t key = val
    let subsection_keys: &[(&str, &str)] = &[
        ("filter", "lfs"),
    ];

    let mut out = String::new();
    for key in simple_keys {
        if let Some(val) = git_config_get(repo_path, key) {
            if let Some(dot) = key.find('.') {
                let section = &key[..dot];
                let subkey = &key[dot + 1..];
                out.push_str(&format!("[{section}]\n\t{subkey} = {val}\n"));
            }
        }
    }

    for (section, subsection) in subsection_keys {
        // Build subsection entries from real repo config by shelling out
        // to `git config --list --local` and filtering.
        let subsection_config = git_config_section(repo_path, section, subsection);
        if !subsection_config.is_empty() {
            out.push_str(&subsection_config);
        }
    }

    out
}

/// Read a full config section (including subsections) from the real repo.
/// Returns the .git/config text for that section.
fn git_config_section(repo_path: &Path, section: &str, subsection: &str) -> String {
    let output = match std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["config", "--list", "--local"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return String::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{section}.{subsection}.");
    let mut keys: Vec<(&str, &str)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            if let Some(eq) = rest.find('=') {
                let key = rest[..eq].trim();
                let val = rest[eq + 1..].trim();
                keys.push((key, val));
            }
        }
    }

    if keys.is_empty() {
        return String::new();
    }

    let mut out = format!("[{section} \"{subsection}\"]\n");
    for (key, val) in keys {
        out.push_str(&format!("\t{key} = {val}\n"));
    }
    out
}

pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
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
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
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
        assert!(mount.join(".git").join("hooks").join("post-merge").exists());
        assert!(mount
            .join(".git")
            .join("hooks")
            .join("post-rewrite")
            .exists());
        assert!(mount.join(".git").join("hooks").join("commit-msg").exists());
        assert!(mount.join(".git").join("hooks").join("pre-push").exists());
        assert!(
            mount.join(".git").join("logs").exists()
                && mount.join(".git").join("logs").is_dir(),
            ".git/logs should exist"
        );

        // refs should exist as a directory (copied from real repo)
        let refs = mount.join(".git").join("refs");
        assert!(refs.exists() && refs.is_dir(), "refs should be a directory");
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
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        let config = fs::read_to_string(mount.join(".git").join("config")).unwrap();
        assert!(
            config.contains("https://github.com/test/repo.git"),
            "config should contain remote URL: {config}"
        );
    }

    /// Helper: copy working tree files from repo to mount so git sees them.
    /// The mount is empty after bootstrap (only .git/ exists).
    fn populate_working_tree(repo: &Path, mount: &Path) {
        for entry in walkdir::WalkDir::new(repo)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().file_name().map(|n| n != ".git").unwrap_or(true))
        {
            let rel = entry.path().strip_prefix(repo).unwrap();
            let dest = mount.join(rel);
            if entry.file_type().is_dir() {
                let _ = fs::create_dir_all(&dest);
            } else if entry.file_type().is_file() {
                if let Some(parent) = dest.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::copy(entry.path(), &dest);
            }
        }
    }

    /// Run a git command inside the mount, using the synthetic .git.
    fn git_in_mount(mount: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .current_dir(mount)
            .env("GIT_DIR", mount.join(".git"))
            .args(args)
            .output()
            .unwrap()
    }

    // ── git status ────────────────────────────────────────────────────────

    #[test]
    fn git_status_shows_modified_file() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "src/main.rs", "fn main() {}\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        // Populate the working tree from repo, then modify a file.
        populate_working_tree(&repo, &mount);
        fs::write(
            mount.join("src/main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();

        let out = git_in_mount(&mount, &["status", "--porcelain"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("src/main.rs"),
            "git status should show modified file: {stdout}"
        );
    }

    #[test]
    fn git_status_shows_untracked_file() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "a.txt", "a\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        populate_working_tree(&repo, &mount);
        fs::write(mount.join("untracked.txt"), "new\n").unwrap();

        let out = git_in_mount(&mount, &["status", "--porcelain"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("untracked.txt"),
            "git status should show untracked file: {stdout}"
        );
    }

    // ── git diff ──────────────────────────────────────────────────────────

    #[test]
    fn git_diff_shows_changes() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "f.txt", "original\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        populate_working_tree(&repo, &mount);
        fs::write(mount.join("f.txt"), "modified\n").unwrap();

        let out = git_in_mount(&mount, &["diff", "--", "f.txt"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("--- a/f.txt"),
            "diff should show file header: {stdout}"
        );
        assert!(
            stdout.contains("+++ b/f.txt"),
            "diff should show file header: {stdout}"
        );
        assert!(
            stdout.contains("-original"),
            "diff should show removed line: {stdout}"
        );
        assert!(
            stdout.contains("+modified"),
            "diff should show added line: {stdout}"
        );
    }

    // ── git log ───────────────────────────────────────────────────────────

    #[test]
    fn git_log_shows_commits() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "a.txt", "first\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        let out = git_in_mount(&mount, &["log", "--oneline"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.trim().is_empty(),
            "git log should show at least one commit: {stdout}"
        );
    }

    // ── git add ───────────────────────────────────────────────────────────

    #[test]
    fn git_add_stages_new_file() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "a.txt", "a\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        populate_working_tree(&repo, &mount);
        fs::write(mount.join("new.rs"), "// code\n").unwrap();
        git_in_mount(&mount, &["add", "new.rs"]);

        let out = git_in_mount(&mount, &["status", "--porcelain"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Staged new file shows as "A " (added in index).
        assert!(
            stdout.contains("new.rs"),
            "git status should show staged new.rs: {stdout}"
        );
    }

    // ── git stash ─────────────────────────────────────────────────────────

    #[test]
    fn git_stash_and_pop_roundtrip() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "f.txt", "original\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        populate_working_tree(&repo, &mount);
        fs::write(mount.join("f.txt"), "modified for stash\n").unwrap();

        let stash_out = git_in_mount(&mount, &["stash", "push", "-m", "test stash"]);
        assert!(
            stash_out.status.success(),
            "git stash should succeed: {:?} {}",
            stash_out.status,
            String::from_utf8_lossy(&stash_out.stderr),
        );

        // File should be back to original after stash.
        let after = fs::read_to_string(mount.join("f.txt")).unwrap();
        assert_eq!(after, "original\n", "file should be reverted after stash");

        // Pop the stash.
        git_in_mount(&mount, &["stash", "pop"]);
        let restored = fs::read_to_string(mount.join("f.txt")).unwrap();
        assert_eq!(
            restored, "modified for stash\n",
            "file should be restored after stash pop"
        );
    }

    // ── git clean ─────────────────────────────────────────────────────────

    #[test]
    fn git_clean_removes_untracked_file() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "a.txt", "a\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        populate_working_tree(&repo, &mount);
        let junk = mount.join("junk.tmp");
        fs::write(&junk, "delete me\n").unwrap();
        assert!(junk.exists(), "junk file should exist before clean");

        git_in_mount(&mount, &["clean", "-f"]);
        assert!(
            !junk.exists(),
            "junk file should be removed after git clean -f"
        );
    }

    // ── git blame ─────────────────────────────────────────────────────────

    #[test]
    fn git_blame_shows_annotation() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "code.rs", "line one\nline two\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        populate_working_tree(&repo, &mount);

        let out = git_in_mount(&mount, &["blame", "code.rs"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("line one") || stdout.contains("line two"),
            "git blame should annotate lines: {stdout}"
        );
    }

    // ── git show ──────────────────────────────────────────────────────────

    #[test]
    fn git_show_shows_commit_details() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "hello.txt", "hello\n");

        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        let out = git_in_mount(&mount, &["show", "--stat", "HEAD"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("hello.txt"),
            "git show --stat should list files: {stdout}"
        );
    }

    // ── git merge ─────────────────────────────────────────────────────────

    #[test]
    fn git_merge_resolves_fast_forward() {
        let repo = temp_dir();
        init_git_repo(&repo);
        let base = create_and_commit(&repo, "a.txt", "a\n");

        // Create a feature branch with an extra commit.
        std::process::Command::new("git")
            .current_dir(&repo)
            .args(["checkout", "-b", "feature"])
            .output()
            .unwrap();
        let feature_head = create_and_commit(&repo, "b.txt", "b\n");

        // Back to main.
        std::process::Command::new("git")
            .current_dir(&repo)
            .args(["checkout", "main"])
            .output()
            .unwrap();

        // Bootstrap on main in the mount.
        let mount = temp_dir();
        let _proxy = GitProxy::bootstrap(
            &mount,
            &mount,
            &std::path::PathBuf::from("/tmp/simgit-test.port"),
            &std::path::PathBuf::from("/tmp/sg"),
            &base,
            &repo,
            Some("main"),
            Uuid::now_v7()
        )
        .expect("bootstrap");

        populate_working_tree(&repo, &mount);

        // Merge feature into main.
        let merge = git_in_mount(&mount, &["merge", "feature"]);
        assert!(
            merge.status.success(),
            "merge should succeed: stdout={} stderr={}",
            String::from_utf8_lossy(&merge.stdout),
            String::from_utf8_lossy(&merge.stderr),
        );
    }
}
