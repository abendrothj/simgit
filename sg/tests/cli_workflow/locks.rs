//! `unlock` and recovery hints.

use crate::{dead_pid, git, isolated, resolved, success, Fixture};
use std::fs;
use std::path::Path;
use std::process::Command;

/// A launcher killed outright cannot release its lock, and the workspace is
/// unusable until someone does. `unlock` is that recovery — but only once the
/// launcher is really gone: while it runs, the fix is to stop it, so the
/// refusal has to name the process to stop.
#[test]
fn unlock_clears_a_stranded_lock_and_refuses_a_live_one() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let mut child = fixture
        .command()
        .args([
            "run",
            "chat/test",
            "--",
            "sh",
            "-c",
            "touch started; while [ ! -f release ]; do sleep 0.05; done",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let launcher = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !fixture.worktree.join("started").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        fixture.worktree.join("started").exists(),
        "command never ran"
    );

    let refused = fixture.run(&["unlock", "chat/test"]);
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    // Kill the launcher without letting it release the lock, then let the
    // command it started finish, so what remains is a genuinely stranded lock.
    child.kill().unwrap();
    child.wait().unwrap();
    fs::write(fixture.worktree.join("release"), "").unwrap();

    assert!(!refused.status.success(), "unlock ignored a live launcher");
    assert!(
        message.contains(&format!("pid {launcher}")),
        "refusal does not name the process holding the lock: {message}"
    );
    let blocked = fixture.run(&["run", "chat/test", "--", "true"]);
    let message = String::from_utf8_lossy(&blocked.stderr).into_owned();
    assert!(
        !blocked.status.success(),
        "the stranded lock did not block a new command"
    );
    assert!(
        message.contains(&format!("pid {launcher}"))
            && message.contains("no longer running")
            && message.contains("simgit unlock"),
        "refusal does not say the lock's owner is gone and how to clear it: {message}"
    );

    let unlocked = fixture.run(&["unlock", "chat/test", "--json"]);
    success(&unlocked);
    let report: serde_json::Value = serde_json::from_slice(&unlocked.stdout).unwrap();
    assert_eq!(
        Path::new(report["unlocked"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        fixture.worktree.canonicalize().unwrap()
    );
    assert_eq!(report["was_locked"], true);
    assert_eq!(report["owner_pid"], launcher);
    success(&fixture.run(&["run", "chat/test", "--", "true"]));

    // Unlocking an unlocked workspace is the state the caller asked for.
    let again = fixture.run(&["unlock", "chat/test", "--json"]);
    success(&again);
    let report: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(report["was_locked"], false);
    assert_eq!(report["owner_pid"], serde_json::Value::Null);
    success(&fixture.run(&["gc", "--older-than", "0s", "--include-persistent"]));

    // An allocation lock is simgit's too: it is refused while its allocator
    // runs (this test process stands in for one) and cleared once it is gone.
    fixture.create(&[]);
    let path = fixture.worktree.display().to_string();
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    git(
        &fixture.worktree,
        &[
            "worktree",
            "lock",
            "--reason",
            &format!("simgit: allocating (pid {})", std::process::id()),
            &path,
        ],
    );
    assert!(!fixture.run(&["unlock", &path]).status.success());
    git(&fixture.worktree, &["worktree", "unlock", &path]);
    git(
        &fixture.worktree,
        &[
            "worktree",
            "lock",
            "--reason",
            &format!("simgit: allocating (pid {dead})"),
            &path,
        ],
    );
    let unlocked = fixture.run(&["unlock", &path, "--json"]);
    success(&unlocked);
    let report: serde_json::Value = serde_json::from_slice(&unlocked.stdout).unwrap();
    assert_eq!(report["was_locked"], true);
    assert_eq!(report["owner_pid"], dead);
    success(&fixture.run(&["run", "chat/test", "--", "true"]));
}

/// A lock a person set with `git worktree lock` guards something simgit
/// cannot see, such as a worktree on removable media. It names no simgit
/// process, so `unlock` must not treat "no live owner" as "stranded".
#[test]
fn unlock_refuses_a_lock_git_set() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let path = fixture.worktree.display().to_string();

    for reason in [Some("usb"), None] {
        let mut args = vec!["worktree", "lock"];
        if let Some(reason) = reason {
            args.extend(["--reason", reason]);
        }
        args.push(&path);
        git(&fixture.worktree, &args);

        let refused = fixture.run(&["unlock", &path]);
        let message = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert!(!refused.status.success(), "unlock cleared Git's lock");
        assert!(
            message.contains("git worktree unlock"),
            "refusal does not point at Git: {message}"
        );
        if let Some(reason) = reason {
            assert!(message.contains(reason), "reason missing: {message}");
        }
        let locked = fixture.run(&["list", "--json"]);
        success(&locked);
        assert!(
            String::from_utf8_lossy(&locked.stdout).contains("\"locked\""),
            "the lock is gone"
        );
        // The refusal covers the branch spelling and the explicit target alike.
        assert!(!fixture.run(&["unlock", "chat/test"]).status.success());

        git(&fixture.worktree, &["worktree", "unlock", &path]);
        success(&fixture.run(&["unlock", &path]));
    }
}

/// A recovery pass that crashed after cleanup re-runs `unlock` before it can
/// know the workspace is gone, so an absent target is the state the caller
/// asked for — the same reasoning that makes `remove` idempotent.
#[test]
fn unlock_of_an_absent_workspace_succeeds() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let path = fixture.worktree.display().to_string();
    success(&fixture.run(&["remove", &path, "--json"]));

    for target in [path.as_str(), "chat/test", "never/allocated"] {
        let output = fixture.run(&["unlock", target, "--json"]);
        success(&output);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["unlocked"], target);
        assert_eq!(report["was_locked"], false);
        assert_eq!(report["owner_pid"], serde_json::Value::Null);
    }
}

fn bin_dir_first() -> std::ffi::OsString {
    let bin = Path::new(env!("CARGO_BIN_EXE_simgit")).parent().unwrap();
    let mut dirs = vec![bin.to_path_buf()];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(dirs).unwrap()
}

/// A recovery hint is meant to be pasted, so a path with a space or a quote in
/// it arrives as one single-quoted shell word.
#[test]
fn recovery_hints_quote_paths_with_spaces_and_quotes() {
    let mut fixture = Fixture::new();
    fixture.worktree = fixture.root.join("it's a job");
    fixture.create(&[]);
    let path = resolved(&fixture.worktree).display().to_string();
    let reason = format!("simgit: allocating (pid {})", dead_pid());
    git(
        &fixture.repo,
        &["worktree", "lock", "--reason", &reason, &path],
    );

    let refused = fixture.run(&["run", "chat/test", "--", "true"]);
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success(), "{message}");
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    assert!(
        message.contains(&format!("simgit unlock {quoted}")),
        "{message}"
    );
    // The hint works as written.
    let unlock = isolated("sh")
        .current_dir(&fixture.repo)
        .env("PATH", bin_dir_first())
        .arg("-c")
        .arg(format!("simgit unlock {quoted}"))
        .output()
        .unwrap();
    success(&unlock);
    assert_eq!(fixture.listed()["locked"], serde_json::Value::Null);
}
