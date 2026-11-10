use std::fs;
use std::path::Path;
use std::process::Command;

mod add;
mod call_counts;
mod deleted_worktrees;
mod doctor_and_output;
mod locks;
mod maintenance;
mod remove;
mod repo_discovery;
mod run;

/// A command with the developer's Git configuration out of reach: a global
/// `init.defaultBranch` or `commit.gpgsign` must not change what a test sees.
fn isolated(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", std::env::temp_dir().join("simgit-test-no-home"))
        .env_remove("XDG_CONFIG_HOME");
    command
}

fn git(repo: &Path, args: &[&str]) {
    let status = isolated("git")
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
        let mut command = isolated(env!("CARGO_BIN_EXE_sg"));
        command
            .current_dir(&self.repo)
            .env("HOME", &self.root)
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

fn running_as_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { geteuid() == 0 }
}

/// Put a `git` wrapper running `script` ahead of the real Git on `PATH`. The
/// script reaches the real Git as `$SIMGIT_TEST_REAL_GIT`; returns the
/// environment a command needs to run under the wrapper.
fn git_wrapper(fixture: &Fixture, script: &str) -> [(&'static str, std::ffi::OsString); 2] {
    use std::os::unix::fs::PermissionsExt;

    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let wrapper_dir = fixture.root.join("bin");
    fs::create_dir(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("git");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\n{script}exec \"$SIMGIT_TEST_REAL_GIT\" \"$@\"\n"),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(wrapper_dir)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    [("PATH", path), ("SIMGIT_TEST_REAL_GIT", real_git.into())]
}

fn rev_parse(dir: &Path, revision: &str) -> String {
    let output = isolated("git")
        .current_dir(dir)
        .args(["rev-parse", revision])
        .output()
        .unwrap();
    success(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn local_refs(repo: &Path) -> String {
    let output = isolated("git")
        .current_dir(repo)
        .args(["for-each-ref", "--format=%(refname)", "refs/"])
        .output()
        .unwrap();
    success(&output);
    String::from_utf8(output.stdout).unwrap()
}

fn add_detached(fixture: &Fixture) -> std::path::PathBuf {
    let output = fixture
        .command()
        .args(["add", "--detach", "--json"])
        .output()
        .unwrap();
    success(&output);
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    std::path::PathBuf::from(created["path"].as_str().unwrap())
}

fn commit_in(worktree: &Path, name: &str) -> String {
    fs::write(worktree.join(name), "result\n").unwrap();
    git(worktree, &["add", "."]);
    git(worktree, &["commit", "-qm", name]);
    rev_parse(worktree, "HEAD")
}

/// A fixture whose repository uses the reftable ref format, or `None` on a
/// Git older than 2.45, which has none. Under reftable every registration's
/// `HEAD` file is a stub naming `refs/heads/.invalid`, detached or not.
fn reftable_fixture() -> Option<Fixture> {
    let version = isolated("git").arg("version").output().unwrap();
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    let mut parts = version
        .trim()
        .trim_start_matches("git version ")
        .split(|c: char| !c.is_ascii_digit())
        .map(|part| part.parse::<u32>().unwrap_or(0));
    if (parts.next()?, parts.next()?) < (2, 45) {
        return None;
    }
    let fixture = Fixture::new();
    fs::remove_dir_all(fixture.repo.join(".git")).unwrap();
    git(&fixture.repo, &["init", "-q", "--ref-format=reftable"]);
    git(&fixture.repo, &["config", "user.email", "test@example.com"]);
    git(&fixture.repo, &["config", "user.name", "Test User"]);
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "initial"]);
    Some(fixture)
}

fn commit_exists(repo: &Path, commit: &str) -> bool {
    isolated("git")
        .current_dir(repo)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .status()
        .unwrap()
        .success()
}

/// The HEAD Git's registry records for `worktree`, or `None` when it is not
/// registered. Works after the directory is gone.
fn registered_head(fixture: &Fixture, worktree: &Path) -> Option<String> {
    let output = isolated("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .unwrap();
    success(&output);
    let wanted = format!("worktree {}", resolved(worktree).display());
    String::from_utf8(output.stdout)
        .unwrap()
        .split("\0\0")
        .find_map(|record| {
            let mut fields = record.split('\0');
            if fields.next()? != wanted {
                return None;
            }
            fields.find_map(|field| field.strip_prefix("HEAD ").map(str::to_owned))
        })
}

/// `path` with its parent resolved, as Git records it. A parent that is gone
/// too (removal drops empty `.simgit` scaffolding) leaves `path` as given.
fn resolved(path: &Path) -> std::path::PathBuf {
    match path.parent().unwrap().canonicalize() {
        Ok(parent) => parent.join(path.file_name().unwrap()),
        Err(_) => path.to_path_buf(),
    }
}

/// Wait for a wrapper to create `marker`, failing rather than hanging when it
/// never runs.
fn wait_for(marker: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            marker.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A pid that named a process which has exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Create a worktree on `branch` at `path` with `run`.
fn create_at(fixture: &Fixture, branch: &str, path: &Path, extra: &[&str]) {
    let output = fixture
        .command()
        .args(["run", branch, "--path"])
        .arg(path)
        .args(extra)
        .args(["--", "true"])
        .output()
        .unwrap();
    success(&output);
}

fn registration(fixture: &Fixture, worktree: &Path) -> String {
    let dir = fixture.repo.join(".git/worktrees");
    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap().path();
        let recorded = fs::read_to_string(entry.join("gitdir")).unwrap();
        if Path::new(recorded.trim()).parent() == Some(worktree.canonicalize().unwrap().as_path()) {
            return entry.display().to_string();
        }
    }
    panic!("no registration for {worktree:?}");
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    isolated("git")
        .current_dir(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap()
        .status
        .success()
}

/// Add `<fixture.root>/lib` to the fixture as the submodule `sub` and return
/// its checkout. Its git dir is `.git/modules/sub`, named nothing like `.git`.
fn add_submodule(fixture: &Fixture) -> std::path::PathBuf {
    add_submodule_to(fixture, &fixture.repo, "lib", "sub")
}

/// Add a new one-commit repository `<fixture.root>/<source>` to `superproject`
/// as a submodule at `path`, commit it there, and return its checkout.
fn add_submodule_to(
    fixture: &Fixture,
    superproject: &Path,
    source: &str,
    path: &str,
) -> std::path::PathBuf {
    let lib = fixture.root.join(source);
    fs::create_dir_all(&lib).unwrap();
    git(&lib, &["init", "-q"]);
    git(&lib, &["config", "user.email", "test@example.com"]);
    git(&lib, &["config", "user.name", "Test User"]);
    fs::write(lib.join("lib.txt"), "lib\n").unwrap();
    git(&lib, &["add", "."]);
    git(&lib, &["commit", "-qm", "lib"]);
    let lib = lib.to_str().unwrap();
    git(
        superproject,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            lib,
            path,
        ],
    );
    git(superproject, &["config", "user.email", "test@example.com"]);
    git(superproject, &["config", "user.name", "Test User"]);
    git(superproject, &["commit", "-qm", "add submodule"]);
    superproject.join(path)
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = isolated("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A linked worktree two commits ahead of the main checkout's HEAD.
fn linked_worktree_ahead(fixture: &Fixture) -> std::path::PathBuf {
    let linked = fixture.root.join("linked");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    for name in ["one", "two"] {
        fs::write(linked.join(name), name).unwrap();
        git(&linked, &["add", "."]);
        git(&linked, &["commit", "-qm", name]);
    }
    linked
}

fn run_from(fixture: &Fixture, dir: &Path, args: &[&str]) -> std::process::Output {
    fixture
        .command()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}
