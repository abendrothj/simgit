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
    for command in [
        vec!["simgit-command-that-does-not-exist"],
        vec!["sh", "-c", "exit 7"],
    ] {
        let output = fixture
            .command()
            .args(["run", "chat/test", "--"])
            .args(command)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(fixture.worktree.is_dir());
        assert!(fixture.listed().get("locked").is_none());
    }
    success(&fixture.run(&["run", "chat/test", "--", "true"]));
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
    assert!(
        !fixture
            .run(&["run", "chat/test", "--", "true"])
            .status
            .success(),
        "the stranded lock did not block a new command"
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
