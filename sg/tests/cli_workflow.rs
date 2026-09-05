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
        .args(["worktree", "run", "agent/test", "--ephemeral", "--path"])
        .arg(&worktree)
        .args([
            "--",
            "sh",
            "-c",
            "printf 'agent output\\n' > result.txt && git add . && git commit -qm agent-result",
        ])
        .output()
        .expect("run sg worktree run");
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
            "worktree",
            "gc",
            "--ephemeral",
            "--older-than",
            "0s",
            "--delete-branches",
            "--force",
        ])
        .output()
        .expect("run sg worktree gc");
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
            .env("SIMGIT_POPULATE", "checkout");
        command
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    fn create(&self, extra: &[&str]) {
        let output = self
            .command()
            .args(["worktree", "run", "chat/test", "--path"])
            .arg(&self.worktree)
            .args(extra)
            .args(["--", "true"])
            .output()
            .unwrap();
        success(&output);
    }

    fn listed(&self) -> serde_json::Value {
        let output = self.run(&["worktree", "list", "--json"]);
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
        "worktree",
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
    let list = fixture.run(&["worktree", "list"]);
    success(&list);
    let text = String::from_utf8(list.stdout).unwrap();
    assert!(text.contains("chat/test") && text.contains("persistent"));
    success(&fixture.run(&["worktree", "gc", "--older-than", "0s", "--force"]));
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
            .args(["worktree", "run", "chat/test"])
            .args(options)
            .args(["--", "sh", "-c", "touch should-not-run"])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert!(!fixture.worktree.join("should-not-run").exists());
    assert!(!fixture
        .run(&["worktree", "add", "chat/test"])
        .status
        .success());
    success(
        &fixture
            .command()
            .args(["worktree", "run", "chat/test", "--path"])
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
        .args(["worktree", "run", "chat/test", "--require-cow", "--path"])
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
    for command in [
        vec!["simgit-command-that-does-not-exist"],
        vec!["sh", "-c", "exit 7"],
    ] {
        let output = fixture
            .command()
            .args(["worktree", "run", "chat/test", "--"])
            .args(command)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(fixture.worktree.is_dir());
        assert!(fixture.listed().get("locked").is_none());
    }
    success(&fixture.run(&["worktree", "run", "chat/test", "--", "true"]));
}

#[test]
fn running_commands_are_protected_from_gc_and_remove() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    let mut child = fixture
        .command()
        .args([
            "worktree",
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
    let gc = fixture.run(&["worktree", "gc", "--older-than", "0s", "--force", "--json"]);
    let remove = fixture.run(&["worktree", "remove", "chat/test", "--force"]);
    let duplicate = fixture.run(&["worktree", "run", "chat/test", "--", "true"]);
    let listed = fixture.listed();
    let status = child.wait().unwrap();
    assert!(started && status.success());
    success(&gc);
    assert!(String::from_utf8_lossy(&gc.stdout).contains("locked"));
    assert!(!remove.status.success() && !duplicate.status.success());
    assert!(listed.get("locked").is_some());
    success(&fixture.run(&["worktree", "gc", "--older-than", "0s", "--force"]));
    assert!(!fixture.worktree.exists());
}

#[test]
fn persistence_can_be_changed_explicitly_on_reuse() {
    let fixture = Fixture::new();
    fixture.create(&["--ephemeral"]);
    assert_eq!(fixture.listed()["ephemeral"], true);
    success(&fixture.run(&["worktree", "run", "chat/test", "--persistent", "--", "true"]));
    assert_eq!(fixture.listed()["ephemeral"], false);
    success(&fixture.run(&[
        "worktree",
        "gc",
        "--ephemeral",
        "--older-than",
        "0s",
        "--force",
    ]));
    assert!(fixture.worktree.exists());
    success(&fixture.run(&[
        "worktree",
        "gc",
        "--include-persistent",
        "--older-than",
        "0s",
        "--force",
    ]));
    assert!(!fixture.worktree.exists());
}

#[test]
fn run_reuses_main_worktree_without_creating_another_checkout() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["checkout", "-qb", "chat/main"]);
    success(&fixture.run(&[
        "worktree",
        "run",
        "chat/main",
        "--",
        "sh",
        "-c",
        "test -f README.md",
    ]));
    assert!(!fixture.repo.join(".git/simgit-run.lock").exists());
    let output = fixture.run(&["worktree", "list", "--json"]);
    success(&output);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(entries.len(), 1);
}
