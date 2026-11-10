use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A command with the developer's Git configuration out of reach: a global
/// `init.defaultBranch` or `commit.gpgsign` must not change what a test sees.
fn isolated(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", std::env::temp_dir().join("simgit-test-no-home"))
        .env_remove("XDG_CONFIG_HOME");
    command
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = isolated("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct HookFixture {
    root: PathBuf,
    repo: PathBuf,
}

impl HookFixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("simgit-claude-hook-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repository");
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test User"]);
        fs::write(repo.join("README.md"), "base\n").expect("write fixture");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        Self { root, repo }
    }

    /// Run `simgit <before> hook claude-code --requires 0.0.0 <extra>` from
    /// outside the repository, feeding `stdin` verbatim.
    fn run(&self, before: &[&str], extra: &[&str], stdin: &str) -> Output {
        self.run_in(&self.repo, before, extra, stdin)
    }

    /// Like `run`, with `project` as `$CLAUDE_PROJECT_DIR`.
    fn run_in(&self, project: &Path, before: &[&str], extra: &[&str], stdin: &str) -> Output {
        self.run_with_env(project, before, extra, stdin, &[])
    }

    /// Like `run_in`, with extra environment variables.
    fn run_with_env(
        &self,
        project: &Path,
        before: &[&str],
        extra: &[&str],
        stdin: &str,
        env: &[(&str, &std::ffi::OsStr)],
    ) -> Output {
        let mut child = isolated(env!("CARGO_BIN_EXE_simgit"))
            .current_dir(&self.root)
            .env("HOME", &self.root)
            // No test here may reach GitHub; the update check has its own.
            .env("SIMGIT_NO_UPDATE_CHECK", "1")
            .envs(env.iter().copied())
            .env("CLAUDE_PROJECT_DIR", project)
            .env("SIMGIT_POPULATE", "checkout")
            .args(before)
            .args(["hook", "claude-code", "--requires", "0.0.0"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn simgit");
        // A hook refused before it reads its payload (`--json`, a bad
        // `--requires`) may exit first; the broken pipe is then expected.
        if let Err(error) = child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
        {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "write stdin");
        }
        child.wait_with_output().expect("wait for simgit")
    }

    fn hook(&self, extra: &[&str], payload: Value) -> Output {
        self.run(&[], extra, &payload.to_string())
    }

    fn snapshot(&self) -> (String, String, String) {
        (
            git(&self.repo, &["status", "--porcelain"]),
            git(&self.repo, &["worktree", "list", "--porcelain"]),
            git(&self.repo, &["branch", "--list"]),
        )
    }
}

impl Drop for HookFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn unknown_event_and_session_start_are_no_ops() {
    let fixture = HookFixture::new();
    let before = fixture.snapshot();
    for event in ["SessionStart", "SomeFutureEvent"] {
        let output = fixture.hook(
            &[],
            json!({ "hook_event_name": event, "cwd": fixture.repo }),
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "{event}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{event}");
    }
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn requires_refuses_an_older_binary_with_exit_1() {
    let fixture = HookFixture::new();
    let mut child = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.root)
        .args(["hook", "claude-code", "--requires", "99.0.0"])
        .stdin(Stdio::null())
        .output()
        .expect("run simgit");
    assert_eq!(child.status.code(), Some(1));
    assert!(child.stdout.is_empty());
    assert!(
        stderr(&child).contains(&format!("found {}", env!("CARGO_PKG_VERSION"))),
        "{}",
        stderr(&child)
    );
    child = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.root)
        .args(["hook", "claude-code", "--requires", "1.x"])
        .stdin(Stdio::null())
        .output()
        .expect("run simgit");
    assert_eq!(child.status.code(), Some(1));
}

/// A newer plugin may pass flags this simgit does not know. The version
/// check comes first, so the person is told to upgrade, not shown a usage error.
#[test]
fn requires_is_checked_before_unknown_flags() {
    let fixture = HookFixture::new();
    let hook = |args: &[&str]| {
        isolated(env!("CARGO_BIN_EXE_simgit"))
            .current_dir(&fixture.root)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run simgit")
    };
    for args in [
        vec![
            "hook",
            "claude-code",
            "--requires",
            "99.0.0",
            "--from-the-future",
        ],
        vec![
            "hook",
            "claude-code",
            "--from-the-future",
            "--requires=99.0.0",
        ],
        vec![
            "--json",
            "hook",
            "claude-code",
            "--requires",
            "99.0.0",
            "--x",
            "y",
        ],
    ] {
        let output = hook(&args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{args:?}");
        let text = stderr(&output);
        assert_eq!(text.lines().count(), 1, "{args:?}: {text}");
        assert!(
            text.contains("requires simgit >= 99.0.0")
                && text.contains(&format!("found {}", env!("CARGO_PKG_VERSION"))),
            "{args:?}: {text}"
        );
    }
    // Once the version is satisfied an unknown flag is still a usage error.
    let output = hook(&[
        "hook",
        "claude-code",
        "--requires",
        "0.0.0",
        "--from-the-future",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(stderr(&output).lines().count(), 1, "{}", stderr(&output));

    // So is a missing --requires, with or without a global flag first.
    for args in [
        &["hook", "claude-code"][..],
        &["--json", "hook", "claude-code"],
    ] {
        let output = hook(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{args:?}");
        assert_eq!(
            stderr(&output).lines().count(),
            1,
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn json_flag_is_rejected() {
    let fixture = HookFixture::new();
    let output = fixture.run(&["--json"], &[], r#"{"hook_event_name":"SessionStart"}"#);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());

    // The refusal comes before stdin is read. A payload larger than a pipe
    // buffer makes the writer outlive the reader every time, not just
    // sometimes.
    let large = format!(
        r#"{{"hook_event_name":"SessionStart","pad":"{}"}}"#,
        "x".repeat(1 << 20)
    );
    let output = fixture.run(&["--json"], &[], &large);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn empty_or_non_json_stdin_fails_with_one_line() {
    let fixture = HookFixture::new();
    for input in ["", "not json"] {
        let output = fixture.run(&[], &[], input);
        assert_eq!(output.status.code(), Some(1), "{input:?}");
        let message = stderr(&output);
        assert!(!message.contains("panicked"), "{message}");
        assert_eq!(message.trim_end().lines().count(), 1, "{message}");
    }
}

/// Claude Code shows a failed hook's stderr to the person, so an error with a
/// cause chain still arrives as one line.
#[test]
fn a_failure_with_causes_prints_one_stderr_line() {
    let fixture = HookFixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).expect("mkdir");
    let output = fixture.run_in(
        &outside,
        &[],
        &[],
        &create_payload("s1", &outside).to_string(),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let message = stderr(&output);
    assert_eq!(message.trim_end().lines().count(), 1, "{message}");
    assert!(message.contains("not in a Git working tree"), "{message}");
}

#[test]
fn unknown_provider_is_an_error() {
    let fixture = HookFixture::new();
    let before = fixture.snapshot();
    let output = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.root)
        .args(["hook", "codex", "--requires", "0.0.0"])
        .stdin(Stdio::null())
        .output()
        .expect("run simgit");
    assert!(!output.status.success());
    assert_eq!(fixture.snapshot(), before);
}

fn create_payload(name: &str, cwd: &Path) -> Value {
    json!({ "hook_event_name": "WorktreeCreate", "name": name, "cwd": cwd })
}

fn stdout_path(output: &Output) -> PathBuf {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let path = stdout
        .strip_suffix('\n')
        .unwrap_or_else(|| panic!("stdout is not newline-terminated: {stdout:?}"));
    assert!(
        !path.contains('\n'),
        "stdout has more than one line: {stdout:?}"
    );
    PathBuf::from(path)
}

fn branch_of(path: &Path) -> String {
    git(path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_owned()
}

fn simgit_refs(repo: &Path) -> String {
    git(repo, &["for-each-ref", "refs/heads/simgit"])
}

#[test]
fn create_prints_exactly_one_path() {
    let fixture = HookFixture::new();
    let output = fixture.hook(&[], create_payload("s1", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let path = stdout_path(&output);
    assert!(path.is_dir(), "{}", path.display());
    assert_eq!(branch_of(&path), "simgit/s1");
    assert!(stderr(&output).contains("mode:"), "{}", stderr(&output));
}

#[test]
fn create_twice_returns_the_same_path() {
    let fixture = HookFixture::new();
    let first = fixture.hook(&[], create_payload("s1", &fixture.repo));
    let second = fixture.hook(&[], create_payload("s1", &fixture.repo));
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(stdout_path(&first), stdout_path(&second));
}

#[test]
fn concurrent_creates_return_one_fully_populated_path() {
    let fixture = HookFixture::new();
    let outputs: Vec<Output> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| fixture.hook(&[], create_payload("s1", &fixture.repo))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    for output in &outputs {
        assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    }
    let path = stdout_path(&outputs[0]);
    assert_eq!(path, stdout_path(&outputs[1]));
    assert!(path.join("README.md").is_file());
}

#[test]
fn create_attaches_an_existing_branch_without_moving_it() {
    let fixture = HookFixture::new();
    let old = git(&fixture.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&fixture.repo, &["branch", "simgit/s2"]);
    fs::write(fixture.repo.join("later.txt"), "later\n").expect("write");
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-q", "-m", "later"]);
    let output = fixture.hook(&[], create_payload("s2", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let path = stdout_path(&output);
    assert_eq!(git(&path, &["rev-parse", "HEAD"]).trim(), old);
    assert_eq!(git(&fixture.repo, &["rev-parse", "simgit/s2"]).trim(), old);
}

#[test]
fn create_never_returns_the_main_checkout() {
    let fixture = HookFixture::new();
    git(&fixture.repo, &["checkout", "-q", "-b", "simgit/s3"]);
    let output = fixture.hook(&[], create_payload("s3", &fixture.repo));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
}

/// `simgit run <branch>` running a command that blocks until a release file
/// appears, so the worktree stays run-locked exactly as long as the test
/// needs. The marker files live outside the worktree: inside it they would be
/// uncommitted changes, and the hook would refuse for that reason instead. The
/// script gives up after a minute on its own, and dropping the value, as a
/// panicking test does, releases and reaps the command.
struct LockedRun {
    runner: std::process::Child,
    path: PathBuf,
    release: PathBuf,
}

impl LockedRun {
    /// Returns once the lock is observed and the command is running.
    fn start(fixture: &HookFixture, branch: &str) -> Self {
        let started = fixture
            .root
            .join(format!("{}.started", branch.replace('/', "-")));
        let release = fixture
            .root
            .join(format!("{}.release", branch.replace('/', "-")));
        let runner = isolated(env!("CARGO_BIN_EXE_simgit"))
            .current_dir(&fixture.repo)
            .env("SIMGIT_POPULATE", "checkout")
            .args(["run", branch, "--", "sh", "-c"])
            .arg(
                "touch \"$1\"; n=0; \
                 while [ ! -f \"$2\" ] && [ \"$n\" -lt 1200 ]; do sleep 0.05; n=$((n+1)); done",
            )
            .arg("sh")
            .arg(&started)
            .arg(&release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn simgit run");
        let mut locked = None;
        for _ in 0..400 {
            let listing = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
            let wanted = format!("branch refs/heads/{branch}");
            if listing.contains("locked") && listing.contains(&wanted) {
                locked = listing
                    .lines()
                    .rev()
                    .skip_while(|line| *line != wanted)
                    .find_map(|line| line.strip_prefix("worktree ").map(PathBuf::from));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let run = Self {
            runner,
            path: locked.expect("run created a locked worktree"),
            release,
        };
        for _ in 0..400 {
            if started.exists() {
                return run;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("the command never started in {}", run.path.display());
    }

    /// Let the command exit and wait for `simgit run` to release the lock.
    fn finish(&mut self) {
        fs::write(&self.release, "").expect("write release");
        self.runner.wait().expect("wait for simgit run");
    }
}

impl Drop for LockedRun {
    fn drop(&mut self) {
        // Give the command time to see the release file before the fixture
        // deletes it; only then kill a launcher that is stuck.
        let _ = fs::write(&self.release, "");
        for _ in 0..100 {
            if matches!(self.runner.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = self.runner.kill();
        let _ = self.runner.wait();
    }
}

#[test]
fn create_refuses_a_run_locked_worktree() {
    let fixture = HookFixture::new();
    let mut run = LockedRun::start(&fixture, "simgit/s4");
    let output = fixture.hook(&[], create_payload("s4", &fixture.repo));
    run.finish();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
}

/// A pid that named a process which has exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id();
    child.wait().expect("wait for true");
    pid
}

/// The main checkout as simgit names it, symlinks resolved.
fn main_checkout(fixture: &HookFixture) -> PathBuf {
    fixture
        .repo
        .canonicalize()
        .expect("resolve the main checkout")
}

/// `simgit <arguments>` as a recovery hint spells it, to be run from anywhere.
fn hint(fixture: &HookFixture, arguments: &str) -> String {
    format!(
        "cd {} && simgit {arguments}",
        main_checkout(fixture).display()
    )
}

/// Run a recovery hint the way a person pastes it into a shell: from a
/// directory in no repository, with this build's simgit first on `PATH`.
fn run_hint(fixture: &HookFixture, hint: &str) -> Output {
    let bin = Path::new(env!("CARGO_BIN_EXE_simgit"))
        .parent()
        .expect("bin dir")
        .to_path_buf();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    )))
    .expect("join PATH");
    isolated("sh")
        .arg("-c")
        .arg(hint)
        .current_dir(&fixture.root)
        .env("PATH", path)
        .env("SIMGIT_POPULATE", "checkout")
        .output()
        .expect("run the hint")
}

/// Leave `simgit/<name>` registered at its hook path as an allocation killed
/// right after `git worktree add` leaves it: locked by a dead pid, with no
/// index and no files. Returns the path and the pid.
fn kill_an_allocation(fixture: &HookFixture, name: &str) -> (PathBuf, u32) {
    let path = create_worktree(fixture, name);
    let removed = fixture.hook(&[], remove_payload(&path));
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    let dead = dead_pid();
    let reason = format!("simgit: allocating (pid {dead})");
    let branch = format!("simgit/{name}");
    let path_arg = path.to_str().expect("utf-8 path");
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
            &branch,
            path_arg,
        ],
    );
    (path, dead)
}

/// An allocation killed after `git worktree add` leaves its registration at
/// the name's path, locked with the dead allocator's pid, with or without its
/// directory. The create names that, and the command that releases it.
#[test]
fn create_names_the_unlock_for_a_killed_allocation() {
    let fixture = HookFixture::new();
    for (name, delete) in [("k1", false), ("k2", true)] {
        let (path, dead) = kill_an_allocation(&fixture, name);
        if delete {
            fs::remove_dir_all(&path).expect("delete");
        }

        let output = fixture.hook(&[], create_payload(name, &fixture.repo));
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
        let complaint = stderr(&output);
        assert_eq!(complaint.lines().count(), 1, "{complaint}");
        let unlock = hint(&fixture, &format!("unlock {}", path.display()));
        assert!(
            complaint.contains(&format!("no longer running (pid {dead})"))
                && complaint.contains(&unlock),
            "{complaint}"
        );
        assert!(registered(&fixture.repo, &path));
    }
}

/// Once its lock is cleared, a killed allocation's worktree is still half
/// made: no index, every tracked file showing as deleted. The create refuses
/// to hand it out, removal never offers to commit that, and the named
/// recovery, run from outside the repository, lets the next create succeed.
#[test]
fn create_refuses_a_half_made_worktree_until_it_is_removed() {
    let fixture = HookFixture::new();
    let (path, _) = kill_an_allocation(&fixture, "h1");
    let unlock = hint(&fixture, &format!("unlock {}", path.display()));
    let discard = hint(
        &fixture,
        &format!("remove {} --discard-dirty", path.display()),
    );
    let locked = fixture.hook(&[], create_payload("h1", &fixture.repo));
    assert_eq!(locked.status.code(), Some(1), "{}", stderr(&locked));
    for step in [&unlock, &discard] {
        assert!(
            stderr(&locked).contains(step.as_str()),
            "{}",
            stderr(&locked)
        );
    }
    let unlocked = run_hint(&fixture, &unlock);
    assert!(unlocked.status.success(), "{}", stderr(&unlocked));

    let refused = fixture.hook(&[], create_payload("h1", &fixture.repo));
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(refused.stdout.is_empty());
    let complaint = stderr(&refused);
    assert_eq!(complaint.lines().count(), 1, "{complaint}");
    assert!(complaint.contains(&discard), "{complaint}");

    let kept = fixture.hook(&[], remove_payload(&path));
    assert_eq!(kept.status.code(), Some(1), "{}", stderr(&kept));
    assert!(stderr(&kept).contains(&discard), "{}", stderr(&kept));
    assert!(!stderr(&kept).contains("--commit"), "{}", stderr(&kept));

    let removed = run_hint(&fixture, &discard);
    assert!(removed.status.success(), "{}", stderr(&removed));
    let again = fixture.hook(&[], create_payload("h1", &fixture.repo));
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert_eq!(stdout_path(&again), path);
    assert_eq!(
        fs::read_to_string(path.join("README.md")).expect("populated"),
        "base\n"
    );
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
}

#[test]
fn invalid_names_fail_cleanly() {
    let fixture = HookFixture::new();
    for name in ["a b", "x..y", "z.lock"] {
        let output = fixture.hook(&[], create_payload(name, &fixture.repo));
        assert_eq!(output.status.code(), Some(1), "{name}: {}", stderr(&output));
        assert!(output.stdout.is_empty(), "{name}");
    }
    assert_eq!(simgit_refs(&fixture.repo), "");
    assert!(!fixture.root.join(".simgit").exists());
}

#[test]
fn base_is_origin_default_branch_else_project_head() {
    let fixture = HookFixture::new();
    // No origin: the base is HEAD.
    let output = fixture.hook(&[], create_payload("local", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        git(&stdout_path(&output), &["rev-parse", "HEAD"]),
        git(&fixture.repo, &["rev-parse", "HEAD"])
    );

    // With origin, whose default branch is ahead of the clone's local one.
    let clone = fixture.root.join("clone");
    git(
        &fixture.root,
        &["clone", "-q", "repo", clone.to_str().expect("utf8")],
    );
    fs::write(fixture.repo.join("ahead.txt"), "ahead\n").expect("write");
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-q", "-m", "ahead"]);
    git(&clone, &["fetch", "-q", "origin"]);
    let origin_head = git(&clone, &["rev-parse", "refs/remotes/origin/HEAD"]);
    assert_ne!(origin_head, git(&clone, &["rev-parse", "HEAD"]));
    let output = fixture.run_in(
        &clone,
        &[],
        &[],
        &create_payload("remote", &clone).to_string(),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        git(&stdout_path(&output), &["rev-parse", "HEAD"]),
        origin_head
    );

    // A project that is itself a linked worktree: its HEAD, not the main
    // checkout's.
    let linked = fixture.root.join("linked");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().expect("utf8"),
        ],
    );
    fs::write(linked.join("linked.txt"), "linked\n").expect("write");
    git(&linked, &["add", "."]);
    git(&linked, &["commit", "-q", "-m", "linked"]);
    let linked_head = git(&linked, &["rev-parse", "HEAD"]);
    assert_ne!(linked_head, git(&fixture.repo, &["rev-parse", "HEAD"]));
    let output = fixture.run_in(
        &linked,
        &[],
        &[],
        &create_payload("from-linked", &linked).to_string(),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        git(&stdout_path(&output), &["rev-parse", "HEAD"]),
        linked_head
    );
}

#[test]
fn create_uses_project_dir_not_payload_cwd() {
    let fixture = HookFixture::new();
    let other = fixture.root.join("other");
    fs::create_dir_all(other.join("sub")).expect("create other repository");
    git(&other, &["init", "-q"]);
    let output = fixture.hook(&[], create_payload("s5", &other.join("sub")));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let path = stdout_path(&output);
    assert_eq!(simgit_refs(&other), "");
    assert_eq!(branch_of(&path), "simgit/s5");
    assert!(git(&fixture.repo, &["worktree", "list", "--porcelain"]).contains("simgit/s5"));
}

/// simgit routinely deletes the directory it was started from, so no `git` it
/// spawns may depend on the working directory it inherited. The hook finds its
/// repository without the process cwd, so it can run from one already gone.
#[test]
fn create_and_remove_work_from_a_deleted_cwd() {
    let fixture = HookFixture::new();
    let gone = fixture.root.join("gone");
    let run_from_gone = |payload: Value| {
        fs::create_dir(&gone).expect("create the doomed cwd");
        let mut child = isolated("sh")
            .current_dir(&gone)
            .env("CLAUDE_PROJECT_DIR", &fixture.repo)
            .args([
                "-c",
                r#"rmdir "$1" && exec "$0" hook claude-code --requires 0.0.0"#,
            ])
            .arg(env!("CARGO_BIN_EXE_simgit"))
            .arg(&gone)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn simgit");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(payload.to_string().as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait for simgit")
    };

    let created = run_from_gone(create_payload("gone-cwd", &fixture.repo));
    assert_eq!(created.status.code(), Some(0), "{}", stderr(&created));
    let path = stdout_path(&created);
    assert!(path.join("README.md").is_file(), "{}", path.display());
    assert_eq!(branch_of(&path), "simgit/gone-cwd");

    let removed = run_from_gone(remove_payload(&path));
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!path.exists(), "{}", path.display());
}

#[test]
fn created_worktree_is_persistent() {
    let fixture = HookFixture::new();
    let output = fixture.hook(&[], create_payload("s6", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let path = stdout_path(&output);
    let listing = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .args(["--json", "list"])
        .output()
        .expect("run simgit list");
    let entries: Value = serde_json::from_slice(&listing.stdout).expect("list json");
    let entry = entries
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["branch"] == "refs/heads/simgit/s6")
        .expect("worktree listed");
    assert_eq!(entry["ephemeral"], json!(false));
    assert!(path.is_dir());
}

/// Add a bare `origin` holding `main` plus a pull request head and a merge
/// request head, each a commit that is not on `main`. Returns the origin path.
fn add_origin_with_changes(fixture: &HookFixture) -> PathBuf {
    let origin = fixture.root.join("origin.git");
    git(
        &fixture.root,
        &["init", "-q", "--bare", origin.to_str().expect("utf8")],
    );
    git(
        &fixture.repo,
        &["remote", "add", "origin", origin.to_str().expect("utf8")],
    );
    git(
        &fixture.repo,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    for (file, remote_ref) in [
        ("pr.txt", "refs/pull/7/head"),
        ("mr.txt", "refs/merge-requests/8/head"),
    ] {
        push_change(fixture, file, remote_ref);
    }
    origin
}

/// Commit a new file on a detached HEAD and push that commit to `remote_ref`.
fn push_change(fixture: &HookFixture, file: &str, remote_ref: &str) -> String {
    let main = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["checkout", "-q", "--detach", main.trim()]);
    fs::write(fixture.repo.join(file), format!("{remote_ref}\n")).expect("write");
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-q", "-m", remote_ref]);
    let sha = git(&fixture.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    git(
        &fixture.repo,
        &["push", "-q", "-f", "origin", &format!("{sha}:{remote_ref}")],
    );
    git(&fixture.repo, &["checkout", "-q", "--detach", main.trim()]);
    sha
}

fn origin_ref(origin: &Path, name: &str) -> String {
    git(origin, &["rev-parse", name]).trim().to_owned()
}

#[test]
fn pr_name_creates_at_the_pull_request_head() {
    let fixture = HookFixture::new();
    let origin = add_origin_with_changes(&fixture);
    let output = fixture.hook(&[], create_payload("pr-7", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let path = stdout_path(&output);
    assert_eq!(
        git(&path, &["rev-parse", "HEAD"]).trim(),
        origin_ref(&origin, "refs/pull/7/head")
    );
    assert_eq!(branch_of(&path), "simgit/pr-7");
}

#[test]
fn pr_name_falls_back_to_merge_requests() {
    let fixture = HookFixture::new();
    let origin = add_origin_with_changes(&fixture);
    let output = fixture.hook(&[], create_payload("pr-8", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        git(&stdout_path(&output), &["rev-parse", "HEAD"]).trim(),
        origin_ref(&origin, "refs/merge-requests/8/head")
    );
}

#[test]
fn pr_name_fails_when_neither_ref_exists() {
    let fixture = HookFixture::new();
    add_origin_with_changes(&fixture);
    let before = fixture.snapshot();
    let output = fixture.hook(&[], create_payload("pr-9", &fixture.repo));
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        stderr(&output).contains(
            "pull request 9: neither pull/9/head nor merge-requests/9/head could be fetched from origin"
        ),
        "{}",
        stderr(&output)
    );
    assert_eq!(simgit_refs(&fixture.repo), "");
    assert_eq!(fixture.snapshot(), before);
}

/// Only a pull ref missing on the remote sends the fetch on to the merge
/// request ref; any other failure is the answer, reported as itself.
#[test]
fn pr_fetch_failure_is_reported_as_itself() {
    let fixture = HookFixture::new();
    let missing = fixture.root.join("missing.git");
    git(
        &fixture.repo,
        &["remote", "add", "origin", missing.to_str().expect("utf8")],
    );
    let output = fixture.hook(&[], create_payload("pr-7", &fixture.repo));
    assert_eq!(output.status.code(), Some(1));
    let complaint = stderr(&output);
    assert!(
        complaint.contains("pull request 7")
            && complaint.contains("does not appear to be a git repository")
            && !complaint.contains("neither"),
        "{complaint}"
    );
}

/// Run the hook on `payload` with a new pseudo-terminal as its controlling
/// terminal, as when Claude Code runs in one. A run still going after a
/// minute is killed and fails the test.
fn hook_on_a_terminal(
    fixture: &HookFixture,
    payload: &Value,
    env: &[(&str, &std::ffi::OsStr)],
) -> Output {
    use std::ffi::{c_char, c_int, c_ulong, CStr};
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn posix_openpt(flags: c_int) -> c_int;
        fn grantpt(fd: c_int) -> c_int;
        fn unlockpt(fd: c_int) -> c_int;
        fn ptsname(fd: c_int) -> *mut c_char;
        fn setsid() -> c_int;
        fn open(path: *const c_char, flags: c_int, ...) -> c_int;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn close(fd: c_int) -> c_int;
    }
    const O_RDWR: c_int = 2;
    #[cfg(target_os = "macos")]
    const O_NOCTTY: c_int = 0x20000;
    #[cfg(target_os = "linux")]
    const O_NOCTTY: c_int = 0o400;
    #[cfg(target_os = "macos")]
    const TIOCSCTTY: c_ulong = 0x2000_7461;
    #[cfg(target_os = "linux")]
    const TIOCSCTTY: c_ulong = 0x540E;

    // SAFETY: plain calls on a descriptor this function owns; `ptsname`'s
    // string is copied before anything else can reuse its buffer.
    let (master, terminal) = unsafe {
        let master = posix_openpt(O_RDWR | O_NOCTTY);
        assert!(master >= 0, "posix_openpt");
        assert_eq!(grantpt(master), 0, "grantpt");
        assert_eq!(unlockpt(master), 0, "unlockpt");
        let name = ptsname(master);
        assert!(!name.is_null(), "ptsname");
        (master, CStr::from_ptr(name).to_owned())
    };
    let mut command = isolated(env!("CARGO_BIN_EXE_simgit"));
    command
        .current_dir(&fixture.root)
        .envs(env.iter().copied())
        .env("CLAUDE_PROJECT_DIR", &fixture.repo)
        .env("SIMGIT_POPULATE", "checkout")
        .args(["hook", "claude-code", "--requires", "0.0.0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: between fork and exec the closure only makes async-signal-safe
    // calls on the name copied above, and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = open(terminal.as_ptr(), O_RDWR | O_NOCTTY);
            // Left open: on macOS the terminal's last close ends its role as
            // the controlling terminal.
            if fd == -1 || ioctl(fd, TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn simgit");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write stdin");
    let started = std::time::Instant::now();
    while child.try_wait().expect("poll simgit").is_none() {
        if started.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the hook was still running after a minute");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("wait for simgit");
    // SAFETY: `master` is this function's descriptor, closed once.
    unsafe { close(master) };
    output
}

/// An ssh that can reach a terminal prompts there and waits for an answer
/// nobody gives, since nobody reads the hook's terminal. The fetch runs with
/// no controlling terminal, so such a prompt fails at once.
#[test]
fn pr_fetch_has_no_terminal_for_ssh_to_prompt_on() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = HookFixture::new();
    git(
        &fixture.repo,
        &["remote", "add", "origin", "ssh://simgit.invalid/origin.git"],
    );
    // Stands in for ssh asking for a passphrase: it records whether it could
    // open the terminal to ask on, then fails as an unanswered prompt would.
    let opened = fixture.root.join("opened-the-terminal");
    let ssh = fixture.root.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n\
             if (: </dev/tty) 2>/dev/null; then : > '{}'; fi\n\
             exit 255\n",
            opened.display()
        ),
    )
    .expect("write ssh");
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).expect("chmod");
    let before = fixture.snapshot();

    let output = hook_on_a_terminal(
        &fixture,
        &create_payload("pr-7", &fixture.repo),
        &[
            ("GIT_SSH_COMMAND", ssh.as_os_str()),
            ("GIT_SSH_VARIANT", "simple".as_ref()),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(
        stderr(&output).contains("pull request 7"),
        "{}",
        stderr(&output)
    );
    assert!(
        !opened.exists(),
        "the fetch's ssh could open the terminal to prompt on"
    );
    assert_eq!(fixture.snapshot(), before);
}

/// `git fetch`'s connectivity check reads every worktree's HEAD, so it dies
/// on a registration another allocation is half-way through writing. The
/// wrapper fails the first fetch that way; the create must still succeed.
#[test]
fn pr_fetch_is_retried_after_a_half_written_registration() {
    assert_pr_fetch_is_retried(
        "fatal: failed to read ${1#--git-dir=}/worktrees/other/commondir: Undefined error: 0",
    );
}

/// `git worktree add` writes a placeholder HEAD of zeros before the real one,
/// and a fetch that reads it then dies on a "bad object".
#[test]
fn pr_fetch_is_retried_after_a_placeholder_head() {
    assert_pr_fetch_is_retried("fatal: bad object worktrees/other/HEAD");
}

/// Fail the first `git fetch` with `complaint`; the create must still succeed.
fn assert_pr_fetch_is_retried(complaint: &str) {
    use std::os::unix::fs::PermissionsExt;

    let fixture = HookFixture::new();
    let origin = add_origin_with_changes(&fixture);
    let real_git = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).expect("mkdir");
    let failed = fixture.root.join("fetch-failed");
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             if [ \"$2\" = fetch ] && [ ! -e '{failed}' ]; then\n\
               : > '{failed}'\n\
               echo \"{complaint}\" >&2\n\
               exit 128\n\
             fi\n\
             exec '{real}' \"$@\"\n",
            failed = failed.display(),
            real = real_git.display(),
        ),
    )
    .expect("write wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = std::env::join_paths(std::iter::once(wrapper_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    )))
    .expect("join PATH");

    let output = fixture.run_with_env(
        &fixture.repo,
        &[],
        &[],
        &create_payload("pr-7", &fixture.repo).to_string(),
        &[("PATH", path.as_os_str())],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(failed.exists(), "the wrapper never failed a fetch");
    assert_eq!(
        git(&stdout_path(&output), &["rev-parse", "HEAD"]).trim(),
        origin_ref(&origin, "refs/pull/7/head")
    );
}

/// A create killed between its fetch and the ref's deletion strands a
/// temporary ref that names its process. `prune` deletes it once that process
/// is gone, and keeps a running process's and one that names none.
#[test]
fn prune_deletes_the_fetch_refs_of_a_killed_create() {
    let fixture = HookFixture::new();
    add_origin_with_changes(&fixture);
    let env = git_wrapper(
        &fixture,
        "case \"$2 $4\" in \"rev-parse refs/simgit/fetch/\"*) kill -9 \"$PPID\"; exit 1;; esac\n",
    );
    let output = hook_with_env(&fixture, &[], &create_payload("pr-7", &fixture.repo), &env);
    assert_eq!(output.status.code(), None, "{}", stderr(&output));
    let refs = || {
        git(
            &fixture.repo,
            &["for-each-ref", "--format=%(refname)", "refs/simgit/fetch/"],
        )
    };
    let stranded = refs();
    let (pid, uuid) = stranded
        .trim()
        .strip_prefix("refs/simgit/fetch/")
        .and_then(|name| name.split_once('-'))
        .unwrap_or_else(|| panic!("not one <pid>-<uuid> ref: {stranded:?}"));
    assert!(pid.parse::<u32>().is_ok(), "{stranded}");
    assert!(uuid::Uuid::parse_str(uuid).is_ok(), "{stranded}");

    let live = format!(
        "refs/simgit/fetch/{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    let unowned = format!("refs/simgit/fetch/{}", uuid::Uuid::new_v4());
    // Names simgit never writes, each naming the dead pid: never taken as its.
    let id = uuid::Uuid::new_v4();
    let malformed = [
        format!("refs/simgit/fetch/+{pid}-{id}"),
        format!("refs/simgit/fetch/0{pid}-{id}"),
        format!("refs/simgit/fetch/{pid}.{id}"),
        format!("refs/simgit/fetch/{pid}-{}", id.simple()),
    ];
    for name in malformed.iter().chain([&live, &unowned]) {
        git(&fixture.repo, &["update-ref", name, "HEAD"]);
    }
    let pruned = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .arg("prune")
        .output()
        .expect("run simgit prune");
    assert!(pruned.status.success(), "{}", stderr(&pruned));
    let mut kept: Vec<String> = refs().lines().map(str::to_owned).collect();
    kept.sort();
    let mut expected = vec![live, unowned];
    expected.extend(malformed);
    expected.sort();
    assert_eq!(kept, expected);
}

#[test]
fn pr_worktree_is_not_refetched_on_reuse() {
    let fixture = HookFixture::new();
    let origin = add_origin_with_changes(&fixture);
    let first = fixture.hook(&[], create_payload("pr-7", &fixture.repo));
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let head = git(&stdout_path(&first), &["rev-parse", "HEAD"]);

    let moved = push_change(&fixture, "moved.txt", "refs/pull/7/head");
    assert_ne!(moved, head.trim());
    assert_eq!(origin_ref(&origin, "refs/pull/7/head"), moved);

    let second = fixture.hook(&[], create_payload("pr-7", &fixture.repo));
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(stdout_path(&second), stdout_path(&first));
    assert_eq!(git(&stdout_path(&second), &["rev-parse", "HEAD"]), head);
}

#[test]
fn plain_names_never_fetch() {
    let fixture = HookFixture::new();
    add_origin_with_changes(&fixture);
    let fetch_head = fixture.repo.join(".git/FETCH_HEAD");
    let before = fs::read(&fetch_head).ok();
    for name in ["feature-7", "pr-seven", "pr-", "pr-7x"] {
        let output = fixture.hook(&[], create_payload(name, &fixture.repo));
        assert_eq!(output.status.code(), Some(0), "{name}: {}", stderr(&output));
    }
    assert_eq!(fs::read(&fetch_head).ok(), before);
    assert_eq!(
        git(&fixture.repo, &["for-each-ref", "refs/simgit/fetch"]),
        ""
    );
}

#[test]
fn concurrent_pr_creates_each_get_their_own_head() {
    let fixture = HookFixture::new();
    let origin = add_origin_with_changes(&fixture);
    let numbers: Vec<u32> = (11..19).collect();
    for number in &numbers {
        push_change(
            &fixture,
            &format!("pr{number}.txt"),
            &format!("refs/pull/{number}/head"),
        );
    }
    let fixture = &fixture;
    std::thread::scope(|scope| {
        let handles: Vec<_> = numbers
            .iter()
            .map(|number| {
                scope.spawn(move || {
                    let name = format!("pr-{number}");
                    (
                        *number,
                        fixture.hook(&[], create_payload(&name, &fixture.repo)),
                    )
                })
            })
            .collect();
        for handle in handles {
            let (number, output) = handle.join().expect("join");
            assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
            assert_eq!(
                git(&stdout_path(&output), &["rev-parse", "HEAD"]).trim(),
                origin_ref(&origin, &format!("refs/pull/{number}/head")),
                "pr-{number}"
            );
        }
    });
    assert_eq!(
        git(&fixture.repo, &["for-each-ref", "refs/simgit/fetch"]),
        ""
    );
}

#[test]
fn oversized_pr_number_is_not_replaced_by_the_base() {
    let fixture = HookFixture::new();
    add_origin_with_changes(&fixture);
    let before = fixture.snapshot();
    let output = fixture.hook(
        &[],
        create_payload("pr-99999999999999999999", &fixture.repo),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        stderr(&output).contains("pull request 99999999999999999999: neither pull/99999999999999999999/head nor merge-requests/99999999999999999999/head could be fetched from origin"),
        "{}",
        stderr(&output)
    );
    assert_eq!(simgit_refs(&fixture.repo), "");
    assert_eq!(fixture.snapshot(), before);
}

fn remove_payload(path: &Path) -> Value {
    json!({ "hook_event_name": "WorktreeRemove", "worktree_path": path, "cwd": path })
}

/// Create `simgit/<name>` through the hook and return its path.
fn create_worktree(fixture: &HookFixture, name: &str) -> PathBuf {
    let output = fixture.hook(&[], create_payload(name, &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    stdout_path(&output)
}

/// A deleted worktree whose sparse checkout is not in cone mode cannot be
/// recreated, and a hook has no `--sparse` to pass: the refusal names a command
/// that works from a shell, on one line.
#[test]
fn create_refuses_a_non_cone_sparse_worktree_deleted_by_hand_with_a_hook_recovery() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "sparse");
    git(&path, &["sparse-checkout", "set", "--no-cone", "README.md"]);
    fs::remove_dir_all(&path).expect("delete by hand");

    let output = fixture.hook(&[], create_payload("sparse", &fixture.repo));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let note = stderr(&output);
    assert_eq!(note.lines().count(), 1, "{note}");
    assert!(!note.contains("pass --sparse"), "{note}");
    assert!(
        note.contains(&format!("&& simgit remove {}", path.display())),
        "{note}"
    );
    assert!(registered(&fixture.repo, &path));

    // The named recovery works, and the name is then free to create again.
    let removed = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .arg("remove")
        .arg(&path)
        .output()
        .expect("run simgit remove");
    assert!(removed.status.success(), "{}", stderr(&removed));
    create_worktree(&fixture, "sparse");
}

fn commit_in(worktree: &Path, file: &str) {
    fs::write(worktree.join(file), "work\n").expect("write");
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-q", "-m", file]);
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    !git(repo, &["branch", "--list", branch]).is_empty()
}

#[test]
fn remove_refuses_a_worktree_not_on_a_simgit_branch() {
    let fixture = HookFixture::new();
    let output = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .env("SIMGIT_POPULATE", "checkout")
        .args(["--json", "add", "other/x"])
        .output()
        .expect("simgit add");
    assert!(output.status.success(), "{}", stderr(&output));
    let added: Value = serde_json::from_slice(&output.stdout).expect("json");
    let path = PathBuf::from(added["path"].as_str().expect("path"));
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not on a simgit/ branch"),
        "{}",
        stderr(&output)
    );
    assert!(path.is_dir());
    assert!(branch_exists(&fixture.repo, "other/x"));
}

/// A detached worktree is the hook's only at a `simgit/<name>` allocation
/// path; one anywhere else is left alone, its HEAD reached or not.
#[test]
fn remove_refuses_a_detached_worktree_not_at_a_hook_path() {
    let fixture = HookFixture::new();
    let output = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .env("SIMGIT_POPULATE", "checkout")
        .args(["--json", "add", "--detach"])
        .output()
        .expect("simgit add");
    assert!(output.status.success(), "{}", stderr(&output));
    let added: Value = serde_json::from_slice(&output.stdout).expect("json");
    let path = PathBuf::from(added["path"].as_str().expect("path"));
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not on a simgit/ branch"),
        "{}",
        stderr(&output)
    );
    assert!(path.join("README.md").is_file());
    assert!(registered(&fixture.repo, &path));
}

#[test]
fn remove_refuses_the_main_checkout() {
    let fixture = HookFixture::new();
    git(&fixture.repo, &["checkout", "-q", "-b", "simgit/m"]);
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&fixture.repo));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("main checkout"),
        "{}",
        stderr(&output)
    );
    assert!(fixture.repo.join("README.md").is_file());
}

/// The hook's worktree is the one at its allocation path, detached or not; a
/// detached HEAD a ref reaches loses nothing when it goes.
#[test]
fn remove_removes_a_detached_worktree_whose_commit_is_reached() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "d1");
    git(&path, &["checkout", "-q", "--detach"]);
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!path.exists());
    assert!(!registered(&fixture.repo, &path));
    assert!(!branch_exists(&fixture.repo, "simgit/d1"));
}

/// A1/A2: commits on a detached HEAD that no ref reaches exist only in the
/// worktree, so removal keeps it, with or without its directory and even with
/// `--discard-dirty`, and names the branch command that keeps the work.
#[test]
fn detached_hook_worktree_with_unreached_commits_is_kept() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "d2");
    git(&path, &["switch", "-q", "--detach"]);
    commit_in(&path, "work.txt");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let keep = format!("git -C {} branch <name>", path.display());
    let then = format!(
        "then remove it with `{}`",
        hint(&fixture, &format!("remove {}", path.display()))
    );
    assert!(stderr(&output).contains(&keep), "{}", stderr(&output));
    assert!(stderr(&output).contains(&then), "{}", stderr(&output));
    assert!(path.join("work.txt").is_file());

    fs::remove_dir_all(&path).expect("delete by hand");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    // The gitfile is gone, so the commit is named from the main checkout, and
    // the step after it is one that works outside Claude Code.
    let then = format!(
        "then remove it with `{}`",
        hint(&fixture, &format!("remove {}", path.display()))
    );
    assert!(
        stderr(&output).contains("branch <name>"),
        "{}",
        stderr(&output)
    );
    assert!(stderr(&output).contains(&then), "{}", stderr(&output));
    assert!(registered(&fixture.repo, &path));
}

/// The hook's worktree is the one at its allocation path: a branch switched
/// inside it must not make the next create allocate a second worktree.
#[test]
fn create_reuses_a_worktree_whose_branch_was_switched() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "sw1");
    for (switch, branch) in [
        (&["switch", "-q", "-c", "other"][..], "other"),
        (&["switch", "-q", "--detach"][..], "HEAD"),
    ] {
        git(&path, switch);
        let output = fixture.hook(&[], create_payload("sw1", &fixture.repo));
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stdout_path(&output), path);
        assert_eq!(branch_of(&path), branch);
    }
    let listing = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
    let worktrees = listing
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count();
    assert_eq!(worktrees, 2, "{listing}");
}

/// Removal takes a worktree whose branch was switched or renamed, and leaves
/// the branch it is on now, which is not simgit's, with the commit only it
/// reaches. A `/` in the name is not spelled out by the directory name.
#[test]
fn remove_removes_a_switched_worktree_and_keeps_its_branch() {
    let fixture = HookFixture::new();
    for (name, switch, branch) in [
        ("sw2", &["switch", "-q", "-c", "other"][..], "other"),
        (
            "nested/sw3",
            &["switch", "-q", "-c", "nested"][..],
            "nested",
        ),
        ("sw4", &["branch", "-m", "renamed"][..], "renamed"),
    ] {
        let path = create_worktree(&fixture, name);
        git(&path, switch);
        commit_in(&path, "work.txt");
        let tip = git(&path, &["rev-parse", "HEAD"]);
        let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
        assert_eq!(output.status.code(), Some(0), "{name}: {}", stderr(&output));
        assert!(!path.exists(), "{name}");
        assert!(!registered(&fixture.repo, &path), "{name}");
        assert_eq!(git(&fixture.repo, &["rev-parse", branch]), tip, "{name}");
        // Left at the base, which the main checkout reaches.
        let simgit = format!("simgit/{name}");
        assert!(!branch_exists(&fixture.repo, &simgit), "{name}");
    }
}

#[test]
fn remove_with_discard_dirty_removes_a_dirty_worktree() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r1");
    fs::write(path.join("scratch.txt"), "x\n").expect("write");
    let mut payload = remove_payload(&path);
    payload["prompt_id"] = json!("whatever");
    payload["cwd"] = json!("/nonexistent");
    let output = fixture.hook(&["--discard-dirty"], payload);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(!path.exists());
}

#[test]
fn remove_without_discard_dirty_refuses_a_dirty_worktree() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r2");
    fs::write(path.join("scratch.txt"), "x\n").expect("write");
    let output = fixture.hook(&[], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("uncommitted changes"),
        "{}",
        stderr(&output)
    );
    // The hook takes no --commit: the fix is a command a person can run,
    // from wherever they are.
    let fix = |flag: &str| hint(&fixture, &format!("remove {} {flag}", path.display()));
    for flag in ["--commit", "--discard-dirty"] {
        let command = format!("`{}`", fix(flag));
        assert!(stderr(&output).contains(&command), "{}", stderr(&output));
    }
    assert!(path.join("scratch.txt").is_file());
    assert!(branch_exists(&fixture.repo, "simgit/r2"));

    let kept = run_hint(&fixture, &fix("--commit"));
    assert!(kept.status.success(), "{}", stderr(&kept));
    assert!(!path.exists());
    assert_eq!(
        git(&fixture.repo, &["show", "simgit/r2:scratch.txt"]),
        "x\n"
    );
}

/// A detached worktree's changes need a branch before a commit can keep them;
/// the refusal says so, then names the removal that works from a shell.
#[test]
fn remove_refuses_a_dirty_detached_worktree_naming_a_branch_first() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r2d");
    git(&path, &["switch", "-q", "--detach"]);
    fs::write(path.join("scratch.txt"), "x\n").expect("write");
    let output = fixture.hook(&[], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let switch = format!("git -C {} switch -c <name>", path.display());
    let commit = format!(
        "`{}`",
        hint(&fixture, &format!("remove {} --commit", path.display()))
    );
    assert!(stderr(&output).contains(&switch), "{}", stderr(&output));
    assert!(stderr(&output).contains(&commit), "{}", stderr(&output));
    assert!(path.join("scratch.txt").is_file());
}

#[test]
fn remove_drops_a_branch_with_nothing_unreached() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r3");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!path.exists());
    assert!(!branch_exists(&fixture.repo, "simgit/r3"));
}

#[test]
fn remove_keeps_a_branch_with_an_unpushed_commit() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r4");
    commit_in(&path, "work.txt");
    let tip = git(&path, &["rev-parse", "HEAD"]).trim().to_owned();
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!path.exists());
    assert_eq!(git(&fixture.repo, &["rev-parse", "simgit/r4"]).trim(), tip);
}

#[test]
fn remove_drops_a_branch_whose_commits_origin_reaches() {
    let fixture = HookFixture::new();
    add_origin_with_changes(&fixture);
    let path = create_worktree(&fixture, "r5");
    commit_in(&path, "work.txt");
    git(
        &path,
        &["push", "-q", "origin", "HEAD:refs/heads/simgit/r5"],
    );
    git(&fixture.repo, &["fetch", "-q", "origin"]);
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!path.exists());
    assert!(!branch_exists(&fixture.repo, "simgit/r5"));
}

#[test]
fn remove_finds_the_repo_from_worktree_path() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r6");
    let other = std::env::temp_dir();
    let mut child = isolated(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&other)
        .env_remove("CLAUDE_PROJECT_DIR")
        .args([
            "hook",
            "claude-code",
            "--requires",
            "0.0.0",
            "--discard-dirty",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn simgit");
    let mut payload = remove_payload(&path);
    payload["cwd"] = json!(other);
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!path.exists());
    assert!(!branch_exists(&fixture.repo, "simgit/r6"));
}

#[test]
fn remove_refuses_a_run_locked_worktree() {
    let fixture = HookFixture::new();
    let mut run = LockedRun::start(&fixture, "simgit/r7");
    let path = run.path.clone();
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    run.finish();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("is locked"), "{}", stderr(&output));
    assert!(path.is_dir());
    assert!(branch_exists(&fixture.repo, "simgit/r7"));
}

#[test]
fn removing_an_absent_worktree_succeeds() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r8");
    for _ in 0..2 {
        let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
    }
    assert!(!path.exists());
}

#[test]
fn remove_refuses_a_directory_that_is_not_a_registered_worktree() {
    let fixture = HookFixture::new();
    let stray = fixture.repo.join("stray");
    fs::create_dir(&stray).expect("mkdir");
    fs::write(stray.join("keep.txt"), "x\n").expect("write");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&stray));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not a registered worktree"),
        "{}",
        stderr(&output)
    );
    assert!(stray.join("keep.txt").is_file());
}

/// Whether Git still has a worktree registered at `path`.
fn registered(repo: &Path, path: &Path) -> bool {
    let listing = git(repo, &["worktree", "list", "--porcelain"]);
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    listing.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|listed| Path::new(listed) == path)
    })
}

/// Put a `git` wrapper running `script` ahead of the real Git on `PATH`. The
/// script reaches the real Git as `$SIMGIT_TEST_REAL_GIT`; returns the
/// environment a hook needs to run under the wrapper.
fn git_wrapper(fixture: &HookFixture, script: &str) -> [(&'static str, std::ffi::OsString); 2] {
    use std::os::unix::fs::PermissionsExt;

    let real_git = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).expect("mkdir");
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\n{script}exec \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n"),
    )
    .expect("write wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = std::env::join_paths(std::iter::once(wrapper_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    )))
    .expect("join PATH");
    [("PATH", path), ("SIMGIT_TEST_REAL_GIT", real_git.into())]
}

fn hook_with_env(
    fixture: &HookFixture,
    extra: &[&str],
    payload: &Value,
    env: &[(&'static str, std::ffi::OsString)],
) -> Output {
    let env: Vec<(&str, &std::ffi::OsStr)> = env
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();
    fixture.run_with_env(&fixture.repo, &[], extra, &payload.to_string(), &env)
}

/// A worktree deleted by hand leaves Git's registration behind, which used to
/// make every later create for that name fail. The branch must come back at
/// its old tip in a working worktree.
#[test]
fn create_after_the_worktree_was_deleted_by_hand_reattaches_the_branch() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "gone");
    commit_in(&path, "work.txt");
    let tip = git(&fixture.repo, &["rev-parse", "simgit/gone"]);
    fs::remove_dir_all(&path).expect("delete by hand");

    let output = fixture.hook(&[], create_payload("gone", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let again = stdout_path(&output);
    assert_eq!(git(&fixture.repo, &["rev-parse", "simgit/gone"]), tip);
    assert_eq!(git(&again, &["rev-parse", "HEAD"]), tip);
    assert_eq!(branch_of(&again), "simgit/gone");
    assert!(again.join("work.txt").is_file());
    assert_eq!(git(&again, &["status", "--porcelain"]), "");
}

#[test]
fn remove_of_a_worktree_deleted_by_hand_prunes_it_and_drops_its_branch() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r10");
    fs::remove_dir_all(&path).expect("delete by hand");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!registered(&fixture.repo, &path));
    assert!(!branch_exists(&fixture.repo, "simgit/r10"));
}

/// Removing a hook worktree deleted by hand drops that one registration, not
/// every stale one in the repository: another worktree deleted by hand is not
/// the hook's to clean up.
#[test]
fn remove_of_a_deleted_worktree_drops_only_its_own_registration() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r13");
    let other = fixture.root.join("other");
    let other_arg = other.to_str().expect("utf-8 path");
    git(
        &fixture.repo,
        &["worktree", "add", "-q", "-b", "other", other_arg],
    );
    // Resolved while it exists, as Git registered it.
    let other = other.canonicalize().expect("resolve");
    fs::remove_dir_all(&path).expect("delete by hand");
    fs::remove_dir_all(&other).expect("delete by hand");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!registered(&fixture.repo, &path));
    assert!(!branch_exists(&fixture.repo, "simgit/r13"));
    assert!(registered(&fixture.repo, &other));
}

/// A lock is someone's claim on the registration, deleted directory or not:
/// removal keeps the registration and the branch, even with `--discard-dirty`.
#[test]
fn remove_of_a_locked_worktree_deleted_by_hand_keeps_it() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r14");
    let path_arg = path.to_str().expect("utf-8 path");
    git(
        &fixture.repo,
        &["worktree", "lock", "--reason", "on a USB disk", path_arg],
    );
    fs::remove_dir_all(&path).expect("delete by hand");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("is locked"), "{}", stderr(&output));
    assert!(registered(&fixture.repo, &path));
    assert!(branch_exists(&fixture.repo, "simgit/r14"));
}

/// A deleted worktree's submodule repositories live in its registration, and
/// a commit in one may exist nowhere else: the hook keeps both.
#[test]
fn remove_of_a_deleted_worktree_keeps_its_submodule_commits() {
    let fixture = HookFixture::new();
    let lib = fixture.root.join("lib");
    fs::create_dir_all(&lib).expect("mkdir");
    git(&lib, &["init", "-q"]);
    git(&lib, &["config", "user.email", "test@example.com"]);
    git(&lib, &["config", "user.name", "Test User"]);
    commit_in(&lib, "lib.txt");
    let lib = lib.to_str().expect("utf-8 path");
    let allow = ["-c", "protocol.file.allow=always"];
    git(
        &fixture.repo,
        &[&allow[..], &["submodule", "add", "-q", lib, "sub"]].concat(),
    );
    git(&fixture.repo, &["commit", "-q", "-m", "add submodule"]);
    let path = create_worktree(&fixture, "subs");
    git(
        &path,
        &[&allow[..], &["submodule", "update", "--init", "-q"]].concat(),
    );
    let sub = path.join("sub");
    git(&sub, &["config", "user.email", "test@example.com"]);
    git(&sub, &["config", "user.name", "Test User"]);
    commit_in(&sub, "precious.txt");
    let precious = git(&sub, &["rev-parse", "HEAD"]);
    fs::remove_dir_all(&path).expect("delete by hand");

    for extra in [&[][..], &["--discard-dirty"][..]] {
        let output = fixture.hook(extra, remove_payload(&path));
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(stderr(&output).contains("modules/"), "{}", stderr(&output));
        assert!(registered(&fixture.repo, &path), "{extra:?}");
        assert!(branch_exists(&fixture.repo, "simgit/subs"), "{extra:?}");
    }
    // The fixture's only registration.
    let admin = fs::read_dir(fixture.repo.join(".git/worktrees"))
        .expect("registry")
        .next()
        .expect("a registration")
        .expect("read registry")
        .path();
    let git_dir = format!("--git-dir={}", admin.join("modules/sub").display());
    // Its `core.worktree` names the deleted checkout.
    let work_tree = format!("--work-tree={}", fixture.repo.display());
    git(
        &fixture.repo,
        &[&git_dir, &work_tree, "cat-file", "-e", precious.trim()],
    );
}

/// Claude Code can fire one remove twice in parallel (the plugin plus the same
/// hook in a settings file). The wrapper delays the first `worktree remove`
/// so both hooks reach it, and the second until the first is done; the second
/// must find the work done, not fail.
#[test]
fn parallel_duplicate_removes_both_succeed() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r11");
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$2\" = worktree ] && [ \"$3\" = remove ]; then\n\
               if mkdir '{first}' 2>/dev/null; then sleep 0.3; else sleep 0.6; fi\n\
             fi\n",
            first = fixture.root.join("first-remove").display(),
        ),
    );
    let payload = remove_payload(&path);
    let outputs: Vec<Output> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| hook_with_env(&fixture, &["--discard-dirty"], &payload, &env)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    for output in &outputs {
        assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    }
    assert!(!path.exists());
    assert!(!branch_exists(&fixture.repo, "simgit/r11"));
}

/// The unreached check and the delete are separate Git commands. The wrapper
/// moves the branch to a new commit right after the check, as a commit landing
/// in between would; the delete must then leave the branch alone.
#[test]
fn remove_never_drops_a_branch_whose_tip_moved_after_the_check() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "r12");
    let tree = git(&fixture.repo, &["rev-parse", "HEAD^{tree}"]);
    let moved = git(
        &fixture.repo,
        &[
            "commit-tree",
            tree.trim(),
            "-p",
            "HEAD",
            "-m",
            "landed late",
        ],
    );
    let env = git_wrapper(
        &fixture,
        &format!(
            "if [ \"$2\" = rev-list ]; then\n\
               \"$SIMGIT_TEST_REAL_GIT\" \"$@\"; status=$?\n\
               \"$SIMGIT_TEST_REAL_GIT\" \"$1\" update-ref refs/heads/simgit/r12 {moved}\n\
               exit $status\n\
             fi\n",
            moved = moved.trim(),
        ),
    );
    let output = hook_with_env(&fixture, &["--discard-dirty"], &remove_payload(&path), &env);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!path.exists());
    assert_eq!(git(&fixture.repo, &["rev-parse", "simgit/r12"]), moved);
}

/// Every other test forces checkout mode; this one runs the default populate
/// mode (a copy-on-write clone on APFS) through create and remove.
#[test]
fn create_and_remove_in_the_default_populate_mode() {
    let fixture = HookFixture::new();
    let hook = |extra: &[&str], payload: Value| {
        let mut child = isolated(env!("CARGO_BIN_EXE_simgit"))
            .current_dir(&fixture.root)
            .env("CLAUDE_PROJECT_DIR", &fixture.repo)
            .env_remove("SIMGIT_POPULATE")
            .args(["hook", "claude-code", "--requires", "0.0.0"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn simgit");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(payload.to_string().as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait for simgit")
    };

    let created = hook(&[], create_payload("cow", &fixture.repo));
    assert_eq!(created.status.code(), Some(0), "{}", stderr(&created));
    #[cfg(target_os = "macos")]
    assert!(
        stderr(&created).contains("mode: cow-clone"),
        "{}",
        stderr(&created)
    );
    let path = stdout_path(&created);
    assert_eq!(branch_of(&path), "simgit/cow");
    assert_eq!(
        fs::read_to_string(path.join("README.md")).unwrap(),
        "base\n"
    );
    assert_eq!(git(&path, &["status", "--porcelain"]), "");

    let removed = hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!path.exists());
    assert!(!registered(&fixture.repo, &path));
    assert!(!branch_exists(&fixture.repo, "simgit/cow"));
}

fn stop_payload(agent_id: &str, cwd: &Path) -> Value {
    json!({ "hook_event_name": "SubagentStop", "agent_id": agent_id, "cwd": cwd })
}

/// Create the worktree Claude Code makes for subagent `id`.
fn create_subagent_worktree(fixture: &HookFixture, id: &str) -> PathBuf {
    create_worktree(fixture, &format!("agent-{id}"))
}

/// Every commit in the repository, so a test can show a hook committed nothing.
fn all_commits(repo: &Path) -> String {
    git(repo, &["rev-list", "--all"])
}

#[test]
fn stop_removes_a_clean_subagent_worktree_and_its_branch() {
    let fixture = HookFixture::new();
    let path = create_subagent_worktree(&fixture, "a1");
    let output = fixture.hook(&[], stop_payload("a1", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(!path.exists());
    assert!(!branch_exists(&fixture.repo, "simgit/agent-a1"));
}

#[test]
fn stop_keeps_a_dirty_subagent_worktree() {
    let fixture = HookFixture::new();
    let path = create_subagent_worktree(&fixture, "a2");
    fs::write(path.join("README.md"), "edited\n").expect("write");
    fs::write(path.join("untracked.txt"), "new\n").expect("write");
    let commits = all_commits(&fixture.repo);
    let output = fixture.hook(&[], stop_payload("a2", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(path.join("README.md")).unwrap(),
        "edited\n"
    );
    assert!(path.join("untracked.txt").is_file());
    assert!(branch_exists(&fixture.repo, "simgit/agent-a2"));
    assert_eq!(all_commits(&fixture.repo), commits);
}

#[test]
fn stop_keeps_a_worktree_with_an_unpushed_commit() {
    let fixture = HookFixture::new();
    let path = create_subagent_worktree(&fixture, "a3");
    commit_in(&path, "work.txt");
    let commits = all_commits(&fixture.repo);
    let output = fixture.hook(&[], stop_payload("a3", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(path.join("work.txt").is_file());
    assert!(branch_exists(&fixture.repo, "simgit/agent-a3"));
    assert_eq!(all_commits(&fixture.repo), commits);
}

#[test]
fn stop_keeps_a_run_locked_worktree() {
    let fixture = HookFixture::new();
    let mut run = LockedRun::start(&fixture, "simgit/agent-a4");
    let path = run.path.clone();
    let commits = all_commits(&fixture.repo);
    let output = fixture.hook(&[], stop_payload("a4", &fixture.repo));
    run.finish();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("is locked (a command may be running)"),
        "{}",
        stderr(&output)
    );
    assert!(path.is_dir());
    assert!(branch_exists(&fixture.repo, "simgit/agent-a4"));
    assert_eq!(all_commits(&fixture.repo), commits);
}

#[test]
fn stop_does_nothing_when_the_branch_was_switched_away() {
    let fixture = HookFixture::new();
    let path = create_subagent_worktree(&fixture, "a5");
    git(&path, &["switch", "-q", "-c", "other"]);
    let commits = all_commits(&fixture.repo);
    let output = fixture.hook(&[], stop_payload("a5", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(path.is_dir());
    assert_eq!(branch_of(&path), "other");
    assert!(branch_exists(&fixture.repo, "simgit/agent-a5"));
    assert_eq!(all_commits(&fixture.repo), commits);
}

#[test]
fn stop_ignores_a_plain_subagent() {
    let fixture = HookFixture::new();
    let before = fixture.snapshot();
    let output = fixture.hook(&[], stop_payload("plain", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert_eq!(fixture.snapshot(), before);
}

/// Most subagents run without a worktree, and their stops must leave no trace:
/// no branch claim file each, and no `.git/simgit` at all in a repository
/// simgit never allocated in.
#[test]
fn plain_subagent_stops_create_no_claim_files() {
    let fixture = HookFixture::new();
    for id in ["p1", "p2", "p3"] {
        let output = fixture.hook(&[], stop_payload(id, &fixture.repo));
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert_eq!(stderr(&output), "");
    }
    let state = fixture.repo.join(".git/simgit");
    assert!(!state.exists(), "{} was created", state.display());
}

#[test]
fn stop_prunes_a_worktree_deleted_by_hand_and_keeps_unpushed_work() {
    let fixture = HookFixture::new();
    let clean = create_subagent_worktree(&fixture, "a8");
    let worked = create_subagent_worktree(&fixture, "a9");
    commit_in(&worked, "work.txt");
    fs::remove_dir_all(&clean).expect("delete by hand");
    fs::remove_dir_all(&worked).expect("delete by hand");

    let output = fixture.hook(&[], stop_payload("a8", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!registered(&fixture.repo, &clean));
    assert!(!branch_exists(&fixture.repo, "simgit/agent-a8"));

    let output = fixture.hook(&[], stop_payload("a9", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!registered(&fixture.repo, &worked));
    assert!(branch_exists(&fixture.repo, "simgit/agent-a9"));
}

/// A stop drops only its own subagent's deleted registration: another
/// worktree deleted by hand keeps its reattach metadata. One that holds staged
/// work is kept too, with the refusal on one stderr line and exit 0.
#[test]
fn stop_drops_only_its_own_deleted_registration_and_keeps_staged_work() {
    let fixture = HookFixture::new();
    let own = create_subagent_worktree(&fixture, "b1");
    let other = create_subagent_worktree(&fixture, "b2");
    let staged = create_subagent_worktree(&fixture, "b3");
    fs::write(staged.join("staged.txt"), "work\n").unwrap();
    git(&staged, &["add", "staged.txt"]);
    for path in [&own, &other, &staged] {
        fs::remove_dir_all(path).expect("delete by hand");
    }

    let output = fixture.hook(&[], stop_payload("b1", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!registered(&fixture.repo, &own));
    assert!(
        registered(&fixture.repo, &other),
        "another registration was pruned"
    );

    let output = fixture.hook(&[], stop_payload("b3", &fixture.repo));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let note = stderr(&output);
    assert_eq!(note.lines().count(), 1, "{note}");
    assert!(note.contains("still holds staged changes"), "{note}");
    assert!(registered(&fixture.repo, &staged));
    assert!(branch_exists(&fixture.repo, "simgit/agent-b3"));
}

#[test]
fn parallel_stops_are_safe() {
    let fixture = HookFixture::new();
    let path = create_subagent_worktree(&fixture, "a6");
    let outputs: Vec<Output> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| fixture.hook(&[], stop_payload("a6", &fixture.repo))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });
    for output in &outputs {
        assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
    }
    assert!(!path.exists());
    assert!(
        !branch_exists(&fixture.repo, "simgit/agent-a6"),
        "{} / {}",
        stderr(&outputs[0]),
        stderr(&outputs[1])
    );
}

#[test]
fn stop_exits_zero_outside_a_repository() {
    let fixture = HookFixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).expect("mkdir");
    let before = fixture.snapshot();
    let output = fixture.run_in(
        &outside,
        &[],
        &[],
        &stop_payload("a7", &outside).to_string(),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // Every subagent stop runs the hook, so a project that is no repository
    // would otherwise get a line for each.
    assert_eq!(stderr(&output), "");
    assert_eq!(fixture.snapshot(), before);
}

/// A project Git refuses to open is still a repository: the stop says why it
/// did nothing, on one line, and still exits 0.
#[test]
fn stop_reports_a_repository_git_refuses() {
    let fixture = HookFixture::new();
    let broken = fixture.root.join("broken");
    fs::create_dir(&broken).expect("mkdir");
    fs::write(broken.join(".git"), "gitdir: /nonexistent/simgit\n").expect("write");
    let output = fixture.run_in(&broken, &[], &[], &stop_payload("a10", &broken).to_string());
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let complaint = stderr(&output);
    assert_eq!(complaint.lines().count(), 1, "{complaint}");
    assert!(
        complaint.starts_with("simgit: SubagentStop: "),
        "{complaint}"
    );
}

/// A payload without an `agent_id` names no subagent worktree: nothing to do,
/// and nothing to say.
#[test]
fn stop_without_an_agent_id_is_silent() {
    let fixture = HookFixture::new();
    let before = fixture.snapshot();
    let output = fixture.hook(
        &[],
        json!({ "hook_event_name": "SubagentStop", "cwd": fixture.repo }),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert_eq!(stderr(&output), "");
    assert_eq!(fixture.snapshot(), before);
}

fn read_json(path: &str) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// Collects the version after every `--requires`, in argv arrays and in command strings.
fn requires_values(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for pair in items.windows(2) {
                if pair[0] == "--requires" {
                    out.extend(pair[1].as_str().map(str::to_string));
                }
            }
            items.iter().for_each(|item| requires_values(item, out));
        }
        Value::Object(map) => map.values().for_each(|v| requires_values(v, out)),
        Value::String(s) => {
            let words: Vec<&str> = s.split_whitespace().collect();
            for pair in words.windows(2) {
                if pair[0] == "--requires" {
                    out.push(pair[1].to_string());
                }
            }
        }
        _ => {}
    }
}

fn semver(v: &str) -> (u64, u64, u64) {
    let parts: Vec<u64> = v
        .split('.')
        .map(|p| p.parse().unwrap_or_else(|_| panic!("not X.Y.Z: {v}")))
        .collect();
    match parts.as_slice() {
        [x, y, z] => (*x, *y, *z),
        _ => panic!("not X.Y.Z: {v}"),
    }
}

#[test]
fn plugin_version_equals_the_cargo_version() {
    let plugin = read_json(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.claude-plugin/plugin.json"
    ));
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn hooks_require_no_newer_than_the_cargo_version() {
    let hooks = read_json(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../integrations/claude-code/hooks.json"
    ));
    let mut values = Vec::new();
    requires_values(&hooks, &mut values);
    assert!(!values.is_empty(), "hooks.json names no --requires version");
    for v in values {
        assert!(
            semver(&v) <= semver(env!("CARGO_PKG_VERSION")),
            "--requires {v} is newer than the crate"
        );
    }
}

/// Claude Code kills a hook after 600 s by default, and a first baseline in a
/// large repository can take longer: a create killed then strands its lock.
#[test]
fn worktree_create_hook_allows_half_an_hour() {
    let hooks = read_json(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../integrations/claude-code/hooks.json"
    ));
    assert_eq!(
        hooks["hooks"]["WorktreeCreate"][0]["hooks"][0]["timeout"],
        1800
    );
}

/// The ref moves to a new tag only after Homebrew serves that release, so
/// while a release is tagged and tested it still names the previous one.
#[test]
fn marketplace_ref_names_a_release_no_newer_than_the_crate() {
    let market = read_json(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.claude-plugin/marketplace.json"
    ));
    // Codex rejects `"source": "github"` entries; a `url` source serves both
    // Claude Code and Codex.
    let source = &market["plugins"][0]["source"];
    assert_eq!(source["source"], "url", "{source}");
    assert_eq!(
        source["url"], "https://github.com/abendrothj/simgit.git",
        "{source}"
    );
    let reference = source["ref"].as_str().expect("marketplace plugin ref");
    let version = reference
        .strip_prefix('v')
        .unwrap_or_else(|| panic!("ref {reference} is not vX.Y.Z"));
    assert!(
        semver(version) <= semver(env!("CARGO_PKG_VERSION")),
        "ref {reference} is newer than the crate"
    );
}

/// The hook runs on every subagent's start and end: its `git` calls may fall
/// but never quietly grow. The listings left are guards: one before the
/// branch claim, a recheck under it, and after teardown, whether the branch
/// is checked out elsewhere.
#[test]
fn hook_keeps_its_git_calls() {
    let fixture = HookFixture::new();
    let log = fixture.root.join("hook-git.log");
    let env = git_wrapper(&fixture, &format!("echo \"$*\" >> '{}'\n", log.display()));
    let calls = |payload: Value| {
        let _ = fs::remove_file(&log);
        let output = hook_with_env(&fixture, &[], &payload, &env);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        (output, fs::read_to_string(&log).unwrap_or_default())
    };
    let (created, create) = calls(create_payload("counted", &fixture.repo));
    let path = stdout_path(&created);
    let (_, reuse) = calls(create_payload("counted", &fixture.repo));
    let (_, remove) = calls(remove_payload(&path));
    for (name, log, limit) in [
        ("create", &create, HOOK_CREATE_GIT_CALLS),
        ("reuse", &reuse, HOOK_REUSE_GIT_CALLS),
        ("remove", &remove, HOOK_REMOVE_GIT_CALLS),
    ] {
        let count = log.lines().count();
        assert!(count <= limit, "{name} made {count} calls:\n{log}");
    }
}

/// The counts `hook_keeps_its_git_calls` holds the hook to, in checkout mode.
const HOOK_CREATE_GIT_CALLS: usize = 16;
const HOOK_REUSE_GIT_CALLS: usize = 4;
const HOOK_REMOVE_GIT_CALLS: usize = 11;

const NEWER_RELEASE: &str = "https://github.com/abendrothj/simgit/releases/tag/v9.9.9";

impl HookFixture {
    /// The cache `SessionStart` reads the latest release from.
    fn update_cache(&self) -> PathBuf {
        self.root.join(".cache/simgit/latest")
    }

    /// `SessionStart` with the update check on and `curl` replaced by one that
    /// answers with `NEWER_RELEASE` after `delay` seconds.
    fn session_start(&self, delay: &str, env: &[(&str, &str)]) -> Output {
        let bin = self.root.join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        let curl = bin.join("curl");
        fs::write(
            &curl,
            format!("#!/bin/sh\nsleep {delay}\nprintf '%s' '{NEWER_RELEASE}'\n"),
        )
        .unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = isolated(env!("CARGO_BIN_EXE_simgit"))
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("CI")
            .env_remove("SIMGIT_NO_UPDATE_CHECK")
            .env_remove("XDG_CACHE_HOME")
            .envs(env.iter().copied())
            .args(["hook", "claude-code", "--requires", "0.0.0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn simgit");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({ "hook_event_name": "SessionStart", "cwd": self.repo })
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn cache_release(&self, url: &str) {
        fs::create_dir_all(self.update_cache().parent().unwrap()).unwrap();
        fs::write(self.update_cache(), url).unwrap();
    }
}

/// Plugin users never type a simgit command, so session start is where they
/// hear of a newer release.
#[test]
fn session_start_shows_a_newer_release_to_the_person() {
    let fixture = HookFixture::new();
    fixture.cache_release(NEWER_RELEASE);
    let output = fixture.session_start("0", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let shown: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let message = shown["systemMessage"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "simgit 9.9.9 is available (you have {}); update with: ",
            env!("CARGO_PKG_VERSION")
        )),
        "{message}"
    );
}

#[test]
fn session_start_says_nothing_when_current_in_ci_or_switched_off() {
    let fixture = HookFixture::new();
    fixture.cache_release("https://github.com/abendrothj/simgit/releases/tag/v0.0.1");
    let current = fixture.session_start("0", &[]);
    assert_eq!(current.status.code(), Some(0), "{}", stderr(&current));
    assert!(
        current.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&current.stdout)
    );
    fixture.cache_release(NEWER_RELEASE);
    for env in [("CI", "true"), ("SIMGIT_NO_UPDATE_CHECK", "1")] {
        let output = fixture.session_start("0", &[env]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{env:?}: {}",
            stderr(&output)
        );
        assert!(output.stdout.is_empty(), "{env:?}");
    }
}

#[test]
fn session_start_refreshes_a_stale_cache_without_waiting() {
    let fixture = HookFixture::new();
    let started = std::time::Instant::now();
    // The check takes six seconds; the session must not wait for it.
    let output = fixture.session_start("6", &[]);
    let took = started.elapsed();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(took < std::time::Duration::from_secs(3), "waited {took:?}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while fs::read_to_string(fixture.update_cache()).unwrap_or_default() != NEWER_RELEASE {
        assert!(
            std::time::Instant::now() < deadline,
            "the cache was never refreshed"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The plugin's `SessionStart` command must pass the hook's message through,
/// and still name the upgrade when simgit is missing or too old.
#[test]
fn session_start_hook_passes_its_message_through() {
    let hooks = read_json(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../integrations/claude-code/hooks.json"
    ));
    let command = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("SessionStart command");
    let fixture = HookFixture::new();
    let bin = fixture.root.join("stub-bin");
    fs::create_dir_all(&bin).unwrap();
    for (script, expected) in [
        (
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"systemMessage\": \"newer\"}'\n",
            "\"newer\"",
        ),
        ("#!/bin/sh\nexit 2\n", "needs simgit 0.5.0 or newer"),
    ] {
        let stub = bin.join("simgit");
        fs::write(&stub, script).unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("sh")
            .args(["-c", command])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(said.contains(expected), "{script:?} → {said:?}");
    }
}
