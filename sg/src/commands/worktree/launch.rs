//! Command launch and workspace selection, independent of the chosen agent.

use super::locks::held_by_live_simgit;
use super::{
    admin_dirs, canonical_path, check_creation_args, create_worktree, deleted_worktree_settings,
    discover_repo, drop_deleted_registration, ensure_unlocked, git_failure, git_output_common,
    list_worktrees, mark_ephemeral, nearest_existing_directory, overlay, refuse_occupied,
    refuse_sparse_overlay, refuse_unfinished, registration_gone, remove_file_if_present,
    select_populate_mode, shell_quote, short_branch, worktree_admin_dir, worktree_description,
    BranchClaim, Caller, CreatedWorktree, RepoContext, WorktreeAdd, WorktreeEntry, WorktreeLock,
};
use crate::{kill, signal};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::ffi::{c_int, OsString};
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicI32, Ordering};

#[derive(Args)]
pub struct WorktreeRun {
    /// Branch to create or reuse. Omit to choose an existing workspace.
    pub branch: Option<String>,

    /// Worktree path. Defaults to a slug of the branch under
    /// `../.simgit/<repo>/`.
    #[arg(long)]
    pub path: Option<PathBuf>,

    /// Commit-ish to start from. Defaults to HEAD.
    #[arg(long)]
    pub base: Option<String>,

    /// Check out only these directories (Git cone-mode sparse checkout).
    /// Repeatable, and creation-only like `--base`.
    #[arg(long = "sparse", value_name = "DIR")]
    pub sparse: Vec<String>,

    /// Fail instead of using a normal Git checkout when CoW is unavailable.
    #[arg(long)]
    pub require_cow: bool,

    /// Explicitly keep the workspace persistent (the default for new worktrees).
    #[arg(long, conflicts_with = "ephemeral")]
    pub persistent: bool,

    /// Mark the workspace as disposable for GC.
    #[arg(long)]
    pub ephemeral: bool,

    /// Command and arguments to execute in the worktree.
    #[arg(required = true, last = true)]
    pub command: Vec<OsString>,
}

/// What `resolve_branch_worktree` found or made for a branch.
pub(super) enum Resolved {
    Existing(PathBuf),
    Created(CreatedWorktree),
}

/// Reject creation-only options on a reused workspace, and a `--path` that
/// names a different directory than the one registered.
fn check_reuse(
    cwd: &Path,
    template: &WorktreeAdd,
    branch: Option<&str>,
    target: &Path,
) -> Result<()> {
    if template.base.is_some() || template.require_cow || !template.sparse.is_empty() {
        bail!("--base, --require-cow and --sparse apply only when creating a worktree");
    }
    if let Some(path) = &template.path {
        // The given path is often the mistake being reported, so it may
        // not exist at all: compare resolved forms where possible and fall
        // back to the literal ones rather than failing with a bare errno.
        let path = &cwd.join(path);
        let given = path.canonicalize().unwrap_or_else(|_| path.clone());
        let registered = target
            .canonicalize()
            .unwrap_or_else(|_| target.to_path_buf());
        if given != registered {
            let owner = match branch {
                Some(name) => format!("branch '{name}'"),
                None => "that workspace".to_owned(),
            };
            bail!(
                "--path {} does not match the workspace registered for {owner}: {}\n\
                 pass --path {} instead, or omit --path to reuse it",
                path.display(),
                target.display(),
                target.display()
            );
        }
    }
    Ok(())
}

/// Reuse the worktree registered for `template.branch`, or create one, all
/// under `claim` so that a concurrent caller cannot create it in between. One
/// deleted by hand is created again, at its old path unless `--path` names
/// another, once dropping its registration loses nothing.
pub(super) fn resolve_branch_worktree(
    repo: &RepoContext,
    cwd: &Path,
    template: WorktreeAdd,
    claim: BranchClaim,
) -> Result<Resolved> {
    let branch = template
        .branch
        .as_deref()
        .context("branch is required to create a workspace")?;
    let wanted = format!("refs/heads/{branch}");
    let registered = list_worktrees(repo)?
        .into_iter()
        .find(|entry| entry.branch.as_deref() == Some(wanted.as_str()));
    resolve_registered(repo, cwd, template, claim, registered, Caller::Cli)
}

/// `resolve_branch_worktree` once the caller has read, under `claim`, the
/// registered worktree that answers for `template.branch`, if any. Refusals
/// name fixes in `caller`'s terms.
pub(super) fn resolve_registered(
    repo: &RepoContext,
    cwd: &Path,
    template: WorktreeAdd,
    claim: BranchClaim,
    registered: Option<WorktreeEntry>,
    caller: Caller,
) -> Result<Resolved> {
    let branch = template
        .branch
        .clone()
        .context("branch is required to create a workspace")?;
    if let Some(entry) = &registered {
        if !entry.is_main && registration_gone(repo, &entry.path) {
            return reattach(repo, cwd, entry, &template, Some(claim), caller)
                .map(Resolved::Created);
        }
    }
    let existing = registered
        .map(|entry| entry.path)
        .or_else(|| overlay::worktree_for_branch(repo, &branch));
    if let Some(target) = existing {
        check_reuse(cwd, &template, Some(&branch), &target)?;
        return Ok(Resolved::Existing(target));
    }
    let reference = format!("refs/heads/{branch}");
    let exists = git_output_common(repo, ["show-ref", "--verify", "--quiet", &reference])?;
    let attach = match exists.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => return Err(git_failure("look up branch", &exists)),
    };
    let created = create_worktree(repo, cwd, &WorktreeAdd { attach, ..template }, Some(claim))?;
    Ok(Resolved::Created(created))
}

/// Create again the worktree `entry` names, which was deleted by hand: on its
/// branch, or detached at its HEAD, at its old path unless `template.path`
/// names another.
fn reattach(
    repo: &RepoContext,
    cwd: &Path,
    entry: &WorktreeEntry,
    template: &WorktreeAdd,
    claim: Option<BranchClaim>,
    caller: Caller,
) -> Result<CreatedWorktree> {
    let refused = || {
        format!(
            "cannot reattach {}, which was deleted by hand",
            entry.path.display()
        )
    };
    // Its lifetime and cone go with the registration, so read them first;
    // `run --persistent` removes the marker again once the lock is taken.
    let (ephemeral, sparse) =
        deleted_worktree_settings(repo, entry, template.sparse.is_empty(), caller)
            .with_context(refused)?;
    let args = WorktreeAdd {
        branch: entry
            .branch
            .as_deref()
            .map(|reference| short_branch(reference).to_owned()),
        detach: entry.branch.is_none(),
        attach: entry.branch.is_some(),
        path: Some(template.path.clone().unwrap_or_else(|| entry.path.clone())),
        sparse: if template.sparse.is_empty() {
            sparse
        } else {
            template.sparse.clone()
        },
        base: match entry.branch {
            Some(_) => template.base.clone(),
            None => entry.head.clone(),
        },
        require_cow: template.require_cow,
        ephemeral: template.ephemeral || ephemeral,
    };
    // Every error that can be known before the drop, so a refused command
    // changes nothing: arguments, the destination, and the populate mode.
    check_creation_args(&args)?;
    let target = canonical_path(&cwd.join(args.path.as_deref().unwrap_or(&entry.path)));
    if target != canonical_path(&entry.path) {
        refuse_occupied(&target)?;
    }
    // The probe stands in for the parent `create_worktree` makes later.
    let probe =
        nearest_existing_directory(target.parent().context("worktree path has no parent")?)?;
    refuse_sparse_overlay(
        &args.sparse,
        select_populate_mode(repo, &probe, args.require_cow)?,
    )?;
    drop_deleted_registration(repo, entry, "then run again", caller).with_context(refused)?;
    // The marker and cone went with the registration and are not restored on a
    // failure here (restoring them would need the registration this just
    // dropped), so the error names them for the retry to pass again.
    create_worktree(repo, cwd, &args, claim).with_context(|| {
        let mut lost = Vec::new();
        if args.ephemeral {
            lost.push("--ephemeral".to_owned());
        }
        lost.extend(args.sparse.iter().map(|dir| format!("--sparse {dir}")));
        let lost = if lost.is_empty() {
            String::new()
        } else {
            format!("; to keep its settings, pass {}", lost.join(" "))
        };
        format!(
            "the registration of {}, which was deleted by hand, was dropped before this failed{lost}",
            entry.path.display()
        )
    })
}

/// Refuse `--ephemeral` and `--persistent` (`marking`) on the main checkout:
/// it has no registration of its own, so the marker would land on the repository.
fn refuse_persistence_on_main(repo: &RepoContext, marking: bool, target: &Path) -> Result<()> {
    if marking && canonical_path(target) == canonical_path(&repo.main_worktree) {
        bail!(
            "--ephemeral and --persistent apply only to linked worktrees, and {} is the main checkout",
            target.display()
        );
    }
    Ok(())
}

pub fn run_in_worktree(args: WorktreeRun, json: bool) -> Result<()> {
    if json {
        bail!("--json is not supported with `run` because command output is streamed");
    }
    let cwd = std::env::current_dir()?;
    let repo = discover_repo(&cwd)?;
    let template = WorktreeAdd {
        branch: args.branch,
        detach: false,
        attach: false,
        path: args.path,
        sparse: args.sparse,
        base: args.base,
        require_cow: args.require_cow,
        ephemeral: args.ephemeral,
    };
    let marking = args.ephemeral || args.persistent;
    let (branch, resolved) = match template.branch.clone() {
        Some(branch) => {
            let claim = BranchClaim::acquire(&repo, &branch)?;
            let resolved = resolve_branch_worktree(&repo, &cwd, template, claim)?;
            (Some(branch), resolved)
        }
        None => {
            if template.base.is_some() || template.require_cow || !template.sparse.is_empty() {
                bail!(
                    "--base, --require-cow and --sparse require an explicit branch to create a worktree"
                );
            }
            pick_worktree(&repo, &cwd, marking, template)?
        }
    };
    // Git's lock also protects against native `git worktree remove/prune`.
    // A launcher killed outright leaves the lock in place; `simgit unlock`
    // clears it, and refuses while the recorded launcher process is still alive.
    let (target, lock) = match resolved {
        Resolved::Existing(path) => {
            refuse_persistence_on_main(&repo, marking, &path)?;
            let lock = WorktreeLock::acquire(&repo, &path)?;
            if let Some(held) = lock.path() {
                refuse_unfinished(&repo, held, &path)?;
            }
            (path, lock)
        }
        Resolved::Created(created) => {
            eprintln!("mode: {}", created.mode.label());
            let lock = WorktreeLock::take_over_allocation(&repo, &created.target)?;
            (created.target, lock)
        }
    };
    overlay::repair(&repo, &target)?;
    let actual = discover_repo(&target)?;
    if actual.common_git_dir != repo.common_git_dir
        || actual.top_level.canonicalize()? != target.canonicalize()?
    {
        bail!("registered worktree is unavailable: {}", target.display());
    }
    if args.ephemeral {
        mark_ephemeral(&target)?;
    } else if args.persistent {
        remove_file_if_present(&worktree_admin_dir(&target)?.join("simgit-ephemeral"))?;
    }
    eprintln!(
        "worktree: {} (branch: {})",
        target.display(),
        branch.as_deref().unwrap_or("(detached)")
    );
    let (program, command_args) = args.command.split_first().context("command is required")?;
    let spawned = run_to_completion(
        Command::new(program)
            .args(command_args)
            .current_dir(&target)
            // Shells and tools trust $PWD over the real directory.
            .env("PWD", &target),
    );
    // The child's outcome matters more than a lock that would not come off.
    if let Err(error) = lock.release() {
        eprintln!(
            "could not unlock worktree: {error:#}\nrelease it with: simgit unlock {}",
            shell_quote(&target)
        );
    }
    let status = match spawned {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Exit the way a shell does for a missing command, so a caller
            // can tell "not found" from a command that ran and failed.
            eprintln!(
                "Error: command not found: {}\nworkspace retained at {}",
                program.to_string_lossy(),
                target.display()
            );
            io::stdout().flush()?;
            io::stderr().flush()?;
            std::process::exit(127);
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            // A shell's 126: the command exists but cannot be executed.
            eprintln!(
                "Error: command not executable: {} (workspace retained at {})",
                program.to_string_lossy(),
                target.display()
            );
            io::stdout().flush()?;
            io::stderr().flush()?;
            std::process::exit(126);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "run '{}' in {}",
                    program.to_string_lossy(),
                    target.display()
                )
            })
        }
    };
    if !status.success() {
        // Collapsing every failure to 1 defeats `simgit run … && git merge …`
        // and hides a child shell's 127. The lock is released and nothing
        // else is outstanding, so this process can adopt the child's status;
        // the retained-workspace notice stays on stderr.
        eprintln!(
            "command exited with {status}; worktree retained at {}",
            target.display()
        );
        io::stdout().flush()?;
        io::stderr().flush()?;
        std::process::exit(child_exit_code(&status));
    }
    Ok(())
}

// Signal numbers and `pthread_sigmask` operations as libc defines them on the
// supported targets. Linux numbers SIGCHLD and the operations differently from
// macOS and the BSDs.
const SIGHUP: c_int = 1;
const SIGINT: c_int = 2;
const SIGQUIT: c_int = 3;
const SIGTERM: c_int = 15;
#[cfg(target_os = "linux")]
const SIGCHLD: c_int = 17;
#[cfg(not(target_os = "linux"))]
const SIGCHLD: c_int = 20;
#[cfg(target_os = "linux")]
const SIG_BLOCK: c_int = 0;
#[cfg(not(target_os = "linux"))]
const SIG_BLOCK: c_int = 1;
#[cfg(target_os = "linux")]
const SIG_SETMASK: c_int = 2;
#[cfg(not(target_os = "linux"))]
const SIG_SETMASK: c_int = 3;
const SIG_IGN: usize = 1;

/// Room for a `sigset_t`, which is 4 bytes on macOS and 128 on Linux. Only
/// libc's own functions read or write it.
#[repr(C)]
#[derive(Clone, Copy)]
struct SigSet([u64; 16]);

extern "C" {
    fn sigemptyset(set: *mut SigSet) -> c_int;
    fn sigaddset(set: *mut SigSet, signum: c_int) -> c_int;
    fn sigdelset(set: *mut SigSet, signum: c_int) -> c_int;
    fn pthread_sigmask(how: c_int, set: *const SigSet, old: *mut SigSet) -> c_int;
    fn sigsuspend(mask: *const SigSet) -> c_int;
}

/// The pid of the child `run_to_completion` waits for, read by `forward`.
static CHILD: AtomicI32 = AtomicI32::new(0);

/// Pass SIGTERM or SIGHUP on to the child. Async-signal-safe: it only loads an
/// atomic and calls `kill`. Any errno `kill` sets is never read, because this
/// runs only inside `sigsuspend`.
extern "C" fn forward(signum: c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: `kill` takes two integers and dereferences nothing.
        unsafe { kill(pid, signum) };
    }
}

/// Wakes `sigsuspend` when the child changes state; the loop checks which.
extern "C" fn child_changed(_: c_int) {}

/// Run `command` the way a shell runs a foreground job, so that the signals a
/// terminal or service manager sends cannot keep this process from releasing
/// its lock afterwards.
///
/// Call it once, as the process's last child: the signal setup is
/// process-global and stays until exit, and it relies on there being no other
/// threads that could take these signals outside `sigsuspend`.
///
/// Ctrl-C and Ctrl-\ reach the child through the terminal's process group, so
/// this process ignores SIGINT and SIGQUIT and leaves the outcome to the child.
/// SIGTERM and SIGHUP sent to this process alone are forwarded to the child.
fn run_to_completion(command: &mut Command) -> io::Result<ExitStatus> {
    let mut handled = SigSet([0; 16]);
    let mut waiting = SigSet([0; 16]);
    // SAFETY: both sets are owned, writable and large enough for a `sigset_t`,
    // and every signal number is valid, so these calls only fill in the sets
    // and the mask. Handlers are installed only once the child exists.
    let failed = unsafe {
        sigemptyset(&mut handled);
        for signum in [SIGINT, SIGQUIT, SIGTERM, SIGHUP, SIGCHLD] {
            sigaddset(&mut handled, signum);
        }
        pthread_sigmask(SIG_BLOCK, &handled, &mut waiting)
    };
    if failed != 0 {
        return Err(io::Error::from_raw_os_error(failed));
    }
    // These stay blocked until this process exits, and are taken only inside
    // `sigsuspend`, so none can interrupt the lock's release. The child gets
    // the dispositions this process inherited, since its own are set only
    // after the spawn, and the mask back, since std passes the blocked one on.
    let inherited = waiting;
    // SAFETY: the closure runs between fork and exec and only calls
    // `pthread_sigmask`, which is async-signal-safe, on a set it owns.
    unsafe {
        command.pre_exec(
            move || match pthread_sigmask(SIG_SETMASK, &inherited, null_mut()) {
                0 => Ok(()),
                error => Err(io::Error::from_raw_os_error(error)),
            },
        );
    }
    let mut child = command.spawn()?;
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    // SAFETY: the handlers are `extern "C" fn(c_int)` that only touch an
    // atomic and call `kill`; `SIG_IGN` is a reserved value, not a pointer.
    unsafe {
        signal(SIGINT, SIG_IGN);
        signal(SIGQUIT, SIG_IGN);
        signal(SIGTERM, forward as extern "C" fn(c_int) as usize);
        signal(SIGHUP, forward as extern "C" fn(c_int) as usize);
        signal(SIGCHLD, child_changed as extern "C" fn(c_int) as usize);
        sigdelset(&mut waiting, SIGCHLD);
    }
    loop {
        // The child is reaped only here, with the signals blocked, so `forward`
        // never signals a pid that has been reused.
        if let Some(status) = child.try_wait()? {
            CHILD.store(0, Ordering::SeqCst);
            return Ok(status);
        }
        // SAFETY: `waiting` is the mask this process had, minus SIGCHLD; the
        // call returns once a handler has run.
        unsafe { sigsuspend(&waiting) };
    }
}

/// The status a shell reports for a finished child: its own exit code, or
/// `128 + signal` when a signal killed it.
fn child_exit_code(status: &ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Ask which existing workspace to run in. A choice that cannot be launched
/// into is explained and asked again; one deleted by hand is created again.
fn pick_worktree(
    repo: &RepoContext,
    cwd: &Path,
    marking: bool,
    template: WorktreeAdd,
) -> Result<(Option<String>, Resolved)> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!(
            "workspace selection requires a terminal; pass a branch: simgit run <branch> -- <command>"
        );
    }
    let entries = list_worktrees(repo)?;
    if entries.is_empty() {
        bail!("no workspaces found; create one with simgit run <branch> -- <command>");
    }
    eprintln!("Choose a workspace:");
    let admins = admin_dirs(repo);
    // The registry was read once above; an overlay's directory is remountable,
    // and a live simgit process's lock means it is populating the directory.
    let missing = |entry: &WorktreeEntry| {
        matches!(entry.path.try_exists(), Ok(false))
            && admins.get(&canonical_path(&entry.path)).map_or_else(
                || registration_gone(repo, &entry.path),
                |admin| {
                    overlay::read_state(admin).is_none()
                        && !held_by_live_simgit(&admin.join("locked"))
                },
            )
    };
    for (index, entry) in entries.iter().enumerate() {
        let marker = if missing(entry) { " [missing]" } else { "" };
        eprintln!(
            "  {}. {}{marker}",
            index + 1,
            worktree_description(repo, &admins, entry)
        );
    }
    loop {
        eprint!("Workspace [1-{}], or q to cancel: ", entries.len());
        io::stderr().flush()?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 || answer.trim().eq_ignore_ascii_case("q") {
            // Deliberate cancellation is not a failure to report as one; exit
            // the way interactive tools do on an aborted prompt.
            eprintln!("cancelled");
            std::process::exit(130);
        }
        if let Ok(number) = answer.trim().parse::<usize>() {
            if (1..=entries.len()).contains(&number) {
                let entry = &entries[number - 1];
                let branch = entry.branch.as_deref().map(|r| short_branch(r).to_owned());
                check_reuse(cwd, &template, branch.as_deref(), &entry.path)?;
                // A workspace that is in use cannot be launched into, but an
                // unavailable choice is no reason to abandon the prompt: say
                // why and re-ask, exactly as an out-of-range number does.
                let chosen =
                    refuse_persistence_on_main(repo, marking, &entry.path).and_then(|()| {
                        if !missing(entry) {
                            return ensure_unlocked(repo, &entry.path)
                                .and_then(|lock| refuse_unfinished(repo, &lock, &entry.path))
                                .map(|()| Resolved::Existing(entry.path.clone()));
                        }
                        let claim = match &branch {
                            Some(name) => Some(BranchClaim::acquire(repo, name)?),
                            None => None,
                        };
                        reattach(repo, cwd, entry, &template, claim, Caller::Cli)
                            .map(Resolved::Created)
                    });
                match chosen {
                    Ok(resolved) => return Ok((branch, resolved)),
                    Err(reason) => {
                        eprintln!("{reason:#}");
                        eprintln!("Choose another workspace.");
                        continue;
                    }
                }
            }
        }
        eprintln!(
            "Enter a number from 1 to {}, or q to cancel.",
            entries.len()
        );
    }
}
