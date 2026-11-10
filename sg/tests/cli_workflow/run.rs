//! `run`: launching, signals, reuse and reattaching deleted worktrees.

use crate::{
    commit_in, dead_pid, git, git_stdout, git_wrapper, isolated, linked_worktree_ahead,
    registered_head, registration, resolved, rev_parse, run_from, success, wait_for, Fixture,
};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn run_creates_executes_and_gc_removes_the_agent_worktree_and_branch() {
    let root = std::env::temp_dir().join(format!(
        "simgit-cli-workflow-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let repo = root.join("repo with spaces");
    let worktree = root.join("agent worktree");
    fs::create_dir_all(&repo).expect("create repository");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test User"]);
    fs::write(repo.join("README.md"), "base\n").expect("write fixture");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "initial"]);

    let sg = env!("CARGO_BIN_EXE_sg");
    let run = isolated(sg)
        .current_dir(&repo)
        .args(["run", "agent/test", "--ephemeral", "--path"])
        .arg(&worktree)
        .args([
            "--",
            "sh",
            "-c",
            "printf 'agent output\\n' > result.txt && git add . && git commit -qm agent-result",
        ])
        .output()
        .expect("run sg run");
    assert!(
        run.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        fs::read_to_string(worktree.join("result.txt")).expect("agent result"),
        "agent output\n"
    );

    let gc = isolated(sg)
        .current_dir(&repo)
        .args([
            "gc",
            "--older-than",
            "0s",
            "--delete-branches",
            "--delete-unmerged",
        ])
        .output()
        .expect("run sg gc");
    assert!(
        gc.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(!worktree.exists());
    let branch = isolated("git")
        .current_dir(&repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/agent/test"])
        .status()
        .expect("inspect branch");
    assert!(!branch.success());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn run_reuses_dirty_workspace_and_preserves_command_arguments() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    fs::write(fixture.worktree.join("scratch.txt"), "unfinished chat").unwrap();
    let output = fixture.run(&[
        "run",
        "chat/test",
        "--",
        "sh",
        "-c",
        "test \"$(cat scratch.txt)\" = 'unfinished chat' && test \"$1\" = --resume",
        "sh",
        "--resume",
    ]);
    success(&output);
    let entry = fixture.listed();
    assert_eq!(
        entry["worktree"],
        fixture.worktree.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(entry["ephemeral"], false);
    assert!(entry.get("locked").is_none());
    let list = fixture.run(&["list"]);
    success(&list);
    let text = String::from_utf8(list.stdout).unwrap();
    assert!(text.contains("chat/test") && text.contains("persistent"));
    success(&fixture.run(&["gc", "--older-than", "0s", "--discard-dirty"]));
    assert!(fixture.worktree.join("scratch.txt").is_file());
}

#[test]
fn run_attaches_existing_branch_without_resetting_it() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-qb", "chat/test"]);
    fs::write(fixture.repo.join("branch.txt"), "branch content").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "branch work"]);
    git(&fixture.repo, &["checkout", "--detach", "HEAD~1"]);
    fixture.create(&[]);
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("branch.txt")).unwrap(),
        "branch content"
    );
    assert!(!fixture.repo.join("branch.txt").exists());
}

#[test]
fn run_rejects_conflicting_options_and_add_remains_strict() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    for options in [
        vec!["--base", "HEAD"],
        vec!["--require-cow"],
        vec!["--path", "."],
    ] {
        let output = fixture
            .command()
            .args(["run", "chat/test"])
            .args(options)
            .args(["--", "sh", "-c", "touch should-not-run"])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert!(!fixture.worktree.join("should-not-run").exists());
    assert!(!fixture.run(&["add", "chat/test"]).status.success());
    success(
        &fixture
            .command()
            .args(["run", "chat/test", "--path"])
            .arg(&fixture.worktree)
            .args(["--", "true"])
            .output()
            .unwrap(),
    );
}

#[test]
fn failed_commands_retain_workspace_and_release_lock() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    for (command, code) in [
        (vec!["simgit-command-that-does-not-exist"], 127),
        (vec!["sh", "-c", "exit 7"], 7),
    ] {
        let output = fixture
            .command()
            .args(["run", "chat/test", "--"])
            .args(command)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code));
        assert!(fixture.worktree.is_dir());
        assert!(fixture.listed().get("locked").is_none());
    }
    success(&fixture.run(&["run", "chat/test", "--", "true"]));
}

/// The child starts in the worktree, so `$PWD` must say so; a shell trusts it
/// over the real working directory.
#[test]
fn run_sets_pwd_to_the_worktree() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let output = fixture
        .command()
        .env("PWD", &fixture.repo)
        .args(["run", "chat/test", "--", "printenv", "PWD"])
        .output()
        .unwrap();
    success(&output);
    let reported = fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim_end()).unwrap();
    assert_eq!(reported, fs::canonicalize(&fixture.worktree).unwrap());
}

/// A command that exists but cannot be executed exits 126, as in a shell, and
/// is told apart from one that is missing (127).
#[test]
fn run_exits_126_for_a_non_executable() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.create(&[]);
    let script = fixture.worktree.join("plain.sh");
    fs::write(&script, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    let output = fixture
        .command()
        .args(["run", "chat/test", "--", "./plain.sh"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126));
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines = stderr
        .lines()
        .filter(|line| line.contains("not executable"));
    assert_eq!(lines.count(), 1, "stderr: {stderr}");
    assert!(fixture.listed().get("locked").is_none());
}

/// A lock that cannot be removed afterwards is reported, and the child's
/// status still wins: here the child makes its own lock's directory read-only.
#[test]
fn run_keeps_the_child_status_when_release_fails() {
    use std::os::unix::fs::PermissionsExt;
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if unsafe { geteuid() } == 0 {
        eprintln!("skipped: root ignores read-only directories");
        return;
    }
    let fixture = Fixture::new();
    fixture.create(&[]);
    let script = "dir=$(git rev-parse --absolute-git-dir) && chmod a-w \"$dir\" && exit 7";
    let output = fixture
        .command()
        .args(["run", "chat/test", "--", "sh", "-c", script])
        .output()
        .unwrap();
    let admin = fs::read_to_string(fixture.worktree.join(".git"))
        .unwrap()
        .trim()
        .strip_prefix("gitdir: ")
        .unwrap()
        .to_owned();
    fs::set_permissions(&admin, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(output.status.code(), Some(7));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not unlock worktree"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("simgit unlock"), "stderr: {stderr}");
}

/// A `run` whose command blocks until `release` exists. The release file is
/// outside the worktree, so a test that deletes the worktree can still write
/// it, and the script gives up after a minute on its own. Dropping it, as a
/// panicking test does, releases and reaps the command.
struct BlockedRun {
    child: std::process::Child,
    release: std::path::PathBuf,
}

impl BlockedRun {
    fn start(fixture: &Fixture) -> Self {
        let started = fixture.root.join("started");
        let release = fixture.root.join("release");
        let child = fixture
            .command()
            .args(["run", "chat/test", "--", "sh", "-c"])
            .arg(
                "touch \"$1\"; n=0; \
                 while [ ! -f \"$2\" ] && [ \"$n\" -lt 1200 ]; do sleep 0.05; n=$((n+1)); done",
            )
            .arg("sh")
            .arg(&started)
            .arg(&release)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let run = Self { child, release };
        wait_for(&started);
        run
    }

    fn finish(&mut self) -> std::process::ExitStatus {
        fs::write(&self.release, "").unwrap();
        self.child.wait().unwrap()
    }
}

impl Drop for BlockedRun {
    fn drop(&mut self) {
        // Give the command time to see the release file before the fixture
        // deletes it; only then kill a launcher that is stuck.
        let _ = fs::write(&self.release, "");
        for _ in 0..100 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn running_commands_are_protected_from_gc_and_remove() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let mut run = BlockedRun::start(&fixture);
    fs::write(fixture.worktree.join("unsaved.txt"), "agent work").unwrap();

    let gc = fixture.run(&["gc", "--older-than", "0s", "--discard-dirty", "--json"]);
    let remove = fixture.run(&["remove", "chat/test", "--discard-dirty"]);
    let duplicate = fixture.run(&["run", "chat/test", "--", "true"]);
    let listed = fixture.listed();
    let survived = fixture.worktree.join("unsaved.txt").is_file();
    let status = run.finish();

    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    assert_eq!(
        report["skipped"],
        serde_json::json!([{ "worktree": resolved(&fixture.worktree), "reason": "locked" }])
    );
    assert!(!remove.status.success(), "remove deleted a running command");
    assert!(!duplicate.status.success(), "a second run shared the lock");
    assert!(survived, "the running command's files did not survive");
    assert!(listed.get("locked").is_some());
    assert!(status.success());

    // Once the command is done, the same worktree is reaped.
    success(&fixture.run(&["gc", "--older-than", "0s", "--discard-dirty"]));
    assert!(!fixture.worktree.exists());
}

const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGQUIT: i32 = 3;
const SIGTERM: i32 = 15;

/// Send `signal` to `pid`, or to the process group `-pid`.
fn send_signal(pid: i32, signal: i32) {
    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // SAFETY: `kill` takes two integers and dereferences nothing.
    assert_eq!(unsafe { kill(pid, signal) }, 0, "kill({pid}, {signal})");
}

/// Start `run chat/test -- sh -c <script>` as the leader of its own process
/// group, the way a shell starts a foreground job, so the test can signal that
/// group as a terminal does. Returns once the script has written `ready`.
///
/// SIGINT and SIGQUIT are reset to their defaults first: a harness that runs
/// the tests as a background job (`cmd &` in a non-interactive shell) leaves
/// them ignored, simgit passes that on, and `sh` cannot trap a signal that was
/// ignored on entry.
fn start_run_in_group(fixture: &Fixture, script: &str) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    let mut command = fixture.command();
    // SAFETY: the closure runs between fork and exec and only calls `signal`,
    // which is async-signal-safe, with `SIG_DFL` (0), a reserved value.
    unsafe {
        command.pre_exec(|| {
            signal(SIGINT, 0);
            signal(SIGQUIT, 0);
            Ok(())
        });
    }
    let child = command
        .args(["run", "chat/test", "--", "sh", "-c", script])
        .process_group(0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&fixture.worktree.join("ready"));
    child
}

/// Wait for `child` to exit, killing it and failing rather than hanging.
fn wait_with_deadline(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("simgit run did not exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A script loop that gives up after 20 seconds, so a child orphaned by a
/// launcher that died does not outlive the test by long.
const BOUNDED_WAIT: &str = "i=0; while [ $i -lt 400 ]; do sleep 0.05; i=$((i + 1)); done";

/// Ctrl-C signals the whole foreground process group. `run` leaves it to the
/// child: one that traps it and finishes decides the exit status, and the
/// lock is released once it has.
#[test]
fn ctrl_c_lets_the_child_finish_and_releases_the_lock() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    // The trap sets a flag the loop only reads. Having it set the loop's own
    // counter loses to the loop's `i=$((i + 1))` when the signal lands between
    // that expansion and its assignment, and the loop then runs on.
    let script = "trap 'touch interrupted; stop=1' INT; touch ready; i=0; \
                  while [ $i -lt 400 ] && [ -z \"$stop\" ]; do sleep 0.05; i=$((i + 1)); done; \
                  exit 7";
    let mut run = start_run_in_group(&fixture, script);
    send_signal(-(run.id() as i32), SIGINT);
    let status = wait_with_deadline(&mut run);
    assert_eq!(
        status.code(),
        Some(7),
        "run did not exit with the child's status: {status}"
    );
    assert!(fixture.worktree.join("interrupted").exists());
    assert!(fixture.listed().get("locked").is_none(), "lock left behind");
}

/// A hangup sent to `run` alone reaches the child, and `run` still releases
/// its lock and reports how the child exited.
#[test]
fn sighup_is_forwarded_and_the_lock_released() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let script = format!("trap 'touch hung-up; exit 3' HUP; touch ready; {BOUNDED_WAIT}");
    let mut run = start_run_in_group(&fixture, &script);
    send_signal(run.id() as i32, SIGHUP);
    let status = wait_with_deadline(&mut run);
    assert_eq!(
        status.code(),
        Some(3),
        "run did not exit with the child's status: {status}"
    );
    assert!(fixture.worktree.join("hung-up").exists());
    assert!(fixture.listed().get("locked").is_none(), "lock left behind");
}

/// SIGTERM sent to `run` alone terminates the child, and `run` exits the way
/// a shell reports a child killed by it.
#[test]
fn sigterm_is_forwarded() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let mut run = start_run_in_group(&fixture, &format!("touch ready; {BOUNDED_WAIT}"));
    send_signal(run.id() as i32, SIGTERM);
    let status = wait_with_deadline(&mut run);
    assert_eq!(
        status.code(),
        Some(128 + SIGTERM),
        "run did not report the child's signal: {status}"
    );
    assert!(fixture.listed().get("locked").is_none(), "lock left behind");
}

#[test]
fn persistence_can_be_changed_explicitly_on_reuse() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    assert_eq!(fixture.listed()["ephemeral"], true);
    success(&fixture.run(&["run", "chat/test", "--persistent", "--", "true"]));
    assert_eq!(fixture.listed()["ephemeral"], false);
    success(&fixture.run(&["gc", "--older-than", "0s", "--discard-dirty"]));
    assert!(fixture.worktree.exists());
    success(&fixture.run(&[
        "gc",
        "--include-persistent",
        "--older-than",
        "0s",
        "--discard-dirty",
    ]));
    assert!(!fixture.worktree.exists());
}

#[test]
fn run_reuses_main_worktree_without_creating_another_checkout() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-qb", "chat/main"]);
    success(&fixture.run(&["run", "chat/main", "--", "sh", "-c", "test -f README.md"]));
    assert!(!fixture.repo.join(".git/simgit-run.lock").exists());
    let output = fixture.run(&["list", "--json"]);
    success(&output);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(entries.len(), 1);
}

/// `run` told people a lock Git set was "a running command" and sent them to
/// `unlock`, which refuses it. It should say what the lock really is.
#[test]
fn run_reports_gits_lock_reason() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let path = fixture.worktree.display().to_string();
    git(
        &fixture.worktree,
        &["worktree", "lock", "--reason", "usb", &path],
    );

    let refused = fixture.run(&["run", "chat/test", "--", "true"]);
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success());
    assert!(message.contains("usb"), "Git's reason missing: {message}");
    assert!(
        !message.contains("running command"),
        "Git's lock reported as a running command: {message}"
    );
    assert!(message.contains("git worktree unlock"), "{message}");
}

/// Once a killed allocation's lock is cleared, its worktree has no index and
/// shows every tracked file as deleted. `run` refuses to start a command in
/// it, on one line naming the removal, and after that removal runs again.
#[test]
fn run_refuses_a_half_made_worktree_after_unlock() {
    let fixture = Fixture::new();
    let path = resolved(&fixture.root.join("half"));
    let reason = format!("simgit: allocating (pid {})", dead_pid());
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--no-checkout",
            "--lock",
            "--reason",
            &reason,
            "-b",
            "chat/half",
            path.to_str().unwrap(),
        ],
    );
    success(&fixture.run(&["unlock", path.to_str().unwrap()]));

    let marker = fixture.root.join("ran");
    let marker_arg = marker.to_str().unwrap();
    let refused = fixture.run(&["run", "chat/half", "--", "touch", marker_arg]);
    assert!(!refused.status.success());
    assert!(!marker.exists(), "the command ran in a half-made worktree");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let main = fixture.repo.canonicalize().unwrap();
    let discard = format!(
        "cd {} && simgit remove {} --discard-dirty",
        main.display(),
        path.display()
    );
    assert!(stderr.contains(&discard), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");

    // Committing it would record every tracked file as deleted.
    let tip = || {
        let output = isolated("git")
            .current_dir(&fixture.repo)
            .args(["rev-parse", "chat/half"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let before = tip();
    let committed = fixture.run(&["remove", path.to_str().unwrap(), "--commit"]);
    assert!(!committed.status.success());
    assert!(
        String::from_utf8_lossy(&committed.stderr).contains("never populated"),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_eq!(tip(), before);

    success(&fixture.run(&["remove", path.to_str().unwrap(), "--discard-dirty"]));
    success(&fixture.run(&[
        "run",
        "chat/half",
        "--path",
        path.to_str().unwrap(),
        "--",
        "touch",
        marker_arg,
    ]));
    assert!(marker.exists());
    assert!(path.join("README.md").is_file());
}

#[test]
fn run_from_a_linked_worktree_starts_at_its_head() {
    let fixture = Fixture::new();
    let linked = linked_worktree_ahead(&fixture);
    let head = git_stdout(&linked, &["rev-parse", "HEAD"]);

    let path = fixture.root.join("run-from-linked");
    let output = run_from(
        &fixture,
        &linked,
        &[
            "run",
            "agent/run-linked",
            "--path",
            path.to_str().unwrap(),
            "--",
            "true",
        ],
    );
    success(&output);
    assert_eq!(git_stdout(&path, &["rev-parse", "HEAD"]), head);
}

/// A worktree deleted by hand leaves its registration, which keeps its branch
/// checked out at a missing path. `run` drops that registration when it holds
/// nothing, and checks the branch out again where it was.
#[test]
fn run_reattaches_a_worktree_deleted_by_hand() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let tip = commit_in(&fixture.worktree, "kept.txt");
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let output = fixture.run(&["run", "chat/test", "--", "sh", "-c", "test -f kept.txt"]);
    success(&output);
    assert_eq!(
        registered_head(&fixture, &fixture.worktree),
        Some(tip.clone())
    );
    assert_eq!(rev_parse(&fixture.repo, "chat/test"), tip);
    let listed = fixture.run(&["list", "--json"]);
    success(&listed);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert!(fixture.listed().get("locked").is_none(), "lock left behind");
}

/// Dropping a registration that holds staged work would lose it, so `run`
/// refuses as `remove` does and keeps the registration.
#[test]
fn run_refuses_to_reattach_a_deleted_worktree_holding_staged_work() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    fs::write(fixture.worktree.join("staged.txt"), "work\n").unwrap();
    git(&fixture.worktree, &["add", "staged.txt"]);
    let head = rev_parse(&fixture.worktree, "HEAD");
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let refused = fixture.run(&["run", "chat/test", "--", "true"]);
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("cannot reattach"), "{message}");
    assert!(message.contains("still holds staged changes"), "{message}");
    assert!(message.contains("diff --cached --stat"), "{message}");
    assert!(
        message.contains("`simgit remove chat/test --discard-dirty`"),
        "{message}"
    );
    assert!(!fixture.worktree.exists());
    assert_eq!(registered_head(&fixture, &fixture.worktree), Some(head));
}

/// A command refused for its arguments changes nothing, even on a worktree
/// deleted by hand whose registration it would otherwise drop.
#[test]
fn run_rejects_bad_arguments_before_dropping_a_deleted_worktree() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let head = rev_parse(&fixture.worktree, "HEAD");
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let occupied = fixture.root.join("occupied");
    fs::create_dir(&occupied).unwrap();
    fs::write(occupied.join("data"), "someone's\n").unwrap();
    let occupied = occupied.display().to_string();

    for (args, expected) in [
        (&["--base", "HEAD"][..], "--base cannot be used"),
        (
            &["--sparse", "../outside"][..],
            "must be a path inside the repository",
        ),
        (&["--path", occupied.as_str()][..], "already exists"),
    ] {
        let refused = fixture
            .command()
            .args(["run", "chat/test"])
            .args(args)
            .args(["--", "true"])
            .output()
            .unwrap();
        let message = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert!(!refused.status.success(), "{args:?}: {message}");
        assert!(message.contains(expected), "{args:?}: {message}");
        assert_eq!(
            registered_head(&fixture, &fixture.worktree),
            Some(head.clone()),
            "{args:?} dropped the registration"
        );
    }
}

/// Reattaching keeps what the registration recorded about the worktree: an
/// ephemeral one stays ephemeral and a sparse one keeps its cone, unless the
/// command says otherwise.
#[test]
fn run_reattach_keeps_the_ephemeral_marker_and_sparse_cone() {
    let fixture = Fixture::new();
    for dir in ["kept", "other"] {
        fs::create_dir(fixture.repo.join(dir)).unwrap();
        fs::write(fixture.repo.join(dir).join("file"), "x\n").unwrap();
    }
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "dirs"]);
    let run = |extra: &[&str]| {
        let output = fixture
            .command()
            .args(["run", "chat/test", "--path"])
            .arg(&fixture.worktree)
            .args(extra)
            .args(["--", "true"])
            .output()
            .unwrap();
        success(&output);
    };
    let cone = || git_stdout(&fixture.worktree, &["sparse-checkout", "list"]);
    run(&["--ephemeral", "--sparse", "kept"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();

    run(&[]);
    assert_eq!(fixture.listed()["ephemeral"], true);
    assert_eq!(cone(), "kept");
    assert!(fixture.worktree.join("kept/file").exists());
    assert!(!fixture.worktree.join("other").exists());

    // Explicit flags win over what was recorded.
    fs::remove_dir_all(&fixture.worktree).unwrap();
    run(&["--persistent", "--sparse", "other"]);
    assert_eq!(fixture.listed()["ephemeral"], false);
    assert_eq!(cone(), "other");
    assert!(!fixture.worktree.join("kept").exists());
}

/// A reattach that cannot succeed is refused before the registration is
/// dropped, so a retry still finds the ephemeral marker. Here the forced
/// overlay mode is unavailable, which is known before anything changes.
#[test]
fn run_reattach_refused_for_its_populate_mode_keeps_the_registration() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    // Whatever the host has, simgit must not find fuse-overlayfs. It often
    // shares a directory with git, so keep git and the command reachable
    // through links of their own.
    let tools = fixture.root.join("tools-without-fuse");
    fs::create_dir_all(&tools).unwrap();
    let host_path = std::env::var_os("PATH").unwrap();
    for tool in ["git", "true"] {
        let found = std::env::split_paths(&host_path)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
            .unwrap();
        std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
    }
    let path = std::iter::once(tools).chain(
        std::env::split_paths(&host_path).filter(|dir| !dir.join("fuse-overlayfs").is_file()),
    );

    let refused = fixture
        .command()
        .env("SIMGIT_POPULATE", "overlay")
        .env("PATH", std::env::join_paths(path).unwrap())
        .args(["run", "chat/test", "--", "true"])
        .output()
        .unwrap();
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success(), "{message}");
    assert!(
        message.contains("fuse-overlayfs is not installed"),
        "{message}"
    );
    assert!(registered_head(&fixture, &fixture.worktree).is_some());

    fixture.create(&[]);
    assert_eq!(fixture.listed()["ephemeral"], true);
}

/// A failure that only shows after the drop says what was dropped, and how to
/// pass the lost settings again.
#[test]
fn run_reattach_failing_after_the_drop_names_the_settings_it_lost() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fs::create_dir(fixture.repo.join("kept")).unwrap();
    fs::write(fixture.repo.join("kept/file"), "x\n").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "dir"]);
    fixture.create(&["--ephemeral", "--sparse", "kept"]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let hook = fixture.repo.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let failed = fixture.run(&["run", "chat/test", "--", "true"]);
    let message = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(!failed.status.success(), "{message}");
    assert!(
        message.contains("was dropped before this failed"),
        "{message}"
    );
    assert!(
        message.contains("pass --ephemeral --sparse kept"),
        "{message}"
    );
}

/// A refused reattach names the recovery that works for its reason: a lock
/// set with Git is released with Git, and `remove` would refuse it too.
#[test]
fn run_refuses_to_reattach_a_locked_deleted_worktree_with_gits_reason() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let path = fixture.worktree.display().to_string();
    git(
        &fixture.repo,
        &["worktree", "lock", "--reason", "usb", &path],
    );
    fs::remove_dir_all(&fixture.worktree).unwrap();

    let refused = fixture.run(&["run", "chat/test", "--", "true"]);
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("usb"), "{message}");
    assert!(message.contains("git worktree unlock"), "{message}");
    assert!(!message.contains("simgit remove"), "{message}");
    assert!(registered_head(&fixture, &fixture.worktree).is_some());
}

/// A registration whose work cannot be judged is kept, and the refusal says so.
#[test]
fn run_refuses_to_reattach_a_deleted_worktree_it_cannot_judge() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let admin = registration(&fixture, &fixture.worktree);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    // A symlink to itself cannot be read even by root.
    let index = Path::new(&admin).join("index");
    fs::remove_file(&index).unwrap();
    std::os::unix::fs::symlink("index", &index).unwrap();

    let refused = fixture.run(&["run", "chat/test", "--", "true"]);
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success(), "{message}");
    assert!(
        message.contains(&format!(
            "cannot tell whether the registration of {} holds work",
            resolved(&fixture.worktree).display()
        )),
        "{message}"
    );
    assert!(registered_head(&fixture, &fixture.worktree).is_some());
}

/// The main checkout has no registration to mark, so `--ephemeral` used to
/// mark the whole repository's git dir instead.
#[test]
fn run_refuses_persistence_flags_on_the_main_checkout() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-qb", "chat/main"]);
    let started = fixture.root.join("started");
    for flag in ["--ephemeral", "--persistent"] {
        let refused = fixture
            .command()
            .args(["run", "chat/main", flag, "--", "touch"])
            .arg(&started)
            .output()
            .unwrap();
        let message = String::from_utf8_lossy(&refused.stderr).into_owned();
        assert!(!refused.status.success(), "{flag}: {message}");
        assert!(message.contains("main checkout"), "{flag}: {message}");
        assert_eq!(message.trim_end().lines().count(), 1, "{flag}: {message}");
        assert!(!started.exists(), "{flag}: the command ran");
    }
    assert!(!fixture.repo.join(".git/simgit-ephemeral").exists());
    assert!(!fixture.repo.join(".git/simgit-run.lock").exists());
}

/// `run --ephemeral` allocates a disposable worktree and then launches into
/// it. The wrapper runs `gc --older-than 0s` after every Git command `run`
/// makes once the worktree is registered, so if the worktree is ever unlocked
/// before the command starts, gc reaps it there.
#[test]
fn gc_cannot_reap_a_fresh_ephemeral_worktree_before_run_starts_its_command() {
    let fixture = Fixture::new();
    let armed = fixture.root.join("armed");
    let log = fixture.root.join("gc.log");
    let path = fixture.root.join("fresh");
    let env = git_wrapper(
        &fixture,
        &format!(
            "\"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n\
             status=$?\n\
             if [ -n \"$SIMGIT_TEST_RACE\" ]; then\n\
               if [ \"$2\" = worktree ] && [ \"$3\" = add ]; then\n\
                 : > '{armed}'\n\
               elif [ -e '{armed}' ]; then\n\
                 (cd '{repo}' && SIMGIT_TEST_RACE= '{sg}' gc --older-than 0s) >> '{log}' 2>&1\n\
               fi\n\
             fi\n\
             exit $status\n",
            armed = armed.display(),
            repo = fixture.repo.display(),
            sg = env!("CARGO_BIN_EXE_sg"),
            log = log.display(),
        ),
    );

    let output = fixture
        .command()
        .envs(env)
        .env("SIMGIT_TEST_RACE", "1")
        .args(["run", "agent/fresh", "--ephemeral", "--path"])
        .arg(&path)
        .args(["--", "sh", "-c", "test -f README.md"])
        .output()
        .unwrap();
    let gc = fs::read_to_string(&log).unwrap_or_default();
    success(&output);
    assert!(!gc.contains("reaped: "), "{gc}");
    // gc saw the worktree marked ephemeral and still locked.
    assert!(gc.contains("skipped (locked)"), "{gc}");
    assert!(path.exists());
}
