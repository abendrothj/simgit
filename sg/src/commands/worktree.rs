//! `sg worktree` — drop-in replacement for `git worktree`.
//!
//! # Commands
//!
//! - **add** `<branch>` `[path]` — create a session, auto-start daemon, print mount path
//! - **remove** `[path|session]` — abort (or commit) a session, unmount, clean up
//! - **list** — show all active sessions in worktree style
//!
//! # Examples
//!
//! ```bash
//! cd $(sg worktree add feat/my-branch)
//! # ... agent works ...
//! git commit -m "done"      # forwarded to sg commit via hook
//! sg worktree remove        # from inside the mount
//! ```

use clap::{Args, Subcommand};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use simgit_sdk::Client;

// ---------------------------------------------------------------------------
// CLI types
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum Worktree {
    /// Create a new worktree session on a branch.
    Add(WorktreeAdd),
    /// Remove a worktree session (abort or commit + cleanup).
    Remove(WorktreeRemove),
    /// List active worktree sessions.
    List(WorktreeList),
}

#[derive(Args)]
pub struct WorktreeAdd {
    /// Branch name to create (e.g. feat/my-feature).
    pub branch: String,

    /// Optional mount path. Defaults to `.git/simgit/worktrees/<branch>`.
    pub path: Option<String>,

    /// Base commit to start from. Defaults to HEAD.
    #[arg(long)]
    pub base: Option<String>,
}

#[derive(Args)]
pub struct WorktreeRemove {
    /// Session ID or mount path to remove.
    /// Defaults to the session that owns the current directory.
    pub target: Option<String>,

    /// Commit uncommitted changes before removing.
    #[arg(long)]
    pub commit: bool,

    /// Commit message for --commit.
    #[arg(short, long, default_value = "simgit worktree remove")]
    pub message: String,
}

#[derive(Args)]
pub struct WorktreeList {
    /// JSON output.
    #[arg(long)]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(cmd: Worktree, socket_override: &Option<String>, json: bool) -> Result<()> {
    let socket = resolve_socket(socket_override)?;
    match cmd {
        Worktree::Add(a) => add(a, &socket).await,
        Worktree::Remove(r) => remove(r, &socket).await,
        Worktree::List(l) => list(l, &socket, json).await,
    }
}

/// Resolve the daemon socket path. If `--socket` is explicitly provided,
/// use it. Otherwise, discover from the git repository root.
fn resolve_socket(socket_override: &Option<String>) -> Result<String> {
    if let Some(ref s) = socket_override {
        return Ok(s.clone());
    }
    match find_repo_root() {
        Ok(repo) => Ok(repo_port_file(&repo).to_string_lossy().into_owned()),
        Err(_) => {
            // No repo — fall back to global default.
            Ok(simgit_sdk::client::default_port_file()
                .to_string_lossy()
                .into_owned())
        }
    }
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

async fn add(cmd: WorktreeAdd, socket: &str) -> Result<()> {
    let repo_root = find_repo_root()?;
    ensure_daemon(&repo_root, socket).await?;

    let client = Client::new(socket);
    let base = cmd.base.unwrap_or_else(|| "HEAD".to_owned());
    let resolved_base = resolve_head_or_commit(&repo_root, &base)?;

    let info = client
        .session_create(
            format!("worktree:{}", cmd.branch),
            Some(format!("worktree:{}", cmd.branch)),
            Some(resolved_base),
            false,
            Some(cmd.branch.clone()),
        )
        .await
        .context("session.create RPC")?;

    let mount = cmd.path.as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| mount_default_path(&repo_root, &cmd.branch));

    // Always print the session ID on stderr so scripts can capture the
    // mount path from stdout and still see the session ID.
    eprintln!("session: {}", info.session_id);

    println!("{}", info.mount_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

async fn remove(cmd: WorktreeRemove, socket: &str) -> Result<()> {
    let session_id = if let Some(ref target) = cmd.target {
        // Try parsing as UUID first, otherwise treat as path.
        if let Ok(id) = target.parse::<uuid::Uuid>() {
            id
        } else {
            find_session_by_path(socket, &PathBuf::from(target)).await?
        }
    } else {
        let cwd = std::env::current_dir()?;
        find_session_by_cwd(socket, &cwd).await?
    };

    let client = Client::new(socket);

    if cmd.commit {
        let result = client
            .session_commit(session_id, None, Some(cmd.message))
            .await
            .context("session.commit RPC")?;
        println!(
            "Committed to branch {}",
            result.session.branch_name.as_deref().unwrap_or("(none)")
        );
    } else {
        client.session_abort(session_id).await.context("session.abort RPC")?;
        println!("Session {} aborted", session_id);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(_cmd: WorktreeList, socket: &str, json: bool) -> Result<()> {
    ensure_daemon_connectable(socket).await?;
    let client = Client::new(socket);
    let sessions = client.session_list(None).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }

    if sessions.is_empty() {
        println!("(no active worktrees)");
        return Ok(());
    }

    // Column widths
    let max_branch = sessions.iter()
        .filter_map(|s| s.branch_name.as_deref())
        .map(|b| b.len())
        .max()
        .unwrap_or(10)
        .max(10);

    println!("{:<max_branch$}  {}  PATH", "BRANCH", "STATUS", max_branch = max_branch);
    for s in &sessions {
        let branch = s.branch_name.as_deref().unwrap_or("(detached)");
        let status = match s.status {
            simgit_sdk::SessionStatus::Active => "active",
            simgit_sdk::SessionStatus::Committed => "committed",
            simgit_sdk::SessionStatus::Aborted => "aborted",
            simgit_sdk::SessionStatus::Stale => "stale",
        };
        println!("{:<max_branch$}  {:<8}  {}", branch, status, s.mount_path.display(), max_branch = max_branch);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon lifecycle helpers
// ---------------------------------------------------------------------------

/// Find the repository root from CWD.
///
/// Walks up the directory tree looking for a `.git` *directory* (not a file)
/// so that simgit session mounts (which contain a `.git` gitdir-pointer file)
/// are skipped in favour of the real repository above them.
fn find_repo_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    bail!("not in a git repository (no .git/ directory found)");
}

/// Resolve HEAD or a commit-ish to a full SHA.
fn resolve_head_or_commit(repo: &Path, base: &str) -> Result<String> {
    if base == "HEAD" {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .context("git rev-parse HEAD")?;
        if !output.status.success() {
            bail!("cannot resolve HEAD in {}", repo.display());
        }
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    // For non-HEAD, check if it resolves to a commit.
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", &format!("{}^{{commit}}", base)])
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned());
        }
    }
    // Fallback: try as raw commit SHA.
    let output = std::process::Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", base])
        .output()
        .context("git rev-parse")?;
    if !output.status.success() {
        bail!("cannot resolve base commit '{}' in {}", base, repo.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Default mount path: `$repo/.git/simgit/worktrees/<branch>`.
fn mount_default_path(repo: &Path, branch: &str) -> PathBuf {
    repo.join(".git").join("simgit").join("worktrees").join(branch)
}

/// Generate the state-dir path for a repo: `$repo/.git/simgit`.
fn repo_state_dir(repo: &Path) -> PathBuf {
    repo.join(".git").join("simgit")
}

/// Generate the port-file path for a repo.
fn repo_port_file(repo: &Path) -> PathBuf {
    repo_state_dir(repo).join("control.port")
}

/// Try connecting to the daemon. Returns Ok(()) if the daemon responds,
/// or an error if it's unreachable.
async fn ensure_daemon_connectable(socket: &str) -> Result<()> {
    let client = Client::new(socket);
    // Use a short internal timeout by doing a quick session list.
    match client.session_list(None).await {
        Ok(_) => Ok(()),
        Err(_) => bail!("daemon not reachable at {}", socket),
    }
}

/// Ensure the daemon is running for this repo. If the port file exists and
/// the daemon responds, return. Otherwise, spawn a new daemon.
async fn ensure_daemon(repo: &Path, socket: &str) -> Result<()> {
    if ensure_daemon_connectable(socket).await.is_ok() {
        return Ok(());
    }

    let state_dir = repo_state_dir(repo);
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("create {}", state_dir.display()))?;

    // Find simgitd binary next to sg.
    let sg_exe = std::env::current_exe().context("current_exe")?;
    let simgitd_exe = sg_exe
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("simgitd");

    let child = std::process::Command::new(&simgitd_exe)
        .env("SIMGIT_REPO", repo)
        .env("SIMGIT_STATE_DIR", &state_dir)
        .env("SIMGIT_METRICS_ENABLED", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawn simgitd at {}", simgitd_exe.display()))?;

    let pid = child.id();
    // Wait for the port file to appear.
    let port_file = repo_port_file(repo);
    for _ in 0..50 {
        if port_file.exists() && std::fs::read_to_string(&port_file).map(|s| !s.trim().is_empty()).unwrap_or(false) {
            // Verify the daemon is actually responding.
            if ensure_daemon_connectable(socket).await.is_ok() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Daemon didn't come up. Kill the child.
    let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    bail!("daemon did not start within 10s (pid {})", pid);
}

// ---------------------------------------------------------------------------
// Session discovery helpers
// ---------------------------------------------------------------------------

/// Find the session that owns `cwd` by walking up the directory tree and
/// matching against known sessions.
async fn find_session_by_cwd(socket: &str, cwd: &Path) -> Result<uuid::Uuid> {
    let client = Client::new(socket);
    let sessions = client.session_list(None).await?;

    let mut dir = cwd.to_path_buf();
    loop {
        for s in &sessions {
            if dir == s.mount_path || dir.starts_with(&s.mount_path) {
                return Ok(s.session_id);
            }
        }
        if !dir.pop() {
            break;
        }
    }

    bail!("no simgit session found for {}", cwd.display());
}

/// Find a session by its mount path.
async fn find_session_by_path(socket: &str, path: &Path) -> Result<uuid::Uuid> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    find_session_by_cwd(socket, &abs).await
}
