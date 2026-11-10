//! Budgets on the `git` calls a command makes, so per-worktree work cannot creep back.

use crate::{git, git_stdout, git_wrapper, success, Fixture};
use std::fs;
use std::path::Path;

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

/// Ten agents starting on a commit with no baseline yet wait for one build
/// instead of each checking out the whole tree and throwing all but one away.
#[test]
fn concurrent_first_allocations_build_one_baseline() {
    let fixture = Fixture::new();
    let log = fixture.root.join("checkout-index.log");
    let env = git_wrapper(
        &fixture,
        &format!(
            "case \" $* \" in *' checkout-index '*) echo >> '{}' ;; esac\n",
            log.display()
        ),
    );
    let children: Vec<_> = (0..10)
        .map(|index| {
            fixture
                .command()
                .envs(env.iter().cloned())
                .env_remove("SIMGIT_POPULATE")
                .args(["--json", "add", &format!("agent/b{index}")])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let added: Vec<serde_json::Value> = children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            success(&output);
            serde_json::from_slice(&output.stdout).unwrap()
        })
        .collect();
    if added
        .iter()
        .all(|worktree| worktree["mode"] == "git-checkout")
    {
        eprintln!("skipped: no backend here uses a baseline");
        return;
    }
    let builds = fs::read_to_string(&log).unwrap_or_default().lines().count();
    assert_eq!(builds, 1, "baseline checkouts: {builds}");
    for worktree in &added {
        let path = worktree["path"].as_str().unwrap();
        let status = git_stdout(Path::new(path), &["status", "--porcelain"]);
        assert!(status.trim().is_empty(), "{path}: {status}");
    }
}

/// `gc` reads each worktree's markers and lock from the registry it already
/// listed: a `git` per worktree before it decides anything made a routine
/// `gc` take seconds at a few hundred worktrees. With nothing due it runs no
/// `git` per worktree; a due worktree costs its one `git status`, no more.
#[test]
fn gc_runs_no_git_per_worktree() {
    let fixture = Fixture::new();
    let log = fixture.root.join("gc-git.log");
    let env = git_wrapper(&fixture, &format!("echo >> '{}'\n", log.display()));
    let gits = |extra: &[&str]| {
        let _ = fs::remove_file(&log);
        let output = fixture
            .command()
            .envs(env.iter().cloned())
            .args(["--json", "gc"])
            .args(extra)
            .output()
            .unwrap();
        success(&output);
        fs::read_to_string(&log).unwrap_or_default().lines().count()
    };
    let mut made = 0;
    let mut grow_to = |count: usize| {
        while made < count {
            let name = format!("agent/g{made}");
            // Half persistent, half ephemeral, so each kind is judged.
            let mut args = vec!["add", name.as_str()];
            if made % 2 == 1 {
                args.push("--ephemeral");
            }
            success(&fixture.run(&args));
            made += 1;
        }
    };
    // `--older-than 0s` makes every ephemeral worktree due, so the checks
    // after the age filter run too; `--dry-run` keeps them for the next count.
    let due = ["--older-than", "0s", "--dry-run"];
    grow_to(4);
    let (idle_few, due_few) = (gits(&[]), gits(&due));
    grow_to(16);
    let (idle_many, due_many) = (gits(&[]), gits(&due));
    assert_eq!(
        idle_few, idle_many,
        "gc ran {idle_few} git calls at 4 worktrees, {idle_many} at 16"
    );
    // Six more ephemeral worktrees, one `git status` each.
    assert_eq!(
        due_many - due_few,
        6,
        "due gc ran {due_few} git calls at 4 worktrees, {due_many} at 16"
    );
}

/// Reaping is linear: teardown rechecks one registration's HEAD instead of
/// listing them all, and the reaped branches go in one `git branch -d`.
#[test]
fn gc_reaps_with_one_listing_and_one_branch_deletion() {
    let fixture = Fixture::new();
    for index in 0..6 {
        success(&fixture.run(&["add", &format!("agent/r{index}"), "--ephemeral"]));
    }
    // One branch holds a commit nothing else has: the batch must keep it.
    let listed: serde_json::Value =
        serde_json::from_slice(&fixture.run(&["--json", "list"]).stdout).unwrap();
    let unmerged = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|worktree| worktree["branch"] == "refs/heads/agent/r0")
        .map(|worktree| std::path::PathBuf::from(worktree["worktree"].as_str().unwrap()))
        .unwrap();
    fs::write(unmerged.join("only-here"), "x\n").unwrap();
    git(&unmerged, &["add", "only-here"]);
    git(&unmerged, &["commit", "-qm", "only here"]);
    // A tag of the same name makes Git shorten the branch to `heads/agent/r0`.
    git(&fixture.repo, &["tag", "agent/r0"]);
    let log = fixture.root.join("reap-git.log");
    let env = git_wrapper(
        &fixture,
        &format!(
            "case \" $* \" in *' worktree list '*) echo list >> '{0}' ;; *' branch -d '*|*' branch -D '*) echo branch >> '{0}' ;; esac\n",
            log.display()
        ),
    );
    let output = fixture
        .command()
        .envs(env.iter().cloned())
        .args(["--json", "gc", "--older-than", "0s", "--delete-branches"])
        .output()
        .unwrap();
    success(&output);
    let gc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(gc["reaped"].as_array().unwrap().len(), 6, "{gc}");
    assert_eq!(gc["deleted_branches"].as_array().unwrap().len(), 5, "{gc}");
    assert_eq!(
        gc["retained_branches"],
        serde_json::json!(["agent/r0"]),
        "{gc}"
    );
    assert!(
        !git_stdout(&fixture.repo, &["branch", "--list", "agent/r0"])
            .trim()
            .is_empty()
    );
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(calls.matches("list").count(), 1, "{calls}");
    assert_eq!(calls.matches("branch").count(), 1, "{calls}");
}

/// `remove` finds its target and that target's entry in one listing of the
/// registry: each listing reads every registration.
#[test]
fn remove_lists_the_registry_once() {
    let fixture = Fixture::new();
    for index in 0..3 {
        success(&fixture.run(&["add", &format!("agent/m{index}")]));
    }
    let log = fixture.root.join("remove-git.log");
    let env = git_wrapper(
        &fixture,
        &format!(
            "case \" $* \" in *' worktree list '*) echo list >> '{}' ;; esac\n",
            log.display()
        ),
    );
    let output = fixture
        .command()
        .envs(env.iter().cloned())
        .args(["remove", "agent/m1", "--delete-branch"])
        .output()
        .unwrap();
    success(&output);
    let lists = fs::read_to_string(&log).unwrap_or_default().lines().count();
    assert_eq!(lists, 1, "remove listed the registry {lists} times");
    assert!(git_stdout(&fixture.repo, &["branch", "--list", "agent/m1"])
        .trim()
        .is_empty());
}

/// An allocation drops its own lock by removing the file it wrote: `git
/// worktree unlock` would read every registration to find it, under the
/// registry lock, so each `add` waited on every other.
#[test]
fn add_releases_its_lock_without_git() {
    let fixture = Fixture::new();
    let log = fixture.root.join("unlock-git.log");
    let env = git_wrapper(
        &fixture,
        &format!(
            "case \" $* \" in *' worktree unlock '*) echo unlock >> '{}' ;; esac\n",
            log.display()
        ),
    );
    let output = fixture
        .command()
        .envs(env.iter().cloned())
        .args(["add", "agent/u"])
        .output()
        .unwrap();
    success(&output);
    assert_eq!(fs::read_to_string(&log).unwrap_or_default(), "");
    let listed: Vec<serde_json::Value> =
        serde_json::from_slice(&fixture.run(&["--json", "list"]).stdout).unwrap();
    let added = listed
        .iter()
        .find(|worktree| worktree["branch"] == "refs/heads/agent/u")
        .unwrap();
    assert!(added.get("locked").is_none(), "{added}");
}

/// Every command pays for its `git` calls, and `add` and `run` sit on an
/// agent's hot path: the counts may fall but never quietly grow.
#[test]
fn add_and_run_keep_their_git_calls() {
    let fixture = Fixture::new();
    let log = fixture.root.join("calls-git.log");
    let env = git_wrapper(&fixture, &format!("echo \"$*\" >> '{}'\n", log.display()));
    let calls = |args: &[&str]| {
        let _ = fs::remove_file(&log);
        let output = fixture
            .command()
            .envs(env.iter().cloned())
            .args(args)
            .output()
            .unwrap();
        success(&output);
        fs::read_to_string(&log).unwrap_or_default()
    };
    let add = calls(&["add", "agent/c"]);
    let run = calls(&["run", "agent/c", "--", "true"]);
    let count = |log: &str| log.lines().count();
    assert!(
        count(&add) <= ADD_GIT_CALLS,
        "add made {} calls:\n{add}",
        count(&add)
    );
    assert!(
        count(&run) <= RUN_GIT_CALLS,
        "run made {} calls:\n{run}",
        count(&run)
    );
}

/// The counts `add_and_run_keep_their_git_calls` holds them to, in checkout mode.
const ADD_GIT_CALLS: usize = 11;
const RUN_GIT_CALLS: usize = 4;

/// A published baseline never changes, so `doctor` and `prune` read the size
/// recorded when it was published instead of walking every file of every
/// baseline on each call. One published before sizes were recorded is still
/// walked.
#[test]
fn baseline_sizes_come_from_the_record() {
    let fixture = Fixture::new();
    let added = fixture
        .command()
        .env_remove("SIMGIT_POPULATE")
        .args(["--json", "add", "agent/sized"])
        .output()
        .unwrap();
    success(&added);
    let baselines = fixture.repo.join(".git/simgit/baselines");
    let Some(baseline) = fs::read_dir(&baselines)
        .ok()
        .and_then(|mut entries| entries.next())
        .map(|entry| entry.unwrap().path())
    else {
        eprintln!("skipped: no backend here uses a baseline");
        return;
    };
    let bytes = || {
        let doctor: serde_json::Value =
            serde_json::from_slice(&fixture.run(&["--json", "doctor"]).stdout).unwrap();
        doctor["baseline_cache"]["retained_bytes"].as_u64().unwrap()
    };
    let recorded = bytes();
    assert!(recorded > 0);
    // Not something a baseline ever sees; here it shows nothing walks it.
    fs::write(baseline.join("tree/added-later"), vec![b'x'; 1 << 20]).unwrap();
    assert_eq!(bytes(), recorded, "doctor walked the baseline");
    fs::remove_file(baseline.join("size")).unwrap();
    assert!(
        bytes() > recorded,
        "a baseline without a record must be walked"
    );
}

/// Whether a cached baseline's commit is still reachable is one `rev-list`
/// over every ref and worktree HEAD, not one `merge-base` per detached
/// worktree, and gives the same answer.
#[test]
fn prune_checks_baseline_reachability_in_one_call() {
    let fixture = Fixture::new();
    let cow = |args: &[&str]| {
        let output = fixture
            .command()
            .env_remove("SIMGIT_POPULATE")
            .args(args)
            .output()
            .unwrap();
        success(&output);
    };
    // A commit only a deleted branch reached, with a baseline built for it.
    git(&fixture.repo, &["switch", "-qc", "gone"]);
    fs::write(fixture.repo.join("gone.txt"), "x\n").unwrap();
    git(&fixture.repo, &["add", "gone.txt"]);
    git(&fixture.repo, &["commit", "-qm", "gone"]);
    let gone = git_stdout(&fixture.repo, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git(&fixture.repo, &["switch", "-q", "-"]);
    cow(&["add", "agent/tmp", "--base", &gone]);
    if !fixture
        .repo
        .join(".git/simgit/baselines")
        .join(&gone)
        .is_dir()
    {
        eprintln!("skipped: no backend here uses a baseline");
        return;
    }
    cow(&[
        "remove",
        "agent/tmp",
        "--delete-branch",
        "--delete-unmerged",
    ]);
    git(&fixture.repo, &["branch", "-qD", "gone"]);
    for index in 0..4 {
        cow(&[
            "add",
            "--detach",
            "--path",
            &fixture.root.join(format!("d{index}")).display().to_string(),
        ]);
    }
    let log = fixture.root.join("reach-git.log");
    let env = git_wrapper(
        &fixture,
        &format!(
            "case \" $* \" in *' merge-base '*) echo merge-base >> '{}' ;; esac\n",
            log.display()
        ),
    );
    let output = fixture
        .command()
        .envs(env.iter().cloned())
        .args(["--json", "prune"])
        .output()
        .unwrap();
    success(&output);
    assert_eq!(fs::read_to_string(&log).unwrap_or_default(), "");
    assert!(!fixture
        .repo
        .join(".git/simgit/baselines")
        .join(&gone)
        .exists());
}
