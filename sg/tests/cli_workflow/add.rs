//! `add` and allocation: destinations, attach, sparse, concurrency, failures.

use crate::{
    branch_exists, git, git_stdout, git_wrapper, isolated, linked_worktree_ahead, local_refs,
    rev_parse, run_from, running_as_root, success, wait_for, Fixture,
};
use std::fs;
use std::path::Path;
use std::process::Command;

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
    let commit = isolated("git")
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
    let surviving = isolated("git")
        .current_dir(&fixture.repo)
        .args(["rev-parse", "--verify", "refs/heads/agent/race"])
        .output()
        .unwrap();
    success(&surviving);
    assert_eq!(String::from_utf8(surviving.stdout).unwrap().trim(), commit);
    assert!(!fixture.worktree.exists());
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
        let output = isolated("git")
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
    let output = isolated("git")
        .current_dir(&fixture.repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/chat/test"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
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

    let output = isolated(env!("CARGO_BIN_EXE_sg"))
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

    let status = isolated("git")
        .current_dir(&worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "");

    fs::write(worktree.join("nested/deep/data.txt"), "edited\n").unwrap();
    let status = isolated("git")
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
        let mut command = isolated(env!("CARGO_BIN_EXE_sg"));
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

    let listed = isolated("git")
        .current_dir(&worktree)
        .args(["ls-files", "-t"])
        .output()
        .unwrap();
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(
        listed.lines().any(|line| line.starts_with('S')),
        "paths outside the cone must be marked skip-worktree: {listed}"
    );

    let status = isolated("git")
        .current_dir(&worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(status.stdout).unwrap(), "");

    fs::write(worktree.join("alpha/sub/f0.txt"), "edited\n").unwrap();
    let status = isolated("git")
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
        let status = isolated("git")
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

    let rejected = isolated(env!("CARGO_BIN_EXE_sg"))
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
        let status = isolated("git")
            .current_dir(&target)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        success(&status);
        assert!(status.stdout.is_empty(), "sparse worktree is dirty");
    }
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
    let reference = isolated("git")
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
    let attached = isolated("git")
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
    let registrations = isolated("git")
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
    let checked_out = isolated("git")
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
    let registrations = isolated("git")
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

    let registrations = isolated("git")
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
    let listed = isolated("git")
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
        &isolated(env!("CARGO_BIN_EXE_sg"))
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
/// the end of each allocation, after its clean check: gc must meet that lock
/// and must not name a racer as reapable before its allocation is complete.
/// Writing the markers and dropping the lock follow with no git call between
/// them, so no wrapper can separate the two; the unit test
/// `create_worktree_writes_its_markers_before_releasing_the_lock` pins that order.
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
        "if [ \"$3\" = status ]; then\n\
           \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n\
           status=$?\n\
           sleep 0.1\n\
           exit $status\n\
         fi\n",
    );
    let fully_allocated = |path: &Path| {
        let admin = isolated("git")
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
        // Apart from prune.
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
    let registrations = isolated("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let registrations = String::from_utf8_lossy(&registrations.stdout).into_owned();
    for (index, output) in outcomes.iter().enumerate() {
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let path = Path::new(report["path"].as_str().unwrap());
        assert_eq!(fs::read_to_string(path.join("file-999")).unwrap(), "999\n");
        let status = isolated("git")
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
    let registrations = isolated("git")
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
    let branch = isolated("git")
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

    if running_as_root() {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let fixture = Fixture::new();
    // The allocation removes its own lock file, so a hook that leaves its
    // registration unwritable makes that release fail after the hook does.
    let hook = fixture.repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\nchmod a-w \"$(git rev-parse --absolute-git-dir)\"\n\
         echo 'simulated hook failure' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let failed = fixture
        .command()
        .args(["add", "agent/hooked", "--path"])
        .arg(&fixture.worktree)
        .output()
        .unwrap();
    for admin in fs::read_dir(fixture.repo.join(".git/worktrees"))
        .unwrap()
        .flatten()
    {
        fs::set_permissions(admin.path(), fs::Permissions::from_mode(0o755)).unwrap();
    }
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
fn add_in_a_repository_without_commits_says_so() {
    let root = std::env::temp_dir().join(format!("simgit-unborn-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    let output = isolated(env!("CARGO_BIN_EXE_sg"))
        .current_dir(&root)
        .args(["add", "agent/x"])
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&root);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no commits yet"));
}

/// An allocation that finds another building its commit's baseline waits for
/// that build, and says so on stderr rather than hanging silently.
#[test]
fn an_allocation_waiting_for_a_baseline_build_says_so() {
    use std::io::BufRead;

    let fixture = Fixture::new();
    let doctor = fixture
        .command()
        .env_remove("SIMGIT_POPULATE")
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    success(&doctor);
    let doctor: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    if doctor["cow_supported"] != true {
        eprintln!("skipped: no backend here uses a baseline");
        return;
    }
    // The first build stalls in its checkout until the test releases it,
    // bounded so a failing test cannot hang.
    let started = fixture.root.join("build-started");
    let release = fixture.root.join("build-release");
    let env = git_wrapper(
        &fixture,
        &format!(
            "case \" $* \" in *' checkout-index '*) if [ ! -e '{started}' ]; then : > '{started}'; \
             i=0; while [ ! -e '{release}' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i + 1)); done; fi ;; esac\n",
            started = started.display(),
            release = release.display(),
        ),
    );
    let add = |branch: &str| {
        fixture
            .command()
            .envs(env.iter().cloned())
            .env_remove("SIMGIT_POPULATE")
            .args(["--json", "add", branch])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = add("agent/builds");
    wait_for(&started);
    let mut second = add("agent/waits");
    let mut stderr = std::io::BufReader::new(second.stderr.take().unwrap());
    let mut notice = String::new();
    stderr.read_line(&mut notice).unwrap();
    fs::write(&release, "").unwrap();
    assert!(
        notice.contains("waiting for another simgit to build the baseline"),
        "{notice:?}"
    );
    success(&first.wait_with_output().unwrap());
    success(&second.wait_with_output().unwrap());
}
