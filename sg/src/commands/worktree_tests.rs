use super::*;
use std::sync::{Arc, Barrier};

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
    add_git_worktree(&repo, "exp/eph", &eph, &base)?;
    mark_ephemeral(&eph)?;
    let keep = fixture.root.join("keep");
    add_git_worktree(&repo, "exp/keep", &keep, &base)?;
    let other = fixture.root.join("other");
    add_git_worktree(&repo, "feat/other", &other, &base)?;
    mark_ephemeral(&other)?;

    let args = WorktreeGc {
        ephemeral: true,
        prefix: Some("exp".to_owned()),
        older_than: "0s".to_owned(),
        ..Default::default()
    };
    let outcome = run_gc(&repo, &args)?;
    assert_eq!(outcome.reaped, vec![eph]);
    assert!(outcome.skipped.is_empty());

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
fn gc_skips_dirty_worktrees_without_force() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let dirty = fixture.root.join("dirty");
    add_git_worktree(&repo, "dirty", &dirty, &base)?;
    fs::write(dirty.join("scratch.txt"), "uncommitted")?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            ..Default::default()
        },
    )?;
    assert!(outcome.reaped.is_empty());
    assert_eq!(outcome.skipped, vec![(dirty.clone(), "dirty")]);

    let forced = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
            force: true,
            ..Default::default()
        },
    )?;
    assert_eq!(forced.reaped, vec![dirty]);
    Ok(())
}

#[test]
fn gc_deletes_merged_branches_when_requested() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("merged");
    add_git_worktree(&repo, "agent/merged", &target, &base)?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
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
fn gc_retains_unmerged_branches_without_force() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("unmerged");
    add_git_worktree(&repo, "agent/unmerged", &target, &base)?;
    fs::write(target.join("agent.txt"), "result")?;
    run_git_at(&target, ["add", "."])?;
    run_git_at(&target, ["commit", "-q", "-m", "agent result"])?;

    let outcome = run_gc(
        &repo,
        &WorktreeGc {
            older_than: "0s".to_owned(),
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
fn worktree_paths_with_spaces_are_supported() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let base = resolve_commit(&repo, "HEAD")?;
    let target = fixture.root.join("path with spaces");
    add_git_worktree(&repo, "spaces", &target, &base)?;
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
    add_git_worktree(&repo, "wt", &worktree, &base)?;
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
fn native_worktree_fallback_is_clean_and_registered() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let target = fixture.root.join("fallback");
    let base = resolve_commit(&repo, "HEAD")?;
    add_git_worktree(&repo, "fallback-test", &target, &base)?;
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
            cow::ensure_baseline(&repo, &commit)
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
fn pruning_preserves_baselines_used_by_active_overlays() -> Result<()> {
    let fixture = Fixture::new()?;
    let repo = discover_repo(&fixture.repo)?;
    let commit = resolve_commit(&repo, "HEAD")?;
    let baseline = cow::ensure_baseline(&repo, &commit)?;
    let protected = HashSet::from([baseline.clone()]);
    assert_eq!(
        cow::prune_baselines(&repo.common_git_dir, true, &protected)?,
        0
    );
    assert!(baseline.is_dir());
    assert_eq!(
        cow::prune_baselines(&repo.common_git_dir, true, &HashSet::new())?,
        1
    );
    assert!(!baseline.exists());
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
    add_cow_worktree(&repo, "cow-test", &target, &base)?;
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
        std::os::unix::fs::symlink("file.txt", repo.join("file-link"))?;
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
