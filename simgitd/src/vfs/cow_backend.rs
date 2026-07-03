//! Native copy-on-write backend (default).
//!
//! # Overview
//!
//! Instead of serving every read/write through a user-space filesystem (FUSE /
//! NFS-loopback / WinFSP), this backend gives each session a **real working
//! tree**. Two materialization strategies, chosen per platform at runtime:
//!
//! - **Linux: overlayfs.** `lowerdir` = shared read-only baseline, per-session
//!   `upperdir` for writes. Only changed files land in the upper, so commit-time
//!   capture scans the upper (`O(changes)`), and deletions appear as whiteouts.
//!   Falls back to the clone strategy if the overlay mount is not permitted.
//! - **macOS / other: CoW clone.** APFS `clonefile` (via `cp -c`) or
//!   `cp --reflink=auto`; capture diffs the tree with `git status`.
//!
//! Reads and writes are ordinary filesystem operations served by the kernel
//! page cache — native latency, no per-op RPC/upcall. The disk-scaling benefit
//! is preserved: one shared baseline per `base_commit` + per-session CoW deltas.
//!
//! # Trade-off vs the VFS backends
//!
//! There is no synchronous write-time borrow-checking here. Instead,
//! [`capture_mount_delta`](CowBackend::capture_mount_delta) records what changed
//! into the delta store at commit time, and the commit scheduler performs
//! path-level conflict detection there. Same no-corruption guarantee, detected
//! at commit rather than rejected at write.

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

/// How a session's working tree is materialized, tracked so capture and
/// teardown can pick the matching strategy.
enum Materialization {
    /// Linux overlayfs mount; capture scans `upper`.
    #[cfg(target_os = "linux")]
    Overlay { session_root: PathBuf, upper: PathBuf },
    /// CoW clone (clonefile / reflink) or plain copy; capture uses `git status`.
    Clone,
}

struct MountState {
    mount: PathBuf,
    base_commit: String,
    materialization: Materialization,
}

/// A materialized baseline is evicted once no active session references it and
/// it has been idle (unreferenced) for at least this long. Re-materializing is
/// cheap (`git archive | tar`), so this only trades a little rework for bounded
/// disk in `state_dir/baselines`.
const BASELINE_IDLE_TTL_SECS: u64 = 3600;

pub struct CowBackend {
    cfg: Arc<Config>,
    deltas: Arc<DeltaStore>,
    #[allow(dead_code)]
    borrows: Arc<BorrowRegistry>,
    #[allow(dead_code)]
    metrics: Arc<Metrics>,
    mounts: Mutex<HashMap<Uuid, MountState>>,
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
    /// mutated. Subsequent sessions on the same base reuse this directory.
    fn ensure_baseline(&self, base_commit: &str) -> Result<PathBuf> {
        let baselines = self.cfg.state_dir.join("baselines");
        let dir = baselines.join(base_commit);
        let ready = baselines.join(format!("{base_commit}.ready"));
        if ready.exists() && dir.exists() {
            // Bump the marker's mtime so idle-eviction (see `gc_baselines`)
            // treats a reused baseline as recently active.
            let _ = std::fs::write(&ready, b"");
            return Ok(dir);
        }
        std::fs::create_dir_all(&baselines)
            .with_context(|| format!("create baselines dir {}", baselines.display()))?;

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

        if dir.exists() {
            let _ = std::fs::remove_dir_all(&tmp);
        } else if let Err(e) = std::fs::rename(&tmp, &dir) {
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

    /// Evict materialized baselines that no active session references and that
    /// have been idle longer than [`BASELINE_IDLE_TTL_SECS`]. Keeps
    /// `state_dir/baselines` bounded without losing warm reuse.
    fn gc_baselines(&self) {
        let baselines = self.cfg.state_dir.join("baselines");
        let Ok(entries) = std::fs::read_dir(&baselines) else {
            return;
        };
        // base_commits still in use by a live session.
        let active: std::collections::HashSet<String> = {
            let guard = self.mounts.lock().unwrap();
            guard.values().map(|s| s.base_commit.clone()).collect()
        };
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            // Only consider `<base>.ready` markers; each pairs with a `<base>` dir.
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(base) = name.strip_suffix(".ready") else {
                continue;
            };
            if active.contains(base) {
                continue;
            }
            let idle = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if idle < BASELINE_IDLE_TTL_SECS {
                continue;
            }
            let dir = baselines.join(base);
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(&path);
            info!(base = base, idle_secs = idle, "evicted idle CoW baseline");
        }
    }

    /// Try to materialize the working tree, returning how it was done.
    fn materialize(&self, baseline: &Path, mount: &Path, session_id: Uuid) -> Result<Materialization> {
        #[cfg(target_os = "linux")]
        {
            match self.overlay_mount(baseline, mount, session_id) {
                Ok(m) => return Ok(m),
                Err(e) => {
                    warn!(
                        session = %session_id,
                        err = %e,
                        "overlayfs mount unavailable; falling back to reflink clone"
                    );
                }
            }
        }
        let _ = session_id;
        if mount.exists() {
            std::fs::remove_dir_all(mount).ok();
        }
        clone_tree(baseline, mount)?;
        Ok(Materialization::Clone)
    }

    #[cfg(target_os = "linux")]
    fn overlay_mount(
        &self,
        baseline: &Path,
        mount: &Path,
        session_id: Uuid,
    ) -> Result<Materialization> {
        let session_root = self.cfg.mnt_dir.join(format!("{session_id}.overlay"));
        let upper = session_root.join("upper");
        let work = session_root.join("work");
        std::fs::create_dir_all(&upper)?;
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(mount)?;

        let opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            baseline.display(),
            upper.display(),
            work.display()
        );
        let out = Command::new("mount")
            .args(["-t", "overlay", "overlay", "-o", &opts])
            .arg(mount)
            .output()
            .context("spawn overlay mount")?;
        if !out.status.success() {
            let _ = std::fs::remove_dir_all(&session_root);
            anyhow::bail!(
                "overlay mount failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Materialization::Overlay {
            session_root,
            upper,
        })
    }

    /// Capture via `git status` (clone strategy): compares the working tree to
    /// the index (initialized from base at bootstrap).
    fn capture_git_status(&self, session: &SessionInfo) -> Result<()> {
        let mount = &session.mount_path;
        let git_dir = Self::git_dir_for(mount);
        if !git_dir.exists() {
            return Ok(());
        }
        self.deltas
            .reset_session(session.session_id, &session.base_commit)?;

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
        for record in out.stdout.split(|b| *b == 0) {
            if record.len() < 4 {
                continue;
            }
            let rel = PathBuf::from(String::from_utf8_lossy(&record[3..]).into_owned());
            let full = mount.join(&rel);
            if full.is_file() {
                let content = std::fs::read(&full)
                    .with_context(|| format!("read changed file {}", full.display()))?;
                self.deltas
                    .write_blob(session.session_id, &rel, &content, None)?;
            } else if !full.exists() {
                self.deltas.mark_deleted(session.session_id, &rel)?;
            }
        }
        Ok(())
    }

    /// Capture via overlayfs upperdir scan: only changed files live there.
    /// Regular files are writes; overlay whiteouts (char device 0:0) are
    /// deletions. `O(changes)` rather than `O(tree)`.
    #[cfg(target_os = "linux")]
    fn capture_upperdir(&self, session: &SessionInfo, upper: &Path) -> Result<()> {
        self.deltas
            .reset_session(session.session_id, &session.base_commit)?;
        let mut stack = vec![upper.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in rd.flatten() {
                let path = entry.path();
                let rel = match path.strip_prefix(upper) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                // Skip the bootstrapped .git — not part of the tracked tree.
                if rel.starts_with(".git") {
                    continue;
                }
                let md = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if is_whiteout(&md) {
                    self.deltas.mark_deleted(session.session_id, &rel)?;
                } else if md.is_dir() {
                    stack.push(path);
                } else if md.is_file() {
                    let content = std::fs::read(&path)
                        .with_context(|| format!("read upper file {}", path.display()))?;
                    self.deltas
                        .write_blob(session.session_id, &rel, &content, None)?;
                }
                // symlinks / other types ignored for v1.
            }
        }
        Ok(())
    }
}

/// Copy-on-write clone a directory tree. `dst` must not already exist.
///
/// - macOS: `cp -c` (APFS `clonefile`, block-shared).
/// - other Unix: `cp --reflink=auto` (reflink on btrfs/xfs, else a full copy).
/// - Windows: recursive `std::fs` copy (a full copy — correct, but not
///   block-shared; ReFS `FSCTL_DUPLICATE_EXTENTS_TO_FILE` is a future
///   optimization). Windows defaults to WinFSP, so this path is opt-in.
#[cfg(unix)]
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

#[cfg(windows)]
fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    copy_dir_recursive(src, dst)
        .with_context(|| format!("copy clone {} -> {}", src.display(), dst.display()))
}

#[cfg(windows)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// An overlayfs whiteout is a character device with rdev 0:0.
#[cfg(target_os = "linux")]
fn is_whiteout(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    md.file_type().is_char_device() && md.rdev() == 0
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
        let materialization = self.materialize(&baseline, &mount, session.session_id)?;

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

        self.mounts.lock().unwrap().insert(
            session.session_id,
            MountState {
                mount: mount.clone(),
                base_commit: session.base_commit.clone(),
                materialization,
            },
        );

        info!(
            session = %session.session_id,
            path = %mount.display(),
            "CoW working tree materialized"
        );
        Ok(())
    }

    async fn unmount(&self, session_id: Uuid) -> Result<()> {
        let state = self.mounts.lock().unwrap().remove(&session_id);
        let Some(state) = state else {
            let path = self.cfg.mnt_dir.join(session_id.to_string());
            if path.exists() {
                let _ = std::fs::remove_dir_all(&path);
            }
            self.gc_baselines();
            return Ok(());
        };

        match state.materialization {
            #[cfg(target_os = "linux")]
            Materialization::Overlay { session_root, .. } => {
                let _ = Command::new("umount").arg(&state.mount).status();
                if let Err(e) = std::fs::remove_dir_all(&session_root) {
                    warn!(session = %session_id, err = %e, "failed to remove overlay dirs");
                }
                let _ = std::fs::remove_dir_all(&state.mount);
            }
            Materialization::Clone => {
                if let Err(e) = std::fs::remove_dir_all(&state.mount) {
                    warn!(session = %session_id, err = %e, "failed to remove CoW working tree");
                }
            }
        }
        self.gc_baselines();
        Ok(())
    }

    fn capture_mount_delta(&self, session: &SessionInfo) -> Result<()> {
        if !session.mount_path.exists() {
            return Ok(());
        }
        // Pick the capture strategy that matches how this session was mounted.
        #[cfg(target_os = "linux")]
        {
            let upper = {
                let guard = self.mounts.lock().unwrap();
                match guard.get(&session.session_id).map(|s| &s.materialization) {
                    Some(Materialization::Overlay { upper, .. }) => Some(upper.clone()),
                    _ => None,
                }
            };
            if let Some(upper) = upper {
                return self.capture_upperdir(session, &upper);
            }
        }
        self.capture_git_status(session)
    }
}

/// Absolute path to the `sg` CLI binary, which lives next to `simgitd`.
fn sg_binary_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sg")))
        .unwrap_or_else(|| PathBuf::from("sg"))
}
