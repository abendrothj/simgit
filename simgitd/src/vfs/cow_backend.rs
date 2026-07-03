//! Native copy-on-write backend (default).
//!
//! # Overview
//!
//! Instead of serving every read/write through a user-space filesystem (FUSE /
//! NFS-loopback / WinFSP), this backend gives each session a **real working
//! tree** that is a copy-on-write clone of a shared baseline:
//!
//! - **macOS**: APFS `clonefile` (via `cp -c`) — blocks shared until written.
//! - **Linux / other**: `cp --reflink=auto` — reflink CoW on btrfs/xfs, a plain
//!   copy elsewhere (still correct, just not space-shared).
//!
//! Reads and writes are ordinary filesystem operations served by the kernel
//! page cache — native latency, no per-op RPC/upcall. The disk-scaling benefit
//! is preserved: one shared baseline per `base_commit` + per-session CoW deltas.
//!
//! # Trade-off vs the VFS backends
//!
//! There is no synchronous write-time borrow-checking here (nothing intercepts
//! writes). Instead, [`capture_mount_delta`](CowBackend::capture_mount_delta)
//! diffs the working tree into the delta store at commit time, and the existing
//! commit scheduler performs path-level conflict detection there. Same
//! no-corruption guarantee, detected at commit rather than rejected at write.
//!
//! Because the session is a real git working tree (a `.git` is bootstrapped
//! inside it), `git checkout`/`merge`/`rebase` work natively with no special
//! base-commit tracking — the index and working tree stay consistent on their
//! own; only `git commit` is forwarded to the daemon by the pre-commit hook.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tracing::{info, warn};
use uuid::Uuid;

use simgit_sdk::SessionInfo;

use crate::borrow::BorrowRegistry;
use crate::config::Config;
use crate::delta::DeltaStore;
use crate::metrics::Metrics;

pub struct CowBackend {
    cfg: Arc<Config>,
    deltas: Arc<DeltaStore>,
    #[allow(dead_code)]
    borrows: Arc<BorrowRegistry>,
    #[allow(dead_code)]
    metrics: Arc<Metrics>,
    /// session_id → mount_path, for teardown.
    mounts: Mutex<HashMap<Uuid, PathBuf>>,
}

impl CowBackend {
    pub fn new(
        cfg: Arc<Config>,
        deltas: Arc<DeltaStore>,
        borrows: Arc<BorrowRegistry>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            cfg,
            deltas,
            borrows,
            metrics,
            mounts: Mutex::new(HashMap::new()),
        }
    }

    /// Materialize (once) a shared read-only baseline checkout of `base_commit`
    /// and return its path. Uses `git archive | tar` so the real repo is never
    /// mutated. Subsequent sessions on the same base clone this directory.
    fn ensure_baseline(&self, base_commit: &str) -> Result<PathBuf> {
        let baselines = self.cfg.state_dir.join("baselines");
        let dir = baselines.join(base_commit);
        let ready = baselines.join(format!("{base_commit}.ready"));
        if ready.exists() && dir.exists() {
            return Ok(dir);
        }
        std::fs::create_dir_all(&baselines).with_context(|| {
            format!("create baselines dir {}", baselines.display())
        })?;

        let tmp = baselines.join(format!("{base_commit}.tmp-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&tmp)?;
        let tar = baselines.join(format!("{base_commit}.tar-{}", Uuid::now_v7()));

        let st = Command::new("git")
            .current_dir(&self.cfg.repo_path)
            .arg("archive")
            .arg("-o")
            .arg(&tar)
            .arg(base_commit)
            .status()
            .context("git archive")?;
        if !st.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            let _ = std::fs::remove_file(&tar);
            anyhow::bail!("git archive failed for base {base_commit}");
        }

        let st = Command::new("tar")
            .arg("-xf")
            .arg(&tar)
            .arg("-C")
            .arg(&tmp)
            .status()
            .context("tar extract baseline")?;
        let _ = std::fs::remove_file(&tar);
        if !st.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            anyhow::bail!("tar extract failed for base {base_commit}");
        }

        // Publish atomically; if another thread won the race, drop our tmp.
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&tmp);
        } else if let Err(e) = std::fs::rename(&tmp, &dir) {
            // Lost the race (dir now exists) or a real error.
            let _ = std::fs::remove_dir_all(&tmp);
            if !dir.exists() {
                return Err(e).context("publish baseline");
            }
        }
        std::fs::write(&ready, b"")?;
        Ok(dir)
    }

    fn git_dir_for(mount: &Path) -> PathBuf {
        mount.join(".git")
    }
}

/// Copy-on-write clone a directory tree. `dst` must not already exist.
fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    let mut cmd = Command::new("cp");
    #[cfg(target_os = "macos")]
    cmd.arg("-c").arg("-R");
    #[cfg(not(target_os = "macos"))]
    cmd.arg("--reflink=auto").arg("-R");
    let st = cmd
        .arg(src)
        .arg(dst)
        .status()
        .with_context(|| format!("cp clone {} -> {}", src.display(), dst.display()))?;
    if !st.success() {
        anyhow::bail!("cp clone failed: {} -> {}", src.display(), dst.display());
    }
    Ok(())
}

#[async_trait::async_trait]
impl super::VfsBackendTrait for CowBackend {
    async fn mount(&self, session: &SessionInfo) -> Result<()> {
        let mount = session.mount_path.clone();
        if let Some(parent) = mount.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create mount parent {}", parent.display()))?;
        }

        let baseline = self.ensure_baseline(&session.base_commit)?;

        // `cp` requires the destination not to exist.
        if mount.exists() {
            std::fs::remove_dir_all(&mount).ok();
        }
        clone_tree(&baseline, &mount)?;

        // Bootstrap a real .git *inside* the working tree so native git commands
        // (status/checkout/commit) work and the pre-commit hook forwards commits
        // to the daemon. Passing `mount` as the git-data dir puts .git in-tree.
        if session.git_proxy_enabled {
            let socket_path = self.cfg.state_dir.join("control.port");
            crate::git_proxy::GitProxy::bootstrap(
                &mount,
                &mount,
                &socket_path,
                &sg_binary_path(),
                &session.base_commit,
                &self.cfg.repo_path,
                session.initial_branch.as_deref(),
                session.session_id,
            )?;
        }

        self.mounts
            .lock()
            .unwrap()
            .insert(session.session_id, mount.clone());

        info!(
            session = %session.session_id,
            path = %mount.display(),
            "CoW working tree materialized"
        );
        Ok(())
    }

    async fn unmount(&self, session_id: Uuid) -> Result<()> {
        let path = self.mounts.lock().unwrap().remove(&session_id);
        let path = path.unwrap_or_else(|| self.cfg.mnt_dir.join(session_id.to_string()));
        if path.exists() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                warn!(session = %session_id, err = %e, "failed to remove CoW working tree");
            }
        }
        Ok(())
    }

    /// Diff the working tree against its base and materialize the changes into
    /// the delta store, so the commit scheduler + flatten see exactly what the
    /// session changed. Called at commit time (for the committing session and
    /// for each active peer, so overlaps are detected).
    fn capture_mount_delta(&self, session: &SessionInfo) -> Result<()> {
        let mount = &session.mount_path;
        if !mount.exists() {
            return Ok(());
        }
        let git_dir = Self::git_dir_for(mount);
        if !git_dir.exists() {
            // No bootstrapped git (git_proxy disabled) — nothing to diff against.
            return Ok(());
        }

        // Fresh manifest: the working tree is the single source of truth.
        self.deltas
            .reset_session(session.session_id, &session.base_commit)?;

        // `git status` compares the working tree against the index, which was
        // initialized from base_commit at bootstrap (and is kept correct by any
        // native checkout/merge). --no-renames so a rename shows as delete+add.
        let out = Command::new("git")
            .env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", mount)
            .args(["status", "--porcelain=v1", "-z", "--no-renames"])
            .output()
            .context("git status for CoW capture")?;
        if !out.status.success() {
            anyhow::bail!(
                "git status failed during capture: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // Records are NUL-separated: "XY <path>" (2 status chars, a space, path).
        for record in out.stdout.split(|b| *b == 0) {
            if record.len() < 4 {
                continue;
            }
            let path_bytes = &record[3..];
            let rel = PathBuf::from(String::from_utf8_lossy(path_bytes).into_owned());
            let full = mount.join(&rel);
            if full.is_file() {
                let content = std::fs::read(&full)
                    .with_context(|| format!("read changed file {}", full.display()))?;
                self.deltas
                    .write_blob(session.session_id, &rel, &content, None)?;
            } else if !full.exists() {
                // Tracked file removed from the working tree → deletion.
                self.deltas.mark_deleted(session.session_id, &rel)?;
            }
            // Directories and other types are ignored (git tracks files).
        }
        Ok(())
    }
}

/// Absolute path to the `sg` CLI binary, which lives next to `simgitd`.
fn sg_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sg")))
        .unwrap_or_else(|| PathBuf::from("sg"))
}
