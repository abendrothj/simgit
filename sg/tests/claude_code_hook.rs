use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_simgit"))
            .current_dir(&self.root)
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
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_simgit"))
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
    child = Command::new(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.root)
        .args(["hook", "claude-code", "--requires", "1.x"])
        .stdin(Stdio::null())
        .output()
        .expect("run simgit");
    assert_eq!(child.status.code(), Some(1));
}

#[test]
fn json_flag_is_rejected() {
    let fixture = HookFixture::new();
    let output = fixture.run(&["--json"], &[], r#"{"hook_event_name":"SessionStart"}"#);
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
    let output = Command::new(env!("CARGO_BIN_EXE_simgit"))
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

#[test]
fn create_refuses_a_run_locked_worktree() {
    let fixture = HookFixture::new();
    let mut runner = Command::new(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .env("SIMGIT_POPULATE", "checkout")
        .args(["run", "simgit/s4", "--", "sleep", "3"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn simgit run");
    for _ in 0..100 {
        let listing = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
        if listing.contains("simgit/s4") && listing.contains("locked") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = fixture.hook(&[], create_payload("s4", &fixture.repo));
    runner.wait().expect("wait for simgit run");
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
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
        let mut child = Command::new("sh")
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
    let listing = Command::new(env!("CARGO_BIN_EXE_simgit"))
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

/// `git fetch`'s connectivity check reads every worktree's HEAD, so it dies
/// on a registration another allocation is half-way through writing. The
/// wrapper fails the first fetch that way; the create must still succeed.
#[test]
fn pr_fetch_is_retried_after_a_half_written_registration() {
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
               echo \"fatal: failed to read ${{1#--git-dir=}}/worktrees/other/commondir: Undefined error: 0\" >&2\n\
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
    let output = Command::new(env!("CARGO_BIN_EXE_simgit"))
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

#[test]
fn remove_refuses_a_detached_worktree() {
    let fixture = HookFixture::new();
    let path = create_worktree(&fixture, "d1");
    git(&path, &["checkout", "-q", "--detach"]);
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not on a simgit/ branch"),
        "{}",
        stderr(&output)
    );
    assert!(path.is_dir());
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
    assert!(path.join("scratch.txt").is_file());
    assert!(branch_exists(&fixture.repo, "simgit/r2"));
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_simgit"))
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
    let mut runner = Command::new(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .env("SIMGIT_POPULATE", "checkout")
        .args(["run", "simgit/r7", "--", "sleep", "3"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn simgit run");
    let mut path = None;
    for _ in 0..100 {
        let listing = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
        if listing.contains("simgit/r7") && listing.contains("locked") {
            path = listing
                .lines()
                .rev()
                .skip_while(|line| *line != "branch refs/heads/simgit/r7")
                .find_map(|line| line.strip_prefix("worktree ").map(PathBuf::from));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let path = path.expect("run created a locked worktree");
    let output = fixture.hook(&["--discard-dirty"], remove_payload(&path));
    runner.wait().expect("wait for simgit run");
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
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_simgit"))
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
    let mut runner = Command::new(env!("CARGO_BIN_EXE_simgit"))
        .current_dir(&fixture.repo)
        .env("SIMGIT_POPULATE", "checkout")
        .args(["run", "simgit/agent-a4", "--", "sleep", "3"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn simgit run");
    let mut path = None;
    for _ in 0..100 {
        let listing = git(&fixture.repo, &["worktree", "list", "--porcelain"]);
        if listing.contains("simgit/agent-a4") && listing.contains("locked") {
            path = listing
                .lines()
                .rev()
                .skip_while(|line| *line != "branch refs/heads/simgit/agent-a4")
                .find_map(|line| line.strip_prefix("worktree ").map(PathBuf::from));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let path = path.expect("run created a locked worktree");
    let commits = all_commits(&fixture.repo);
    let output = fixture.hook(&[], stop_payload("a4", &fixture.repo));
    runner.wait().expect("wait for simgit run");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
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
    assert_eq!(
        stderr(&output).trim_end().lines().count(),
        1,
        "{}",
        stderr(&output)
    );
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
    assert_eq!(source["url"], "https://github.com/abendrothj/simgit.git");
    let reference = market["plugins"][0]["source"]["ref"]
        .as_str()
        .expect("marketplace plugin ref");
    let version = reference
        .strip_prefix('v')
        .unwrap_or_else(|| panic!("ref {reference} is not vX.Y.Z"));
    assert!(
        semver(version) <= semver(env!("CARGO_PKG_VERSION")),
        "ref {reference} is newer than the crate"
    );
}
