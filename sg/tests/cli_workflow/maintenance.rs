//! `gc` and `prune`.

use crate::{
    add_detached, branch_exists, commit_in, create_at, dead_pid, git, git_stdout, git_wrapper,
    registered_head, registration, resolved, success, Fixture,
};
use std::fs;
use std::path::Path;

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

/// Branches are deleted after their worktrees are gone, so a failure there
/// must still report what was reaped, keeping every branch it cannot vouch for.
#[test]
fn gc_reports_what_it_reaped_when_branch_deletion_fails() {
    let fixture = Fixture::new();
    for index in 0..2 {
        success(&fixture.run(&["add", &format!("agent/f{index}"), "--ephemeral"]));
    }
    let env = git_wrapper(
        &fixture,
        "case \" $* \" in *' branch -d '*|*' for-each-ref '*) exit 1 ;; esac\n",
    );
    let output = fixture
        .command()
        .envs(env.iter().cloned())
        .args(["--json", "gc", "--older-than", "0s", "--delete-branches"])
        .output()
        .unwrap();
    success(&output);
    let gc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(gc["reaped"].as_array().unwrap().len(), 2, "{gc}");
    assert_eq!(gc["deleted_branches"], serde_json::json!([]), "{gc}");
    let mut retained: Vec<&str> = gc["retained_branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|branch| branch.as_str().unwrap())
        .collect();
    retained.sort_unstable();
    assert_eq!(retained, ["agent/f0", "agent/f1"], "{gc}");
    assert!(branch_exists(&fixture.repo, "agent/f0"));
    assert!(branch_exists(&fixture.repo, "agent/f1"));
}

/// A baseline whose commit only a detached worktree's HEAD reaches is still in
/// use: `prune` relies on `rev-list --all` counting that HEAD.
#[test]
fn prune_keeps_a_baseline_only_a_detached_worktree_reaches() {
    let fixture = Fixture::new();
    git(&fixture.repo, &["switch", "-qc", "soon-gone"]);
    fs::write(fixture.repo.join("detached.txt"), "x\n").unwrap();
    git(&fixture.repo, &["add", "detached.txt"]);
    git(&fixture.repo, &["commit", "-qm", "detached"]);
    let commit = git_stdout(&fixture.repo, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git(&fixture.repo, &["switch", "-q", "-"]);
    let added = fixture
        .command()
        .env_remove("SIMGIT_POPULATE")
        .args(["add", "--detach", "--base", &commit, "--path"])
        .arg(fixture.root.join("detached"))
        .output()
        .unwrap();
    success(&added);
    let baseline = fixture.repo.join(".git/simgit/baselines").join(&commit);
    if !baseline.is_dir() {
        eprintln!("skipped: no backend here uses a baseline");
        return;
    }
    git(&fixture.repo, &["branch", "-qD", "soon-gone"]);
    success(&fixture.run(&["--json", "prune"]));
    assert!(
        baseline.is_dir(),
        "prune dropped a baseline a worktree's HEAD reaches"
    );
}
