//! Shared `#[cfg(test)]` fixtures used by both `commit::tests` and `locks::tests`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::borrow::BorrowRegistry;
use crate::config::{Config, VfsBackend};
use crate::daemon::AppState;
use crate::delta::DeltaStore;
use crate::events::EventBroker;
use crate::session::SessionManager;
use crate::vfs::VfsManager;

static TEST_STATE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(super) fn temp_state_root() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let seq = TEST_STATE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "simgit-rpc-commit-test-{}-{}-{}",
        std::process::id(),
        nanos,
        seq
    ))
}

pub(super) fn run_git(repo: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("git command should execute");
    assert!(status.success(), "git {:?} failed", args);
}

pub(super) fn init_repo(root: &std::path::Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");
    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "tests@simgit.local"]);
    run_git(&repo, &["config", "user.name", "simgit-tests"]);
    std::fs::write(repo.join("README.md"), b"base\n").expect("write readme");
    std::fs::write(repo.join("src.txt"), b"base-src\n").expect("write src");
    std::fs::write(repo.join("merge.txt"), b"line1\nline2\nline3\n").expect("write merge fixture");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "init"]);
    repo
}

pub(super) async fn build_state_for_commit_tests() -> (Arc<AppState>, std::path::PathBuf) {
    let root = temp_state_root();
    let repo = init_repo(&root);
    let state_dir = root.join("state");
    let mnt_dir = state_dir.join("mnt");
    std::fs::create_dir_all(&mnt_dir).expect("create mnt");

    let cfg = Arc::new(Config {
        repo_path: repo.clone(),
        state_dir: state_dir.clone(),
        mnt_dir,
        max_sessions: 8,
        max_delta_bytes: 2 * 1024 * 1024,
        lock_ttl_seconds: 3600,
        session_recovery_ttl_seconds: 86400,
        vfs_backend: VfsBackend::NfsLoopback,
        metrics_enabled: false,
        metrics_addr: "127.0.0.1:0".to_owned(),
        commit_peer_capture_concurrency: 4,
        commit_wait_secs: 30,
    });

    let db_path = state_dir.join("state.db");
    let sessions = Arc::new(SessionManager::open(&db_path).await.expect("open sessions"));
    let borrows = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
    let deltas = Arc::new(DeltaStore::new(state_dir.join("deltas")));
    let events = Arc::new(EventBroker::new());
    let vfs = Arc::new(VfsManager::new(
        Arc::clone(&cfg),
        Arc::clone(&deltas),
        Arc::clone(&borrows),
        Arc::new(crate::metrics::Metrics::new().expect("metrics")),
    ));

    let state = Arc::new(AppState {
        config: cfg,
        sessions,
        borrows,
        deltas,
        events,
        vfs,
        metrics: Arc::new(crate::metrics::Metrics::new().expect("metrics")),
        commit_scheduler: Arc::new(crate::commit_scheduler::CommitScheduler::new(
            std::time::Duration::from_secs(30),
        )),
    });
    (state, root)
}
