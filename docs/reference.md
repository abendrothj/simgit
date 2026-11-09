# simgit reference

Detailed semantics for the flat command surface
(`simgit [--json] doctor|add|list|remove|run|unlock|gc|prune|repair|hook`).
[README.md](../README.md) is the overview; this file is the contract each
command actually implements. `simgit` is the canonical executable and `sg` is an
equivalent alias; examples use `simgit`.

## Removal and unlock semantics

`remove` accepts either a path or a branch name, and defaults to the worktree
containing the current directory. Plain `remove` refuses a dirty worktree; use
`--discard-dirty` only with the owner's explicit authority to discard it.
Deleting a branch that is not merged additionally requires `--delete-unmerged`
next to `--delete-branch`. `--commit` with an optional `-m <message>` commits
leftover changes first, instead of discarding them; `commit` in the `--json`
output carries that commit's full hash, and `committed` stays `false` when
there was nothing to commit.

`remove` (with or without `--commit` or `--discard-dirty`) refuses a detached
worktree whose HEAD commit no branch, tag, or other ref reaches, because
removing it would leave those commits dangling. The check runs before
`--commit` commits anything and before teardown, and no flag overrides it. The
error names the fix: `git -C <path> branch <name>`, then remove. When the
worktree's directory or its `.git` file is gone, it names
`git -C <main-checkout> branch <name> <commit>` instead, which works without
the directory. A detached worktree at a commit some ref reaches is removed as
usual, and so is one on a branch.

`remove --commit` on a detached worktree with something to commit is refused
before it commits: the new commit would be one no ref reaches, which removal
then refuses to drop. Create a branch first with
`git -C <path> switch -c <name>`, then remove with `--commit`; the error names
that command, and so does the plain dirty refusal of a detached worktree.

`remove` refuses the main checkout, by path or as the default target, before
`--commit` commits anything: Git cannot remove it, and the files stay as they
were.

`--commit` commits before removing the worktree, and the commit is never
rolled back if the removal then fails: it is the work you asked to keep. Such a
failure names the commit it kept, so a caller can tell it from one that
committed nothing. Repeating the same command is safe either way — `--commit`
stages and commits only what the worktree still holds, so a retry after a
successful commit commits nothing a second time.

Removing a target that no longer has a worktree succeeds and reports
`already_absent: true`, so a retried or at-least-once cleanup is safe. That
includes the residue of an interrupted teardown: Git can delete a worktree's
contents and its registration and still fail to unlink the directory itself, and
the empty directory left behind is reported as already absent (and deleted when
the filesystem now permits it) rather than mistaken for a live worktree. A
directory at that path that still holds files is refused instead — simgit does
not delete directories it does not manage. With `--delete-branch` the
idempotence holds only for the branch-name form, which still deletes a leftover
branch of that name: a path with no worktree cannot name a branch, so
`remove <absent-path> --delete-branch` is an error telling you to pass the
branch name instead.

A worktree whose directory was deleted by hand, while Git still registers it,
is already absent too. `remove`, by branch or by path, drops that one
registration, and `--commit` commits nothing. The registration still names the
branch, so `--delete-branch` deletes it under the usual merged check by either
form. The registration keeps the worktree's index, any operation in
progress, its submodules' repositories (`modules/`) and its own refs
(`refs/worktree/*`, `refs/bisect/*`, `refs/rewritten/*`), and the last two can
hold the only copy of a commit. So one whose index differs from its HEAD
(staged changes), that holds a merge, rebase, cherry-pick or revert in
progress, that holds a submodule repository, or that has refs of its own counts
as dirty: `remove` refuses it without `--discard-dirty`, and the error names
the commands that show what is staged and which refs it holds. A submodule
repository counts even when every commit in it is pushed, since simgit cannot
tell that without asking the remote: a hand-deleted worktree whose submodules
were initialized always needs `--discard-dirty`. Two other such registrations are refused and kept
whatever the flags: a locked one, since `git worktree lock` exists to protect a
worktree on an unplugged drive, and a detached one whose HEAD no ref reaches
(above), since the registration holds the only reference to its commits. A path
that cannot be checked, such as one whose parent denies access, is not taken
for deleted.

`unlock` is idempotent in the same way and one step further: a target that is
not locked, and a target that no longer exists at all, both exit 0 and report
`was_locked: false`. A worktree deleted by hand that Git still registers is
found through the registry by its path as well as its branch, so a stranded lock
on it is cleared rather than reported as absent. Crash recovery can call it
unconditionally. It clears only locks simgit wrote (see [Run locks and
recovery](#run-locks-and-recovery)).

## `hook`

```sh
simgit hook claude-code --requires <version> [--discard-dirty]
```

Adapts a Claude Code hook to simgit's worktrees: it reads one hook payload on
stdin and acts on its `hook_event_name`. `claude-code` is the only provider.
The Claude Code plugin runs it, in Codex too; see [Claude Code](agent-integration.md#claude-code)
and [Codex](agent-integration.md#codex) for install, behaviour and limits. Do
not run it by hand.

- `--requires <version>` exits 1 with one stderr line when the running simgit
  is older than the `X.Y.Z` given. A newer one is accepted.
- `--discard-dirty` lets `WorktreeRemove` discard uncommitted work, as Claude
  Code's own `git worktree remove --force` does. Without it `hook` never
  discards. Untracked and uncommitted files are discarded; gitignored files go
  with the worktree, as with `remove`.
- Global `--json` is rejected: the harness fixes the output.
- Exit codes are 0 and 1. clap usage errors, such as an unknown subcommand,
  exit 2.

Branches are `simgit/<name>`, checked with `git check-ref-format --branch`; a
subagent's is `simgit/agent-<agent_id>`. New worktrees start at
`refs/remotes/origin/HEAD` when it exists, else the project's `HEAD` (the worktree
`$CLAUDE_PROJECT_DIR` names), with no fetch. A name
matching `pr-<digits>` instead fetches `pull/<n>/head`, or
`merge-requests/<n>/head` when origin has no such pull ref, from origin (git
2.29 or newer, for `--no-write-fetch-head`) and starts there. If origin has
neither ref, creation fails; any other fetch failure fails it with Git's error.
`GIT_TERMINAL_PROMPT=0` stops HTTPS credential prompts, but an ssh key that
needs a passphrase and has no agent can still prompt. A reused `pr-<n>`
worktree is not refetched. Allocations are persistent.

| Event | Action | stdout | Exit |
|---|---|---|---|
| `WorktreeCreate` | Under the branch claim: reuse the `simgit/<name>` worktree, else attach the branch if it exists, else create it from the base. Never returns the main checkout, and refuses a worktree locked by `run`. A worktree deleted by hand is pruned first, so its branch is attached again at its old tip; one whose registration holds uncommitted work (staged changes, an operation in progress, submodule repositories or refs of its own) is kept, and the create fails. | the path only | 1 with one stderr line on failure |
| `WorktreeRemove` | Only a worktree on a `simgit/*` branch; others are refused. Removes it (refusing a dirty one without `--discard-dirty`), then drops the branch if no commit on it is unreached by another ref. A path that no longer exists succeeds; if Git still registers it, the registration is pruned and the branch dropped by the same rule, unless it holds uncommitted work as for `remove`, which keeps both. | nothing | 1 when refused or failed |
| `SubagentStop` | Removes the `simgit/agent-<agent_id>` worktree and branch when it is clean, unlocked and holds no unreached commit. Otherwise keeps it. A worktree deleted by hand has its registration pruned and its branch dropped by the same rule, unless it holds uncommitted work as for `remove`. It does nothing when the branch is not checked out in a worktree. | nothing | always 0 |
| `SessionStart` | Nothing; the plugin uses it to check the version. | nothing | 0 |
| any other | Nothing. | nothing | 0 |

A branch is dropped by `hook` only when `git rev-list --single-worktree
<branch> --not --exclude=refs/heads/<branch> --all` is empty. `--all` counts
every other ref, including `refs/stash` and remote-tracking refs, so pushed
commits count as reached. That differs from `remove --delete-branch`, whose
merged check compares against the main checkout's `HEAD`; `hook` never passes
`--delete-unmerged` and never deletes a commit no other ref reaches.

## Where worktrees live

New worktrees are created beside the repository, under `../.simgit/<repo>/`;
`SIMGIT_WORKTREE_ROOT` or `--path` overrides that. The directory name is not the
branch name: characters Git allows in a branch but a single path component does
not — `/` above all — are replaced with `-`, and whenever that rewriting changes
the name an 8-hex digest of the original branch is appended, so distinct
branches that flatten to the same string still get distinct directories.
`feat/my-feature` becomes `../.simgit/<repo>/feat-my-feature-c75e230f`, while an
already-safe name like `feature-x` is used as-is. Script against the
`worktree`/`path` field that `add --json` returns, not against a formula.

`<repo>` is the main working tree, wherever its Git data lives, so a
`--separate-git-dir` repository's worktrees go beside its checkout. Run from a
linked worktree of such a repository, which records no path back to its main
checkout, they go beside the Git directory instead. `list` and `doctor`'s
`common_git_owner` name the same main working tree.

A submodule's checkout lies inside its superproject's working tree, so its
worktrees go beside the outermost superproject's main checkout instead, under
`.simgit/<super>/.modules/<submodule path>-<digest>/`, whether simgit runs from
the submodule's checkout, a subdirectory of it, or one of its linked worktrees.
The digest is of the submodule's Git directory, which keeps apart submodules
with the same name and the copies each superproject worktree checks out.
`.modules` cannot clash with a branch, since Git refuses ref components that
start with `.`. A submodule whose superproject cannot be found keeps the
ordinary rule.

`--path` names the worktree directory itself, not a parent to allocate inside.
Missing parents are created. The directory may already exist if it is empty —
harnesses that pre-create one directory per job work — but a non-empty directory
is refused, and two concurrent allocations to the same path can never both
succeed. Neither can two for the same branch, whether they create it or attach
it: the second fails at once instead of waiting. A destination inside the
source repository is allowed but warned about on stderr: it becomes untracked
clutter in `git status` and `git clean` can delete it, so pass a path outside
the repository or set `SIMGIT_WORKTREE_ROOT`.

`add` creates a branch, so it refuses one that already exists and names the two
commands that get a worktree for it. `simgit add --attach <branch>` checks the
existing branch out at its current commit in a new worktree, with the same
`--json` record as any `add`; it never moves the branch, and it refuses a branch
that does not exist or that is already checked out in another worktree, naming
that worktree, because a cleanup token must never name a workspace the caller
did not create. `simgit run <branch> -- <command>` attaches to the branch's
registered worktree instead, or creates one when only the branch survives — the
situation you are in after a `remove` that retained the branch.

Both use the local branch as it stands. To start from a newer remote tip, update
the local branch first: `git fetch origin <branch>:<branch>` fast-forwards it and
refuses when the two have diverged, rather than discarding local commits.

If allocation fails after checking that a branch name is available, something
outside simgit, such as a plain `git branch`, may have created that name;
another simgit allocating the same branch fails at once instead. simgit does
not force-delete a branch it cannot prove it created. Git may also leave a
branch behind after a partial `worktree add` failure; inspect any surviving
branch before retrying or deleting it. After successful registration, a
population failure instead attempts to roll back that worktree and the branch
it created.

Worktrees are deliberately **not** placed inside `.git`: agent harnesses and
editors treat everything under `.git/` as off-limits or invisible — Claude Code
refuses to edit files there — so a worktree nested in the git dir is unusable by
the tools this exists to serve. The worktree itself is still registered in Git's
normal `.git/worktrees/` registry, and the cached baseline stays internal in
`.git/simgit/baselines/`. Removing the last worktree also removes the empty
`.simgit` directory.

## What the agent sees

`simgit run` does not emulate Git or intercept Git commands. It launches the
child with its working directory set to a real, registered linked worktree.
From the agent's perspective it is an ordinary repository: `git status`, `diff`,
`add`, `commit`, `restore`, `switch`, `merge`, `rebase`, `cherry-pick`, hooks,
attributes and ignores work through Git itself. The files are ordinary local
files; copy-on-write is handled below Git by the filesystem.

Each worktree has its own files, index, `HEAD`, current branch and in-progress
merge/rebase state. Git objects, refs, remotes, configuration, hooks and stashes
are shared with the main repository, exactly as with `git worktree`. That means
Git will not let two worktrees check out the same branch, and repository-wide
operations can affect the other worktrees. simgit provides workspace isolation,
not a security boundary.

When an agent commits, Git writes the commit to the shared object database and
advances only that agent's branch. The commit is immediately visible from the
main checkout, but the main branch and its files do not change until you merge
or rebase it:

```bash
git log agent/auth
git merge agent/auth
simgit gc --older-than 0s --delete-branches
```

## Reuse, persistence, and populate backends

New `run` worktrees are persistent by default. Existing worktrees keep their
persistence setting unless you pass `--ephemeral` or `--persistent` explicitly.
GC selects only ephemeral worktrees by default, even with `--discard-dirty`; use
`--include-persistent` to explicitly include persistent workspaces. This changes
the earlier defaults: automation that relied on `run` creating disposable
workspaces should now pass `--ephemeral`.

On reuse, `--path` must identify the existing workspace. `--base`,
`--require-cow` and `--sparse` are creation-only options and are rejected on
reuse; `--base` is also rejected when attaching an existing branch.

`--base` is any revision `git rev-parse` accepts, and defaults to `HEAD`. It is
resolved in the working tree you run simgit from, so from a linked worktree the
default is that worktree's `HEAD` (detached is fine) and `HEAD~1` is relative to
it, not to the main checkout. A branch name or commit resolves the same from
anywhere. A repository with no commits fails with a message saying so.
`--require-cow` cannot be combined with `SIMGIT_POPULATE=checkout`.

`SIMGIT_POPULATE` forces the populate backend and accepts exactly one of
`reflink`, `overlay`, or `checkout`; anything else is rejected by name. That is
the *request* vocabulary, and it is deliberately not the *result* vocabulary
that `list` and `add --json` report in `mode`: `reflink` yields `cow-clone`,
`overlay` yields `overlay`, and `checkout` yields `git-checkout`. `overlay`
requires `fuse-overlayfs`, so it is Linux-only and fails outright on macOS
rather than falling back.

`--sparse` is rejected with the overlay backend, where the lower layer is the
whole baseline and narrowing the view saves nothing. Repeated directories and
overlapping cones are accepted: an ancestor subsumes its descendants before
population, so no directory is cloned twice. Reusing a workspace keeps whatever
cone it was created with, and passing `--sparse` again on reuse is rejected
rather than silently ignored.

## Run locks and recovery

While a command launched by `run` is active, its linked worktree is locked
against removal and GC, including `--discard-dirty`. A second `run` in the same
workspace is refused until the first exits. The lock file records the launcher's
PID. If the launcher is killed the lock is deliberately retained; recover it
with `simgit unlock [TARGET]`, where TARGET is a workspace path or branch that
defaults to the workspace containing the current directory.

It clears the lock wherever it lives: a linked worktree's lock is Git's own
`.git/worktrees/<slug>/locked`, while `.git/simgit-run.lock` is used only when
`run` targets the main checkout, which Git cannot lock. `unlock` refuses while
the recorded owner is still alive and names that PID — stop that process instead
of looking for an override flag, because there is none. A simgit lock file
written before locks recorded a PID counts as unknown-owner and can be cleared.
`unlock` removes only the lock it read and judged: if that lock was cleared
meanwhile it reports success, and if a new one has taken its place, such as a
`run` that started after another `unlock`, it leaves that lock and fails, to be
run again.

`run` writes its lock complete and links it into place, so no reader sees it
half-written. On a filesystem without hard links (exFAT, some network mounts)
it creates the lock exclusively and then writes it instead, and there a
concurrent `unlock` or `run` can see it empty for an instant and refuse it as
Git's.

`unlock` clears only locks whose content starts with `simgit:`. A lock set with
`git worktree lock`, perhaps to protect a worktree on removable media, is
refused: the error names Git's reason, when it has one, and
`git worktree unlock <path>`, and the lock stays. `run` on such a worktree is
refused with the same reason rather than as a running command. `gc` and
`remove` refuse any locked worktree, as before, and `remove` also refuses one
whose lock simgit cannot find to rule out.

An allocation also locks its own registration through Git, with the reason
`simgit: allocating (pid N)`, from `git worktree add` until the worktree is
populated and its mode and `--ephemeral` markers are written. CoW population
moves the worktree's `.git` aside while it clones, and the lock keeps Git's
`worktree prune`, `simgit prune` and `gc` from dropping the registration, or
reaping the worktree, before it is whole. An allocation killed after registering,
or one whose failure leaves the workspace retained for recovery, keeps this
lock, and `unlock` clears it like a `run` lock: once the recorded PID is gone,
never while it runs.

Allocating takes four more locks, and none of them needs `unlock`. An `add`,
or a `run` that creates its workspace, claims the destination path and, when
it names a branch, the branch, so two allocations for one path or one branch
never both proceed. For `add` and `run` the second fails at once; `hook`
waits for the first to finish and then reuses its worktree. While it
uses a baseline it also holds the baseline cache lock shared, which keeps
`prune` from removing that baseline (see [Baselines and `prune`](#baselines-and-prune)).
The branch claims (`.git/simgit/claims/branches/`), the path claims
(`.git/simgit/claims/paths/`) and the cache lock
(`.git/simgit/baselines.lock`) are operating-system file locks on files that
are never deleted, so the kernel releases them when the process exits, however
it exits, and a killed allocator leaves no claim to take over. Nothing deletes
those files, `prune` included, so one empty claim file stays for every branch
and every path ever claimed. Deleting one would break the exclusion it
provides: a process that had opened it but not yet locked it would lock the
deleted file while the next allocator created and locked a new one, and both
would proceed. Allocations for
different branches and paths run concurrently, waiting for one another only
while Git registers each new worktree, under the registry lock
(`.git/simgit/claims/registry.lock`), another operating-system file lock.
`list`, branch deletion and `prune` wait for a registration in progress, and
it waits for them. `list` and branch deletion hold the lock for one quick Git
command, `prune` for its whole pass over the registrations. Removing a live
worktree does not take it, so an allocation never waits for a large removal.
Dropping the registration of a worktree whose directory is gone takes it
exclusively for that one Git command, and so does the last step of an overlay
teardown, once the overlay's files are deleted.

Commands launched outside `simgit run` are not tracked. Idle age comes from the
modification time of the worktree's index, which Git updates when it stages,
commits or checks out, and from the worktree directory's when there is no
index. Work outside `run` that never touches Git, such as editing files, is not
activity, so idle age is only a cleanup heuristic for explicitly disposable
workspaces. A worktree with neither an index nor a directory to read counts as
idle; `gc` still keeps it while it is locked, or while it may hold uncommitted
work.

## Machine-readable output

The global `--json` flag gives orchestrators structured output from `doctor`,
`add`, `remove`, `list`, `unlock`, `gc`, `prune` and `repair`; it is accepted
before or after the command, and `run` and `hook` are the commands that reject it. A
`--json` failure, including an argument-parsing error, emits no JSON: nonzero
exit, empty stdout, one line of diagnostic text on stderr. That line replaces
each line break with one space, drops blank lines, and keeps all other
whitespace, so a path it names appears verbatim unless the path itself
contains a line break. `--help` and `--version` remain successful
human-readable requests, not JSON results.

Every worktree path simgit prints or returns is symlink-resolved and lexically
normalized, so one worktree always has exactly one string form and orchestrators
can compare those strings directly. That resolution applies whenever a target
resolves to a real worktree, so `remove`'s `removed` is the resolved absolute
path even when you named the worktree by branch or by a relative path. It is
only when nothing resolves — `remove` on a target Git no longer registers,
`unlock` on one — that `removed` and `unlocked` repeat the target verbatim,
because there is no worktree left to resolve against and inventing a resolved
form would imply one exists.

| Command | Fields |
|---|---|
| `doctor` | `identity`, `product`, `version`, `filesystem`, `cow_supported`, `populate_mode`, `repository`, `repository_details`, `git_worktree_supported`, `default_worktree_root`, `default_worktree_root_inside_repository`, `baseline_cache`, `stale_worktree_registrations` |
| `add` | `worktree`, `path`, `cleanup_token`, `branch`, `base`, `mode`, `ephemeral` |
| `list` | array of `worktree`, `branch`, `HEAD`, `ephemeral`, `mode`, and `locked` while a command runs |
| `remove` | `removed`, `already_absent`, `branch_deleted`, `committed`, `commit` |
| `unlock` | `unlocked`, `was_locked`, `owner_pid` |
| `gc` | `reaped`, `skipped`, `deleted_branches`, `retained_branches`, `dry_run` |
| `prune` | `pruned`, `pruned_registrations`, `kept_registrations`, `retained`, `retained_bytes`, `cache_busy` |

`doctor` exits 0 outside a Git worktree: identity, version and filesystem are
always reported, and every repository-dependent field is `null` rather than an
error. `cow_supported` is `true`/`false` when a probe could be written in the
current directory and `null` when one could not, so an unprobed directory is
never reported as unsupported.

`list` reports each worktree's `ephemeral` flag, its `locked` state while a
command runs, and the `mode` it was populated with (`cow-clone`, `overlay`,
`git-checkout`, or `null` for the main worktree) — so you can confirm a workspace
really is CoW-backed rather than a silent full-copy fallback. The human `list`
appends the same mode as a fourth tab-separated field.

## GC

GC selects ephemeral worktrees only unless `--include-persistent`, skips
uncommitted changes unless `--discard-dirty`, and accepts
`--prefix <branch-prefix>`, `--older-than <90s|30m|24h|7d>`,
`--delete-branches`, and `--dry-run`. Safe branch deletion retains unmerged
work; `--delete-unmerged` next to `--delete-branches` explicitly discards
unmerged branches.

Every worktree GC considered but did not reap appears in `skipped` with a
reason: `persistent`, `dirty`, `locked`, `unreached-commits` (below),
`recently-active` for one that is simply younger than `--older-than`,
`status-failed` for one whose state simgit could not read (below), or
`remove-failed` for one it selected but could not remove, such as one locked
or changed meanwhile; its branch is kept. An empty `reaped` list is therefore
always explained.

A detached worktree whose HEAD commit no ref reaches (for example, commits made
in an `add --detach` worktree) is skipped as `unreached-commits`: removing it
would leave those commits dangling. No flag overrides this; give the commits a
branch with `git -C <path> branch <name>` and `gc` can then reap the worktree.
A worktree on a branch is unaffected, since its commits stay on the branch.
If Git cannot answer whether the commits are reached, the worktree is skipped
as `status-failed`, as is one whose lock simgit cannot find to rule out and one
whose uncommitted changes Git cannot list.

A worktree whose directory was deleted by hand is reaped by dropping its
registration, and `--delete-branches` deletes its branch under the usual rule.
Its idle age comes from the index kept in its registration. It is selected,
and skipped as `persistent`, `locked`, `recently-active`, `unreached-commits`,
`dirty` (uncommitted work in its registration, as for `remove`) or
`status-failed` (a registration it cannot read), like
any other worktree. GC drops only the registrations of what it reaps: every
worktree it skips, or does not select under `--prefix`, stays registered, so a
later GC can still reap it and delete its branch.

## Baselines and `prune`

`prune` does two things and reports both. It deregisters stale Git worktree
registrations — entries whose directory is gone — listing their paths in
`pruned_registrations` and printing `pruned N stale registration(s)`. It keeps a
locked entry, a detached one whose HEAD no ref reaches, whose commits would
dangle, and one holding uncommitted work as for `remove`: staged changes, an
operation in progress, submodule repositories or refs of its own. It also
keeps an entry whose directory is still there without its `.git` file, or
cannot be checked, because that worktree's index and Git state live in the
registration. It reads Git's admin dirs itself, so an entry whose `gitdir` file
is missing or corrupt, which `git worktree list` does not show, is pruned too
and listed by its admin dir (`.git/worktrees/<name>`). One whose HEAD cannot
be read is kept, never deleted unjudged. A stale entry kept because it may hold
work is named with gc's reason for it: `dirty`, `unreached-commits`, or
`status-failed` when it cannot be judged. Human output names each on stderr;
`--json` lists them in `kept_registrations` as `worktree` (the admin dir for
an entry whose `gitdir` cannot be read) and `reason`, and
writes nothing to stderr for them, so a failing `prune --json` still writes
one line. A locked entry and a remountable overlay are kept without being
named. Every entry is
judged and dropped in one pass under the registry lock, so concurrent prunes
never act on each other's half-finished view. It also deletes the temporary
files a killed simgit leaves, once the process they name is gone: a lock's
`<lock>.<pid>.<uuid>.pending` copy beside Git's `locked` or
`.git/simgit-run.lock`, and a copy-on-write probe, recorded in
`.git/simgit/clone-probes/<pid>.<uuid>.source` with the
`.simgit-clone-probe-<pid>.<uuid>` file it names. A running process's are
kept. A probe an older simgit left names no process
(`clone-probes/<uuid>.source` and `.simgit-clone-probe-<uuid>`), so `prune`
keeps it; delete both by hand while no simgit is running. And it reclaims the baseline cache, reporting what that cache still costs in both
formats (`retained_bytes` in JSON).

One baseline is materialized per distinct base commit, so branching from several
commits costs one full tree each until pruned. Plain `prune` drops a baseline
once it has gone unused for seven days, or as soon as no ref and no detached
worktree HEAD reaches its commit any more — the leftover of a rebase and
force-push, or of a deleted branch, which only an explicit object id could ask
for again. `prune --all` drops every cached baseline immediately, including
recently used and still-reachable ones. A dropped baseline is rematerialized on
the next `add` that needs it.

Neither form removes a baseline an allocation is using, and neither waits for
one. An `add` or `run` that is creating a worktree holds the cache lock from
the moment it uses a baseline until it has finished populating the worktree,
and `prune` removes baselines only while it holds that lock itself. When an
allocation holds it, plain `prune` keeps every baseline it would have dropped,
reports `cache_busy: true` (and a line saying so in human output), and should
simply be run again later; `prune --all` fails instead, because it promises an
empty cache, and can be retried once the allocation finishes. Building a
baseline for the first time happens before the lock is taken, so a slow first
checkout does not hold `prune` off.

Compare the retained baseline cost directly with the marginal cost of each
additional worktree; the savings depend on repository shape, not a fixed
multiplier.

## Repository layout

```text
simgit/
├── sg/                 the CLI (`simgit`; `sg` is the short alias)
│   ├── src/commands/worktree.rs    registration, allocation, removal, listing
│   └── src/commands/worktree/      doctor, locks, maintenance, launch, CoW, overlay
├── tests/              CoW scaling benchmarks + overlay integration test
├── skills/             first-party `simgit-worktrees` agent skill
├── packaging/          Homebrew formula (prebuilt-binary install)
├── install.sh          checksummed installer for pinned release binaries
└── docs/               reference, agent integration, benchmark methodology
```
