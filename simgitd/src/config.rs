//! Daemon configuration — loaded from environment and defaults.
//!
//! # Overview
//!
//! Centralizes all daemon configuration options:
//! - Repository location (git workspace to serve)
//! - State directory (SQLite database, delta blobs, sockets)
//! - Session limits and quotas
//! - VFS backend selection (FUSE vs. NFS-loopback)
//!
//! # Configuration Sources
//!
//! Priority (high to low):
//! 1. Environment variables (`SIMGIT_REPO`, `SIMGIT_STATE_DIR`)
//! 2. XDG_STATE_HOME (`$XDG_STATE_HOME/simgit` on Linux)
//! 3. Home directory (`~/.local/state/simgit` on Linux, macOS)
//! 4. Hardcoded defaults (repo = cwd, max_sessions = 256, etc.)
//!
//! # Example
//!
//! ```bash
//! export SIMGIT_REPO=/home/alice/myproject
//! export SIMGIT_STATE_DIR=/var/lib/simgit
//! simgitd
//! ```
//!
//! # Directory Structure
//!
//! ```text\n//! $state_dir/\n//!   ├── db.sqlite         (sessions, locks, metadata)\n//!   ├── blobs/             (delta content-addressed storage)\n//!   │   └── <hash>/\n//!   ├── mnt/               (session mount points)\n//!   │   ├── <session-id>/\n//!   │   └── <session-id>/\n//!   └── simgitd.sock      (control socket)\n//! ```
//!
//! # Phase Roadmap
//!
//! - **Phase 0**: Environment variables + hardcoded defaults
//! - **Phase 1**: Read `simgit.toml` from repo root (custom limits, git auth)
//! - **Phase 2+**: Config file schema versioning, reload on SIGHUP
//!
//! # Security
//!
//! - `repo_path` must be readable by daemon user
//! - `state_dir` must be writable and mode 0700 (prevent other users from accessing blobs/sockets)
//! - `mnt_dir` is mode 0755 (per session mounts are 0700)

use std::path::PathBuf;
use anyhow::{Context, Result};

/// Daemon configuration.
///
/// # Fields
///
/// - **repo_path**: Absolute path to git repository (canonicalized).
///   Must be a valid git repo with `HEAD` commit resolvable.
/// - **state_dir**: Daemon state directory (SQLite, blobs, sockets, mounts).
///   Created on first run; should be mode 0700.
/// - **mnt_dir**: Subdirectory of state_dir where agent mounts are created.
///   Each session gets a subdirectory.
/// - **max_sessions**: Maximum concurrent ACTIVE sessions (soft limit; enforced
///   by RPC `session.create()` when capacity exceeded).
/// - **max_delta_bytes**: Maximum bytes of delta writes per session (soft limit;
///   enforced by delta store write path). Default 2 GiB.
/// - **lock_ttl_seconds**: Default TTL for write locks (seconds). 0 = no TTL
///   (locks never auto-expire). Default 3600 (1 hour).
/// - **vfs_backend**: Backend VFS implementation (platform-specific selection).
///
/// # Example
///
/// ```ignore
/// let cfg = Config::load()?;
/// println!("Repo: {}", cfg.repo_path.display());
/// println!("State: {}", cfg.state_dir.display());
/// println!("VFS Backend: {:?}", cfg.vfs_backend);
/// ```
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

    /// How long a recovered ACTIVE session is allowed to keep its ACTIVE status
    /// after a crash before the startup sweep marks it STALE.
    /// Default 86400 seconds (24 hours). 0 = keep indefinitely (not recommended).
    pub session_recovery_ttl_seconds: u64,

    /// VFS backend to use.
    pub vfs_backend: VfsBackend,

    /// Enable embedded Prometheus endpoint.
    pub metrics_enabled: bool,

    /// Listen address for Prometheus metrics endpoint.
    pub metrics_addr: String,

    /// Max number of peer sessions to capture concurrently during commit.
    pub commit_peer_capture_concurrency: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsBackend {
    /// FUSE (Linux primary).
    ///
    /// Uses the `fuser` crate to handle filesystem requests.
    /// Requires FUSE kernel module loaded (present on all Linux distros).
    /// No kernel extension or user approval needed.
    ///
    /// See [crate::vfs::fuse_backend].
    Fuse,

    /// NFS-loopback (macOS Phase 0, stub; Phase 1+, full NFSv3 server).
    ///
    /// Phase 0: Creates plain directories (no kernel involvement).
    /// Phase 1+: Embeds an NFSv3 RPC server to avoid macFUSE kernel extension.
    ///
    /// See [crate::vfs::nfs_backend].
    NfsLoopback,
}

impl Config {
    /// Load configuration from environment variables and defaults.
    ///
    /// # Environment Variables
    ///
    /// - `SIMGIT_REPO`: Path to git repository (defaults to cwd)
    /// - `SIMGIT_STATE_DIR`: State directory (defaults to `$XDG_STATE_HOME/simgit`)
    ///
    /// # VFS Backend Selection
    ///
    /// Auto-detected by platform:
    /// - Linux: FUSE
    /// - macOS: NFS-loopback
    /// - Other: FUSE (requires user configuration)
    ///
    /// # Returns
    ///
    /// Fully initialized `Config` struct with canonicalized paths.
    ///
    /// # Errors
    ///
    /// - Repository path not found or not readable
    /// - Repository path is not a valid git repository
    /// - Cannot determine state directory
    ///
    /// # Future
    ///
    /// Phase 1 will also read a `simgit.toml` config file from repo root,
    /// allowing per-repo overrides (git auth, cache sizes, etc.).
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
            session_recovery_ttl_seconds: std::env::var("SIMGIT_SESSION_RECOVERY_TTL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(86400),
            vfs_backend,
            metrics_enabled: std::env::var("SIMGIT_METRICS_ENABLED")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            metrics_addr: std::env::var("SIMGIT_METRICS_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:9100".to_owned()),
            commit_peer_capture_concurrency: std::env::var("SIMGIT_COMMIT_PEER_CAPTURE_CONCURRENCY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(8),
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
