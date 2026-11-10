//! `doctor`, `list`, identity, and machine-readable output and errors.

use crate::{isolated, running_as_root, success, Fixture};
use std::fs;
use std::path::Path;

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
            let mut child = isolated(bin)
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

/// `sg` is only an alias; the canonical name must identify itself as simgit
/// and behave identically.
#[test]
fn canonical_binary_identifies_as_simgit_and_matches_the_alias() {
    let fixture = Fixture::new();
    let version = isolated(env!("CARGO_BIN_EXE_simgit"))
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
    let canonical = isolated(env!("CARGO_BIN_EXE_simgit"))
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
        isolated(env!("CARGO_BIN_EXE_sg"))
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
