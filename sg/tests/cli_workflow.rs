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

#[test]
fn default_worktree_path_stays_outside_the_git_directory() {
    // Agent harnesses (Claude Code, editors, ripgrep) treat everything under
    // `.git/` as off-limits or invisible, so a worktree nested in the git dir
    // cannot be edited by the tools simgit exists to serve. The default must
    // be a sibling of the main working tree.
    let fixture = Fixture::new();
    let output = fixture.run(&["worktree", "add", "agent/edit"]);
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
        .args(["worktree", "add", "agent/clone"])
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
fn add_accepts_the_same_path_flag_as_run() {
    // `sg run` spells the location `--path`; `add` took it only positionally,
    // so the documented flag failed with a clap error. Both spellings must
    // land the worktree exactly where asked.
    let fixture = Fixture::new();
    let flagged = fixture.root.join("by-flag");
    let positional = fixture.root.join("by-position");

    success(&fixture.run(&[
        "worktree",
        "add",
        "feat/flag",
        "--path",
        flagged.to_str().unwrap(),
    ]));
    success(&fixture.run(&[
        "worktree",
        "add",
        "feat/positional",
        positional.to_str().unwrap(),
    ]));

    assert!(flagged.join("README.md").is_file());
    assert!(positional.join("README.md").is_file());
}

#[test]
fn prune_and_list_answer_machine_readable_questions() {
    // `prune --json` advertised JSON in --help and printed prose, which breaks
    // any orchestrator parsing it. And the populate mode was printed once at
    // creation and never again, so nothing could answer "is this worktree
    // actually CoW-backed?" — the one thing simgit exists to provide.
    let fixture = Fixture::new();
    let created = fixture.run(&["worktree", "add", "feat/mode"]);
    success(&created);
    let mode = String::from_utf8_lossy(&created.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("mode: ").map(str::to_owned))
        .expect("add reports the populate mode");

    let listed = fixture.run(&["worktree", "list", "--json"]);
    success(&listed);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&listed.stdout).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry["branch"] == "refs/heads/feat/mode")
        .expect("worktree is listed");
    assert_eq!(entry["mode"], mode);

    let human = fixture.run(&["worktree", "list"]);
    success(&human);
    assert!(String::from_utf8_lossy(&human.stdout)
        .lines()
        .any(|line| line.starts_with("feat/mode\t") && line.ends_with(&mode)));

    let pruned = fixture.run(&["worktree", "prune", "--json"]);
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
    // the only lever that reduces it on macOS. The contract: just the cone on
    // disk, everything else marked skip-worktree, a clean status, and edits in
    // the cone behaving normally.
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
            .args(["worktree", "add", branch, "--sparse", "alpha"])
            .output()
            .unwrap()
    };

    let output = create(Some("checkout"), "agent/cone");
    success(&output);
    let printed = String::from_utf8(output.stdout).unwrap();
    let worktree = std::path::PathBuf::from(printed.lines().last().unwrap().trim());

    assert!(worktree.join("alpha/sub/f0.txt").is_file());
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
        .args(["worktree", "add", "agent/escape", "--sparse", "../outside"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());

    let _ = fs::remove_dir_all(root);
}
