//! Worktrees whose directory was deleted by hand.

use crate::{
    add_detached, add_submodule, branch_exists, commit_exists, commit_in, create_at, dead_pid, git,
    git_wrapper, local_refs, reftable_fixture, registered_head, registration, resolved, rev_parse,
    success, wait_for, Fixture,
};
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn reftable_prune_keeps_a_deleted_unreached_detached_registration() {
    let Some(fixture) = reftable_fixture() else {
        return;
    };
    let path = add_detached(&fixture);
    let head = commit_in(&path, "agent.txt");
    let kept = resolved(&path).display().to_string();
    fs::remove_dir_all(&path).unwrap();

    let pruned = fixture.run(&["--json", "prune"]);
    success(&pruned);
    let report: serde_json::Value = serde_json::from_slice(&pruned.stdout).unwrap();
    assert_eq!(report["pruned_registrations"], serde_json::json!([]));
    assert_eq!(
        report["kept_registrations"],
        serde_json::json!([{ "worktree": kept, "reason": "unreached-commits" }])
    );
    assert_eq!(registered_head(&fixture, &path), Some(head.clone()));
    assert!(commit_exists(&fixture.repo, &head));
}

/// A worktree deleted by hand stays registered, its branch checked out at a
/// path that is gone. Removing it is already the state the caller asked for.
#[test]
fn remove_by_branch_of_a_deleted_worktree_is_already_absent() {
    let fixture = Fixture::new();
    let path = resolved(&fixture.worktree).display().to_string();
    fixture.create(&[]);
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let removed = fixture.run(&["--json", "remove", "chat/test"]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["removed"], path);
    assert_eq!(report["already_absent"], true);
    assert_eq!(report["committed"], false);
    assert_eq!(report["branch_deleted"], false);
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
    assert!(local_refs(&fixture.repo).contains("refs/heads/chat/test"));

    fixture.create(&[]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let deleted = fixture.run(&["--json", "remove", "chat/test", "--delete-branch"]);
    success(&deleted);
    let report: serde_json::Value = serde_json::from_slice(&deleted.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert_eq!(report["branch_deleted"], true);
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
    assert!(!local_refs(&fixture.repo).contains("refs/heads/chat/test"));

    // While the registration is there it names the branch, so the path form
    // can delete it too; nothing is left on disk for --commit to commit.
    fixture.create(&[]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let by_path = fixture.run(&["--json", "remove", &path, "--commit", "--delete-branch"]);
    success(&by_path);
    let report: serde_json::Value = serde_json::from_slice(&by_path.stdout).unwrap();
    assert_eq!(report["removed"], path);
    assert_eq!(report["already_absent"], true);
    assert_eq!(report["committed"], false);
    assert_eq!(report["branch_deleted"], true);
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
    assert!(!local_refs(&fixture.repo).contains("refs/heads/chat/test"));
}

#[test]
fn gc_prunes_a_deleted_worktree_and_honours_delete_branches() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let admin = registration(&fixture, &fixture.worktree);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    // Idle age comes from the registration once the directory is gone:
    // backdate its index past the default --older-than.
    let touched = Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(Path::new(&admin).join("index"))
        .status()
        .unwrap();
    assert!(touched.success());

    let gc = fixture.run(&["--json", "gc", "--delete-branches"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(
        report["reaped"],
        serde_json::json!([resolved(&fixture.worktree).display().to_string()])
    );
    assert_eq!(report["skipped"], serde_json::json!([]));
    assert_eq!(report["deleted_branches"], serde_json::json!(["chat/test"]));
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
    assert!(!local_refs(&fixture.repo).contains("refs/heads/chat/test"));
}

/// With neither its directory nor its index left, nothing records when a
/// deleted worktree was last touched. It used to count as touched just now,
/// so it was `recently-active` forever and never reaped.
#[test]
fn gc_reaps_a_deleted_worktree_whose_index_is_gone() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let admin = registration(&fixture, &fixture.worktree);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    fs::remove_file(Path::new(&admin).join("index")).unwrap();

    let gc = fixture.run(&["--json", "gc"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["skipped"], serde_json::json!([]));
    assert_eq!(
        report["reaped"],
        serde_json::json!([resolved(&fixture.worktree).display().to_string()])
    );
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
}

/// Counting an unreadable index as idle must not make it reapable: whether it
/// holds staged work cannot be told, so gc keeps it and says why.
#[test]
fn gc_keeps_a_deleted_worktree_whose_index_cannot_be_read() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let admin = registration(&fixture, &fixture.worktree);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    // A symlink to itself cannot be read even by root.
    let index = Path::new(&admin).join("index");
    fs::remove_file(&index).unwrap();
    std::os::unix::fs::symlink("index", &index).unwrap();

    let gc = fixture.run(&["--json", "gc"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    assert_eq!(
        report["skipped"],
        serde_json::json!([{
            "worktree": resolved(&fixture.worktree).display().to_string(),
            "reason": "status-failed",
        }])
    );
    assert!(registered_head(&fixture, &fixture.worktree).is_some());
}

/// Pruning a detached registration whose HEAD no ref reaches leaves its
/// commits dangling, directory or not, so no path does it.
#[test]
fn remove_and_gc_keep_a_deleted_detached_worktree_with_unreached_commits() {
    let fixture = Fixture::new();
    let path = add_detached(&fixture);
    let head = commit_in(&path, "agent.txt");
    let target = path.to_str().unwrap();
    fs::remove_dir_all(&path).unwrap();
    let main = fixture.repo.canonicalize().unwrap();
    let fix = format!("git -C {} branch <name> {head}", main.display());

    for args in [
        vec!["remove", target],
        vec!["remove", target, "--discard-dirty"],
        vec!["remove", target, "--commit"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&fix), "{stderr}");
        assert_eq!(registered_head(&fixture, &path), Some(head.clone()));
    }

    let gc = fixture.run(&[
        "--json",
        "gc",
        "--older-than",
        "0s",
        "--include-persistent",
        "--discard-dirty",
    ]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    assert_eq!(
        report["skipped"],
        serde_json::json!([{ "worktree": target, "reason": "unreached-commits" }])
    );
    assert_eq!(registered_head(&fixture, &path), Some(head.clone()));
    success(&fixture.run(&["prune"]));
    assert_eq!(registered_head(&fixture, &path), Some(head.clone()));

    // The advertised fix works without the directory.
    git(&main, &["branch", "kept", &head]);
    let removed = fixture.run(&["--json", "remove", target]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert_eq!(registered_head(&fixture, &path), None);
    assert_eq!(rev_parse(&fixture.repo, "kept"), head);
}

/// A directory that is gone because its drive is unplugged is what
/// `git worktree lock` protects, so a locked registration is never pruned.
#[test]
fn remove_and_gc_keep_a_locked_worktree_whose_directory_is_gone() {
    let fixture = Fixture::new();
    let path = resolved(&fixture.worktree).display().to_string();
    fixture.create(&["--ephemeral"]);
    let head = rev_parse(&fixture.worktree, "HEAD");
    git(
        &fixture.repo,
        &["worktree", "lock", "--reason", "usb", &path],
    );
    fs::remove_dir_all(&fixture.worktree).unwrap();

    for target in ["chat/test", path.as_str()] {
        let output = fixture.run(&["--json", "remove", target, "--delete-branch"]);
        assert!(!output.status.success(), "removed {target}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("usb"), "{stderr}");
        assert_eq!(
            registered_head(&fixture, &fixture.worktree),
            Some(head.clone())
        );
    }

    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--delete-branches"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    assert_eq!(
        report["skipped"],
        serde_json::json!([{ "worktree": path, "reason": "locked" }])
    );
    success(&fixture.run(&["prune"]));
    assert_eq!(registered_head(&fixture, &fixture.worktree), Some(head));
    assert!(local_refs(&fixture.repo).contains("refs/heads/chat/test"));
}

/// An allocation killed while its directory is missing (CoW population moves
/// it aside) leaves its lock on a registration that `unlock <path>` must find.
#[test]
fn unlock_clears_a_stranded_allocation_lock_on_a_deleted_worktree() {
    let fixture = Fixture::new();
    let path = resolved(&fixture.worktree).display().to_string();
    fixture.create(&[]);
    let dead = dead_pid();
    let reason = format!("simgit: allocating (pid {dead})");
    git(
        &fixture.repo,
        &["worktree", "lock", "--reason", &reason, &path],
    );
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let unlocked = fixture.run(&["--json", "unlock", &path]);
    success(&unlocked);
    let report: serde_json::Value = serde_json::from_slice(&unlocked.stdout).unwrap();
    assert_eq!(report["unlocked"], path);
    assert_eq!(report["was_locked"], true);
    assert_eq!(report["owner_pid"], dead);
    assert_eq!(fixture.listed()["locked"], serde_json::Value::Null);

    let removed = fixture.run(&["--json", "remove", &path]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
}

/// Assert that every remove form refuses a deleted worktree as dirty and that
/// gc and prune keep its registration.
fn assert_deleted_worktree_kept_as_dirty(fixture: &Fixture, branch: &str) {
    let path = resolved(&fixture.worktree).display().to_string();
    let head = registered_head(fixture, &fixture.worktree);
    assert!(head.is_some());
    let refs = local_refs(&fixture.repo);
    for args in [
        vec!["remove", branch],
        vec!["remove", path.as_str()],
        vec!["remove", branch, "--commit"],
        vec!["remove", branch, "--delete-branch"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--discard-dirty"), "{stderr}");
        assert_eq!(registered_head(fixture, &fixture.worktree), head);
    }
    let gc = fixture.run(&[
        "--json",
        "gc",
        "--older-than",
        "0s",
        "--include-persistent",
        "--delete-branches",
    ]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(
        report["skipped"],
        serde_json::json!([{ "worktree": path, "reason": "dirty" }])
    );
    success(&fixture.run(&["prune"]));
    assert_eq!(registered_head(fixture, &fixture.worktree), head);
    assert_eq!(local_refs(&fixture.repo), refs, "a branch was deleted");
}

/// A deleted worktree's index outlives its directory, and staged changes in it
/// are uncommitted work: losing them takes --discard-dirty, as for a live one.
#[test]
fn remove_and_gc_keep_staged_work_of_a_deleted_worktree() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    fs::write(fixture.worktree.join("staged.txt"), "staged\n").unwrap();
    git(&fixture.worktree, &["add", "staged.txt"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();

    assert_deleted_worktree_kept_as_dirty(&fixture, "chat/test");

    let removed = fixture.run(&["--json", "remove", "chat/test", "--discard-dirty"]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
}

/// A merge, rebase, cherry-pick or revert in progress is uncommitted work too,
/// even when the index matches HEAD.
#[test]
fn a_deleted_worktree_with_an_operation_in_progress_counts_as_dirty() {
    deleted_worktree_with_marker_counts_as_dirty("MERGE_HEAD");
}

#[test]
fn a_deleted_worktree_mid_cherry_pick_counts_as_dirty() {
    deleted_worktree_with_marker_counts_as_dirty("CHERRY_PICK_HEAD");
}

#[test]
fn a_deleted_worktree_mid_revert_counts_as_dirty() {
    deleted_worktree_with_marker_counts_as_dirty("REVERT_HEAD");
}

#[test]
fn a_deleted_worktree_mid_rebase_merge_counts_as_dirty() {
    deleted_worktree_with_marker_counts_as_dirty("rebase-merge");
}

#[test]
fn a_deleted_worktree_mid_rebase_apply_counts_as_dirty() {
    deleted_worktree_with_marker_counts_as_dirty("rebase-apply");
}

#[test]
fn a_deleted_worktree_mid_sequencer_counts_as_dirty() {
    deleted_worktree_with_marker_counts_as_dirty("sequencer");
}

/// One test per marker, so they run in parallel: each is kept as dirty until
/// `--discard-dirty`, which then drops it.
fn deleted_worktree_with_marker_counts_as_dirty(marker: &str) {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let head = rev_parse(&fixture.worktree, "HEAD");
    let admin = std::path::PathBuf::from(registration(&fixture, &fixture.worktree));
    if marker.ends_with("_HEAD") {
        fs::write(admin.join(marker), format!("{head}\n")).unwrap();
    } else {
        fs::create_dir(admin.join(marker)).unwrap();
    }
    fs::remove_dir_all(&fixture.worktree).unwrap();

    assert_deleted_worktree_kept_as_dirty(&fixture, "chat/test");
    success(&fixture.run(&["remove", "chat/test", "--discard-dirty"]));
    assert_eq!(
        registered_head(&fixture, &fixture.worktree),
        None,
        "{marker}"
    );
}

/// On an unborn HEAD there is no commit to compare with: anything in the
/// index is staged, and an empty index holds nothing.
#[test]
fn a_deleted_worktree_on_an_unborn_branch_is_dirty_only_with_staged_files() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    git(&fixture.worktree, &["checkout", "-q", "--orphan", "fresh"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    assert_deleted_worktree_kept_as_dirty(&fixture, "fresh");
    success(&fixture.run(&["remove", "fresh", "--discard-dirty"]));
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);

    fixture.create(&[]);
    git(&fixture.worktree, &["checkout", "-q", "--orphan", "empty"]);
    git(&fixture.worktree, &["rm", "-rqf", "--cached", "."]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let removed = fixture.run(&["--json", "remove", "empty"]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
}

/// A linked worktree keeps its submodules' repositories (`modules/`) and its
/// own refs (`refs/worktree`, `refs/bisect`) in its registration, so dropping
/// it can lose commits nothing else holds.
#[test]
fn a_deleted_worktree_with_submodule_commits_or_its_own_refs_counts_as_dirty() {
    let fixture = Fixture::new();
    add_submodule(&fixture);
    fixture.create(&["--ephemeral"]);
    git(
        &fixture.worktree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "-q",
        ],
    );
    let sub = fixture.worktree.join("sub");
    git(&sub, &["config", "user.email", "test@example.com"]);
    git(&sub, &["config", "user.name", "Test User"]);
    let precious = commit_in(&sub, "precious.txt");
    let modules =
        std::path::PathBuf::from(registration(&fixture, &fixture.worktree)).join("modules/sub");
    fs::remove_dir_all(&fixture.worktree).unwrap();

    assert_deleted_worktree_kept_as_dirty(&fixture, "chat/test");
    git(
        &fixture.repo,
        &[
            &format!("--git-dir={}", modules.display()),
            // Its `core.worktree` names the deleted checkout.
            &format!("--work-tree={}", fixture.repo.display()),
            "cat-file",
            "-e",
            &format!("{precious}^{{commit}}"),
        ],
    );
    success(&fixture.run(&["remove", "chat/test", "--discard-dirty"]));
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);

    // A commit only a per-worktree ref reaches.
    fixture.create(&["--ephemeral"]);
    commit_in(&fixture.worktree, "kept.txt");
    git(
        &fixture.worktree,
        &["update-ref", "refs/worktree/kept", "HEAD"],
    );
    git(&fixture.worktree, &["reset", "-q", "--hard", "HEAD~1"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();

    assert_deleted_worktree_kept_as_dirty(&fixture, "chat/test");
    success(&fixture.run(&["remove", "chat/test", "--discard-dirty"]));
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
}

/// gc drops only what it reaps. A deleted worktree it skips, or never selects,
/// keeps its registration, so a later gc can still reap it and its branch.
#[test]
fn gc_keeps_the_registrations_of_deleted_worktrees_it_skips() {
    let fixture = Fixture::new();
    let fresh = resolved(&fixture.worktree);
    let persistent = fixture.root.join("persistent");
    fixture.create(&["--ephemeral"]);
    create_at(&fixture, "kept/test", &persistent, &[]);
    let admin = registration(&fixture, &fixture.worktree);
    let persistent = persistent.canonicalize().unwrap();
    fs::remove_dir_all(&fresh).unwrap();
    fs::remove_dir_all(&persistent).unwrap();
    let both = |fixture: &Fixture| {
        (
            registered_head(fixture, &fresh).is_some(),
            registered_head(fixture, &persistent).is_some(),
        )
    };

    let gc = fixture.run(&["--json", "gc", "--delete-branches"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    assert_eq!(
        report["skipped"],
        serde_json::json!([
            { "worktree": fresh.display().to_string(), "reason": "recently-active" },
            { "worktree": persistent.display().to_string(), "reason": "persistent" },
        ])
    );
    assert_eq!(both(&fixture), (true, true));

    let filtered = fixture.run(&[
        "--json",
        "gc",
        "--older-than",
        "0s",
        "--include-persistent",
        "--prefix",
        "none/",
    ]);
    success(&filtered);
    assert_eq!(both(&fixture), (true, true));

    let touched = Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(Path::new(&admin).join("index"))
        .status()
        .unwrap();
    assert!(touched.success());
    let gc = fixture.run(&["--json", "gc", "--delete-branches"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(
        report["reaped"],
        serde_json::json!([fresh.display().to_string()])
    );
    assert_eq!(report["deleted_branches"], serde_json::json!(["chat/test"]));
    assert_eq!(both(&fixture), (false, true));
    assert!(local_refs(&fixture.repo).contains("refs/heads/kept/test"));
}

/// A prune judges which registrations to keep and then drops the rest. Another
/// simgit's lock on a kept registration (a concurrent prune protecting it) can
/// be released in between, so protection must never rest on that lock. The
/// wrapper holds the first prune inside the registry lock (at its dirty check,
/// or at Git's own prune or remove) while a second waits behind it, and logs
/// each git call, so the second's first call under the lock is shown to come
/// after the first is released.
#[test]
fn prunes_keep_an_unreached_registration_whose_lock_is_released_meanwhile() {
    use std::io::Write;

    let fixture = Fixture::new();
    let detached = add_detached(&fixture);
    let head = commit_in(&detached, "agent.txt");
    let detached_path = detached.display().to_string();
    let reason = format!("simgit: workspace in use (pid {})", std::process::id());
    git(
        &fixture.repo,
        &["worktree", "lock", "--reason", &reason, &detached_path],
    );
    fs::remove_dir_all(&detached).unwrap();
    let branch = resolved(&fixture.worktree);
    fixture.create(&[]);
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let started = fixture.root.join("prune-started");
    let release = fixture.root.join("prune-release");
    let log = fixture.root.join("prune-log");
    let env = git_wrapper(
        &fixture,
        &format!(
            "case \"$*\" in\n\
               *'rev-parse --show-toplevel'*|*--git-common-dir*)\n\
                 \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n\
                 status=$?\n\
                 echo \"$PPID discovered\" >> '{log}'\n\
                 exit $status ;;\n\
             esac\n\
             echo \"$PPID locked\" >> '{log}'\n\
             if {{ [ \"$2\" = diff-index ] || {{ [ \"$2\" = worktree ] \
               && {{ [ \"$3\" = prune ] || [ \"$3\" = remove ]; }}; }}; }} \
               && mkdir '{started}' 2>/dev/null; then\n\
               tries=0\n\
               while [ ! -e '{release}' ] && [ $tries -lt 200 ]; do sleep 0.05; tries=$((tries + 1)); done\n\
             fi\n",
            started = started.display(),
            release = release.display(),
            log = log.display(),
        ),
    );
    let spawn = || {
        fixture
            .command()
            .envs(env.clone())
            .args(["--json", "prune"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    wait_for(&started);
    let second = spawn();
    let logged = |line: &str| {
        let log = fs::read_to_string(&log).unwrap_or_default();
        log.lines().position(|logged| logged == line)
    };
    // `discover_repo`'s two rev-parse calls come before the registry lock:
    // once they are done, a prune without that lock would reach Git at once.
    let discovered = format!("{} discovered", second.id());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == discovered)
        .count()
        < 2
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the second prune never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let under_lock = format!("{} locked", second.id());
    git(&fixture.repo, &["worktree", "unlock", &detached_path]);
    fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"released\n")
        .unwrap();
    fs::write(&release, "").unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    success(&first);
    success(&second);
    let released = logged("released").unwrap();
    let reached = logged(&under_lock).expect("the second prune ran no git under the lock");
    assert!(
        reached > released,
        "a second prune ran beside the first:\n{}",
        fs::read_to_string(&log).unwrap()
    );

    assert_eq!(registered_head(&fixture, &detached), Some(head));
    assert_eq!(registered_head(&fixture, &branch), None);
    let reported = |output: &std::process::Output| {
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["pruned_registrations"]
            .clone()
    };
    assert_eq!(
        reported(&first),
        serde_json::json!([branch.display().to_string()])
    );
    assert_eq!(reported(&second), serde_json::json!([]));
}

/// The gone check is repeated under the registry lock, just before the drop:
/// a directory that is back by then is a live worktree, which Git would
/// delete if clean. The wrapper holds the dirty check while it comes back.
#[test]
fn remove_leaves_a_deleted_worktree_that_reappears_alone() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let aside = fixture.root.join("aside");
    fs::rename(&fixture.worktree, &aside).unwrap();
    let started = fixture.root.join("check-started");
    let release = fixture.root.join("check-release");
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$2\" = diff-index ] && mkdir '{started}' 2>/dev/null; then\n\
               tries=0\n\
               while [ ! -e '{release}' ] && [ $tries -lt 200 ]; do sleep 0.05; tries=$((tries + 1)); done\n\
             fi\n",
            started = started.display(),
            release = release.display(),
        ),
    );
    let remove = fixture
        .command()
        .envs(env)
        .args(["--json", "remove", "chat/test"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&started);
    fs::rename(&aside, &fixture.worktree).unwrap();
    fs::write(&release, "").unwrap();
    let output = remove.wait_with_output().unwrap();

    assert!(!output.status.success(), "dropped a live worktree");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("reappeared"), "{stderr}");
    assert!(fixture.worktree.join("README.md").is_file());
    assert!(registered_head(&fixture, &fixture.worktree).is_some());
}

/// `--delete-branch` and `--delete-branches` keep their merged check when the
/// worktree is gone: the registration goes, an unmerged branch stays.
#[test]
fn a_deleted_worktree_keeps_its_unmerged_branch() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    commit_in(&fixture.worktree, "agent.txt");
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let output = fixture.run(&["--json", "remove", "chat/test", "--delete-branch"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not fully merged"), "{stderr}");
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
    assert!(local_refs(&fixture.repo).contains("refs/heads/chat/test"));

    fixture.create(&["--ephemeral"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--delete-branches"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(
        report["reaped"],
        serde_json::json!([resolved(&fixture.worktree).display().to_string()])
    );
    assert_eq!(
        report["retained_branches"],
        serde_json::json!(["chat/test"])
    );
    assert_eq!(registered_head(&fixture, &fixture.worktree), None);
    assert!(local_refs(&fixture.repo).contains("refs/heads/chat/test"));
}

/// A path that cannot be checked (its parent denies search) is not known to be
/// gone, so its registration is never dropped as if it were.
#[test]
fn an_unreadable_worktree_path_is_not_treated_as_gone() {
    use std::os::unix::fs::PermissionsExt;

    let uid = Command::new("id").arg("-u").output().unwrap();
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let fixture = Fixture::new();
    let parent = fixture.root.join("sealed");
    let path = parent.join("wt");
    create_at(&fixture, "chat/test", &path, &["--ephemeral"]);
    let path = path.canonicalize().unwrap();
    let head = registered_head(&fixture, &path);
    // Restores the permission even when an assertion fails, so Drop can clean up.
    struct Unseal(std::path::PathBuf);
    impl Drop for Unseal {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    let _unseal = Unseal(parent.clone());
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();

    let output = fixture.run(&["--json", "remove", "chat/test"]);
    assert!(!output.status.success(), "removed an unreadable worktree");
    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--delete-branches"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    success(&fixture.run(&["prune"]));
    assert_eq!(registered_head(&fixture, &path), head);
    assert!(local_refs(&fixture.repo).contains("refs/heads/chat/test"));
}

/// With `worktree.useRelativePaths` (Git 2.48+), the registry records a
/// worktree relative to its admin dir. A deleted one must be found, and its
/// lock honoured, just as when the path is absolute. Older Git ignores the
/// setting and cannot read a relative entry itself, so there is nothing to test.
#[test]
fn a_deleted_worktree_with_relative_paths_is_handled_like_an_absolute_one() {
    let fixture = Fixture::new();
    git(
        &fixture.repo,
        &["config", "worktree.useRelativePaths", "true"],
    );
    let path = resolved(&fixture.worktree).display().to_string();

    fixture.create(&["--ephemeral"]);
    let admin = Path::new(&registration_by_branch(&fixture, "chat/test")).to_path_buf();
    let recorded = fs::read_to_string(admin.join("gitdir")).unwrap();
    if Path::new(recorded.trim()).is_absolute() {
        eprintln!("skipped: this Git predates worktree.useRelativePaths");
        return;
    }
    git(
        &fixture.repo,
        &["worktree", "lock", "--reason", "usb", &path],
    );
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let removed = fixture.run(&["--json", "remove", "chat/test"]);
    assert!(!removed.status.success(), "removed a locked worktree");
    let stderr = String::from_utf8_lossy(&removed.stderr);
    assert!(stderr.contains("usb"), "{stderr}");
    let gc = fixture.run(&["--json", "gc", "--older-than", "0s"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(
        report["skipped"],
        serde_json::json!([{ "worktree": path, "reason": "locked" }])
    );
    assert!(admin.is_dir());

    git(&fixture.repo, &["worktree", "unlock", &path]);
    let removed = fixture.run(&["--json", "remove", "chat/test"]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["removed"], path);
    assert_eq!(report["already_absent"], true);
    assert!(!admin.exists());

    fixture.create(&["--ephemeral"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--delete-branches"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([path]));
    assert_eq!(report["skipped"], serde_json::json!([]));
    assert!(!branch_exists(&fixture.repo, "chat/test"));
}

/// Git counts a `locked` entry that is a dangling symlink as a lock (it checks
/// with lstat), so simgit must too. `git worktree list` then fails reading the
/// reason, and so does every command that lists worktrees; none may drop it.
#[test]
fn a_dangling_symlink_lock_keeps_a_deleted_worktree() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let admin = Path::new(&registration(&fixture, &fixture.worktree)).to_path_buf();
    std::os::unix::fs::symlink(fixture.root.join("nowhere"), admin.join("locked")).unwrap();
    // From inside the worktree, unlock finds the lock without listing.
    let unlocked = fixture
        .command()
        .current_dir(&fixture.worktree)
        .args(["--json", "unlock"])
        .output()
        .unwrap();
    assert!(!unlocked.status.success(), "unlock called it unlocked");
    assert!(fs::symlink_metadata(admin.join("locked")).is_ok());
    // `git worktree unlock` dies reading it too, so the advice is to delete it.
    let stderr = String::from_utf8_lossy(&unlocked.stderr);
    assert!(
        stderr.contains("rm ") && stderr.contains("/locked"),
        "{stderr}"
    );
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let pruned = fixture.run(&["prune"]);
    let stdout = String::from_utf8_lossy(&pruned.stdout);
    assert!(!stdout.contains("pruned 1 stale"), "{stdout}");
    assert!(admin.join("HEAD").is_file(), "prune dropped it");

    let removed = fixture.run(&["remove", "chat/test", "--discard-dirty"]);
    assert!(!removed.status.success(), "removed a locked worktree");
    assert!(admin.join("HEAD").is_file(), "remove dropped it");

    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--discard-dirty"]);
    if gc.status.success() {
        let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
        assert_eq!(report["reaped"], serde_json::json!([]));
    }
    assert!(admin.join("HEAD").is_file(), "gc dropped it");
}

/// The admin dir of the registration whose HEAD is on `branch`.
fn registration_by_branch(fixture: &Fixture, branch: &str) -> String {
    let wanted = format!("ref: refs/heads/{branch}");
    let dir = fixture.repo.join(".git/worktrees");
    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap().path();
        if fs::read_to_string(entry.join("HEAD")).unwrap().trim() == wanted {
            return entry.display().to_string();
        }
    }
    panic!("no registration on {branch}");
}

/// A registration may spell its absolute path through a symlink, as one whose
/// parent was moved and linked back does. Git lists that spelling, and the
/// lock lookup, which compares canonical forms, must still match it.
#[test]
fn a_deleted_worktree_registered_through_a_symlink_can_be_removed() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let admin = Path::new(&registration(&fixture, &fixture.worktree)).to_path_buf();
    let alias = fixture.root.join("alias");
    std::os::unix::fs::symlink(fixture.root.canonicalize().unwrap(), &alias).unwrap();
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let spelled = alias
        .join(fixture.worktree.file_name().unwrap())
        .join(".git");
    fs::write(admin.join("gitdir"), format!("{}\n", spelled.display())).unwrap();

    let removed = fixture.run(&["--json", "remove", "chat/test", "--discard-dirty"]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert!(!admin.exists());
}
