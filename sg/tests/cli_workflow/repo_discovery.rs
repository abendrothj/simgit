//! Submodules, bare repositories, separate git dirs and nested repositories.

use crate::{add_submodule, add_submodule_to, git, isolated, rev_parse, success, Fixture};
use std::fs;
use std::path::Path;

/// The directory `add` puts `branch` in by default when run from `dir`.
fn default_parent(fixture: &Fixture, dir: &Path, branch: &str) -> std::path::PathBuf {
    let output = fixture
        .command()
        .current_dir(dir)
        .args(["--json", "add", branch])
        .output()
        .unwrap();
    success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let path = Path::new(report["path"].as_str().unwrap());
    assert!(path.join(".git").is_file(), "{report}");
    path.parent().unwrap().to_path_buf()
}

/// Assert that `parent` is `<root>/.simgit/repo/.modules/<prefix>-<8 hex>`:
/// beside the superproject's main checkout, apart from its branches.
fn assert_module_root(fixture: &Fixture, parent: &Path, prefix: &str) {
    let modules = fixture
        .root
        .canonicalize()
        .unwrap()
        .join(".simgit/repo/.modules");
    assert_eq!(parent.parent(), Some(modules.as_path()), "{parent:?}");
    let name = parent.file_name().unwrap().to_str().unwrap();
    let digest = name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .unwrap_or_else(|| panic!("{name} does not start with {prefix}-"));
    assert!(
        digest.len() == 8 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{name}"
    );
}

/// What `git status` reports in `checkout`, ignored files included.
fn status_with_ignored(checkout: &Path) -> String {
    let output = isolated("git")
        .current_dir(checkout)
        .args(["status", "--porcelain", "--ignored"])
        .output()
        .unwrap();
    success(&output);
    String::from_utf8(output.stdout).unwrap()
}

/// A submodule's checkout sits inside its superproject's working tree, so its
/// worktrees go beside the superproject instead, wherever simgit runs from.
#[test]
fn default_path_in_a_submodule_is_outside_the_superproject() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    let before = status_with_ignored(&fixture.repo);

    let from_checkout = default_parent(&fixture, &sub, "agent/checkout");
    assert_module_root(&fixture, &from_checkout, "sub");
    let nested = sub.join("deeper");
    fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        default_parent(&fixture, &nested, "agent/nested"),
        from_checkout
    );
    let linked = fixture.root.join("sub-linked");
    git(&sub, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    assert_eq!(
        default_parent(&fixture, &linked, "agent/linked"),
        from_checkout
    );

    fs::remove_dir(&nested).unwrap();
    assert_eq!(status_with_ignored(&fixture.repo), before);
}

/// Each superproject worktree has its own module git dir, so the same
/// submodule there gets its own root, beside the superproject's main checkout
/// and outside both working trees.
#[test]
fn default_path_in_a_submodule_of_a_linked_worktree_is_outside_both() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    let linked = fixture.root.join("linked");
    git(
        &fixture.repo,
        &["worktree", "add", "-q", linked.to_str().unwrap()],
    );
    git(
        &linked,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "-q",
            "--init",
        ],
    );
    let before = (
        status_with_ignored(&fixture.repo),
        status_with_ignored(&linked),
    );

    let in_linked = default_parent(&fixture, &linked.join("sub"), "agent/linked");
    assert_module_root(&fixture, &in_linked, "sub");
    let in_main = default_parent(&fixture, &sub, "agent/main");
    assert_module_root(&fixture, &in_main, "sub");
    assert_ne!(in_linked, in_main);
    assert_eq!(
        (
            status_with_ignored(&fixture.repo),
            status_with_ignored(&linked),
        ),
        before
    );
}

/// A submodule of a submodule goes beside the outermost superproject, never
/// beside the intermediate one, which is itself inside a working tree.
#[test]
fn default_path_in_a_nested_submodule_is_beside_the_outermost_superproject() {
    let fixture = Fixture::new();
    let middle = add_submodule_to(&fixture, &fixture.repo, "middle", "a");
    let inner = add_submodule_to(&fixture, &middle, "inner", "b");
    let before = status_with_ignored(&fixture.repo);

    assert_module_root(
        &fixture,
        &default_parent(&fixture, &inner, "agent/inner"),
        "a-b",
    );
    assert_eq!(status_with_ignored(&fixture.repo), before);
}

#[test]
fn same_named_submodules_get_distinct_default_roots() {
    let fixture = Fixture::new();
    let first = add_submodule_to(&fixture, &fixture.repo, "one", "x/lib");
    let second = add_submodule_to(&fixture, &fixture.repo, "two", "y/lib");

    let first = default_parent(&fixture, &first, "agent/first");
    let second = default_parent(&fixture, &second, "agent/second");
    assert_module_root(&fixture, &first, "x-lib");
    assert_module_root(&fixture, &second, "y-lib");
}

#[test]
fn default_path_with_separate_git_dir() {
    let fixture = Fixture::new();
    let checkout = fixture.root.join("checkout");
    let git_dir = fixture.root.join("store");
    git(
        &fixture.root,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            git_dir.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    git(&checkout, &["config", "user.email", "test@example.com"]);
    git(&checkout, &["config", "user.name", "Test User"]);
    fs::write(checkout.join("README.md"), "separate\n").unwrap();
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-qm", "initial"]);
    assert_eq!(
        default_parent(&fixture, &checkout, "agent/edit"),
        fixture
            .root
            .canonicalize()
            .unwrap()
            .join(".simgit/checkout")
    );
    // The same from a subdirectory, where the checkout's `.git` is a level up.
    let sub = checkout.join("sub");
    fs::create_dir(&sub).unwrap();
    assert_eq!(
        default_parent(&fixture, &sub, "agent/deeper"),
        fixture
            .root
            .canonicalize()
            .unwrap()
            .join(".simgit/checkout")
    );
    let doctor: serde_json::Value = serde_json::from_slice(
        &fixture
            .command()
            .current_dir(&sub)
            .args(["--json", "doctor"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(
        doctor["repository_details"]["is_main_worktree"], true,
        "{doctor}"
    );
}

#[test]
fn doctor_is_main_worktree_in_a_submodule() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    let doctor = |dir: &Path| {
        let output = fixture
            .command()
            .current_dir(dir)
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        success(&output);
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let report = doctor(&sub);
    assert_eq!(report["repository_details"]["is_main_worktree"], true);
    assert_eq!(
        report["repository_details"]["common_git_owner"],
        sub.canonicalize().unwrap().display().to_string()
    );
    assert_module_root(
        &fixture,
        Path::new(report["default_worktree_root"].as_str().unwrap()),
        "sub",
    );
    assert_eq!(report["default_worktree_root_inside_repository"], false);
    // `list --json` names the checkout, not the git dir Git reports for it.
    assert_eq!(
        first_listed(&fixture, &sub)["worktree"],
        sub.canonicalize().unwrap().display().to_string()
    );

    let linked = fixture.root.join("sub-linked");
    git(&sub, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    assert_eq!(
        doctor(&linked)["repository_details"]["is_main_worktree"],
        false
    );
}

/// `remove` knows the main checkout from the registry's first entry, which Git
/// reports as a submodule's git dir; the guard must still recognise it.
#[test]
fn remove_commit_refuses_a_submodules_main_checkout() {
    let fixture = Fixture::new();
    let sub = add_submodule(&fixture);
    git(&sub, &["config", "user.email", "test@example.com"]);
    git(&sub, &["config", "user.name", "Test User"]);
    fs::write(sub.join("scratch.txt"), "work in progress\n").unwrap();
    let head = rev_parse(&sub, "HEAD");
    let target = sub.canonicalize().unwrap().display().to_string();

    for args in [
        vec!["remove", "--commit"],
        vec!["remove", &target, "--commit"],
    ] {
        let output = fixture
            .command()
            .current_dir(&sub)
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("main checkout"), "{args:?}: {stderr}");
        assert_eq!(rev_parse(&sub, "HEAD"), head, "{args:?} committed");
    }
    assert!(sub.join("scratch.txt").is_file());
}

/// The first entry of `list --json` run from `dir`: the main worktree.
fn first_listed(fixture: &Fixture, dir: &Path) -> serde_json::Value {
    let output = fixture
        .command()
        .current_dir(dir)
        .args(["--json", "list"])
        .output()
        .unwrap();
    success(&output);
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    entries.into_iter().next().unwrap()
}

/// `doctor --json` run from `dir`.
fn doctor_report(fixture: &Fixture, dir: &Path) -> serde_json::Value {
    let output = fixture
        .command()
        .current_dir(dir)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

/// From a linked worktree of a layout with no main checkout to find, the main
/// worktree is what Git lists first, and worktrees go beside it.
fn assert_fallback_main(fixture: &Fixture, linked: &Path, main: &Path) {
    let main = main.canonicalize().unwrap();
    let report = doctor_report(fixture, linked);
    assert_eq!(report["repository_details"]["is_main_worktree"], false);
    assert_eq!(
        report["repository_details"]["common_git_owner"],
        main.display().to_string()
    );
    assert_eq!(
        Path::new(report["default_worktree_root"].as_str().unwrap()),
        main.parent()
            .unwrap()
            .join(".simgit")
            .join(main.file_name().unwrap())
    );
    assert_eq!(
        first_listed(fixture, linked)["worktree"],
        main.display().to_string()
    );
    // Refused as the main checkout, before its lock state is even asked.
    let removed = fixture
        .command()
        .current_dir(linked)
        .args(["remove", main.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!removed.status.success(), "removed the main worktree");
    let stderr = String::from_utf8_lossy(&removed.stderr);
    assert!(stderr.contains("is the main checkout"), "{stderr}");
}

/// Clone the fixture bare to `<root>/<path>` and add a linked worktree.
fn bare_with_linked_worktree(fixture: &Fixture, path: &str) -> std::path::PathBuf {
    let bare = fixture.root.join(path);
    let source = fixture.repo.to_str().unwrap();
    git(
        &fixture.root,
        &["clone", "-q", "--bare", source, bare.to_str().unwrap()],
    );
    let linked = fixture.root.join("bare-linked");
    git(&bare, &["worktree", "add", "-q", linked.to_str().unwrap()]);
    linked
}

#[test]
fn a_bare_repository_is_its_own_main_worktree() {
    let fixture = Fixture::new();
    let linked = bare_with_linked_worktree(&fixture, "x.git");
    assert_fallback_main(&fixture, &linked, &fixture.root.join("x.git"));
    assert_eq!(first_listed(&fixture, &linked)["bare"], true);
}

/// A bare repository at `proj/.git` keeps worktrees beside `proj`, as before.
#[test]
fn a_bare_repository_named_dot_git_is_owned_by_its_parent() {
    let fixture = Fixture::new();
    let linked = bare_with_linked_worktree(&fixture, "proj/.git");
    assert_fallback_main(&fixture, &linked, &fixture.root.join("proj"));
}

/// A `--separate-git-dir` repository records no path back to its checkout, so
/// from a linked worktree its git dir stands in for the main worktree.
#[test]
fn a_separate_git_dir_seen_from_a_linked_worktree_is_its_own_main_worktree() {
    let fixture = Fixture::new();
    let checkout = fixture.root.join("checkout");
    let git_dir = fixture.root.join("store");
    git(
        &fixture.root,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            git_dir.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    git(&checkout, &["config", "user.email", "test@example.com"]);
    git(&checkout, &["config", "user.name", "Test User"]);
    fs::write(checkout.join("README.md"), "separate\n").unwrap();
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-qm", "initial"]);
    let linked = fixture.root.join("separate-linked");
    git(
        &checkout,
        &["worktree", "add", "-q", linked.to_str().unwrap()],
    );
    assert_fallback_main(&fixture, &linked, &git_dir);
}

/// A repository inside another's working tree that is not its submodule has
/// no superproject to find, so it keeps the ordinary rule.
#[test]
fn a_nested_repository_that_is_not_a_submodule_keeps_its_own_root() {
    let fixture = Fixture::new();
    let inner = fixture.repo.join("inner");
    fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q"]);
    git(&inner, &["config", "user.email", "test@example.com"]);
    git(&inner, &["config", "user.name", "Test User"]);
    fs::write(inner.join("inner.txt"), "inner\n").unwrap();
    git(&inner, &["add", "."]);
    git(&inner, &["commit", "-qm", "inner"]);
    assert_eq!(
        default_parent(&fixture, &inner, "agent/inner"),
        fixture.repo.canonicalize().unwrap().join(".simgit/inner")
    );
}
