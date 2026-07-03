use super::FuseBackend;
use crate::borrow::BorrowRegistry;
use crate::config::{Config, VfsBackend};
use crate::delta::DeltaStore;
use crate::session::SessionManager;
use crate::vfs::VfsBackendTrait;
use chrono::Utc;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use simgit_sdk::{SessionInfo, SessionStatus};

static TEST_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let seq = TEST_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "simgit-fuse-int-{}-{}-{}",
        std::process::id(),
        nanos,
        seq
    ))
}

fn run_git(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("git command should execute");
    assert!(status.success(), "git {:?} failed", args);
}

fn init_repo(root: &std::path::Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo");

    run_git(&repo, &["init"]);
    run_git(&repo, &["config", "user.email", "tests@simgit.local"]);
    run_git(&repo, &["config", "user.name", "simgit-tests"]);
    std::fs::write(repo.join("old.txt"), b"old-content\n").expect("write file");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "init"]);
    repo
}

fn wait_for_mount_ready(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if std::fs::read_dir(path).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("mount path did not become ready: {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[tokio::test]
#[ignore = "requires Linux FUSE kernel + fusermount available"]
async fn fuse_mount_roundtrip_create_unlink_rename() {
    let root = temp_root();
    let repo = init_repo(&root);
    let state_dir = root.join("state");
    let mnt_dir = state_dir.join("mnt");
    std::fs::create_dir_all(&mnt_dir).expect("create mnt dir");

    let cfg = Arc::new(Config {
        repo_path: repo.clone(),
        state_dir: state_dir.clone(),
        mnt_dir: mnt_dir.clone(),
        max_sessions: 8,
        max_delta_bytes: 2 * 1024 * 1024,
        lock_ttl_seconds: 3600,
        session_recovery_ttl_seconds: 86400,
        vfs_backend: VfsBackend::Fuse,
        metrics_enabled: false,
        metrics_addr: "127.0.0.1:0".to_owned(),
        commit_peer_capture_concurrency: 4,
        commit_wait_secs: 30,
    });

    let db_path = state_dir.join("state.db");
    let sessions = Arc::new(SessionManager::open(&db_path).await.expect("open sessions"));
    let borrows = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
    let deltas = Arc::new(DeltaStore::new(state_dir.join("deltas")));
    let backend = FuseBackend::new(Arc::clone(&cfg), Arc::clone(&deltas), Arc::clone(&borrows));

    let session_id = Uuid::now_v7();
    let mount_path = mnt_dir.join(session_id.to_string());
    let session = SessionInfo {
        session_id,
        task_id: "fuse-int".to_owned(),
        agent_label: Some("linux-int".to_owned()),
        base_commit: "HEAD".to_owned(),
        created_at: Utc::now(),
        status: SessionStatus::Active,
        mount_path: mount_path.clone(),
        branch_name: None,
        peers_enabled: false,
        git_proxy_enabled: false,
        initial_branch: None,
        socket_path: std::path::PathBuf::new(),
    };

    backend.mount(&session).await.expect("mount");
    wait_for_mount_ready(&mount_path);

    let new_path = mount_path.join("new.txt");
    std::fs::write(&new_path, b"hello-from-create\n").expect("create/write new file");
    let created = std::fs::read(&new_path).expect("read created file");
    assert_eq!(created, b"hello-from-create\n");

    let old_path = mount_path.join("old.txt");
    let renamed_path = mount_path.join("renamed.txt");
    std::fs::rename(&old_path, &renamed_path).expect("rename baseline file");
    assert!(!old_path.exists(), "old path should be gone after rename");
    let renamed = std::fs::read(&renamed_path).expect("read renamed file");
    assert_eq!(renamed, b"old-content\n");

    std::fs::remove_file(&renamed_path).expect("unlink renamed file");
    assert!(!renamed_path.exists(), "renamed path should be removed");

    let _ = backend.unmount(session_id).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "requires Linux FUSE kernel + fusermount available"]
async fn fuse_mount_can_remount_same_session_path() {
    let root = temp_root();
    let repo = init_repo(&root);
    let state_dir = root.join("state");
    let mnt_dir = state_dir.join("mnt");
    std::fs::create_dir_all(&mnt_dir).expect("create mnt dir");

    let cfg = Arc::new(Config {
        repo_path: repo.clone(),
        state_dir: state_dir.clone(),
        mnt_dir: mnt_dir.clone(),
        max_sessions: 8,
        max_delta_bytes: 2 * 1024 * 1024,
        lock_ttl_seconds: 3600,
        session_recovery_ttl_seconds: 86400,
        vfs_backend: VfsBackend::Fuse,
        metrics_enabled: false,
        metrics_addr: "127.0.0.1:0".to_owned(),
        commit_peer_capture_concurrency: 4,
        commit_wait_secs: 30,
    });

    let db_path = state_dir.join("state.db");
    let sessions = Arc::new(SessionManager::open(&db_path).await.expect("open sessions"));
    let borrows = Arc::new(BorrowRegistry::new(Arc::clone(&sessions)));
    let deltas = Arc::new(DeltaStore::new(state_dir.join("deltas")));
    let backend = FuseBackend::new(Arc::clone(&cfg), Arc::clone(&deltas), Arc::clone(&borrows));

    let session_id = Uuid::now_v7();
    let mount_path = mnt_dir.join(session_id.to_string());
    let session = SessionInfo {
        session_id,
        task_id: "fuse-remount".to_owned(),
        agent_label: Some("linux-int".to_owned()),
        base_commit: "HEAD".to_owned(),
        created_at: Utc::now(),
        status: SessionStatus::Active,
        mount_path: mount_path.clone(),
        branch_name: None,
        peers_enabled: false,
        git_proxy_enabled: false,
        initial_branch: None,
        socket_path: std::path::PathBuf::new(),
    };

    backend.mount(&session).await.expect("first mount");
    wait_for_mount_ready(&mount_path);
    let bytes = std::fs::read(mount_path.join("old.txt")).expect("read baseline file");
    assert_eq!(bytes, b"old-content\n");

    let _ = backend.unmount(session_id).await;

    backend.mount(&session).await.expect("second mount");
    wait_for_mount_ready(&mount_path);
    let bytes2 = std::fs::read(mount_path.join("old.txt")).expect("read baseline after remount");
    assert_eq!(bytes2, b"old-content\n");

    let _ = backend.unmount(session_id).await;
    let _ = std::fs::remove_dir_all(&root);
}
