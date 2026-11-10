use std::fs;
use std::path::Path;
use std::process::Command;
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
    let run = Command::new(sg)
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

    let gc = Command::new(sg)
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
    let branch = Command::new("git")
        .current_dir(&repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/agent/test"])
        .status()
        .expect("inspect branch");
    assert!(!branch.success());

    let _ = fs::remove_dir_all(root);
}

/// A competing creator can claim a branch after `add` checks that the name is
/// available, but before Git registers its worktree. Failed allocation must
/// never delete that creator's commits while cleaning up its own state.
#[test]
fn failed_allocation_does_not_delete_a_competing_branch() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-qb", "competitor"]);
    fs::write(fixture.repo.join("competitor.txt"), "unmerged work").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "competitor work"]);
    let commit = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    success(&commit);
    let commit = String::from_utf8(commit.stdout).unwrap().trim().to_owned();
    git(&fixture.repo, &["checkout", "-q", "-"]);

    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$2\" = worktree ] && [ \"$3\" = add ]; then\n\
           \"$SIMGIT_TEST_REAL_GIT\" -C \"$SIMGIT_TEST_REPO\" branch agent/race \"$SIMGIT_TEST_COMMIT\" || exit\n\
         fi\n\
         exec \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(wrapper_dir)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = fixture
        .command()
        .env("PATH", path)
        .env("SIMGIT_TEST_REAL_GIT", real_git)
        .env("SIMGIT_TEST_REPO", &fixture.repo)
        .env("SIMGIT_TEST_COMMIT", &commit)
        .args(["add", "agent/race", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    assert!(!output.status.success(), "competing branch must block add");
    let surviving = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["rev-parse", "--verify", "refs/heads/agent/race"])
        .output()
        .unwrap();
    success(&surviving);
    assert_eq!(String::from_utf8(surviving.stdout).unwrap().trim(), commit);
    assert!(!fixture.worktree.exists());
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?}");
}

struct Fixture {
    root: std::path::PathBuf,
    repo: std::path::PathBuf,
    worktree: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("simgit-resume-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        let worktree = root.join("agent \"quoted\"\tworkspace");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test User"]);
        fs::write(repo.join("README.md"), "original\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "initial"]);
        Self {
            root,
            repo,
            worktree,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sg"));
        command
            .current_dir(&self.repo)
            .env("SIMGIT_POPULATE", "checkout")
            .env_remove("SIMGIT_WORKTREE_ROOT");
        command
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    fn create(&self, extra: &[&str]) {
        let output = self
            .command()
            .args(["run", "chat/test", "--path"])
            .arg(&self.worktree)
            .args(extra)
            .args(["--", "true"])
            .output()
            .unwrap();
        success(&output);
    }

    fn listed(&self) -> serde_json::Value {
        let output = self.run(&["list", "--json"]);
        success(&output);
        let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        entries
            .into_iter()
            .find(|e| e["branch"] == "refs/heads/chat/test")
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
fn add_attach_checks_out_an_existing_branch_and_refuses_a_taken_one() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-qb", "chat/test"]);
    fs::write(fixture.repo.join("branch.txt"), "branch content").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "branch work"]);
    git(&fixture.repo, &["checkout", "-q", "--detach", "HEAD~1"]);
    let tip = || {
        let output = Command::new("git")
            .current_dir(&fixture.repo)
            .args(["rev-parse", "--verify", "refs/heads/chat/test"])
            .output()
            .unwrap();
        success(&output);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let branch_tip = tip();
    let attach = |path: &Path| {
        fixture
            .command()
            .args([
                "--json",
                "add",
                "--attach",
                "chat/test",
                "--ephemeral",
                "--path",
            ])
            .arg(path)
            .output()
            .unwrap()
    };

    // --attach never creates the branch it was asked for.
    let missing_path = fixture.root.join("missing");
    let missing = fixture
        .command()
        .args(["--json", "add", "--attach", "chat/missing", "--path"])
        .arg(&missing_path)
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(!missing_path.exists());

    let output = attach(&fixture.worktree);
    success(&output);
    let added: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let worktree = fixture.worktree.canonicalize().unwrap();
    assert_eq!(added["branch"], "chat/test");
    assert_eq!(added["base"], branch_tip.as_str());
    assert_eq!(added["ephemeral"], true);
    assert_eq!(added["cleanup_token"], worktree.to_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("branch.txt")).unwrap(),
        "branch content"
    );
    assert_eq!(tip(), branch_tip);

    // A second allocation must not be handed the first one's worktree: its
    // cleanup token would remove a workspace it never created.
    let taken_path = fixture.root.join("taken");
    let taken = attach(&taken_path);
    assert!(!taken.status.success());
    assert!(taken.stdout.is_empty());
    assert!(!taken_path.exists());
    // The one-line JSON diagnostic names that worktree verbatim, tab and all.
    let stderr = String::from_utf8(taken.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains(worktree.to_str().unwrap()), "{stderr}");

    // Removing an attached worktree keeps the branch it checked out.
    success(&fixture.run(&["--json", "remove", worktree.to_str().unwrap()]));
    assert!(!worktree.exists());
    assert_eq!(tip(), branch_tip);
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
fn require_cow_rejects_forced_checkout_before_creating_branch() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args(["run", "chat/test", "--require-cow", "--path"])
        .arg(&fixture.worktree)
        .args(["--", "true"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts with --require-cow"));
    assert!(!fixture.worktree.exists());
    let output = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/chat/test"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
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

#[test]
fn running_commands_are_protected_from_gc_and_remove() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let mut child = fixture
        .command()
        .args([
            "run",
            "chat/test",
            "--",
            "sh",
            "-c",
            "touch started; sleep 3",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !fixture.worktree.join("started").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let started = fixture.worktree.join("started").exists();
    let gc = fixture.run(&["gc", "--older-than", "0s", "--discard-dirty", "--json"]);
    let remove = fixture.run(&["remove", "chat/test", "--discard-dirty"]);
    let duplicate = fixture.run(&["run", "chat/test", "--", "true"]);
    let listed = fixture.listed();
    let status = child.wait().unwrap();
    assert!(started && status.success());
    success(&gc);
    assert!(String::from_utf8_lossy(&gc.stdout).contains("locked"));
    assert!(!remove.status.success() && !duplicate.status.success());
    assert!(listed.get("locked").is_some());
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

#[test]
fn default_worktree_path_stays_outside_the_git_directory() {
    // Agent harnesses (Claude Code, editors, ripgrep) treat everything under
    // `.git/` as off-limits or invisible, so a worktree nested in the git dir
    // cannot be edited by the tools simgit exists to serve. The default must
    // be a sibling of the main working tree.
    let fixture = Fixture::new();
    let output = fixture.run(&["add", "agent/edit"]);
    success(&output);
    let printed = String::from_utf8(output.stdout).unwrap();
    let path = std::path::PathBuf::from(printed.lines().last().unwrap().trim());

    assert!(
        !path.components().any(|part| part.as_os_str() == ".git"),
        "default worktree must not live under .git: {}",
        path.display()
    );
    assert_eq!(
        path.parent().unwrap().canonicalize().unwrap(),
        fixture.root.join(".simgit/repo").canonicalize().unwrap()
    );
    assert!(path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("agent-edit"));
    assert!(path.join("README.md").is_file());
}

#[test]
fn cow_materialization_reproduces_the_checkout_git_would_make() {
    // The fast path replaces the registered worktree directory with a
    // whole-tree clone and installs the baseline's index, so this pins what
    // that dance must preserve: file content, symlinks, the executable bit, a
    // clean first status, and normal change detection afterwards.
    let root = std::env::temp_dir().join(format!("simgit-materialize-{}", uuid::Uuid::new_v4()));
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("nested/deep")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test User"]);
    fs::write(repo.join("nested/deep/data.txt"), "payload\n").unwrap();
    fs::write(repo.join("script.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(
        repo.join("script.sh"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("nested/deep/data.txt", repo.join("link.txt")).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "initial"]);

    let output = Command::new(env!("CARGO_BIN_EXE_sg"))
        .current_dir(&repo)
        .env_remove("SIMGIT_POPULATE")
        .env_remove("SIMGIT_WORKTREE_ROOT")
        .args(["add", "agent/clone"])
        .output()
        .unwrap();
    success(&output);
    let printed = String::from_utf8(output.stdout).unwrap();
    let worktree = std::path::PathBuf::from(printed.lines().last().unwrap().trim());

    assert_eq!(
        fs::read_to_string(worktree.join("nested/deep/data.txt")).unwrap(),
        "payload\n"
    );
    assert_eq!(
        fs::read_link(worktree.join("link.txt")).unwrap(),
        Path::new("nested/deep/data.txt")
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &fs::metadata(worktree.join("script.sh"))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o111, 0o111, "executable bit lost: {mode:o}");

    let status = Command::new("git")
        .current_dir(&worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "");

    fs::write(worktree.join("nested/deep/data.txt"), "edited\n").unwrap();
    let status = Command::new("git")
        .current_dir(&worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(status.stdout).unwrap(),
        " M nested/deep/data.txt\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn add_takes_its_location_only_from_the_path_flag() {
    // `add` once accepted the location both positionally and as `--path`, two
    // spellings for one thing. Only the flag survives, and a stray positional
    // must be rejected rather than quietly reinterpreted as something else.
    let fixture = Fixture::new();
    let flagged = fixture.root.join("by-flag");
    let positional = fixture.root.join("by-position");

    success(&fixture.run(&["add", "feat/flag", "--path", flagged.to_str().unwrap()]));
    assert!(flagged.join("README.md").is_file());

    let rejected = fixture.run(&["add", "feat/positional", positional.to_str().unwrap()]);
    assert!(!rejected.status.success());
    assert!(!positional.exists());
}

/// `--delete-unmerged` only widens branch deletion, so on its own it promises
/// a deletion that never happens. It must be refused before anything is torn
/// down, leaving both the worktree and the branch untouched.
#[test]
fn remove_refuses_delete_unmerged_without_delete_branch() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let output = fixture.run(&["remove", "chat/test", "--delete-unmerged"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--delete-branch"),
        "error must name the missing flag: {stderr}"
    );
    assert!(fixture.worktree.is_dir(), "worktree was torn down anyway");
    let branch = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/chat/test"])
        .status()
        .unwrap();
    assert!(branch.success(), "branch chat/test was deleted anyway");
}

/// `remove --commit` on the main checkout used to commit the user's files and
/// only then fail, because Git cannot remove the main worktree. It must refuse
/// before committing, by path and by the default target alike.
#[test]
fn remove_commit_refuses_the_main_checkout_before_committing() {
    let fixture = Fixture::new();
    fs::write(fixture.repo.join("README.md"), "edited\n").unwrap();
    fs::write(fixture.repo.join("scratch.txt"), "work in progress\n").unwrap();
    let head = |repo: &Path| {
        let output = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let status = |repo: &Path| {
        let output = Command::new("git")
            .current_dir(repo)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let before = (head(&fixture.repo), status(&fixture.repo));
    let path = fixture.repo.display().to_string();

    for args in [
        vec!["remove", path.as_str(), "--commit"],
        vec!["remove", "--commit"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("main checkout"), "{args:?}: {stderr}");
        assert_eq!(
            (head(&fixture.repo), status(&fixture.repo)),
            before,
            "{args:?} committed or changed the main checkout"
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("scratch.txt")).unwrap(),
        "work in progress\n"
    );
}

/// `list` ran four `git rev-parse` per worktree, 16 s for 600 of them. It
/// reads the registrations once instead, so its `git` count stays put.
#[test]
fn list_runs_no_git_per_worktree() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        "#!/bin/sh\necho >> \"$SIMGIT_TEST_GIT_LOG\"\nexec \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(wrapper_dir)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let log = fixture.root.join("git.log");
    let gits_for = |args: &[&str]| {
        let _ = fs::remove_file(&log);
        let output = fixture
            .command()
            .env("PATH", &path)
            .env("SIMGIT_TEST_REAL_GIT", &real_git)
            .env("SIMGIT_TEST_GIT_LOG", &log)
            .args(args)
            .output()
            .unwrap();
        success(&output);
        fs::read_to_string(&log).unwrap().lines().count()
    };
    let add = |name: &str, extra: &[&str]| {
        success(
            &fixture
                .command()
                .args(["add", name, "--path"])
                .arg(fixture.root.join(name))
                .args(extra)
                .output()
                .unwrap(),
        );
    };

    add("one", &["--ephemeral"]);
    let few = (gits_for(&["list"]), gits_for(&["list", "--json"]));
    for name in ["two", "three", "four", "five"] {
        add(name, &[]);
    }
    let many = (gits_for(&["list"]), gits_for(&["list", "--json"]));
    assert_eq!(few, many, "list ran more git for more worktrees");
}

#[test]
fn prune_and_list_answer_machine_readable_questions() {
    // `prune --json` advertised JSON in --help and printed prose, which breaks
    // any orchestrator parsing it. And the populate mode was printed once at
    // creation and never again, so nothing could answer "is this worktree
    // actually CoW-backed?" — the one thing simgit exists to provide.
    let fixture = Fixture::new();
    let created = fixture.run(&["add", "feat/mode"]);
    success(&created);
    let mode = String::from_utf8_lossy(&created.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("mode: ").map(str::to_owned))
        .expect("add reports the populate mode");

    let listed = fixture.run(&["list", "--json"]);
    success(&listed);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry["branch"] == "refs/heads/feat/mode")
        .expect("worktree is listed");
    assert_eq!(entry["mode"], mode);

    let human = fixture.run(&["list"]);
    success(&human);
    assert!(String::from_utf8_lossy(&human.stdout)
        .lines()
        .any(|line| line.starts_with("feat/mode\t") && line.ends_with(&mode)));

    let pruned = fixture.run(&["prune", "--json"]);
    success(&pruned);
    let report: serde_json::Value =
        serde_json::from_slice(&pruned.stdout).expect("prune --json must emit JSON, not prose");
    assert!(report["pruned"].is_array());
    assert!(report["retained"].is_array());
    assert!(report["retained_bytes"].is_u64());
}

#[test]
fn sparse_worktrees_check_out_only_the_requested_directories() {
    // A worktree costs metadata per path, so narrowing what is materialized is
    // the only lever that reduces it on macOS. The contract: the cone plus the
    // files cone mode always keeps (everything directly in the repository root
    // and in the cone's ancestors) on disk, everything else marked
    // skip-worktree, a clean status, and edits in the cone behaving normally.
    let root = std::env::temp_dir().join(format!("simgit-sparse-{}", uuid::Uuid::new_v4()));
    let repo = root.join("repo");
    for area in ["alpha", "beta"] {
        fs::create_dir_all(repo.join(area).join("sub")).unwrap();
    }
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test User"]);
    for area in ["alpha", "beta"] {
        for i in 0..8 {
            fs::write(
                repo.join(area).join("sub").join(format!("f{i}.txt")),
                format!("{area}-{i}\n"),
            )
            .unwrap();
        }
    }
    // Cone mode keeps root-level files and files in the cone's ancestors, so a
    // populated worktree that omits them is missing tracked content.
    fs::write(repo.join("README.md"), "readme\n").unwrap();
    fs::write(repo.join("alpha/note.txt"), "note\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "initial"]);

    // Cone behavior is backend-independent, and the plain-checkout path is
    // available everywhere, so it is the deterministic case. The CoW path is
    // additionally covered wherever the filesystem provides it; the
    // fuse-overlayfs backend rejects --sparse by design and is skipped.
    let create = |mode: Option<&str>, branch: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sg"));
        command
            .current_dir(&repo)
            .env_remove("SIMGIT_WORKTREE_ROOT");
        match mode {
            Some(mode) => command.env("SIMGIT_POPULATE", mode),
            None => command.env_remove("SIMGIT_POPULATE"),
        };
        command
            .args(["add", branch, "--sparse", "alpha/sub"])
            .output()
            .unwrap()
    };

    let output = create(Some("checkout"), "agent/cone");
    success(&output);
    let printed = String::from_utf8(output.stdout).unwrap();
    let worktree = std::path::PathBuf::from(printed.lines().last().unwrap().trim());

    assert!(worktree.join("alpha/sub/f0.txt").is_file());
    assert!(
        worktree.join("README.md").is_file() && worktree.join("alpha/note.txt").is_file(),
        "cone mode keeps root-level and ancestor files; they must be materialized"
    );
    assert!(
        !worktree.join("beta").exists(),
        "paths outside the cone must not be materialized"
    );

    let listed = Command::new("git")
        .current_dir(&worktree)
        .args(["ls-files", "-t"])
        .output()
        .unwrap();
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(
        listed.lines().any(|line| line.starts_with('S')),
        "paths outside the cone must be marked skip-worktree: {listed}"
    );

    let status = Command::new("git")
        .current_dir(&worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "");

    fs::write(worktree.join("alpha/sub/f0.txt"), "edited\n").unwrap();
    let status = Command::new("git")
        .current_dir(&worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(status.stdout).unwrap(),
        " M alpha/sub/f0.txt\n"
    );

    // Same invariants on the default backend, unless that is fuse-overlayfs.
    let native = create(None, "agent/cone-native");
    let complaint = String::from_utf8_lossy(&native.stderr).to_string();
    if native.status.success() {
        let printed = String::from_utf8(native.stdout).unwrap();
        let native_worktree = std::path::PathBuf::from(printed.lines().last().unwrap().trim());
        assert!(native_worktree.join("alpha/sub/f0.txt").is_file());
        assert!(
            native_worktree.join("README.md").is_file()
                && native_worktree.join("alpha/note.txt").is_file(),
            "cone mode keeps root-level and ancestor files; they must be cloned"
        );
        assert!(!native_worktree.join("beta").exists());
        let status = Command::new("git")
            .current_dir(&native_worktree)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(status.stdout).unwrap(), "");
    } else {
        assert!(
            complaint.contains("fuse-overlayfs"),
            "--sparse must work on every backend except overlay: {complaint}"
        );
    }

    let rejected = Command::new(env!("CARGO_BIN_EXE_sg"))
        .current_dir(&repo)
        .args(["add", "agent/escape", "--sparse", "../outside"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn overlapping_sparse_cones_allocate_clean_worktrees() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.repo.join("alpha/sub")).unwrap();
    fs::create_dir_all(fixture.repo.join("beta")).unwrap();
    fs::write(fixture.repo.join("alpha/sub/result.txt"), "in cone").unwrap();
    fs::write(fixture.repo.join("beta/other.txt"), "outside cone").unwrap();
    git(&fixture.repo, &["add", "-A"]);
    git(&fixture.repo, &["commit", "-qm", "add sparse directories"]);

    let doctor = fixture.run(&["doctor", "--json"]);
    success(&doctor);
    let capability: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let mode = if capability["cow_supported"] == true {
        "reflink"
    } else {
        "checkout"
    };
    for (branch, cones) in [
        ("agent/parent-first", ["alpha", "alpha/sub", "alpha"]),
        ("agent/child-first", ["alpha/sub", "alpha", "alpha/sub"]),
    ] {
        let target = fixture.root.join(branch.replace('/', "-"));
        let mut command = fixture.command();
        command
            .env("SIMGIT_POPULATE", mode)
            .args(["add", branch, "--path"])
            .arg(&target);
        for cone in cones {
            command.args(["--sparse", cone]);
        }
        let created = command.output().unwrap();
        success(&created);
        assert!(target.join("alpha/sub/result.txt").is_file());
        assert!(!target.join("beta").exists());
        let status = Command::new("git")
            .current_dir(&target)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        success(&status);
        assert!(status.stdout.is_empty(), "sparse worktree is dirty");
    }
}

#[test]
fn json_failures_leave_stdout_empty_and_one_diagnostic_line() {
    let fixture = Fixture::new();
    let absent = fixture.root.join("already-removed");
    let absent = absent.to_str().unwrap();
    for (args, expected) in [
        (
            vec!["--json", "remove", absent, "--delete-branch"],
            "pass the branch name",
        ),
        (
            vec!["remove", absent, "--delete-branch", "--json"],
            "pass the branch name",
        ),
        (
            vec!["--json", "add", "--unknown-option"],
            "unexpected argument",
        ),
        (
            vec!["add", "--json", "--unknown-option"],
            "unexpected argument",
        ),
        (
            vec!["--json", "run", "agent/test", "--", "true"],
            "--json is not supported",
        ),
    ] {
        let result = fixture.run(&args);
        assert!(!result.status.success(), "unexpected success for {args:?}");
        assert!(result.stdout.is_empty(), "failure wrote JSON for {args:?}");
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
        assert_eq!(stderr.lines().count(), 1, "{args:?}: {stderr}");
        assert!(stderr.ends_with('\n'), "{args:?}: {stderr}");
    }

    // A failed allocation inside the source must not emit its success-only
    // location warning before returning the JSON-mode error.
    let inside = fixture.repo.join("new-worktree");
    let refused = fixture
        .command()
        .args(["--json", "add", "agent/invalid", "--path"])
        .arg(&inside)
        .args(["--sparse", "../outside"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(refused.stdout.is_empty());
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("must be a path inside"), "{stderr}");

    let human = fixture.run(&["remove", absent, "--delete-branch"]);
    assert!(String::from_utf8_lossy(&human.stderr).lines().count() > 1);
}

/// `RUST_BACKTRACE` must not turn a failure into a stack dump: with `--json`
/// (and for `hook`) every failing command is exactly one stderr line.
#[test]
fn backtrace_variables_never_add_stderr_lines_to_machine_errors() {
    use std::io::Write;
    use std::process::Stdio;
    let fixture = Fixture::new();
    let outside = fixture.root.join("not a repository");
    fs::create_dir_all(&outside).unwrap();
    let absent = fixture.root.join("already-removed");
    let absent = absent.to_str().unwrap();
    let payload = format!(
        r#"{{"hook_event_name":"WorktreeCreate","name":"x","cwd":{}}}"#,
        serde_json::json!(outside)
    );
    for (dir, args, stdin) in [
        (&outside, vec!["--json", "list"], ""),
        (
            &fixture.repo,
            vec!["--json", "remove", absent, "--delete-branch"],
            "",
        ),
        (
            &outside,
            vec!["hook", "claude-code", "--requires", "0.0.0"],
            payload.as_str(),
        ),
        (
            &outside,
            vec!["--json", "hook", "claude-code", "--requires", "0.0.0"],
            "",
        ),
        (&fixture.repo, vec!["--json", "add", "--nonsense"], ""),
    ] {
        for bin in [env!("CARGO_BIN_EXE_sg"), env!("CARGO_BIN_EXE_simgit")] {
            let mut child = Command::new(bin)
                .current_dir(dir)
                .env("GIT_CEILING_DIRECTORIES", &fixture.root)
                .env("CLAUDE_PROJECT_DIR", dir)
                .env("RUST_BACKTRACE", "full")
                .env("RUST_LIB_BACKTRACE", "full")
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
            let output = child.wait_with_output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(if args.contains(&"--nonsense") { 2 } else { 1 }),
                "{args:?}: {stderr}"
            );
            assert!(output.stdout.is_empty(), "{args:?}: stdout not empty");
            assert_eq!(stderr.lines().count(), 1, "{args:?}: {stderr}");
        }
    }
}

/// A human-mode failure keeps its cause chain, and no backtrace joins it.
#[test]
fn human_errors_keep_their_chain_and_never_print_a_backtrace() {
    let fixture = Fixture::new();
    let absent = fixture.root.join("already-removed");
    let output = fixture
        .command()
        .env("RUST_BACKTRACE", "full")
        .env("RUST_LIB_BACKTRACE", "full")
        .args(["remove", absent.to_str().unwrap(), "--delete-branch"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("Error: "), "{stderr}");
    assert!(stderr.lines().count() > 1, "{stderr}");
    assert!(!stderr.contains("backtrace"), "{stderr}");
    assert!(!stderr.contains("   0:"), "{stderr}");
}

/// `doctor --json` is the contract an agent reads before it decides whether
/// this checkout is usable, so every field it keys off must be present and
/// describe the repository it was run from.
#[test]
fn doctor_json_reports_identity_repository_and_provider_readiness() {
    let fixture = Fixture::new();
    let output = fixture.run(&["doctor", "--json"]);
    success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(report["identity"], "simgit");
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    let top_level = fixture.repo.canonicalize().unwrap();
    assert_eq!(
        Path::new(report["repository"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        top_level
    );
    assert_eq!(
        report["repository_details"]["top_level"],
        report["repository"]
    );
    assert_eq!(report["repository_details"]["is_main_worktree"], true);
    assert!(
        ["cow-clone", "overlay", "git-checkout"]
            .contains(&report["populate_mode"].as_str().unwrap()),
        "unexpected populate mode: {}",
        report["populate_mode"]
    );
    assert_eq!(report["git_worktree_supported"], true);
    assert_eq!(
        report["stale_worktree_registrations"],
        serde_json::json!([])
    );

    // macOS `stat -f %T` prints a file-type suffix, so the report once said
    // the filesystem was "/". An allocator uses this to decide whether the
    // destination can share extents, so it must name a filesystem.
    let filesystem = report["filesystem"].as_str().unwrap();
    assert!(
        filesystem != "/" && filesystem != "unknown" && !filesystem.is_empty(),
        "filesystem not identified: {filesystem:?}"
    );

    // Worktrees must never land inside the repository: Git would treat them as
    // untracked content of the very tree they branch from.
    assert_eq!(report["default_worktree_root_inside_repository"], false);
    let root = Path::new(report["default_worktree_root"].as_str().unwrap());
    assert!(root.is_absolute());
    assert!(
        !root.starts_with(&top_level),
        "worktree root {root:?} is inside the repository"
    );
}

fn running_as_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { geteuid() == 0 }
}

/// A harness may hand each job an empty directory it owns inside a parent it
/// cannot write. No clone probe fits there, which means no CoW, not an error.
#[test]
fn add_into_an_empty_directory_under_an_unwritable_parent_checks_out() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let fixture = Fixture::new();
    let jobs = fixture.root.join("jobs");
    let job = jobs.join("job-1");
    fs::create_dir_all(&job).unwrap();
    fs::set_permissions(&jobs, fs::Permissions::from_mode(0o555)).unwrap();
    let output = fixture
        .command()
        .env_remove("SIMGIT_POPULATE")
        .args(["--json", "add", "job1", "--path"])
        .arg(&job)
        .output()
        .unwrap();
    fs::set_permissions(&jobs, fs::Permissions::from_mode(0o755)).unwrap();
    success(&output);
    let added: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // No CoW: a checkout, or overlay where fuse-overlayfs is installed.
    assert_ne!(added["mode"], "cow-clone", "{added}");
}

/// A probe that could not be written measured nothing, so `cow_supported` is
/// `null`, never `false`, wherever the root is unwritable: in a repository
/// (the default root is the probed directory), under an unwritable ancestor of
/// a root that does not exist yet, and outside any repository.
#[test]
fn doctor_reports_null_cow_support_where_no_probe_can_be_written() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        eprintln!("skipped: root ignores read-only directories");
        return;
    }
    let fixture = Fixture::new();
    let locked = fixture.root.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let missing_below_locked = locked.join("not-yet").join("deeper");

    let doctor = |worktree_root: &Path| {
        let output = fixture
            .command()
            .env("SIMGIT_WORKTREE_ROOT", worktree_root)
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        success(&output);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        (report, output)
    };
    let (unwritable, _) = doctor(&locked);
    let (missing, _) = doctor(&missing_below_locked);
    let outside = fixture
        .command()
        .current_dir(&locked)
        .env("GIT_CEILING_DIRECTORIES", &fixture.root)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    success(&outside);
    let outside: serde_json::Value = serde_json::from_slice(&outside.stdout).unwrap();
    let human = fixture
        .command()
        .env("SIMGIT_WORKTREE_ROOT", &locked)
        .arg("doctor")
        .output()
        .unwrap();
    // Restore before asserting, so a failure cannot strand an unremovable tree.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(unwritable["cow_supported"], serde_json::Value::Null);
    assert_eq!(missing["cow_supported"], serde_json::Value::Null);
    assert_eq!(outside["cow_supported"], serde_json::Value::Null);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("CoW: unknown"), "{text}");
    assert!(!locked.join("not-yet").exists());
}

/// A root that does not exist yet is probed at its nearest existing ancestor,
/// and the probe leaves no directory behind.
#[test]
fn doctor_probes_the_nearest_existing_ancestor_of_a_missing_root() {
    let fixture = Fixture::new();
    let missing = fixture.root.join("not-yet").join("deeper");
    let output = fixture
        .command()
        .env("SIMGIT_WORKTREE_ROOT", &missing)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["cow_supported"].is_boolean(), "{report}");
    assert!(!fixture.root.join("not-yet").exists());
}

/// `SIMGIT_POPULATE` naming a backend this machine lacks is a finding for
/// `doctor` to report, not a failure of `doctor`: `populate_mode` is `null` and
/// `populate_error` carries the reason. With no override both say the usual.
#[test]
fn doctor_reports_an_unavailable_forced_populate_mode() {
    let fixture = Fixture::new();
    let report = |forced: Option<&str>| {
        let mut command = fixture.command();
        match forced {
            Some(mode) => command.env("SIMGIT_POPULATE", mode),
            None => command.env_remove("SIMGIT_POPULATE"),
        };
        let output = command.args(["doctor", "--json"]).output().unwrap();
        success(&output);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        (report, output)
    };

    let (unset, _) = report(None);
    assert_eq!(unset["populate_error"], serde_json::Value::Null);
    assert!(unset["populate_mode"].is_string(), "{unset}");

    let (bogus, output) = report(Some("no-such-backend"));
    assert_eq!(bogus["populate_mode"], serde_json::Value::Null);
    assert!(
        bogus["populate_error"]
            .as_str()
            .is_some_and(|reason| reason.contains("SIMGIT_POPULATE=no-such-backend")),
        "{bogus}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");

    // `overlay` is unavailable unless fuse-overlayfs is installed.
    let (overlay, _) = report(Some("overlay"));
    match overlay["populate_mode"].as_str() {
        Some(mode) => assert_eq!(mode, "overlay"),
        None => assert!(
            overlay["populate_error"]
                .as_str()
                .is_some_and(|reason| reason.contains("SIMGIT_POPULATE=overlay")),
            "{overlay}"
        ),
    }

    let human = fixture
        .command()
        .env("SIMGIT_POPULATE", "no-such-backend")
        .arg("doctor")
        .output()
        .unwrap();
    success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("populate mode: unavailable") && text.contains("no-such-backend"),
        "{text}"
    );
}

/// An agent that passes no branch must still get a usable, uniquely named
/// workspace, and one cleanup token it can hand back later.
#[test]
fn add_without_a_branch_generates_an_agent_branch_and_one_cleanup_token() {
    let fixture = Fixture::new();
    let output = fixture.run(&["add", "--json", "--ephemeral"]);
    success(&output);
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(created["path"], created["worktree"]);
    assert_eq!(created["path"], created["cleanup_token"]);
    assert_eq!(created["ephemeral"], true);
    let path = Path::new(created["path"].as_str().unwrap());
    assert!(path.is_absolute());
    assert!(path.is_dir(), "worktree {path:?} was not created");
    assert_eq!(
        fs::read_to_string(path.join("README.md")).unwrap(),
        "original\n"
    );

    let branch = created["branch"].as_str().expect("generated branch name");
    let uuid = branch
        .strip_prefix("agent/")
        .unwrap_or_else(|| panic!("branch {branch} is not an agent branch"));
    assert!(
        uuid.parse::<uuid::Uuid>().is_ok(),
        "branch suffix {uuid} is not a uuid"
    );
    let reference = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .status()
        .unwrap();
    assert!(reference.success(), "no ref created for {branch}");
}

/// `--detach` is the throwaway-inspection mode: it must leave the ref
/// namespace untouched and still be reapable as an ephemeral workspace.
#[test]
fn add_detach_creates_no_ref_and_is_reaped_by_gc() {
    let fixture = Fixture::new();
    fs::write(fixture.repo.join("README.md"), "second\n").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "second"]);
    let first = rev_parse(&fixture.repo, "HEAD~1");
    let refs_before = local_refs(&fixture.repo);

    let output = fixture
        .command()
        .args(["add", "--detach", "--base", &first])
        .args(["--ephemeral", "--json"])
        .output()
        .unwrap();
    success(&output);
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(created["branch"], serde_json::Value::Null);
    assert_eq!(created["base"], first);
    let path = std::path::PathBuf::from(created["path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(path.join("README.md")).unwrap(),
        "original\n"
    );
    assert_eq!(rev_parse(&path, "HEAD"), first);
    let attached = Command::new("git")
        .current_dir(&path)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .unwrap();
    assert!(!attached.status.success(), "HEAD is attached to a branch");
    assert_eq!(local_refs(&fixture.repo), refs_before);

    success(&fixture.run(&["gc", "--older-than", "0s"]));
    assert!(
        !path.exists(),
        "ephemeral detached worktree {path:?} survived gc"
    );
    assert_eq!(local_refs(&fixture.repo), refs_before);
}

/// `sg` is only an alias; the canonical name must identify itself as simgit
/// and behave identically.
#[test]
fn canonical_binary_identifies_as_simgit_and_matches_the_alias() {
    let fixture = Fixture::new();
    let version = Command::new(env!("CARGO_BIN_EXE_simgit"))
        .arg("--version")
        .output()
        .unwrap();
    success(&version);
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        format!("simgit {}", env!("CARGO_PKG_VERSION"))
    );

    fixture.create(&[]);
    let alias = fixture.run(&["list", "--json"]);
    success(&alias);
    let canonical = Command::new(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .env("SIMGIT_POPULATE", "checkout")
        .env_remove("SIMGIT_WORKTREE_ROOT")
        .args(["list", "--json"])
        .output()
        .unwrap();
    success(&canonical);
    assert_eq!(canonical.stdout, alias.stdout);
}

/// An agent runs `doctor` to find out whether the directory it woke up in is
/// usable — which includes directories that are not repositories at all. That
/// answer must arrive as a successful report: identity, version and the
/// filesystem facts present, everything a repository would supply null.
#[test]
fn doctor_outside_a_repository_reports_null_repository_fields() {
    let root = std::env::temp_dir().join(format!("simgit-no-repo-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let outside = root.join("plain directory");
    fs::create_dir_all(&outside).unwrap();

    let doctor = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_sg"))
            .current_dir(&outside)
            // Keep Git from discovering a repository above the temporary root.
            .env("GIT_CEILING_DIRECTORIES", &root)
            .args(args)
            .output()
            .unwrap()
    };

    let output = doctor(&["doctor", "--json"]);
    success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["identity"], "simgit");
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        report["cow_supported"].is_boolean(),
        "cow support not probed: {}",
        report["cow_supported"]
    );
    let filesystem = report["filesystem"].as_str().unwrap();
    assert!(
        filesystem != "/" && filesystem != "unknown" && !filesystem.is_empty(),
        "filesystem not identified: {filesystem:?}"
    );
    for field in [
        "repository",
        "repository_details",
        "populate_mode",
        "populate_error",
        "default_worktree_root",
        "default_worktree_root_inside_repository",
        "git_worktree_supported",
        "baseline_cache",
    ] {
        assert_eq!(
            report[field],
            serde_json::Value::Null,
            "{field} must be null without a repository"
        );
    }
    assert_eq!(
        report["stale_worktree_registrations"],
        serde_json::json!([])
    );

    let human = doctor(&["doctor"]);
    success(&human);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("repository: none"),
        "human report does not say the repository is absent: {text}"
    );

    // Probing the filesystem must leave the user's directory as it found it.
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_dir_all(root).unwrap();
}

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

/// `remove` shares the refusal with `gc` and the hook; a lock Git set must
/// not read as "a command may be running", and its reason is what matters.
#[test]
fn remove_names_the_reason_of_a_lock_git_set() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let path = fixture.worktree.display().to_string();
    git(
        &fixture.worktree,
        &["worktree", "lock", "--reason", "usb", &path],
    );

    let refused = fixture.run(&["remove", &path]);
    let message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(!refused.status.success());
    assert!(message.contains("usb"), "Git's reason missing: {message}");
    assert!(!message.contains("may be running"), "{message}");
    assert!(fixture.worktree.exists(), "locked worktree was removed");
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

/// Cleanup must survive being repeated: an agent that crashed mid-teardown, or
/// handed the same token back twice, gets a success saying the workspace is
/// already gone. Removals that can still lose work must still fail.
#[test]
fn remove_is_idempotent_but_still_refuses_a_dirty_workspace() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    fs::write(fixture.worktree.join("scratch.txt"), "work in progress").unwrap();

    let dirty = fixture.run(&["remove", "chat/test", "--json"]);
    assert!(!dirty.status.success(), "removed a dirty workspace");
    assert!(fixture.worktree.is_dir());

    let removed = fixture.run(&["remove", "chat/test", "--discard-dirty", "--json"]);
    success(&removed);
    let report: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(report["already_absent"], false);
    assert!(!fixture.worktree.exists());

    let again = fixture.run(&["remove", "chat/test", "--json"]);
    success(&again);
    let report: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(report["removed"], "chat/test");
    assert_eq!(report["already_absent"], true);
    assert_eq!(report["committed"], false);
    assert_eq!(report["branch_deleted"], false);

    let path = fixture.worktree.display().to_string();
    let by_path = fixture.run(&["remove", &path, "--json"]);
    success(&by_path);
    let report: serde_json::Value = serde_json::from_slice(&by_path.stdout).unwrap();
    assert_eq!(report["removed"], path);
    assert_eq!(report["already_absent"], true);

    // The branch outlived its worktree, and --delete-branch still cleans it up.
    let branch = fixture.run(&["remove", "chat/test", "--delete-branch", "--json"]);
    success(&branch);
    let report: serde_json::Value = serde_json::from_slice(&branch.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert_eq!(report["branch_deleted"], true);
    assert!(!local_refs(&fixture.repo).contains("refs/heads/chat/test"));
}

/// The Usage block tells you to `cd` into the workspace, so the commands that
/// remove it run from inside it. Deleting a worktree deletes that directory,
/// and every later `git` that inherited it died with "Unable to read current
/// working directory": `remove --delete-branch` left the branch behind and
/// reported failure, and `gc` stopped after its first deletion.
#[test]
fn remove_and_gc_work_from_inside_the_worktree_they_delete() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let removed = fixture
        .command()
        .current_dir(&fixture.worktree)
        .args(["remove", "chat/test", "--delete-branch"])
        .output()
        .unwrap();
    success(&removed);
    assert!(!fixture.worktree.exists());
    assert!(
        !local_refs(&fixture.repo).contains("refs/heads/chat/test"),
        "the worktree went but its branch survived"
    );

    fixture.create(&["--ephemeral"]);
    let second = fixture.root.join("second workspace");
    success(
        &fixture
            .command()
            .args(["add", "agent/second", "--ephemeral", "--path"])
            .arg(&second)
            .output()
            .unwrap(),
    );
    let collected = fixture
        .command()
        .current_dir(&fixture.worktree)
        .args(["gc", "--older-than", "0s"])
        .output()
        .unwrap();
    success(&collected);
    let report = String::from_utf8_lossy(&collected.stdout);
    assert!(
        !fixture.worktree.exists() && !second.exists(),
        "gc stopped after deleting the directory it was run from: {report}"
    );
    assert!(
        report.contains("reaped 2 worktree(s)"),
        "gc reported nothing about what it did: {report}"
    );
}

/// Two agents allocating the same `--path` both used to succeed: Git kept two
/// registrations for one directory, the winner's JSON named the loser's
/// branch, and the cleanup token then failed permanently with "does not point
/// back to". Exactly one allocation may proceed, and it must be removable.
#[test]
fn concurrent_allocations_to_one_path_leave_one_removable_worktree() {
    let fixture = Fixture::new();
    let target = fixture.root.join("contested");
    let resolved = fixture.root.canonicalize().unwrap().join("contested");
    let racers: Vec<_> = (0..4)
        .map(|_| {
            fixture
                .command()
                .args(["add", "--json", "--ephemeral", "--path"])
                .arg(&target)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outcomes: Vec<_> = racers
        .into_iter()
        .map(|racer| racer.wait_with_output().unwrap())
        .collect();

    let winners: Vec<_> = outcomes
        .iter()
        .filter(|outcome| outcome.status.success())
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "{} allocations claimed one path",
        winners.len()
    );
    for loser in outcomes.iter().filter(|outcome| !outcome.status.success()) {
        let complaint = String::from_utf8_lossy(&loser.stderr);
        assert!(
            complaint.contains(resolved.to_str().unwrap()),
            "a refused allocation must name the path it wanted: {complaint}"
        );
    }

    let created: serde_json::Value = serde_json::from_slice(&winners[0].stdout).unwrap();
    let registrations = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&registrations.stdout)
            .lines()
            .filter(|line| *line == format!("worktree {}", resolved.display()))
            .count(),
        1,
        "one directory holds more than one registration"
    );
    let checked_out = Command::new("git")
        .current_dir(&resolved)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&checked_out.stdout).trim(),
        created["branch"].as_str().unwrap(),
        "the reported branch is not the one checked out there"
    );
    assert_eq!(
        local_refs(&fixture.repo)
            .matches("refs/heads/agent/")
            .count(),
        1,
        "a refused allocation left its branch behind"
    );

    let token = created["cleanup_token"].as_str().unwrap();
    success(&fixture.run(&["remove", token, "--delete-branch", "--json"]));
    assert!(!resolved.exists());
    assert!(!local_refs(&fixture.repo).contains("refs/heads/agent/"));
}

/// Two `add --attach` runs for one branch at different paths both used to
/// succeed, because the "already checked out" check and the registration were
/// separate steps: two worktrees then shared a branch, and a commit in one
/// silently rewrote what the other saw. Exactly one may get the branch.
#[test]
fn concurrent_attaches_of_one_branch_leave_one_worktree_on_it() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["branch", "chat/contested"]);
    let racers: Vec<_> = (0..4)
        .map(|index| {
            fixture
                .command()
                .args(["--json", "add", "--attach", "chat/contested", "--path"])
                .arg(fixture.root.join(format!("racer-{index}")))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outcomes: Vec<_> = racers
        .into_iter()
        .map(|racer| racer.wait_with_output().unwrap())
        .collect();

    let winners = outcomes
        .iter()
        .filter(|outcome| outcome.status.success())
        .count();
    assert_eq!(winners, 1, "{winners} allocations got one branch");
    let registrations = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&registrations.stdout)
            .lines()
            .filter(|line| *line == "branch refs/heads/chat/contested")
            .count(),
        1,
        "one branch is checked out in more than one worktree"
    );
}

/// While Git registers a worktree its `.git/worktrees/<name>/` is briefly
/// half-written, and any other `git worktree add` or `list` that reads it then
/// dies with "failed to read .../commondir". Agents allocate in parallel, so
/// allocations for different branches, and listings beside them, must all
/// succeed. The wrapper stretches Git's window from microseconds to 200ms so
/// the race fails every time instead of once in hundreds of adds.
#[test]
fn concurrent_allocations_of_different_branches_all_succeed() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$2\" = worktree ] && [ \"$3\" = add ]; then\n\
           pending=\"${1#--git-dir=}/worktrees/pending-$$\"\n\
           mkdir -p \"$pending\" || exit\n\
           echo initializing > \"$pending/locked\"\n\
           echo \"$pending/.git\" > \"$pending/gitdir\"\n\
           : > \"$pending/commondir\"\n\
           sleep 0.2\n\
           rm -rf \"$pending\"\n\
         fi\n\
         exec \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(wrapper_dir)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let command = || {
        let mut command = fixture.command();
        command
            .env("PATH", &path)
            .env("SIMGIT_TEST_REAL_GIT", &real_git)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    };

    // Staggered so each racer's own Git runs while a later racer's
    // registration is half-written.
    let racers: Vec<_> = (0..8)
        .map(|index| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            command()
                .args(["add", &format!("agent/racer-{index}"), "--path"])
                .arg(fixture.root.join(format!("racer-{index}")))
                .spawn()
                .unwrap()
        })
        .collect();
    let done = std::sync::atomic::AtomicBool::new(false);
    let listings = std::thread::scope(|scope| {
        let lister = scope.spawn(|| {
            let mut listings = Vec::new();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                listings.push(command().args(["list", "--json"]).output().unwrap());
            }
            listings
        });
        let outcomes: Vec<_> = racers
            .into_iter()
            .map(|racer| racer.wait_with_output().unwrap())
            .collect();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        for outcome in &outcomes {
            success(outcome);
        }
        lister.join().unwrap()
    });
    for listing in &listings {
        success(listing);
    }

    let registrations = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let registrations = String::from_utf8_lossy(&registrations.stdout);
    for index in 0..8 {
        assert_eq!(
            registrations
                .lines()
                .filter(|line| *line == format!("branch refs/heads/agent/racer-{index}"))
                .count(),
            1,
            "agent/racer-{index} is not registered once: {registrations}"
        );
    }
}

/// Put a `git` wrapper running `script` ahead of the real Git on `PATH`. The
/// script reaches the real Git as `$SIMGIT_TEST_REAL_GIT`; returns the
/// environment a command needs to run under the wrapper.
fn git_wrapper(fixture: &Fixture, script: &str) -> [(&'static str, std::ffi::OsString); 2] {
    use std::os::unix::fs::PermissionsExt;

    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\n{script}exec \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n"),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(wrapper_dir)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    [("PATH", path), ("SIMGIT_TEST_REAL_GIT", real_git.into())]
}

/// `git worktree remove` reads every registration, so it dies on one another
/// allocation is half-way through writing. Removal holds no registry lock, so
/// it must retry; the wrapper fails each first `worktree remove` that way.
#[test]
fn removal_is_retried_after_a_half_written_registration() {
    let fixture = Fixture::new();
    let failed = fixture.root.join("remove-failed");
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$2\" = worktree ] && [ \"$3\" = remove ] && [ ! -e '{failed}' ]; then\n\
               : > '{failed}'\n\
               echo \"fatal: failed to read ${{1#--git-dir=}}/worktrees/other/commondir: Undefined error: 0\" >&2\n\
               exit 128\n\
             fi\n",
            failed = failed.display(),
        ),
    );

    for discard in [&[][..], &["--discard-dirty"][..]] {
        let _ = fs::remove_file(&failed);
        fixture.create(&[]);
        let removed = fixture
            .command()
            .envs(env.clone())
            .args(["remove", "chat/test"])
            .args(discard)
            .output()
            .unwrap();
        success(&removed);
        assert!(failed.exists(), "the wrapper never failed a removal");
        assert!(!fixture.worktree.exists(), "{discard:?}");
    }
}

/// Removing a large worktree spends seconds deleting files, and allocations
/// for other branches must not wait for it. The wrapper holds `worktree
/// remove` until the test releases it, so the create finishing first is an
/// ordering check, not a timing one.
#[test]
fn creates_do_not_wait_for_a_removal_in_progress() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let started = fixture.root.join("remove-started");
    let release = fixture.root.join("remove-release");
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$2\" = worktree ] && [ \"$3\" = remove ]; then\n\
               : > '{started}'\n\
               tries=0\n\
               while [ ! -e '{release}' ] && [ $tries -lt 200 ]; do sleep 0.05; tries=$((tries + 1)); done\n\
             fi\n",
            started = started.display(),
            release = release.display(),
        ),
    );

    let mut removal = fixture
        .command()
        .envs(env.clone())
        .args(["remove", "chat/test"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    while !started.exists() {
        assert!(
            removal.try_wait().unwrap().is_none(),
            "removal exited early"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let created = fixture
        .command()
        .envs(env)
        .args(["add", "agent/other", "--path"])
        .arg(fixture.root.join("other"))
        .output()
        .unwrap();
    let still_removing = removal.try_wait().unwrap().is_none();
    fs::write(&release, "").unwrap();
    success(&created);
    success(&removal.wait_with_output().unwrap());
    assert!(
        still_removing,
        "the create waited for the removal to finish"
    );
}

/// A checkout-mode allocation runs the repository's `post-checkout` hook once,
/// with the arguments `git worktree add` passes: a null old HEAD, the new HEAD
/// and the branch-checkout flag.
#[test]
fn checkout_allocation_runs_the_post_checkout_hook() {
    let fixture = Fixture::new();
    let hook = fixture.repo.join(".git/hooks/post-checkout");
    assert_checkout_runs_post_checkout_hook(&fixture, &mut fixture.command(), &hook);
}

/// Git also runs hooks defined in configuration, with no file in `hooks/`.
#[test]
fn checkout_allocation_runs_a_config_defined_post_checkout_hook() {
    let fixture = Fixture::new();
    let hook = fixture.root.join("marker-hook");
    git(
        &fixture.repo,
        &["config", "hook.marker.command", hook.to_str().unwrap()],
    );
    git(
        &fixture.repo,
        &["config", "hook.marker.event", "post-checkout"],
    );
    let listed = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["hook", "list", "post-checkout"])
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&listed.stdout).contains("marker") {
        eprintln!("skipped: this Git has no config-defined hooks");
        return;
    }
    assert_checkout_runs_post_checkout_hook(&fixture, &mut fixture.command(), &hook);
}

/// Git before 2.36 has no `git hook`. Ubuntu 22.04 ships 2.34, ext4 sends
/// allocations to checkout mode, and git-lfs installs a `post-checkout` hook,
/// so the hook must still run there. The wrapper answers like Git 2.34.
#[test]
fn checkout_allocation_runs_the_post_checkout_hook_without_git_hook() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        "#!/bin/sh\n\
         case \"$1\" in\n\
           version) echo 'git version 2.34.1'; exit 0 ;;\n\
           hook) echo \"git: 'hook' is not a git command. See 'git --help'.\" >&2; exit 1 ;;\n\
         esac\n\
         exec \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(wrapper_dir)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = fixture.command();
    command
        .env("PATH", path)
        .env("SIMGIT_TEST_REAL_GIT", real_git);
    let hook = fixture.repo.join(".git/hooks/post-checkout");
    assert_checkout_runs_post_checkout_hook(&fixture, &mut command, &hook);
}

/// Install a hook script at `hook` that logs its arguments, allocate in
/// checkout mode with `command`, and check the hook ran once as Git runs it.
fn assert_checkout_runs_post_checkout_hook(fixture: &Fixture, command: &mut Command, hook: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let marker = fixture.root.join("post-checkout.log");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        hook,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let target = fixture.root.join("hooked");

    success(
        &command
            .args(["add", "agent/hooked", "--path"])
            .arg(&target)
            .output()
            .unwrap(),
    );

    let head = rev_parse(&target, "HEAD");
    assert_eq!(
        fs::read_to_string(&marker).expect("the hook did not run"),
        format!("{} {head} 1\n", "0".repeat(head.len()))
    );
}

/// Harnesses pre-create one directory per job and pass it as `--path`. An
/// empty one holds nothing to lose; anything else is someone's data.
#[test]
fn add_accepts_an_empty_destination_but_refuses_a_populated_one() {
    let fixture = Fixture::new();
    let prepared = fixture.root.join("prepared");
    fs::create_dir_all(&prepared).unwrap();
    success(
        &fixture
            .command()
            .args(["add", "agent/prepared", "--path"])
            .arg(&prepared)
            .output()
            .unwrap(),
    );
    assert!(prepared.join("README.md").is_file());

    // The whole-tree clone needs a destination that does not exist, so that
    // path has to give up the pre-created directory rather than refuse it.
    let cloned = fixture.root.join("prepared clone");
    fs::create_dir_all(&cloned).unwrap();
    success(
        &Command::new(env!("CARGO_BIN_EXE_sg"))
            .current_dir(&fixture.repo)
            .env_remove("SIMGIT_POPULATE")
            .env_remove("SIMGIT_WORKTREE_ROOT")
            .args(["add", "agent/prepared-clone", "--path"])
            .arg(&cloned)
            .output()
            .unwrap(),
    );
    assert!(cloned.join("README.md").is_file());

    let occupied = fixture.root.join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("precious.txt"), "keep").unwrap();
    let refused = fixture
        .command()
        .args(["add", "agent/occupied", "--path"])
        .arg(&occupied)
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let complaint = String::from_utf8_lossy(&refused.stderr);
    assert!(
        complaint.contains("worktree path already exists"),
        "{complaint}"
    );
    assert_eq!(
        fs::read_to_string(occupied.join("precious.txt")).unwrap(),
        "keep"
    );
    assert!(
        !local_refs(&fixture.repo).contains("agent/occupied"),
        "a refused allocation left its branch behind"
    );
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

/// `--delete-branch` needs a branch, and a path that is already gone names
/// none: the registration that mapped one to the other went with it.
/// Reporting `branch_deleted: false` there is a guess dressed as a fact.
#[test]
fn remove_delete_branch_refuses_an_already_removed_path() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let path = fixture.worktree.display().to_string();
    success(&fixture.run(&["remove", &path, "--json"]));

    let refused = fixture.run(&["remove", &path, "--delete-branch", "--json"]);
    assert!(!refused.status.success());
    let complaint = String::from_utf8_lossy(&refused.stderr);
    assert!(complaint.contains("pass the branch name"), "{complaint}");
    assert!(local_refs(&fixture.repo).contains("refs/heads/chat/test"));

    // The branch form still deletes the leftover ref.
    let deleted = fixture.run(&["remove", "chat/test", "--delete-branch", "--json"]);
    success(&deleted);
    let report: serde_json::Value = serde_json::from_slice(&deleted.stdout).unwrap();
    assert_eq!(report["already_absent"], true);
    assert_eq!(report["branch_deleted"], true);
    assert!(!local_refs(&fixture.repo).contains("refs/heads/chat/test"));
}

/// A simgit killed mid-lock or mid-probe leaves its temporary files behind.
/// `prune` removes those whose process is gone and keeps a live one's, and a
/// probe record never makes it delete anything but a probe.
#[test]
fn prune_removes_the_temporary_files_of_dead_processes() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let admin = Path::new(&registration(&fixture, &fixture.worktree)).to_path_buf();
    let git_dir = fixture.repo.join(".git");
    let probes = git_dir.join("simgit/clone-probes");
    fs::create_dir_all(&probes).unwrap();
    let dead = dead_pid();
    let live = std::process::id();
    let token = |pid: u32| format!("{pid}.{}", uuid::Uuid::new_v4());
    let pending = |dir: &Path, lock: &str, pid: u32| {
        let path = dir.join(format!("{lock}.{}.pending", token(pid)));
        fs::write(&path, "simgit: workspace in use\n").unwrap();
        path
    };
    let probe = |pid: u32, destination: Option<&Path>| {
        let token = token(pid);
        let target = fixture.root.join(format!(".simgit-clone-probe-{token}"));
        let destination = destination.unwrap_or(&target);
        fs::write(&target, "probe").unwrap();
        let source = probes.join(format!("{token}.source"));
        fs::write(&source, destination.as_os_str().as_encoded_bytes()).unwrap();
        (source, target)
    };

    let dead_files = [
        pending(&admin, "locked", dead),
        pending(&git_dir, "simgit-run.lock", dead),
    ];
    let live_files = [
        pending(&admin, "locked", live),
        pending(&git_dir, "simgit-run.lock", live),
    ];
    let (dead_source, dead_probe) = probe(dead, None);
    let (live_source, live_probe) = probe(live, None);
    let readme = fixture.repo.join("README.md");
    let (misdirected, _) = probe(dead, Some(&readme));
    // Claim files are never deleted: removing one would break its exclusion.
    let claims = || {
        ["branches", "paths"].map(|kind| {
            fs::read_dir(git_dir.join("simgit/claims").join(kind))
                .unwrap()
                .count()
        })
    };
    let claimed = claims();
    assert!(claimed.iter().all(|count| *count > 0), "{claimed:?}");

    success(&fixture.run(&["prune"]));
    assert_eq!(claims(), claimed, "prune deleted claim files");
    for path in dead_files.iter().chain([&dead_source, &dead_probe]) {
        assert!(!path.exists(), "kept {}", path.display());
    }
    for path in live_files.iter().chain([&live_source, &live_probe]) {
        assert!(path.exists(), "removed {}", path.display());
    }
    assert!(readme.is_file(), "a probe record made prune delete a file");
    assert!(!misdirected.exists(), "kept {}", misdirected.display());
}

/// A stale registration prune keeps is named in `kept_registrations` under
/// `--json`, never on stderr, so a failing `prune --json` still writes exactly
/// one diagnostic line. Human output names it on stderr.
#[test]
fn prune_json_reports_kept_registrations_in_its_output() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let kept = resolved(&fixture.worktree).display().to_string();
    let admin = std::path::PathBuf::from(registration(&fixture, &fixture.worktree));
    fs::remove_dir_all(&fixture.worktree).unwrap();
    fs::write(admin.join("HEAD"), "not a head\n").unwrap();
    // An allocation's hold on the cache makes `prune --all` fail.
    let cache = fixture.repo.join(".git/simgit");
    fs::create_dir_all(cache.join("baselines/held")).unwrap();
    let lock = fs::File::create(cache.join("baselines.lock")).unwrap();
    lock.lock_shared().unwrap();

    let failed = fixture.run(&["--json", "prune", "--all"]);
    assert!(
        !failed.status.success(),
        "prune --all ignored the held cache"
    );
    assert!(failed.stdout.is_empty());
    let stderr = String::from_utf8(failed.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains("retry `simgit prune --all`"), "{stderr}");

    drop(lock);
    let pruned = fixture.run(&["--json", "prune"]);
    success(&pruned);
    assert!(pruned.stderr.is_empty(), "{:?}", pruned.stderr);
    let report: serde_json::Value = serde_json::from_slice(&pruned.stdout).unwrap();
    assert_eq!(
        report["kept_registrations"],
        serde_json::json!([{ "worktree": kept, "reason": "status-failed" }])
    );
    let human = fixture.run(&["prune"]);
    success(&human);
    let stderr = String::from_utf8_lossy(&human.stderr);
    assert!(
        stderr.contains(&format!("kept stale registration {kept}")),
        "{stderr}"
    );
    assert!(admin.join("HEAD").is_file());
}

/// Pruning a stale registration mutates the Git registry, and it is the thing
/// `doctor` reports as stale — so reporting only "pruned 0 cached baseline(s)"
/// told the user nothing had happened.
#[test]
fn prune_reports_the_stale_registrations_it_cleared() {
    let fixture = Fixture::new();
    let resolved = |path: &Path| {
        path.parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(path.file_name().unwrap())
    };

    fixture.create(&[]);
    // What a user does by hand instead of `simgit remove`.
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let human = fixture.run(&["prune"]);
    success(&human);
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("pruned 1 stale registration(s)"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );

    fixture.create(&[]);
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let pruned = fixture.run(&["prune", "--json"]);
    success(&pruned);
    let report: serde_json::Value = serde_json::from_slice(&pruned.stdout).unwrap();
    let cleared = report["pruned_registrations"].as_array().unwrap();
    assert_eq!(cleared.len(), 1);
    assert_eq!(
        Path::new(cleared[0].as_str().unwrap()),
        resolved(&fixture.worktree)
    );

    let again = fixture.run(&["prune", "--json"]);
    success(&again);
    let report: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(report["pruned_registrations"], serde_json::json!([]));
}

fn rev_parse(dir: &Path, revision: &str) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", revision])
        .output()
        .unwrap();
    success(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn local_refs(repo: &Path) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["for-each-ref", "--format=%(refname)", "refs/"])
        .output()
        .unwrap();
    success(&output);
    String::from_utf8(output.stdout).unwrap()
}

fn add_detached(fixture: &Fixture) -> std::path::PathBuf {
    let output = fixture
        .command()
        .args(["add", "--detach", "--json"])
        .output()
        .unwrap();
    success(&output);
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    std::path::PathBuf::from(created["path"].as_str().unwrap())
}

fn commit_in(worktree: &Path, name: &str) -> String {
    fs::write(worktree.join(name), "result\n").unwrap();
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-qm", name]);
    rev_parse(worktree, "HEAD")
}

fn object_count(repo: &Path) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["count-objects", "-v"])
        .output()
        .unwrap();
    success(&output);
    String::from_utf8(output.stdout).unwrap()
}

fn assert_refused_to_orphan(fixture: &Fixture, args: &[&str], path: &Path, head: &str) {
    let output = fixture.run(args);
    assert!(!output.status.success(), "{args:?} succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("git -C {} branch <name>", path.display())),
        "{stderr}"
    );
    assert!(path.is_dir(), "worktree removed by {args:?}");
    assert_eq!(rev_parse(path, "HEAD"), head);
}

#[test]
fn remove_refuses_a_detached_worktree_with_unreached_commits() {
    let fixture = Fixture::new();
    let path = add_detached(&fixture);
    let head = commit_in(&path, "agent.txt");
    let target = path.to_str().unwrap();

    assert_refused_to_orphan(&fixture, &["remove", target], &path, &head);
    assert_refused_to_orphan(
        &fixture,
        &["remove", target, "--discard-dirty"],
        &path,
        &head,
    );

    // The advertised fix works.
    git(&path, &["branch", "kept"]);
    success(&fixture.run(&["remove", target]));
    assert!(!path.exists());
    assert_eq!(rev_parse(&fixture.repo, "kept"), head);
}

#[test]
fn remove_commit_refuses_a_detached_worktree() {
    let fixture = Fixture::new();
    let path = add_detached(&fixture);
    let head = commit_in(&path, "agent.txt");
    fs::write(path.join("more.txt"), "uncommitted\n").unwrap();
    let objects = object_count(&fixture.repo);

    assert_refused_to_orphan(
        &fixture,
        &["remove", path.to_str().unwrap(), "--commit"],
        &path,
        &head,
    );

    assert_eq!(object_count(&fixture.repo), objects, "a commit was made");
    assert!(path.join("more.txt").is_file());

    // At a commit main reaches, the commit --commit would make is what no ref
    // reaches: refused before committing, with neither refusal offering it.
    let path = add_detached(&fixture);
    let head = rev_parse(&path, "HEAD");
    fs::write(path.join("more.txt"), "uncommitted\n").unwrap();
    let objects = object_count(&fixture.repo);
    let target = path.to_str().unwrap();
    let switch = format!("git -C {target} switch -c <name>");
    for args in [vec!["remove", target, "--commit"], vec!["remove", target]] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&switch), "{stderr}");
        assert!(!stderr.contains("committed as"), "{stderr}");
        assert!(!stderr.contains("keep the work with --commit"), "{stderr}");
        assert_eq!(object_count(&fixture.repo), objects, "a commit was made");
        assert_eq!(rev_parse(&path, "HEAD"), head);
        assert!(path.join("more.txt").is_file());
    }

    // The advertised fix works.
    git(&path, &["switch", "-qc", "kept"]);
    success(&fixture.run(&["remove", target, "--commit"]));
    assert!(!path.exists());
    assert_ne!(rev_parse(&fixture.repo, "kept"), head);
}

#[test]
fn remove_keeps_uncommitted_files_and_unreached_commits_on_every_path() {
    let fixture = Fixture::new();
    let path = add_detached(&fixture);
    let head = commit_in(&path, "agent.txt");
    fs::write(path.join("scratch.txt"), "uncommitted\n").unwrap();
    let target = path.to_str().unwrap();

    for args in [
        vec!["remove", target],
        vec!["remove", target, "--discard-dirty"],
        vec!["remove", target, "--commit"],
    ] {
        assert_refused_to_orphan(&fixture, &args, &path, &head);
        assert_eq!(
            fs::read_to_string(path.join("scratch.txt")).unwrap(),
            "uncommitted\n"
        );
    }
    success(&fixture.run(&[
        "gc",
        "--older-than",
        "0s",
        "--include-persistent",
        "--discard-dirty",
    ]));
    assert!(path.is_dir());
    assert_eq!(rev_parse(&path, "HEAD"), head);
}

/// A fixture whose repository uses the reftable ref format, or `None` on a
/// Git older than 2.45, which has none. Under reftable every registration's
/// `HEAD` file is a stub naming `refs/heads/.invalid`, detached or not.
fn reftable_fixture() -> Option<Fixture> {
    let version = Command::new("git").arg("version").output().unwrap();
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    let mut parts = version
        .trim()
        .trim_start_matches("git version ")
        .split(|c: char| !c.is_ascii_digit())
        .map(|part| part.parse::<u32>().unwrap_or(0));
    if (parts.next()?, parts.next()?) < (2, 45) {
        return None;
    }
    let fixture = Fixture::new();
    fs::remove_dir_all(fixture.repo.join(".git")).unwrap();
    git(&fixture.repo, &["init", "-q", "--ref-format=reftable"]);
    git(&fixture.repo, &["config", "user.email", "test@example.com"]);
    git(&fixture.repo, &["config", "user.name", "Test User"]);
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "initial"]);
    Some(fixture)
}

fn commit_exists(repo: &Path, commit: &str) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .status()
        .unwrap()
        .success()
}

#[test]
fn reftable_remove_keeps_unreached_detached_commits() {
    let Some(fixture) = reftable_fixture() else {
        return;
    };
    let path = add_detached(&fixture);
    let head = commit_in(&path, "agent.txt");
    let target = path.to_str().unwrap();
    assert_refused_to_orphan(&fixture, &["remove", target], &path, &head);
    assert_refused_to_orphan(&fixture, &["remove", target, "--commit"], &path, &head);

    // Deleted by hand, the registration holds the only reference.
    fs::remove_dir_all(&path).unwrap();
    let output = fixture.run(&["remove", target]);
    assert!(
        !output.status.success(),
        "removed an unreached registration"
    );
    assert_eq!(registered_head(&fixture, &path), Some(head.clone()));
    assert!(commit_exists(&fixture.repo, &head));
}

#[test]
fn reftable_remove_commit_refuses_a_dirty_detached_worktree() {
    let Some(fixture) = reftable_fixture() else {
        return;
    };
    let path = add_detached(&fixture);
    let head = rev_parse(&path, "HEAD");
    fs::write(path.join("more.txt"), "uncommitted\n").unwrap();
    let objects = object_count(&fixture.repo);

    let output = fixture.run(&["remove", path.to_str().unwrap(), "--commit"]);
    assert!(!output.status.success(), "committed and removed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("switch -c <name>"), "{stderr}");
    assert_eq!(object_count(&fixture.repo), objects, "a commit was made");
    assert_eq!(rev_parse(&path, "HEAD"), head);
    assert!(path.join("more.txt").is_file());
}

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

#[test]
fn remove_removes_a_detached_worktree_at_a_reached_commit() {
    let fixture = Fixture::new();
    let path = add_detached(&fixture);

    success(&fixture.run(&["remove", path.to_str().unwrap()]));

    assert!(!path.exists());
}

#[test]
fn remove_and_gc_keep_a_detached_worktree_whose_gitfile_is_gone() {
    let fixture = Fixture::new();
    let path = add_detached(&fixture);
    let head = commit_in(&path, "agent.txt");
    let main_head = rev_parse(&fixture.repo, "HEAD");
    // Without its gitfile git walks up and would measure the main checkout.
    fs::remove_file(path.join(".git")).unwrap();

    let output = fixture.run(&["--json", "remove", path.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    // `git -C <path> branch` would walk up and branch the main checkout's HEAD.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("branch <name> {head}")),
        "{stderr}"
    );
    assert!(path.is_dir());

    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--include-persistent"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    assert!(path.is_dir());
    assert_eq!(rev_parse(&fixture.repo, "HEAD"), main_head);
    // The registration holds the only reference to the commit, so neither gc's
    // closing prune nor `prune` may drop it while the directory is there.
    assert_eq!(registered_head(&fixture, &path), Some(head.clone()));
    success(&fixture.run(&["prune"]));
    assert_eq!(registered_head(&fixture, &path), Some(head));
}

/// Without its gitfile, git run in a worktree nested in another repository
/// answers for that repository, so the lock that keeps the registration from
/// being pruned must still be found through this repository's registry.
#[test]
fn gc_keeps_a_gitfile_less_worktree_inside_another_repository() {
    let fixture = Fixture::new();
    let other = fixture.root.join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-q"]);
    let path = other.join("wt");
    let output = fixture
        .command()
        .args(["add", "--detach", "--path"])
        .arg(&path)
        .output()
        .unwrap();
    success(&output);
    let head = commit_in(&path, "agent.txt");
    fs::remove_file(path.join(".git")).unwrap();

    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--include-persistent"]);
    success(&gc);
    assert_eq!(registered_head(&fixture, &path), Some(head));
    assert!(!other.join(".git/locked").exists());
}

/// The HEAD Git's registry records for `worktree`, or `None` when it is not
/// registered. Works after the directory is gone.
fn registered_head(fixture: &Fixture, worktree: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .unwrap();
    success(&output);
    let wanted = format!("worktree {}", resolved(worktree).display());
    String::from_utf8(output.stdout)
        .unwrap()
        .split("\0\0")
        .find_map(|record| {
            let mut fields = record.split('\0');
            if fields.next()? != wanted {
                return None;
            }
            fields.find_map(|field| field.strip_prefix("HEAD ").map(str::to_owned))
        })
}

/// `path` with its parent resolved, as Git records it. A parent that is gone
/// too (removal drops empty `.simgit` scaffolding) leaves `path` as given.
fn resolved(path: &Path) -> std::path::PathBuf {
    match path.parent().unwrap().canonicalize() {
        Ok(parent) => parent.join(path.file_name().unwrap()),
        Err(_) => path.to_path_buf(),
    }
}

/// Wait for a wrapper to create `marker`, failing rather than hanging when it
/// never runs.
fn wait_for(marker: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            marker.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A pid that named a process which has exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
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
    let unlock = Command::new("sh")
        .current_dir(&fixture.repo)
        .env("PATH", bin_dir_first())
        .arg("-c")
        .arg(format!("simgit unlock {quoted}"))
        .output()
        .unwrap();
    success(&unlock);
    assert_eq!(fixture.listed()["locked"], serde_json::Value::Null);
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
        let output = Command::new("git")
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

/// Create a worktree on `branch` at `path` with `run`.
fn create_at(fixture: &Fixture, branch: &str, path: &Path, extra: &[&str]) {
    let output = fixture
        .command()
        .args(["run", branch, "--path"])
        .arg(path)
        .args(extra)
        .args(["--", "true"])
        .output()
        .unwrap();
    success(&output);
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
    let fixture = Fixture::new();
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
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
    // `discover_repo`'s three rev-parse calls come before the registry lock:
    // once they are done, a prune without that lock would reach Git at once.
    let discovered = format!("{} discovered", second.id());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == discovered)
        .count()
        < 3
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

/// Git does not list a registration whose `gitdir` file is empty or corrupt,
/// but its own prune drops one. simgit judges each admin dir itself: a corrupt
/// one on a branch goes, one whose detached HEAD no ref reaches stays, and one
/// whose HEAD cannot be read stays and is reported.
#[test]
fn prune_drops_a_corrupt_registration_and_keeps_what_it_cannot_lose() {
    let fixture = Fixture::new();
    // Git 2.51+ auto maintenance, run by `git commit` in the background, would
    // prune a corrupt registration itself before simgit sees it.
    git(&fixture.repo, &["config", "maintenance.auto", "false"]);
    let admins = fixture.repo.canonicalize().unwrap().join(".git/worktrees");
    let corrupt = |worktree: &Path| {
        let admin = std::path::PathBuf::from(registration(&fixture, worktree));
        fs::remove_dir_all(worktree).unwrap();
        fs::write(admin.join("gitdir"), "").unwrap();
        admins.join(admin.file_name().unwrap())
    };
    fixture.create(&[]);
    let on_branch = corrupt(&fixture.worktree);
    let detached = add_detached(&fixture);
    let head = commit_in(&detached, "agent.txt");
    let unreached = corrupt(&detached);
    let other = fixture.root.join("other");
    create_at(&fixture, "other/test", &other, &[]);
    let unreadable = corrupt(&other);
    fs::write(unreadable.join("HEAD"), "not a head\n").unwrap();

    let pruned = fixture.run(&["--json", "prune"]);
    success(&pruned);
    let report: serde_json::Value = serde_json::from_slice(&pruned.stdout).unwrap();
    assert_eq!(
        report["pruned_registrations"],
        serde_json::json!([on_branch.display().to_string()])
    );
    assert!(!on_branch.exists());
    assert_eq!(
        fs::read_to_string(unreached.join("HEAD")).unwrap().trim(),
        head
    );
    assert!(unreadable.join("HEAD").is_file());
    let kept = report["kept_registrations"].as_array().unwrap();
    assert_eq!(kept.len(), 2, "{kept:?}");
    for (admin, reason) in [
        (&unreached, "unreached-commits"),
        (&unreadable, "status-failed"),
    ] {
        let entry =
            serde_json::json!({ "worktree": admin.display().to_string(), "reason": reason });
        assert!(kept.contains(&entry), "{entry} not in {kept:?}");
    }
    assert!(pruned.stderr.is_empty(), "{:?}", pruned.stderr);
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

fn registration(fixture: &Fixture, worktree: &Path) -> String {
    let dir = fixture.repo.join(".git/worktrees");
    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap().path();
        let recorded = fs::read_to_string(entry.join("gitdir")).unwrap();
        if Path::new(recorded.trim()).parent() == Some(worktree.canonicalize().unwrap().as_path()) {
            return entry.display().to_string();
        }
    }
    panic!("no registration for {worktree:?}");
}

/// HEAD names an object that does not exist, so git cannot say whether the
/// commit is reached: that is an error, never permission to delete.
#[test]
fn remove_and_gc_keep_a_detached_worktree_when_git_cannot_answer() {
    let fixture = Fixture::new();
    let path = add_detached(&fixture);
    let registered = registration(&fixture, &path);
    fs::write(
        Path::new(&registered).join("HEAD"),
        format!("{}\n", "a".repeat(40)),
    )
    .unwrap();

    let output = fixture.run(&["--json", "remove", path.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.trim().lines().count(), 1, "{stderr}");
    assert!(path.is_dir());

    let gc = fixture.run(&["--json", "gc", "--older-than", "0s", "--include-persistent"]);
    success(&gc);
    let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_eq!(report["reaped"], serde_json::json!([]));
    assert!(
        report["skipped"].to_string().contains("status-failed"),
        "{report}"
    );
    assert!(path.is_dir());
}

/// A prune drops a registration whose `.git` is missing, and CoW population
/// takes it away while it clones the tree into place. With simgit's prunes
/// running beside them, parallel allocations used to lose their registration
/// mid-clone and then fail their own rollback. The tree is large enough that
/// each clone is slow enough for a prune to land in it. Only the root-clone
/// (macOS) and overlay backends move `.git`, so only there can this fail.
///
/// Native `git worktree prune` is out of scope here: it takes no lock, and Git
/// makes the admin dir visible before its `locked` file, so it can drop a
/// registration inside `git worktree add` itself. Native prune during
/// population is pinned on every OS by
/// `prune_during_population_keeps_the_registration`.
///
/// gc runs beside them too, with `--include-persistent --dry-run`, so the
/// allocation lock is all that keeps a racer from it. The wrapper stretches
/// the moments just before and after each allocation drops that lock, and gc
/// must name a racer as reapable only once its allocation has written
/// everything, its mode marker included.
#[test]
fn allocation_survives_a_concurrent_prune() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = Fixture::new();
    for index in 0..1000 {
        fs::write(
            fixture.repo.join(format!("file-{index}")),
            format!("{index}\n"),
        )
        .unwrap();
    }
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "many files"]);
    // A checkout never takes `.git` away; the CoW backends do.
    let command = || {
        let mut command = fixture.command();
        command.env_remove("SIMGIT_POPULATE");
        command
    };
    let unlock = git_wrapper(
        &fixture,
        "if [ \"$2\" = worktree ] && [ \"$3\" = unlock ]; then\n\
           sleep 0.2\n\
           \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n\
           status=$?\n\
           sleep 0.2\n\
           exit $status\n\
         fi\n",
    );
    let fully_allocated = |path: &Path| {
        let admin = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .unwrap();
        let admin = String::from_utf8_lossy(&admin.stdout).trim().to_owned();
        path.join("file-999").is_file() && Path::new(&admin).join("simgit-mode").is_file()
    };

    let done = AtomicBool::new(false);
    let (outcomes, maintenance) = std::thread::scope(|scope| {
        let pruner = scope.spawn(|| {
            let mut outputs = Vec::new();
            while !done.load(Ordering::Relaxed) {
                outputs.push(command().arg("prune").output().unwrap());
            }
            outputs
        });
        // Apart from prune, which waits for each unlock to finish.
        let collector = scope.spawn(|| {
            let mut outputs = Vec::new();
            let (mut met_lock, mut met_reapable) = (false, false);
            while !done.load(Ordering::Relaxed) {
                let gc = command()
                    .args(["--json", "gc", "--include-persistent", "--dry-run"])
                    .args(["--older-than", "0s"])
                    .output()
                    .unwrap();
                success(&gc);
                let report: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
                for path in report["reaped"].as_array().unwrap() {
                    let path = Path::new(path.as_str().unwrap());
                    met_reapable = true;
                    assert!(
                        fully_allocated(path),
                        "gc named {} reapable mid-allocation",
                        path.display()
                    );
                }
                met_lock |= report["skipped"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|skipped| skipped["reason"] == "locked");
                outputs.push(gc);
            }
            assert!(met_lock, "gc never met an allocation lock");
            assert!(met_reapable, "gc never saw an allocated racer");
            outputs
        });
        let racers: Vec<_> = (0..10)
            .map(|index| {
                command()
                    .envs(unlock.clone())
                    .args(["--json", "add", &format!("agent/racer-{index}"), "--path"])
                    .arg(fixture.root.join(format!("racer-{index}")))
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let outcomes: Vec<_> = racers
            .into_iter()
            .map(|racer| racer.wait_with_output().unwrap())
            .collect();
        done.store(true, Ordering::Relaxed);
        let mut maintenance = pruner.join().unwrap();
        maintenance.extend(collector.join().unwrap());
        (outcomes, maintenance)
    });
    for output in &maintenance {
        success(output);
    }

    let failures: Vec<_> = outcomes
        .iter()
        .filter(|output| !output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
        .collect();
    assert!(
        failures.is_empty(),
        "{} of 10 allocations failed: {failures:#?}",
        failures.len()
    );
    let registrations = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let registrations = String::from_utf8_lossy(&registrations.stdout).into_owned();
    for (index, output) in outcomes.iter().enumerate() {
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let path = Path::new(report["path"].as_str().unwrap());
        assert_eq!(fs::read_to_string(path.join("file-999")).unwrap(), "999\n");
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        success(&status);
        assert!(status.stdout.is_empty(), "racer-{index} is not clean");
        let entries: Vec<_> = registrations
            .split("\n\n")
            .filter(|entry| {
                entry
                    .lines()
                    .any(|line| line == format!("branch refs/heads/agent/racer-{index}"))
            })
            .collect();
        assert_eq!(entries.len(), 1, "racer-{index}: {registrations}");
        assert!(
            !entries[0].lines().any(|line| line.starts_with("locked")),
            "racer-{index} kept its allocation lock: {registrations}"
        );
    }
}

/// The allocation lock names the allocating process, so an allocation killed
/// before its worktree was whole leaves a lock that `unlock` refuses while the
/// allocator lives and clears once it is gone.
#[test]
fn a_killed_allocation_leaves_a_lock_that_unlock_clears() {
    let fixture = Fixture::new();
    let started = fixture.root.join("reset-started");
    let release = fixture.root.join("reset-release");
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$1\" = -C ] && [ \"$3\" = reset ]; then\n\
               : > '{started}'\n\
               tries=0\n\
               while [ ! -e '{release}' ] && [ $tries -lt 200 ]; do sleep 0.05; tries=$((tries + 1)); done\n\
               exit 1\n\
             fi\n",
            started = started.display(),
            release = release.display(),
        ),
    );
    let mut allocation = fixture
        .command()
        .envs(env)
        .args(["add", "agent/killed", "--path"])
        .arg(&fixture.worktree)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let allocator = allocation.id();
    while !started.exists() {
        assert!(
            allocation.try_wait().unwrap().is_none(),
            "allocation exited before populating"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let locked = |fixture: &Fixture| {
        let output = fixture.run(&["list", "--json"]);
        success(&output);
        let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        entries
            .into_iter()
            .find(|entry| entry["branch"] == "refs/heads/agent/killed")
            .unwrap()["locked"]
            .clone()
    };
    let while_allocating = locked(&fixture);
    let refused = fixture.run(&["unlock", "agent/killed"]);
    allocation.kill().unwrap();
    allocation.wait().unwrap();
    fs::write(&release, "").unwrap();

    assert_eq!(
        while_allocating,
        format!("simgit: allocating (pid {allocator})")
    );
    assert!(!refused.status.success(), "unlock ignored a live allocator");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&format!("pid {allocator}")),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        locked(&fixture),
        format!("simgit: allocating (pid {allocator})")
    );

    let unlocked = fixture.run(&["--json", "unlock", "agent/killed"]);
    success(&unlocked);
    let report: serde_json::Value = serde_json::from_slice(&unlocked.stdout).unwrap();
    assert_eq!(report["was_locked"], true);
    assert_eq!(report["owner_pid"], allocator);
    assert_eq!(locked(&fixture), serde_json::Value::Null);
}

/// A failing `post-checkout` hook fails the allocation but keeps the worktree,
/// as `git worktree add` does, so it must not keep the allocation lock too.
#[test]
fn a_failing_post_checkout_hook_keeps_the_worktree_unlocked() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let hook = fixture.repo.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let output = fixture
        .command()
        .args(["add", "agent/hooked", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "the failing hook did not fail add"
    );
    assert!(fixture.worktree.join("README.md").is_file());
    let listed = fixture.run(&["list", "--json"]);
    success(&listed);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    let entry = entries
        .into_iter()
        .find(|entry| entry["branch"] == "refs/heads/agent/hooked")
        .unwrap();
    assert_eq!(entry["locked"], serde_json::Value::Null, "{entry}");
}

/// A population failure rolls the allocation back. The registration is still
/// locked at that point, and Git refuses to remove a locked worktree.
#[test]
fn a_failed_population_rolls_back_its_locked_registration() {
    let fixture = Fixture::new();
    let env = git_wrapper(
        &fixture,
        "if [ \"$1\" = -C ] && [ \"$3\" = status ]; then exit 1; fi\n",
    );
    let output = fixture
        .command()
        .envs(env)
        .args(["add", "agent/failed", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "population did not fail");
    assert!(!stderr.contains("rollback"), "{stderr}");
    assert!(!fixture.worktree.exists());
    let registrations = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&registrations.stdout)
            .matches("worktree ")
            .count(),
        1
    );
    let branch = Command::new("git")
        .current_dir(&fixture.repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/agent/failed"])
        .status()
        .unwrap();
    assert!(!branch.success(), "rollback kept the new branch");
}

/// The same race on every OS: in checkout mode, the wrapper does to `.git`
/// what CoW population does, and prunes while it is gone.
#[test]
fn prune_during_population_keeps_the_registration() {
    let fixture = Fixture::new();
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$1\" = -C ] && [ \"$3\" = reset ]; then\n\
               mv \"$2/.git\" \"$2/.git-aside\" || exit\n\
               \"$SIMGIT_TEST_REAL_GIT\" -C '{repo}' worktree prune || exit\n\
               mv \"$2/.git-aside\" \"$2/.git\" || exit\n\
             fi\n",
            repo = fixture.repo.display(),
        ),
    );
    let target = fixture.root.join("pruned");
    let output = fixture
        .command()
        .envs(env)
        .args(["add", "agent/pruned", "--path"])
        .arg(&target)
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        fs::read_to_string(target.join("README.md")).unwrap(),
        "original\n"
    );
    let listed = fixture.run(&["list", "--json"]);
    success(&listed);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    let entry = entries
        .into_iter()
        .find(|entry| entry["branch"] == "refs/heads/agent/pruned")
        .expect("the registration was pruned");
    assert_eq!(entry["locked"], serde_json::Value::Null, "{entry}");
}

/// Stand in for fuse-overlayfs, so the overlay backend runs where it is not
/// installed. The "mount" copies only the gitlink into place, so population
/// fails, and `unmount_script` is every unmount tool. Returns `PATH`.
/// Linux asks the mount table whether the overlay is mounted, and no fake
/// can enter it, so these tests run only where the gitlink is the evidence.
#[cfg(not(target_os = "linux"))]
fn fake_overlay_tools(fixture: &Fixture, unmount_script: &str) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;

    let bin = fixture.root.join("overlay-bin");
    fs::create_dir(&bin).unwrap();
    let tools = [
        (
            "fuse-overlayfs",
            "upper=$(printf '%s' \"$2\" | sed 's/.*upperdir=\\([^,]*\\),.*/\\1/')\n\
             cp \"$upper/.git\" \"$3/.git\"\n",
        ),
        ("fusermount3", unmount_script),
        ("fusermount", unmount_script),
        ("umount", unmount_script),
    ];
    for (name, body) in tools {
        let tool = bin.join(name);
        fs::write(&tool, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap()
}

/// A failed overlay that cannot be unmounted is kept for recovery, and keeps
/// its allocation lock: prune must not drop a registration whose `.git` may
/// survive only in the upperdir. The error says how to release it.
#[cfg(not(target_os = "linux"))]
#[test]
fn a_retained_overlay_keeps_its_allocation_lock() {
    let fixture = Fixture::new();
    let path = fake_overlay_tools(&fixture, "exit 0\n");
    let target = fixture.root.join("overlaid");
    let output = fixture
        .command()
        .env("PATH", path)
        .env("SIMGIT_POPULATE", "overlay")
        .args(["add", "agent/overlaid", "--path"])
        .arg(&target)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "population did not fail");
    assert!(stderr.contains("retained for recovery"), "{stderr}");
    // The population failure stays the cause.
    assert!(stderr.contains("verify overlay worktree"), "{stderr}");
    let canonical = target.canonicalize().unwrap();
    assert!(
        stderr.contains(&format!("simgit unlock {}", canonical.display())),
        "{stderr}"
    );

    let listed = fixture.run(&["list", "--json"]);
    success(&listed);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    let entry = entries
        .into_iter()
        .find(|entry| entry["branch"] == "refs/heads/agent/overlaid")
        .unwrap();
    assert!(
        entry["locked"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("simgit: allocating (pid ")),
        "{entry}"
    );
    let unlocked = fixture.run(&["--json", "unlock", "agent/overlaid"]);
    success(&unlocked);
    let report: serde_json::Value = serde_json::from_slice(&unlocked.stdout).unwrap();
    assert_eq!(report["was_locked"], true);
}

/// The other exit that cannot roll back: the overlay unmounts, but its
/// gitlink cannot be put back. The same rule holds.
#[cfg(not(target_os = "linux"))]
#[test]
fn a_failed_gitlink_restore_keeps_the_allocation_lock() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let mountpoint = "for last; do :; done\nrm -f \"$last/.git\"\nchmod 555 \"$last\"\n";
    let path = fake_overlay_tools(&fixture, mountpoint);
    let target = fixture.root.join("overlaid");
    let output = fixture
        .command()
        .env("PATH", path)
        .env("SIMGIT_POPULATE", "overlay")
        .args(["add", "agent/overlaid", "--path"])
        .arg(&target)
        .output()
        .unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "population did not fail");
    assert!(stderr.contains("restore the gitlink"), "{stderr}");
    assert!(stderr.contains("verify overlay worktree"), "{stderr}");
    let canonical = target.canonicalize().unwrap();
    assert!(
        stderr.contains(&format!("simgit unlock {}", canonical.display())),
        "{stderr}"
    );
    let registrations = fixture.repo.join(".git/worktrees");
    let locked: Vec<_> = fs::read_dir(&registrations)
        .unwrap()
        .filter_map(|entry| fs::read_to_string(entry.unwrap().path().join("locked")).ok())
        .collect();
    assert_eq!(locked.len(), 1, "{locked:?}");
    assert!(
        locked[0].starts_with("simgit: allocating (pid "),
        "{locked:?}"
    );
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap()
        .status
        .success()
}

/// `git status` failing is how these tests make population fail.
const FAIL_POPULATION: &str = "if [ \"$1\" = -C ] && [ \"$3\" = status ]; then\n\
       echo 'simulated population failure' >&2\n\
       exit 1\n\
     fi\n";

/// The rollback of a failed allocation must not hide why it failed, and must
/// delete the branch the allocation created even when it is named `master`.
#[test]
fn failed_allocation_reports_the_cause_and_removes_its_new_branch() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["branch", "-M", "main"]);
    let env = git_wrapper(&fixture, FAIL_POPULATION);
    let failed = fixture
        .command()
        .envs(env)
        .args(["add", "master", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success(), "{stderr}");
    assert!(stderr.contains("simulated population failure"), "{stderr}");
    assert!(!stderr.contains("refusing to delete"), "{stderr}");
    assert!(!branch_exists(&fixture.repo, "master"));
    assert!(!fixture.worktree.exists());
    assert!(branch_exists(&fixture.repo, "main"));
}

/// When the rollback fails too, its error is context on the cause, not a
/// replacement for it.
#[test]
fn failed_rollback_keeps_the_original_cause() {
    let fixture = Fixture::new();
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$2\" = worktree ] && [ \"$3\" = remove ]; then\n\
               echo 'fatal: simulated removal failure' >&2\n\
               exit 1\n\
             fi\n{FAIL_POPULATION}"
        ),
    );
    let failed = fixture
        .command()
        .envs(env)
        .args(["add", "agent/doomed", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success(), "{stderr}");
    assert!(
        stderr.contains("simulated population failure"),
        "cause lost: {stderr}"
    );
    assert!(stderr.contains("simulated removal failure"), "{stderr}");
}

/// A failing hook is the cause even when releasing the allocation lock fails
/// too: the unlock failure is context, not a replacement.
#[test]
fn a_failing_hook_stays_the_cause_when_unlock_fails_too() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let hook = fixture.repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'simulated hook failure' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let env = git_wrapper(
        &fixture,
        "if [ \"$2\" = worktree ] && [ \"$3\" = unlock ]; then\n\
           echo 'fatal: simulated unlock failure' >&2\n\
           exit 1\n\
         fi\n",
    );

    let failed = fixture
        .command()
        .envs(env)
        .args(["add", "agent/hooked", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success(), "{stderr}");
    assert!(
        stderr.contains("simulated hook failure"),
        "cause lost: {stderr}"
    );
    assert!(stderr.contains("simgit unlock"), "{stderr}");
}

/// When marking an allocation ephemeral fails, the marker is the cause and
/// the branch this call created goes, even when it is named `master`. The
/// wrapper makes the marker's path a directory once Git has registered the
/// worktree, so writing the marker fails.
#[test]
fn failed_ephemeral_marker_reports_the_cause_and_removes_its_new_branch() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["branch", "-M", "main"]);
    let env = git_wrapper(
        &fixture,
        "if [ \"$2\" = worktree ] && [ \"$3\" = add ]; then\n\
           \"$SIMGIT_TEST_REAL_GIT\" \"$@\" || exit $?\n\
           for admin in \"${1#--git-dir=}\"/worktrees/*/; do mkdir \"${admin}simgit-ephemeral\"; done\n\
           exit 0\n\
         fi\n",
    );

    let failed = fixture
        .command()
        .envs(env)
        .args(["add", "master", "--ephemeral", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success(), "{stderr}");
    assert!(
        stderr.contains("write ephemeral marker"),
        "cause lost: {stderr}"
    );
    assert!(!stderr.contains("refusing to delete"), "{stderr}");
    assert!(!branch_exists(&fixture.repo, "master"));
    assert!(!fixture.worktree.exists());
    assert!(branch_exists(&fixture.repo, "main"));
}

/// Add `<fixture.root>/lib` to the fixture as the submodule `sub` and return
/// its checkout. Its git dir is `.git/modules/sub`, named nothing like `.git`.
fn add_submodule(fixture: &Fixture) -> std::path::PathBuf {
    add_submodule_to(fixture, &fixture.repo, "lib", "sub")
}

/// Add a new one-commit repository `<fixture.root>/<source>` to `superproject`
/// as a submodule at `path`, commit it there, and return its checkout.
fn add_submodule_to(
    fixture: &Fixture,
    superproject: &Path,
    source: &str,
    path: &str,
) -> std::path::PathBuf {
    let lib = fixture.root.join(source);
    fs::create_dir_all(&lib).unwrap();
    git(&lib, &["init", "-q"]);
    git(&lib, &["config", "user.email", "test@example.com"]);
    git(&lib, &["config", "user.name", "Test User"]);
    fs::write(lib.join("lib.txt"), "lib\n").unwrap();
    git(&lib, &["add", "."]);
    git(&lib, &["commit", "-qm", "lib"]);
    let lib = lib.to_str().unwrap();
    git(
        superproject,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            lib,
            path,
        ],
    );
    git(superproject, &["config", "user.email", "test@example.com"]);
    git(superproject, &["config", "user.name", "Test User"]);
    git(superproject, &["commit", "-qm", "add submodule"]);
    superproject.join(path)
}

/// The directory `add` puts `branch` in by default when run from `dir`.
fn default_parent(fixture: &Fixture, dir: &Path, branch: &str) -> std::path::PathBuf {
    let output = fixture
        .command()
        .current_dir(dir)
        .args(["--json", "add", branch])
        .output()
        .unwrap();
    success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let path = Path::new(report["path"].as_str().unwrap());
    assert!(path.join(".git").is_file(), "{report}");
    path.parent().unwrap().to_path_buf()
}

/// Assert that `parent` is `<root>/.simgit/repo/.modules/<prefix>-<8 hex>`:
/// beside the superproject's main checkout, apart from its branches.
fn assert_module_root(fixture: &Fixture, parent: &Path, prefix: &str) {
    let modules = fixture
        .root
        .canonicalize()
        .unwrap()
        .join(".simgit/repo/.modules");
    assert_eq!(parent.parent(), Some(modules.as_path()), "{parent:?}");
    let name = parent.file_name().unwrap().to_str().unwrap();
    let digest = name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .unwrap_or_else(|| panic!("{name} does not start with {prefix}-"));
    assert!(
        digest.len() == 8 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{name}"
    );
}

/// What `git status` reports in `checkout`, ignored files included.
fn status_with_ignored(checkout: &Path) -> String {
    let output = Command::new("git")
        .current_dir(checkout)
        .args(["status", "--porcelain", "--ignored"])
        .output()
        .unwrap();
    success(&output);
    String::from_utf8(output.stdout).unwrap()
}

/// A submodule's checkout sits inside its superproject's working tree, so its
/// worktrees go beside the superproject instead, wherever simgit runs from.
#[test]
fn default_path_in_a_submodule_is_outside_the_superproject() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    let before = status_with_ignored(&fixture.repo);

    let from_checkout = default_parent(&fixture, &sub, "agent/checkout");
    assert_module_root(&fixture, &from_checkout, "sub");
    let nested = sub.join("deeper");
    fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        default_parent(&fixture, &nested, "agent/nested"),
        from_checkout
    );
    let linked = fixture.root.join("sub-linked");
    git(&sub, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    assert_eq!(
        default_parent(&fixture, &linked, "agent/linked"),
        from_checkout
    );

    fs::remove_dir(&nested).unwrap();
    assert_eq!(status_with_ignored(&fixture.repo), before);
}

/// Each superproject worktree has its own module git dir, so the same
/// submodule there gets its own root, beside the superproject's main checkout
/// and outside both working trees.
#[test]
fn default_path_in_a_submodule_of_a_linked_worktree_is_outside_both() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    let linked = fixture.root.join("linked");
    git(
        &fixture.repo,
        &["worktree", "add", "-q", linked.to_str().unwrap()],
    );
    git(
        &linked,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "-q",
            "--init",
        ],
    );
    let before = (
        status_with_ignored(&fixture.repo),
        status_with_ignored(&linked),
    );

    let in_linked = default_parent(&fixture, &linked.join("sub"), "agent/linked");
    assert_module_root(&fixture, &in_linked, "sub");
    let in_main = default_parent(&fixture, &sub, "agent/main");
    assert_module_root(&fixture, &in_main, "sub");
    assert_ne!(in_linked, in_main);
    assert_eq!(
        (
            status_with_ignored(&fixture.repo),
            status_with_ignored(&linked),
        ),
        before
    );
}

/// A submodule of a submodule goes beside the outermost superproject, never
/// beside the intermediate one, which is itself inside a working tree.
#[test]
fn default_path_in_a_nested_submodule_is_beside_the_outermost_superproject() {
    let fixture = Fixture::new();
    let middle = add_submodule_to(&fixture, &fixture.repo, "middle", "a");
    let inner = add_submodule_to(&fixture, &middle, "inner", "b");
    let before = status_with_ignored(&fixture.repo);

    assert_module_root(
        &fixture,
        &default_parent(&fixture, &inner, "agent/inner"),
        "a-b",
    );
    assert_eq!(status_with_ignored(&fixture.repo), before);
}

#[test]
fn same_named_submodules_get_distinct_default_roots() {
    let fixture = Fixture::new();
    let first = add_submodule_to(&fixture, &fixture.repo, "one", "x/lib");
    let second = add_submodule_to(&fixture, &fixture.repo, "two", "y/lib");

    let first = default_parent(&fixture, &first, "agent/first");
    let second = default_parent(&fixture, &second, "agent/second");
    assert_module_root(&fixture, &first, "x-lib");
    assert_module_root(&fixture, &second, "y-lib");
}

#[test]
fn default_path_with_separate_git_dir() {
    let fixture = Fixture::new();
    let checkout = fixture.root.join("checkout");
    let git_dir = fixture.root.join("store");
    git(
        &fixture.root,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            git_dir.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    git(&checkout, &["config", "user.email", "test@example.com"]);
    git(&checkout, &["config", "user.name", "Test User"]);
    fs::write(checkout.join("README.md"), "separate\n").unwrap();
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-qm", "initial"]);
    assert_eq!(
        default_parent(&fixture, &checkout, "agent/edit"),
        fixture
            .root
            .canonicalize()
            .unwrap()
            .join(".simgit/checkout")
    );
}

#[test]
fn doctor_is_main_worktree_in_a_submodule() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    let doctor = |dir: &Path| {
        let output = fixture
            .command()
            .current_dir(dir)
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        success(&output);
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let report = doctor(&sub);
    assert_eq!(report["repository_details"]["is_main_worktree"], true);
    assert_eq!(
        report["repository_details"]["common_git_owner"],
        sub.canonicalize().unwrap().display().to_string()
    );
    assert_module_root(
        &fixture,
        Path::new(report["default_worktree_root"].as_str().unwrap()),
        "sub",
    );
    assert_eq!(report["default_worktree_root_inside_repository"], false);
    // `list --json` names the checkout, not the git dir Git reports for it.
    assert_eq!(
        first_listed(&fixture, &sub)["worktree"],
        sub.canonicalize().unwrap().display().to_string()
    );

    let linked = fixture.root.join("sub-linked");
    git(&sub, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    assert_eq!(
        doctor(&linked)["repository_details"]["is_main_worktree"],
        false
    );
}

/// `remove` knows the main checkout from the registry's first entry, which Git
/// reports as a submodule's git dir; the guard must still recognise it.
#[test]
fn remove_commit_refuses_a_submodules_main_checkout() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    git(&sub, &["config", "user.email", "test@example.com"]);
    git(&sub, &["config", "user.name", "Test User"]);
    fs::write(sub.join("scratch.txt"), "work in progress\n").unwrap();
    let head = rev_parse(&sub, "HEAD");
    let target = sub.canonicalize().unwrap().display().to_string();

    for args in [
        vec!["remove", "--commit"],
        vec!["remove", &target, "--commit"],
    ] {
        let output = fixture
            .command()
            .current_dir(&sub)
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("main checkout"), "{args:?}: {stderr}");
        assert_eq!(rev_parse(&sub, "HEAD"), head, "{args:?} committed");
    }
    assert!(sub.join("scratch.txt").is_file());
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

/// A `gitdir` of `/` names no worktree directory to check, so the entry
/// cannot be judged stale and is kept.
#[test]
fn prune_keeps_a_registration_whose_gitdir_has_no_parent() {
    let fixture = Fixture::new();
    fixture.create(&[]);
    let admin = Path::new(&registration(&fixture, &fixture.worktree)).to_path_buf();
    fs::remove_dir_all(&fixture.worktree).unwrap();
    fs::write(admin.join("gitdir"), "/\n").unwrap();

    let pruned = fixture.run(&["--json", "prune"]);
    success(&pruned);
    let report: serde_json::Value = serde_json::from_slice(&pruned.stdout).unwrap();
    assert_eq!(report["pruned_registrations"], serde_json::json!([]));
    assert!(admin.join("HEAD").is_file());
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

/// The first entry of `list --json` run from `dir`: the main worktree.
fn first_listed(fixture: &Fixture, dir: &Path) -> serde_json::Value {
    let output = fixture
        .command()
        .current_dir(dir)
        .args(["--json", "list"])
        .output()
        .unwrap();
    success(&output);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    entries.into_iter().next().unwrap()
}

/// `doctor --json` run from `dir`.
fn doctor_report(fixture: &Fixture, dir: &Path) -> serde_json::Value {
    let output = fixture
        .command()
        .current_dir(dir)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

/// From a linked worktree of a layout with no main checkout to find, the main
/// worktree is what Git lists first, and worktrees go beside it.
fn assert_fallback_main(fixture: &Fixture, linked: &Path, main: &Path) {
    let main = main.canonicalize().unwrap();
    let report = doctor_report(fixture, linked);
    assert_eq!(report["repository_details"]["is_main_worktree"], false);
    assert_eq!(
        report["repository_details"]["common_git_owner"],
        main.display().to_string()
    );
    assert_eq!(
        Path::new(report["default_worktree_root"].as_str().unwrap()),
        main.parent()
            .unwrap()
            .join(".simgit")
            .join(main.file_name().unwrap())
    );
    assert_eq!(
        first_listed(fixture, linked)["worktree"],
        main.display().to_string()
    );
    // Refused as the main checkout, before its lock state is even asked.
    let removed = fixture
        .command()
        .current_dir(linked)
        .args(["remove", main.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!removed.status.success(), "removed the main worktree");
    let stderr = String::from_utf8_lossy(&removed.stderr);
    assert!(stderr.contains("is the main checkout"), "{stderr}");
}

/// Clone the fixture bare to `<root>/<path>` and add a linked worktree.
fn bare_with_linked_worktree(fixture: &Fixture, path: &str) -> std::path::PathBuf {
    let bare = fixture.root.join(path);
    let source = fixture.repo.to_str().unwrap();
    git(
        &fixture.root,
        &["clone", "-q", "--bare", source, bare.to_str().unwrap()],
    );
    let linked = fixture.root.join("bare-linked");
    git(&bare, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    linked
}

#[test]
fn a_bare_repository_is_its_own_main_worktree() {
    let fixture = Fixture::new();
    let linked = bare_with_linked_worktree(&fixture, "x.git");
    assert_fallback_main(&fixture, &linked, &fixture.root.join("x.git"));
    assert_eq!(first_listed(&fixture, &linked)["bare"], true);
}

/// A bare repository at `proj/.git` keeps worktrees beside `proj`, as before.
#[test]
fn a_bare_repository_named_dot_git_is_owned_by_its_parent() {
    let fixture = Fixture::new();
    let linked = bare_with_linked_worktree(&fixture, "proj/.git");
    assert_fallback_main(&fixture, &linked, &fixture.root.join("proj"));
}

/// A `--separate-git-dir` repository records no path back to its checkout, so
/// from a linked worktree its git dir stands in for the main worktree.
#[test]
fn a_separate_git_dir_seen_from_a_linked_worktree_is_its_own_main_worktree() {
    let fixture = Fixture::new();
    let checkout = fixture.root.join("checkout");
    let git_dir = fixture.root.join("store");
    git(
        &fixture.root,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            git_dir.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    git(&checkout, &["config", "user.email", "test@example.com"]);
    git(&checkout, &["config", "user.name", "Test User"]);
    fs::write(checkout.join("README.md"), "separate\n").unwrap();
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-qm", "initial"]);
    let linked = fixture.root.join("separate-linked");
    git(
        &checkout,
        &["worktree", "add", "-q", linked.to_str().unwrap()],
    );
    assert_fallback_main(&fixture, &linked, &git_dir);
}

/// A lock simgit cannot locate is not a lock it can rule out. Here the entry's
/// `gitdir` is not UTF-8, so no reader of the registry can match it to the
/// worktree Git lists; nothing may be torn down on that guess.
#[test]
fn an_undeterminable_lock_state_refuses_teardown() {
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let admin = Path::new(&registration(&fixture, &fixture.worktree)).to_path_buf();
    fs::write(admin.join("locked"), "usb").unwrap();
    fs::remove_dir_all(&fixture.worktree).unwrap();
    let mut gitdir = fixture.root.join("gone").as_os_str().as_bytes().to_vec();
    gitdir.extend_from_slice(b"\xff/.git\n");
    fs::write(admin.join("gitdir"), gitdir).unwrap();

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
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{report}");
    assert_eq!(skipped[0]["reason"], "status-failed", "{report}");

    let removed = fixture.run(&["remove", "chat/test", "--discard-dirty"]);
    assert!(!removed.status.success(), "removed it");
    let stderr = String::from_utf8_lossy(&removed.stderr);
    assert!(stderr.contains("cannot tell whether"), "{stderr}");
    assert!(admin.join("HEAD").is_file());
}

/// A repository inside another's working tree that is not its submodule has
/// no superproject to find, so it keeps the ordinary rule.
#[test]
fn a_nested_repository_that_is_not_a_submodule_keeps_its_own_root() {
    let fixture = Fixture::new();
    let inner = fixture.repo.join("inner");
    fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q"]);
    git(&inner, &["config", "user.email", "test@example.com"]);
    git(&inner, &["config", "user.name", "Test User"]);
    fs::write(inner.join("inner.txt"), "inner\n").unwrap();
    git(&inner, &["add", "."]);
    git(&inner, &["commit", "-qm", "inner"]);
    assert_eq!(
        default_parent(&fixture, &inner, "agent/inner"),
        fixture.repo.canonicalize().unwrap().join(".simgit/inner")
    );
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

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A linked worktree two commits ahead of the main checkout's HEAD.
fn linked_worktree_ahead(fixture: &Fixture) -> std::path::PathBuf {
    let linked = fixture.root.join("linked");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    for name in ["one", "two"] {
        fs::write(linked.join(name), name).unwrap();
        git(&linked, &["add", "."]);
        git(&linked, &["commit", "-qm", name]);
    }
    linked
}

fn run_from(fixture: &Fixture, dir: &Path, args: &[&str]) -> std::process::Output {
    fixture
        .command()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn add_from_a_linked_worktree_starts_at_its_head() {
    let fixture = Fixture::new();
    let linked = linked_worktree_ahead(&fixture);
    let head = git_stdout(&linked, &["rev-parse", "HEAD"]);
    assert_ne!(head, git_stdout(&fixture.repo, &["rev-parse", "HEAD"]));

    let output = run_from(&fixture, &linked, &["add", "--json", "agent/from-linked"]);
    success(&output);
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(created["base"], head.as_str());
    let path = Path::new(created["path"].as_str().unwrap());
    assert_eq!(git_stdout(path, &["rev-parse", "HEAD"]), head);

    // `HEAD~1` is relative to the caller too.
    let parent = git_stdout(&linked, &["rev-parse", "HEAD~1"]);
    let output = run_from(
        &fixture,
        &linked,
        &["add", "--json", "--base", "HEAD~1", "agent/parent"],
    );
    success(&output);
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(created["base"], parent.as_str());

    // A branch name resolves the same from anywhere.
    let output = run_from(
        &fixture,
        &linked,
        &["add", "--json", "--base", "linked", "agent/named"],
    );
    success(&output);
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(created["base"], head.as_str());
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

#[test]
fn add_in_a_repository_without_commits_says_so() {
    let root = std::env::temp_dir().join(format!("simgit-unborn-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    let output = Command::new(env!("CARGO_BIN_EXE_sg"))
        .current_dir(&root)
        .args(["add", "agent/x"])
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&root);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no commits yet"));
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
