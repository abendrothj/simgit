//! `remove`: dirty and detached worktrees, retries, `--commit`.

use crate::{
    add_detached, commit_exists, commit_in, git, git_wrapper, isolated, local_refs,
    reftable_fixture, registered_head, registration, rev_parse, success, Fixture,
};
use std::fs;
use std::path::Path;

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
    let branch = isolated("git")
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
        let output = isolated("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let status = |repo: &Path| {
        let output = isolated("git")
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

fn object_count(repo: &Path) -> String {
    let output = isolated("git")
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
