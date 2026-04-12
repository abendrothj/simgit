use std::path::PathBuf;
use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Absolute path to the git repository root.
    pub repo_path: PathBuf,

    /// Where simgitd stores its SQLite database, delta blobs, and sockets.
    /// Defaults to `$XDG_STATE_HOME/simgit` or `~/.local/state/simgit`.
    pub state_dir: PathBuf,

    /// Where agent session mounts are created. Defaults to `$state_dir/mnt`.
    pub mnt_dir: PathBuf,

    /// Maximum number of concurrent ACTIVE sessions.
    pub max_sessions: usize,

    /// Maximum delta store size per session in bytes. Default 2 GiB.
    pub max_delta_bytes: u64,

    /// Default write-lock TTL in seconds (0 = no TTL). Default 3600.
    pub lock_ttl_seconds: u64,

    /// VFS backend to use.
    pub vfs_backend: VfsBackend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsBackend {
    /// FUSE (Linux) — uses `fuser` crate.
    Fuse,
    /// NFS loopback (macOS) — embedded NFSv3 server, no kernel extension.
    NfsLoopback,
}

impl Config {
    /// Load configuration from environment variables and defaults.
    /// In a future phase this will also read a `simgit.toml` from the repo.
    pub fn load() -> Result<Self> {
        let repo_path = std::env::var("SIMGIT_REPO")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let repo_path = repo_path
            .canonicalize()
            .with_context(|| format!("cannot canonicalize repo path: {}", repo_path.display()))?;

        let state_dir = std::env::var("SIMGIT_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_state_dir());

        let mnt_dir = state_dir.join("mnt");

        let vfs_backend = if cfg!(target_os = "macos") {
            VfsBackend::NfsLoopback
        } else {
            VfsBackend::Fuse
        };

        Ok(Self {
            repo_path,
            state_dir,
            mnt_dir,
            max_sessions:    256,
            max_delta_bytes: 2 * 1024 * 1024 * 1024,
            lock_ttl_seconds: 3600,
            vfs_backend,
        })
    }
}

fn default_state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("simgit");
    }
    dirs_home().join(".local").join("state").join("simgit")
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
