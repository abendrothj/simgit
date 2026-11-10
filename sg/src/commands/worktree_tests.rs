use super::*;
use std::collections::HashSet;
use std::sync::{Arc, Barrier};

/// A checkout-backed allocation through to its unlock, as `create_worktree`
/// finishes it, without the markers.
fn add_git_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
    sparse: &[String],
) -> Result<()> {
    allocate_git_worktree(repo, branch, target, base, kind, sparse)?;
    release_allocation_lock(repo, target)
}

/// `add_git_worktree` for the CoW backend.
fn add_cow_worktree(
    repo: &RepoContext,
    branch: &str,
    target: &Path,
    base: &str,
    kind: WorktreeKind,
    sparse: &[String],
) -> Result<()> {
    allocate_cow_worktree(repo, branch, target, base, kind, sparse)?;
    release_allocation_lock(repo, target)
}

#[test]
fn branch_names_become_unique_safe_path_components() {
    let nested = safe_path_component("feat/auth");
    assert!(nested.starts_with("feat-auth-"));
    assert_ne!(nested, safe_path_component("feat-auth"));
    assert!(!safe_path_component("../../escape").contains('/'));
    assert!(safe_path_component("..").starts_with("branch-"));
}

#[test]
fn parse_duration_accepts_units_and_rejects_overflow() {
    assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
    assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
    assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
    assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
    assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
    assert!(parse_duration("5w").is_err());
    assert!(parse_duration("abc").is_err());
    assert!(parse_duration("18446744073709551615d").is_err());
}

#[test]
fn gc_reaps_ephemeral_and_by_prefix_but_spares_others() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;

    let eph = fixture.root.join("eph");
    add_git_worktree(&repo, "exp/eph", &eph, &base, WorktreeKind::NewBranch, &[])?;
    mark_ephemeral(&eph)?;
    let keep = fixture.root.join("keep");
    add_git_worktree(
        &repo,
        "exp/keep",
        &keep,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let other = fixture.root.join("other");
    add_git_worktree(
        &repo,
        "feat/other",
        &other,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    mark_ephemeral(&other)?;

    // Ephemeral-only is the default selection; nothing has to ask for it.
    let args = WorktreeGc {
        prefix: Some("exp".to_owned()),
        older_than: "0s".to_owned(),
        ..Default::default()
    };
    let outcome = run_gc(&repo, &args)?;
    assert_eq!(outcome.reaped, vec![eph]);
    assert_eq!(outcome.skipped, vec![(keep.clone(), "persistent")]);

    let branches: Vec<_> = list_worktrees(&repo)?
        .iter()
        .filter_map(|worktree| worktree.branch.clone())
        .collect();
    assert!(branches
        .iter()
        .any(|branch| branch == "refs/heads/exp/keep"));
    assert!(branches
        .iter()
        .any(|branch| branch == "refs/heads/feat/other"));
    assert!(!branches.iter().any(|branch| branch == "refs/heads/exp/eph"));
    Ok(())
}

#[test]
fn gc_skips_dirty_worktrees_without_discard_dirty() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let dirty = fixture.root.join("dirty");
    add_git_worktree(&repo, "dirty", &dirty, &base, WorktreeKind::NewBranch, &[])?;
    fs::write(dirty.join("scratch.txt"), "uncommitted")?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            include_persistent: true,
            ..Default::default()
        },
    )?;
    assert!(outcome.reaped.is_empty());
    assert_eq!(outcome.skipped, vec![(dirty.clone(), "dirty")]);

    let discarded = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            include_persistent: true,
            discard_dirty: true,
            ..Default::default()
        },
    )?;
    assert_eq!(discarded.reaped, vec![dirty]);
    Ok(())
}

fn resolve_commit_at(worktree: &Path) -> Result<String> {
    let output = git_output_at(worktree, ["rev-parse", "HEAD"])?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// A detached worktree's commits live only in its HEAD; reaping it would
/// leave them dangling, and no flag permits that.
#[test]
fn gc_skips_a_detached_worktree_whose_commits_no_ref_reaches() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let detached = fixture.root.join("detached");
    add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
    mark_ephemeral(&detached)?;
    fs::write(detached.join("agent.txt"), "result")?;
    run_git_at(&detached, ["add", "."])?;
    run_git_at(&detached, ["commit", "-q", "-m", "agent result"])?;
    let head = resolve_commit_at(&detached)?;

    for discard_dirty in [false, true] {
        let outcome = run_gc(
            &repo,
            &WorktreeGc {
                older_than: "0s".to_owned(),
                discard_dirty,
                ..Default::default()
            },
        )?;
        assert!(outcome.reaped.is_empty());
        assert_eq!(
            outcome.skipped,
            vec![(detached.clone(), "unreached-commits")]
        );
    }
    assert!(detached.is_dir());
    assert_eq!(resolve_commit_at(&detached)?, head);
    Ok(())
}

#[test]
fn gc_reaps_a_detached_worktree_at_a_reached_commit() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let detached = fixture.root.join("detached");
    add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
    mark_ephemeral(&detached)?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            ..Default::default()
        },
    )?;
    assert_eq!(outcome.reaped, vec![detached]);
    assert!(outcome.skipped.is_empty());
    Ok(())
}

/// A workspace created moments ago is the one a user is most likely to ask
/// about, and the documented recipe (`gc --older-than 1h`) always spares it.
/// Silence there reads as a bug in gc; the age filter has to say so.
#[test]
fn gc_reports_worktrees_it_spared_as_recently_active() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let fresh = fixture.root.join("fresh");
    add_git_worktree(
        &repo,
        "agent/fresh",
        &fresh,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    mark_ephemeral(&fresh)?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "1h".to_owned(),
            ..Default::default()
        },
    )?;
    assert!(outcome.reaped.is_empty());
    assert_eq!(outcome.skipped, vec![(fresh.clone(), "recently-active")]);
    assert!(fresh.is_dir());
    Ok(())
}

#[test]
fn gc_deletes_merged_branches_when_requested() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("merged");
    add_git_worktree(
        &repo,
        "agent/merged",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            include_persistent: true,
            delete_branches: true,
            ..Default::default()
        },
    )?;
    assert_eq!(outcome.reaped, vec![target]);
    assert!(outcome.retained_branches.is_empty());
    assert_eq!(outcome.deleted_branches, vec!["agent/merged"]);
    assert!(!branch_exists(&repo, "agent/merged")?);
    Ok(())
}

#[test]
fn gc_retains_unmerged_branches_without_delete_unmerged() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("unmerged");
    add_git_worktree(
        &repo,
        "agent/unmerged",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    fs::write(target.join("agent.txt"), "result")?;
    run_git_at(&target, ["add", "."])?;
    run_git_at(&target, ["commit", "-q", "-m", "agent result"])?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            include_persistent: true,
            delete_branches: true,
            ..Default::default()
        },
    )?;
    assert_eq!(outcome.reaped, vec![target]);
    assert_eq!(outcome.retained_branches, vec!["agent/unmerged"]);
    assert!(branch_exists(&repo, "agent/unmerged")?);
    Ok(())
}

#[test]
fn gc_deletes_unmerged_branches_when_permitted() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("unmerged");
    add_git_worktree(
        &repo,
        "agent/unmerged",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    fs::write(target.join("agent.txt"), "result")?;
    run_git_at(&target, ["add", "."])?;
    run_git_at(&target, ["commit", "-q", "-m", "agent result"])?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            include_persistent: true,
            delete_branches: true,
            delete_unmerged: true,
            ..Default::default()
        },
    )?;
    assert_eq!(outcome.reaped, vec![target]);
    assert!(outcome.retained_branches.is_empty());
    assert_eq!(outcome.deleted_branches, vec!["agent/unmerged"]);
    assert!(!branch_exists(&repo, "agent/unmerged")?);
    Ok(())
}

#[test]
fn gc_refuses_delete_unmerged_without_delete_branches() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("kept");
    add_git_worktree(
        &repo,
        "agent/kept",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            include_persistent: true,
            delete_unmerged: true,
            ..Default::default()
        },
    );
    let error = match outcome {
        Ok(_) => panic!("--delete-unmerged alone must be refused"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("--delete-branches"),
        "error must name the missing flag: {error}"
    );
    assert!(target.is_dir(), "nothing may be reaped before validation");
    assert!(branch_exists(&repo, "agent/kept")?);
    Ok(())
}

#[test]
fn worktree_paths_with_spaces_are_supported() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("path with spaces");
    add_git_worktree(
        &repo,
        "spaces",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    ensure_clean(&target)?;
    assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
    Ok(())
}

#[test]
fn overlay_marker_round_trips_through_common_admin_dir() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let worktree = fixture.root.join("wt");
    add_git_worktree(&repo, "wt", &worktree, &base, WorktreeKind::NewBranch, &[])?;
    assert!(overlay::state(&repo, &worktree).is_none());

    let state = overlay::State {
        overlay_dir: fixture.root.join("overlays/abc"),
        lower: Some(fixture.root.join("baseline")),
    };
    overlay::write_marker(&worktree_admin_dir(&worktree)?, &state)?;
    assert_eq!(overlay::state(&repo, &worktree), Some(state));

    let saved_gitlink = fixture.root.join("saved-gitlink");
    fs::rename(worktree.join(".git"), &saved_gitlink)?;
    let registrations = overlay::registrations(&repo);
    assert!(registrations.iter().any(|(path, _)| path == &worktree));
    assert_eq!(
        overlay::branch(&repo, &worktree).as_deref(),
        Some("refs/heads/wt")
    );
    assert_eq!(
        overlay::worktree_for_branch(&repo, "wt"),
        Some(worktree.clone())
    );
    fs::rename(saved_gitlink, worktree.join(".git"))?;
    Ok(())
}

#[test]
fn admin_dir_does_not_escape_to_the_common_git_dir_when_unmounted() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    // Mirrors production overlay worktrees, whose mountpoint lives inside the
    // common git dir itself (e.g. `.git/simgit/worktrees/<name>`).
    let worktree = repo.common_git_dir.join("simgit/worktrees/nested");
    add_git_worktree(
        &repo,
        "nested",
        &worktree,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let admin = overlay::admin_dir(&repo, &worktree).expect("admin dir while mounted");
    assert_ne!(admin, repo.common_git_dir);

    // An unmounted overlay exposes an empty mountpoint. Git's directory
    // discovery would otherwise climb past it and find the outer repo.
    fs::remove_file(worktree.join(".git"))?;
    assert_eq!(overlay::admin_dir(&repo, &worktree), Some(admin));
    Ok(())
}

#[test]
fn overlay_health_requires_the_view_to_reflect_upperdir_data() -> Result<()> {
    let root = std::env::temp_dir().join(format!("simgit-overlay-health-{}", Uuid::new_v4()));
    let upper = root.join("upper");
    let view = root.join("view");
    fs::create_dir_all(upper.join("nested"))?;
    fs::create_dir_all(view.join("nested"))?;
    fs::write(upper.join("nested/result.txt"), "agent result")?;
    fs::write(view.join("nested/result.txt"), "agent result")?;
    assert!(overlay::upper_visible(&upper, &view));
    fs::write(view.join("nested/result.txt"), "stale result")?;
    assert!(!overlay::upper_visible(&upper, &view));
    let original = vec![b'x'; 32 * 1024 + 1];
    fs::write(upper.join("nested/result.txt"), &original)?;
    fs::write(view.join("nested/result.txt"), &original)?;
    assert!(overlay::upper_visible(&upper, &view));

    let mut changed = original.clone();
    changed[16 * 1024 + 1] = b'y';
    fs::write(view.join("nested/result.txt"), &changed)?;
    assert!(!overlay::upper_visible(&upper, &view));
    fs::write(
        view.join("nested/result.txt"),
        &original[..original.len() - 1],
    )?;
    assert!(!overlay::upper_visible(&upper, &view));
    changed = original;
    changed.push(b'y');
    fs::write(view.join("nested/result.txt"), &changed)?;
    assert!(!overlay::upper_visible(&upper, &view));
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn native_worktree_fallback_is_clean_and_registered() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let target = fixture.root.join("fallback");
    let base = resolve_commit(&repo, "HEAD")?;
    add_git_worktree(
        &repo,
        "fallback-test",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    ensure_clean(&target)?;
    let listed = git_output_common(&repo, ["worktree", "list", "--porcelain"])?;
    assert!(String::from_utf8_lossy(&listed.stdout).contains(target.to_string_lossy().as_ref()));
    Ok(())
}

#[test]
fn concurrent_baseline_creation_publishes_one_complete_tree() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let commit = resolve_commit(&repo, "HEAD")?;
    let barrier = Arc::new(Barrier::new(6));
    let mut handles = Vec::new();
    for _ in 0..6 {
        let repo_path = fixture.repo.clone();
        let commit = commit.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || -> Result<PathBuf> {
            let repo = discover_repo(&repo_path)?;
            barrier.wait();
            cow::ensure_baseline(&repo, &commit).map(|baseline| baseline.tree)
        }));
    }
    let paths = handles
        .into_iter()
        .map(|handle| handle.join().expect("baseline thread"))
        .collect::<Result<Vec<_>>>()?;
    assert!(paths.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(fs::read_to_string(paths[0].join("file.txt"))?, "content\n");
    Ok(())
}

#[test]
fn incomplete_published_baseline_is_not_deleted_implicitly() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let commit = resolve_commit(&repo, "HEAD")?;
    let incomplete = state_dir(&repo.common_git_dir)
        .join("baselines")
        .join(&commit);
    fs::create_dir_all(&incomplete)?;
    fs::write(incomplete.join("sentinel"), "do not delete")?;
    assert!(cow::ensure_baseline(&repo, &commit).is_err());
    assert!(incomplete.join("sentinel").is_file());
    Ok(())
}

#[test]
fn prune_leaves_a_baseline_an_allocation_holds() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let output = git_output_common(
        &repo,
        ["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "orphan"],
    )?;
    if !output.status.success() {
        return Err(git_failure("git commit-tree", &output));
    }
    let orphan = String::from_utf8(output.stdout)?.trim().to_owned();

    // An allocation using a baseline holds the cache lock shared, so even one
    // nothing reaches is kept, plain prune says why, and --all refuses.
    let in_use = cow::ensure_baseline(&repo, &orphan)?;
    let tree = in_use.tree.clone();
    let busy = prune_baseline_cache(&repo, false, &HashSet::new)?;
    assert!(busy.cache_busy);
    assert!(busy.removed.is_empty(), "{:?}", busy.removed);
    assert!(prune_baseline_cache(&repo, true, &HashSet::new).is_err());
    assert!(tree.join("file.txt").is_file());

    drop(in_use);
    let freed = prune_baseline_cache(&repo, false, &HashSet::new)?;
    assert!(!freed.cache_busy);
    assert_eq!(freed.removed, vec![orphan]);
    assert!(!tree.exists());
    Ok(())
}

#[test]
fn prune_finishes_deleting_what_a_killed_prune_moved_aside() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    // A fresh `ready` must not make it look like a build in progress.
    let moved = state_dir(&repo.common_git_dir)
        .join("baselines")
        .join(format!(".doomed-{}-1", "a".repeat(40)));
    fs::create_dir_all(moved.join("tree"))?;
    fs::write(moved.join("ready"), "a")?;
    let outcome = prune_baseline_cache(&repo, false, &HashSet::new)?;
    assert!(!moved.exists());
    assert!(outcome.retained.is_empty(), "{:?}", outcome.retained);
    Ok(())
}

#[test]
fn pruning_preserves_baselines_used_by_active_overlays() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let commit = resolve_commit(&repo, "HEAD")?;
    let baseline = cow::ensure_baseline(&repo, &commit)?.tree;
    let protected = || HashSet::from([baseline.clone()]);
    let protected_run = prune_baseline_cache(&repo, true, &protected)?;
    assert!(protected_run.removed.is_empty());
    assert_eq!(protected_run.retained.len(), 1);
    assert!(
        protected_run.retained_bytes > 0,
        "a retained baseline must report its disk cost"
    );
    assert!(baseline.is_dir());
    let full_run = prune_baseline_cache(&repo, true, &HashSet::new)?;
    assert_eq!(full_run.removed.len(), 1);
    assert_eq!(full_run.retained_bytes, 0);
    assert!(!baseline.exists());
    Ok(())
}

#[test]
fn pruning_drops_baselines_nothing_reaches_any_more() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let new_commit = |message: &str| -> Result<String> {
        let output = git_output_common(
            &repo,
            ["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", message],
        )?;
        if !output.status.success() {
            return Err(git_failure("git commit-tree", &output));
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    let on_branch = resolve_commit(&repo, "HEAD")?;
    // What a force-push leaves behind: a commit no ref points at any more.
    let rewritten = new_commit("rewritten")?;
    let detached = new_commit("detached")?;
    let detached_path = fixture.root.join("detached");
    add_git_worktree(
        &repo,
        "",
        &detached_path,
        &detached,
        WorktreeKind::Detached,
        &[],
    )?;
    for commit in [&on_branch, &rewritten, &detached] {
        cow::ensure_baseline(&repo, commit)?;
    }
    // A baseline whose commit Git has since collected.
    let collected = "1".repeat(on_branch.len());
    let collected_dir = state_dir(&repo.common_git_dir)
        .join("baselines")
        .join(&collected);
    fs::create_dir_all(collected_dir.join("tree"))?;
    fs::write(collected_dir.join("ready"), &collected)?;
    // The branch moves on: its old tip is still reachable, as an ancestor.
    git(
        &fixture.repo,
        ["commit", "-q", "--allow-empty", "-m", "advance"],
    )?;

    let sorted = |mut names: Vec<String>| {
        names.sort();
        names
    };
    let first = prune_baseline_cache(&repo, false, &HashSet::new)?;
    assert_eq!(
        sorted(first.removed),
        sorted(vec![rewritten.clone(), collected])
    );
    assert_eq!(
        sorted(first.retained),
        sorted(vec![on_branch.clone(), detached.clone()])
    );

    // Once the detached worktree is gone, nothing reaches its commit.
    git(
        &fixture.repo,
        [
            "worktree",
            "remove",
            detached_path.to_str().context("utf-8 path")?,
        ],
    )?;
    let second = prune_baseline_cache(&repo, false, &HashSet::new)?;
    assert_eq!(second.removed, vec![detached]);
    assert_eq!(second.retained, vec![on_branch]);
    Ok(())
}

#[test]
fn cow_worktree_is_clean_when_filesystem_supports_clones() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
        return Ok(());
    }
    let target = fixture.root.join("cow");
    let base = resolve_commit(&repo, "HEAD")?;
    add_cow_worktree(
        &repo,
        "cow-test",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    ensure_clean(&target)?;
    assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
    assert_eq!(
        fs::read_to_string(target.join("archive-excluded.txt"))?,
        "still part of a checkout\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("filtered.txt"))?,
        "smudged:content\n"
    );
    #[cfg(unix)]
    assert!(fs::symlink_metadata(target.join("file-link"))?
        .file_type()
        .is_symlink());
    Ok(())
}

fn branch_exists(repo: &RepoContext, branch: &str) -> Result<bool> {
    let reference = format!("refs/heads/{branch}");
    let output = git_output_common(repo, ["show-ref", "--verify", "--quiet", &reference])?;
    Ok(output.status.success())
}

#[test]
fn create_worktree_accepts_a_claim_its_caller_holds() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let claim = BranchClaim::acquire(&repo, "held/branch")?;
    let args = WorktreeAdd {
        branch: Some("held/branch".to_owned()),
        detach: false,
        attach: false,
        path: Some(fixture.root.join("held")),
        sparse: Vec::new(),
        base: None,
        require_cow: false,
        ephemeral: false,
    };
    let created = create_worktree(&repo, &fixture.repo, &args, Some(claim))?;
    assert_eq!(created.branch.as_deref(), Some("held/branch"));
    Ok(())
}

#[test]
fn a_blocking_claim_waits_for_the_holder() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = Arc::new(discover_repo(&fixture.repo)?);
    let held = BranchClaim::acquire(&repo, "b")?;
    let started = std::time::Instant::now();
    let waiter = {
        let repo = Arc::clone(&repo);
        std::thread::spawn(move || {
            let claim = BranchClaim::acquire_blocking(&repo, "b");
            (claim.is_ok(), started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    drop(held);
    let (acquired, elapsed) = waiter.join().expect("waiter thread");
    assert!(acquired);
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    Ok(())
}

#[test]
fn resolve_reuses_a_worktree_named_by_a_path_relative_to_cwd() -> Result<()> {
    let _load = SpawnLoad::start();
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let template = |path: Option<PathBuf>| WorktreeAdd {
        branch: Some("rel/branch".to_owned()),
        detach: false,
        attach: false,
        path,
        sparse: Vec::new(),
        base: None,
        require_cow: false,
        ephemeral: false,
    };
    let existing = fixture.root.join("rel-worktree");
    let claim = BranchClaim::acquire(&repo, "rel/branch")?;
    let created = create_worktree(
        &repo,
        &fixture.root,
        &template(Some(existing.clone())),
        Some(claim),
    )?;

    // The process cwd is not fixture.root, so only `cwd` can resolve this.
    // Blocking: a child forked by a parallel test holds the first claim's
    // lock until it execs, so a fail-fast acquire can lose that race.
    let claim = BranchClaim::acquire_blocking(&repo, "rel/branch")?;
    let resolved = launch::resolve_branch_worktree(
        &repo,
        &fixture.root,
        template(Some(PathBuf::from("rel-worktree"))),
        claim,
    )?;
    match resolved {
        launch::Resolved::Existing(path) => assert_eq!(path, created.target),
        launch::Resolved::Created(_) => panic!("expected the existing worktree"),
    }
    Ok(())
}

/// Threads that keep spawning children, as parallel tests and a busy
/// machine do. A child forked while a claim's file is open keeps its lock
/// until it execs, even after the claim is dropped.
struct SpawnLoad {
    stop: Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl SpawnLoad {
    fn start() -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let threads = (0..6)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = std::process::Command::new("true").status();
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for SpawnLoad {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("simgit-worktree-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let repo = root.join("repo");
        fs::create_dir_all(&repo)?;
        git(&repo, ["init", "-q"])?;
        git(&repo, ["config", "user.email", "test@example.com"])?;
        git(&repo, ["config", "user.name", "Test User"])?;
        git(
            &repo,
            [
                "config",
                "filter.simgit.clean",
                "sed 's/^smudged:/stored:/'",
            ],
        )?;
        git(
            &repo,
            [
                "config",
                "filter.simgit.smudge",
                "sed 's/^stored:/smudged:/'",
            ],
        )?;
        fs::write(repo.join("file.txt"), "content\n")?;
        fs::write(repo.join("file with spaces.txt"), "spaces\n")?;
        fs::write(repo.join("filtered.txt"), "smudged:content\n")?;
        fs::write(
            repo.join(".gitattributes"),
            "archive-excluded.txt export-ignore\nfiltered.txt filter=simgit\n",
        )?;
        fs::write(
            repo.join("archive-excluded.txt"),
            "still part of a checkout\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::os::unix::fs::symlink("file.txt", repo.join("file-link"))?;
            fs::write(repo.join("tool.sh"), "#!/bin/sh\n")?;
            fs::set_permissions(repo.join("tool.sh"), fs::Permissions::from_mode(0o755))?;
        }
        git(&repo, ["add", "."])?;
        git(&repo, ["commit", "-q", "-m", "initial"])?;
        Ok(Self { root, repo })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git<const N: usize>(path: &Path, args: [&str; N]) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(args);
    run_command(&mut command, "test git")
}

#[test]
fn adopted_stat_data_equals_gits_own_refresh() -> Result<()> {
    // The baseline index is copied into a clone that has its own inodes and
    // ctimes. Adoption must reproduce, byte for byte, the index Git writes
    // after inspecting every cloned file: anything less and Git either
    // rehashes the whole tree on first use or trusts stale stat data.
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    if !cow::ROOT_CLONE || !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
        return Ok(());
    }
    let base = resolve_commit(&repo, "HEAD")?;
    let baseline = cow::ensure_baseline(&repo, &base)?.tree;
    let published = cow::baseline_index(&baseline).context("baseline publishes an index")?;

    let clone = fixture.root.join("clone");
    cow::clone_root(&baseline, &clone)?;

    let adopted = fixture.root.join("adopted.index");
    fs::copy(&published, &adopted)?;
    index::adopt_stat_data(&adopted, &clone)?;

    let refreshed = fixture.root.join("refreshed.index");
    fs::copy(&published, &refreshed)?;
    let mut refresh = Command::new("git");
    refresh
        .env("GIT_INDEX_FILE", &refreshed)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .arg(format!("--work-tree={}", clone.display()))
        .args(["update-index", "--really-refresh"]);
    run_command(&mut refresh, "git update-index --really-refresh")?;

    assert_eq!(
        fs::read(&adopted)?,
        fs::read(&refreshed)?,
        "adopted stat data differs from Git's own refresh"
    );
    Ok(())
}

#[test]
fn required_reflink_backend_is_available() -> Result<()> {
    if std::env::var_os("SIMGIT_REQUIRE_REFLINK").is_none() {
        return Ok(());
    }
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    assert!(
        cow::clone_supported(&repo.common_git_dir, &fixture.root)?,
        "SIMGIT_REQUIRE_REFLINK=1 but the test filesystem rejected reflink cloning"
    );
    Ok(())
}

#[test]
fn root_clone_failure_restores_empty_worktree_before_fallback() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    if !cow::ROOT_CLONE || !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
        return Ok(());
    }
    let base = resolve_commit(&repo, "HEAD")?;
    let baseline = cow::ensure_baseline(&repo, &base)?.tree;
    let published = cow::baseline_index(&baseline).context("baseline publishes an index")?;
    fs::write(published, b"corrupt index")?;

    let target = fixture.root.join("root-fallback");
    register_worktree(
        &repo,
        "root-fallback",
        &target,
        &base,
        WorktreeKind::NewBranch,
    )?;
    populate_cow_worktree(&target, &baseline)?;

    ensure_clean(&target)?;
    assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
    assert!(target.join(".git").is_file());
    Ok(())
}

#[test]
fn per_file_adopted_stat_data_matches_git_refresh() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
        return Ok(());
    }
    let base = resolve_commit(&repo, "HEAD")?;
    let baseline = cow::ensure_baseline(&repo, &base)?.tree;
    let target = fixture.root.join("per-file-index");
    register_worktree(
        &repo,
        "per-file-index",
        &target,
        &base,
        WorktreeKind::NewBranch,
    )?;
    populate_by_file(&target, &baseline)?;

    let git_dir = PathBuf::from(git_path_output(
        &target,
        ["rev-parse", "--absolute-git-dir"],
    )?);
    let installed = git_dir.join("index");
    let adopted = fixture.root.join("per-file-adopted.index");
    fs::copy(&installed, &adopted)?;
    let refreshed = fixture.root.join("per-file-refreshed.index");
    fs::copy(&installed, &refreshed)?;
    let mut refresh = Command::new("git");
    refresh
        .env("GIT_INDEX_FILE", &refreshed)
        .arg(format!("--git-dir={}", repo.common_git_dir.display()))
        .arg(format!("--work-tree={}", target.display()))
        .args(["update-index", "--really-refresh"]);
    run_command(&mut refresh, "git update-index --really-refresh")?;

    assert_eq!(
        fs::read(&adopted)?,
        fs::read(&refreshed)?,
        "per-file stat adoption differs from Git's own refresh"
    );
    Ok(())
}

#[test]
fn per_file_population_is_clean_and_preserves_file_kinds() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
        return Ok(());
    }
    let base = resolve_commit(&repo, "HEAD")?;
    let baseline = cow::ensure_baseline(&repo, &base)?.tree;
    let target = fixture.root.join("per-file");
    register_worktree(&repo, "per-file", &target, &base, WorktreeKind::NewBranch)?;
    populate_by_file(&target, &baseline)?;
    assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
    assert_eq!(
        fs::read_to_string(target.join("filtered.txt"))?,
        "smudged:content\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(fs::symlink_metadata(target.join("file-link"))?
            .file_type()
            .is_symlink());
        let mode = fs::metadata(target.join("tool.sh"))?.permissions().mode();
        assert_ne!(
            mode & 0o100,
            0,
            "executable bit must survive the per-file clone"
        );
    }
    Ok(())
}

/// Baseline caches published before the stat-refreshed index existed must
/// still populate correctly through the `read-tree` fallback.
#[test]
fn per_file_population_survives_a_baseline_without_published_index() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
        return Ok(());
    }
    let base = resolve_commit(&repo, "HEAD")?;
    let baseline = cow::ensure_baseline(&repo, &base)?.tree;
    let published = cow::baseline_index(&baseline).context("baseline publishes an index")?;
    fs::remove_file(published)?;
    let target = fixture.root.join("pre-index");
    register_worktree(&repo, "pre-index", &target, &base, WorktreeKind::NewBranch)?;
    populate_by_file(&target, &baseline)?;
    assert_eq!(fs::read_to_string(target.join("file.txt"))?, "content\n");
    Ok(())
}

#[test]
fn cow_attachment_and_failed_population_preserve_existing_branch() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    if !cow::clone_supported(&repo.common_git_dir, &fixture.root)? {
        return Ok(());
    }
    let base = resolve_commit(&repo, "HEAD")?;
    run_command(
        git_common_command(&repo).args(["branch", "existing", &base]),
        "git",
    )?;
    let target = fixture.root.join("attached");
    add_cow_worktree(
        &repo,
        "existing",
        &target,
        &base,
        WorktreeKind::ExistingBranch,
        &[],
    )?;
    ensure_clean(&target)?;
    assert_eq!(resolve_commit(&discover_repo(&target)?, "HEAD")?, base);
    remove_worktree_force(&repo, &target)?;

    // Corrupt only the disposable fixture's baseline to force verification failure.
    let baseline = cow::ensure_baseline(&repo, &base)?.tree;
    fs::write(baseline.join("file.txt"), "incorrect baseline")?;
    assert!(add_cow_worktree(
        &repo,
        "existing",
        &target,
        &base,
        WorktreeKind::ExistingBranch,
        &[]
    )
    .is_err());
    assert!(!target.exists());
    assert_eq!(resolve_commit(&repo, "refs/heads/existing")?, base);
    Ok(())
}

#[test]
fn prune_retains_old_unmounted_overlay_registration() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let worktree = fixture.root.join("recoverable");
    add_git_worktree(
        &repo,
        "recoverable",
        &worktree,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let admin = worktree_admin_dir(&worktree)?;
    let overlay_dir = overlay::root(&repo.common_git_dir).join("saved");
    fs::create_dir_all(overlay_dir.join("upper"))?;
    overlay::write_marker(
        &admin,
        &overlay::State {
            overlay_dir,
            lower: None,
        },
    )?;
    fs::remove_file(worktree.join(".git"))?;
    fs::File::open(admin.join("gitdir"))?
        .set_times(fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
    prune_git_worktrees(&repo)?;
    assert!(admin.is_dir());
    assert!(!admin.join("locked").exists());
    assert_eq!(
        overlay::worktree_for_branch(&repo, "recoverable"),
        Some(worktree)
    );
    Ok(())
}

#[test]
fn registry_identifies_main_when_invoked_from_linked_worktree() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("linked");
    add_git_worktree(
        &repo,
        "linked",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let entries = list_worktrees(&discover_repo(&target)?)?;
    assert!(entries[0].is_main);
    assert!(
        !entries
            .iter()
            .find(|entry| entry.path == target)
            .unwrap()
            .is_main
    );
    Ok(())
}

/// Recovery from a stranded lock turns entirely on the lock naming its holder:
/// without that, clearing one is a guess about whether a command is still
/// writing the checkout.
#[test]
fn a_lock_names_its_owner_and_an_exited_owner_is_not_alive() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("locked");
    add_git_worktree(
        &repo,
        "locked",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let path = worktree_lock_path(&repo, &target)?;
    let self_pid = std::process::id() as i32;

    let lock = WorktreeLock::acquire(&repo, &target)?;
    assert_eq!(lock_owner_of(&path), Some(self_pid));
    assert!(process_alive(self_pid));
    assert!(ensure_unlocked(&repo, &target).is_err());

    let refused = match WorktreeLock::acquire(&repo, &target) {
        Ok(_) => panic!("a second launcher acquired a lock this process holds"),
        Err(error) => error.to_string(),
    };
    assert!(
        refused.contains(&format!("pid {self_pid}")) && refused.contains("simgit unlock"),
        "a second launcher is not told who holds the lock: {refused}"
    );
    lock.release()?;
    assert!(ensure_unlocked(&repo, &target).is_ok());

    // A process that has exited holds nothing, so its lock is clearable.
    let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn()?;
    let dead_pid = child.id() as i32;
    child.wait()?;
    assert!(!process_alive(dead_pid));

    // Locks written before simgit recorded a pid have an unknown owner, which
    // must not be mistaken for a live one.
    assert_eq!(lock_owner_pid("simgit: workspace in use\n"), None);
    Ok(())
}

/// `unlock` judges a lock, then removes it. Removing whatever was at the path
/// by then deleted the live lock of a `run` that started after another
/// `unlock` cleared the stranded one, leaving that command unprotected.
#[test]
fn unlock_removes_only_the_lock_it_read() -> Result<()> {
    use super::locks::{read_lock, remove_read_lock};
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("recovered");
    add_git_worktree(
        &repo,
        "recovered",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let path = worktree_lock_path(&repo, &target)?;
    let label = target.display().to_string();
    let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn()?;
    let dead_pid = child.id();
    child.wait()?;
    let strand = || {
        fs::write(
            &path,
            format!("simgit: workspace in use (pid {dead_pid})\n"),
        )
    };

    // No lock read means no lock to remove.
    assert!(read_lock(&path)?.is_none());

    // Cleared by another unlock, then taken by a run: that lock stays.
    strand()?;
    let judged = read_lock(&path)?.expect("a stranded lock");
    fs::remove_file(&path)?;
    let live = WorktreeLock::acquire(&repo, &target)?;
    assert!(remove_read_lock(&path, &judged, &label).is_err());
    assert_eq!(
        lock_owner_of(&path),
        Some(std::process::id() as i32),
        "unlock removed a live run's lock"
    );
    live.release()?;

    // Cleared by another unlock and not retaken: the workspace is unlocked.
    strand()?;
    let judged = read_lock(&path)?.expect("a stranded lock");
    fs::remove_file(&path)?;
    remove_read_lock(&path, &judged, &label)?;
    assert!(!path.exists());

    // Unchanged since it was read: removed.
    strand()?;
    let judged = read_lock(&path)?.expect("a stranded lock");
    remove_read_lock(&path, &judged, &label)?;
    assert!(!path.exists());
    Ok(())
}

/// A lock created empty and written afterwards reads, for an instant, as Git's
/// lock with no reason, which `run` and `unlock` refuse as not simgit's.
#[test]
fn a_lock_is_never_observed_empty() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("watched");
    add_git_worktree(
        &repo,
        "watched",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let path = worktree_lock_path(&repo, &target)?;
    let self_pid = std::process::id() as i32;
    let done = AtomicBool::new(false);
    let (acquired, (reads, ownerless)) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let (mut reads, mut ownerless) = (0, 0);
            while !done.load(Ordering::Relaxed) {
                if let Ok(contents) = fs::read_to_string(&path) {
                    reads += 1;
                    if lock_owner_pid(&contents) != Some(self_pid) {
                        ownerless += 1;
                    }
                }
            }
            (reads, ownerless)
        });
        let acquired = (0..100).try_for_each(|_| WorktreeLock::acquire(&repo, &target)?.release());
        done.store(true, Ordering::Relaxed);
        (acquired, reader.join().expect("reader thread"))
    });
    acquired?;
    assert!(reads > 0, "the reader never saw the lock");
    assert_eq!(
        ownerless, 0,
        "{ownerless} of {reads} reads saw a lock that did not name its owner"
    );

    // Neither a lock taken nor one refused leaves its pending file behind.
    let held = WorktreeLock::acquire(&repo, &target)?;
    assert!(WorktreeLock::acquire(&repo, &target).is_err());
    held.release()?;
    let leftovers: Vec<_> = files_under(path.parent().unwrap())?
        .into_iter()
        .filter(|file| file.to_string_lossy().ends_with(".pending"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    Ok(())
}

/// `run` takes over its new worktree's allocation lock: between the two, no
/// reader may find the worktree unlocked, or a lock that is empty or does not
/// name its owner, or gc could reap the worktree before the command starts.
#[test]
fn taking_over_an_allocation_lock_never_leaves_the_worktree_unlocked() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("handed");
    // Registered and still under its allocation lock, as `create_worktree` returns it.
    allocate_git_worktree(
        &repo,
        "handed",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let path = worktree_lock_path(&repo, &target)?;
    let allocation = fs::read_to_string(&path)?;
    assert!(allocation.starts_with("simgit: allocating"), "{allocation}");
    let self_pid = std::process::id() as i32;
    let done = AtomicBool::new(false);
    let (taken, (reads, unlocked)) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let (mut reads, mut unlocked) = (0, 0);
            while !done.load(Ordering::Relaxed) {
                match fs::read_to_string(&path) {
                    Ok(contents) if lock_owner_pid(&contents) == Some(self_pid) => reads += 1,
                    _ => unlocked += 1,
                }
            }
            (reads, unlocked)
        });
        let taken = (0..100).try_for_each(|_| -> Result<()> {
            let lock = WorktreeLock::take_over_allocation(&repo, &target)?;
            let contents = fs::read_to_string(&path)?;
            assert!(
                contents.starts_with("simgit: workspace in use"),
                "{contents}"
            );
            // Keep the lock file, and put Git's allocation lock back in one step.
            std::mem::forget(lock);
            let staged = path.with_file_name("allocation.staged");
            fs::write(&staged, &allocation)?;
            fs::rename(&staged, &path)?;
            Ok(())
        });
        done.store(true, Ordering::Relaxed);
        (taken, reader.join().expect("reader thread"))
    });
    taken?;
    assert!(reads > 0, "the reader never saw the lock");
    assert_eq!(
        unlocked,
        0,
        "{unlocked} of {} reads found no lock naming its owner",
        reads + unlocked
    );
    let leftovers: Vec<_> = files_under(path.parent().unwrap())?
        .into_iter()
        .filter(|file| file.to_string_lossy().ends_with(".pending"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    Ok(())
}

/// Every file under `dir`, at any depth.
fn files_under(dir: &Path) -> Result<HashSet<PathBuf>> {
    let mut files = HashSet::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(files_under(&path)?);
        } else {
            files.insert(path);
        }
    }
    Ok(files)
}

/// Which file is at `path`, if any.
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// Two allocators that find a path claim left by a killed one used to both
/// conclude it was stale, both unlink it and both retake it, so both went on
/// to allocate the one path.
#[test]
fn two_takeovers_of_a_stale_path_claim_never_both_succeed() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    // Claim files are named for the target alone, so a scratch repository
    // shows which file a claim on it uses without locking the real one.
    let scratch = Fixture::new()?;
    let scratch_repo = discover_repo(&scratch.repo)?;
    let scratch_claims = state_dir(&scratch_repo.common_git_dir).join("claims");
    let claims = state_dir(&repo.common_git_dir).join("claims");
    let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn()?;
    let dead_pid = child.id();
    child.wait()?;

    // What a killed allocator leaves behind: its claim file, naming it.
    let leave_claim = |target: &Path, pid: u32| -> Result<PathBuf> {
        let before = files_under(&scratch_claims)?;
        let probe = PathClaim::acquire(&scratch_repo, target)?;
        let created: Vec<_> = files_under(&scratch_claims)?
            .difference(&before)
            .cloned()
            .collect();
        drop(probe);
        assert_eq!(created.len(), 1, "a claim made {created:?}");
        let claim = claims.join(created[0].strip_prefix(&scratch_claims)?);
        fs::create_dir_all(claim.parent().unwrap())?;
        fs::write(
            &claim,
            format!("simgit: allocating a worktree here (pid {pid})\n"),
        )?;
        Ok(claim)
    };

    for round in 0..50 {
        let target = fixture.root.join(format!("contested-{round}"));
        let stale = leave_claim(&target, dead_pid)?;
        let original = file_identity(&stale);

        let barrier = Barrier::new(2);
        let won = std::thread::scope(|scope| {
            let racers: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        let claim = PathClaim::acquire(&repo, &target);
                        // Hold it until both have tried: only claims held at
                        // the same time compete.
                        barrier.wait();
                        claim.is_ok()
                    })
                })
                .collect();
            racers
                .into_iter()
                .map(|racer| racer.join().expect("racer thread"))
                .filter(|won| *won)
                .count()
        });
        assert_eq!(
            won, 1,
            "round {round}: {won} allocators took one stale claim"
        );
        // The claim is the OS lock on one file that is never replaced: a lock
        // on a replacement would not exclude a holder of the original.
        assert_eq!(
            file_identity(&stale),
            original,
            "round {round}: taking over and releasing the claim replaced its file"
        );

        // A claim file naming a live process, but not locked, holds nothing.
        // It is a fresh target: a child a parallel test forks can hold a
        // lock this process released until it execs.
        let target = fixture.root.join(format!("unlocked-{round}"));
        let unlocked = leave_claim(&target, std::process::id())?;
        let original = file_identity(&unlocked);
        let claim = PathClaim::acquire(&repo, &target);
        assert!(
            claim.is_ok(),
            "round {round}: an unlocked claim file was refused"
        );
        assert_eq!(file_identity(&unlocked), original, "round {round}");
        drop(claim);
        assert_eq!(file_identity(&unlocked), original, "round {round}");
    }
    Ok(())
}

/// Git can delete a worktree's contents and its registration and still fail to
/// unlink the directory itself — a read-only parent is enough. The empty shell
/// that survives is not a worktree, and resolving it as one is what strands a
/// retried cleanup on `fatal: not a git repository`.
#[test]
fn an_unregistered_leftover_directory_does_not_resolve_as_a_worktree() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("leftover");
    add_git_worktree(
        &repo,
        "leftover",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let spec = target.display().to_string();

    match lookup_worktree_target(&repo, Some(&spec))? {
        TargetLookup::Found(path) => assert_eq!(path, canonical_path(&target)),
        _ => panic!("a registered worktree must resolve as found"),
    }

    teardown_worktree(&repo, &target, false, Caller::Cli)?;
    fs::create_dir_all(&target)?;

    match lookup_worktree_target(&repo, Some(&spec))? {
        TargetLookup::Stray(path) => assert_eq!(path, canonical_path(&target)),
        _ => panic!("an unregistered directory must not resolve as a live worktree"),
    }
    Ok(())
}

/// Finishing an interrupted teardown is only safe because the residue is
/// empty. A directory with files in it is somebody's data, whatever its name.
#[test]
fn stray_removal_clears_an_empty_shell_and_refuses_one_holding_files() -> Result<()> {
    let fixture = Fixture::new()?;
    let args = WorktreeRemove {
        target: None,
        commit: false,
        message: "simgit remove".to_owned(),
        discard_dirty: false,
        delete_branch: false,
        delete_unmerged: false,
    };

    let occupied = fixture.root.join("occupied");
    fs::create_dir_all(&occupied)?;
    fs::write(occupied.join("work.txt"), "unexplained")?;
    let refused = remove_stray(&occupied, &args, false).expect_err("files must not be deleted");
    assert!(refused.to_string().contains("not empty"));
    assert!(occupied.join("work.txt").is_file());

    let empty = fixture.root.join("empty");
    fs::create_dir_all(&empty)?;
    remove_stray(&empty, &args, false)?;
    assert!(!empty.exists());
    Ok(())
}

/// A commit that lands after the caller's own check must not slip through:
/// teardown itself refuses a clean detached worktree whose HEAD no ref reaches.
#[test]
fn teardown_refuses_a_clean_detached_worktree_with_unreached_commits() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let detached = fixture.root.join("detached");
    add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
    teardown_worktree(&repo, &detached, false, Caller::Cli)
        .expect("a fresh detached worktree is reached");

    add_git_worktree(&repo, "", &detached, &base, WorktreeKind::Detached, &[])?;
    fs::write(detached.join("agent.txt"), "result")?;
    run_git_at(&detached, ["add", "."])?;
    run_git_at(&detached, ["commit", "-q", "-m", "agent result"])?;
    let head = resolve_commit_at(&detached)?;

    for discard_dirty in [false, true] {
        let error = teardown_worktree(&repo, &detached, discard_dirty, Caller::Cli).unwrap_err();
        assert!(format!("{error:#}").contains("branch <name>"), "{error:#}");
    }
    assert!(detached.is_dir());
    assert_eq!(resolve_commit_at(&detached)?, head);
    Ok(())
}

/// Git 2.48+ under `worktree.useRelativePaths` records a worktree relative to
/// its admin dir. Written by hand here, so it runs on any Git: a deleted
/// worktree's registration and lock must still be found by its path.
#[test]
fn a_relative_gitdir_still_finds_a_deleted_worktrees_lock() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("relative");
    add_git_worktree(
        &repo,
        "relative",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let admin = overlay::admin_dir(&repo, &target).context("no admin dir")?;
    fs::remove_dir_all(&target)?;
    fs::write(admin.join("gitdir"), "../../../../relative/.git\n")?;

    assert_eq!(overlay::admin_dir(&repo, &target), Some(admin.clone()));
    assert!(ensure_unlocked(&repo, &target).is_ok());
    fs::write(admin.join("locked"), "usb")?;
    let refused = ensure_unlocked(&repo, &target).unwrap_err().to_string();
    assert!(refused.contains("usb"), "{refused}");
    Ok(())
}

/// A `locked` symlink to nothing is a lock to Git, which then cannot read its
/// reason or unlock it either; the advice must be the one thing that works.
#[test]
fn a_dangling_lock_is_refused_with_advice_that_works() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("dangling");
    add_git_worktree(
        &repo,
        "dangling",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let lock = worktree_lock_path(&repo, &target)?;
    std::os::unix::fs::symlink(fixture.root.join("nowhere"), &lock)?;
    let advice = format!("rm {}", lock.display());

    let checked = ensure_unlocked(&repo, &target).unwrap_err().to_string();
    assert!(checked.contains(&advice), "{checked}");
    let acquired = match WorktreeLock::acquire(&repo, &target) {
        Ok(_) => panic!("acquired a workspace Git counts as locked"),
        Err(error) => error.to_string(),
    };
    assert!(acquired.contains(&advice), "{acquired}");
    assert!(fs::symlink_metadata(&lock).is_ok());
    Ok(())
}

/// An absolute `gitdir` spelled through a symlink names the same worktree as
/// its real path, as Git's own path lookup treats it.
#[test]
fn a_gitdir_spelled_through_a_symlink_still_finds_the_lock() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("aliased");
    add_git_worktree(
        &repo,
        "aliased",
        &target,
        &base,
        WorktreeKind::NewBranch,
        &[],
    )?;
    let admin = overlay::admin_dir(&repo, &target).context("no admin dir")?;
    fs::remove_dir_all(&target)?;
    let alias = fixture.root.join("alias");
    std::os::unix::fs::symlink(&fixture.root, &alias)?;
    fs::write(
        admin.join("gitdir"),
        format!("{}\n", alias.join("aliased/.git").display()),
    )?;

    assert_eq!(overlay::admin_dir(&repo, &target), Some(admin.clone()));
    assert_eq!(
        overlay::admin_dir(&repo, &alias.join("aliased")),
        Some(admin.clone())
    );
    assert!(ensure_unlocked(&repo, &target).is_ok());
    fs::write(admin.join("locked"), "usb")?;
    assert!(ensure_unlocked(&repo, &target).is_err());
    Ok(())
}
