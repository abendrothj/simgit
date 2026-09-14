# simgit reference

Detailed semantics for the flat command surface
(`simgit [--json] doctor|add|list|remove|run|unlock|gc|prune|repair`).
[README.md](../README.md) is the overview; this file is the contract each
command actually implements. `simgit` is the canonical executable and `sg` is an
equivalent alias; examples use `sg`.

## Removal and unlock semantics

`remove` accepts either a path or a branch name, and defaults to the worktree
containing the current directory. Plain `remove` refuses a dirty worktree; use
`--discard-dirty` only with the owner's explicit authority to discard it.
Deleting a branch that is not merged additionally requires `--delete-unmerged`
next to `--delete-branch`. `--commit` with an optional `-m <message>` commits
leftover changes first, instead of discarding them.

Removing a target that no longer has a worktree succeeds and reports
`already_absent: true`, so a retried or at-least-once cleanup is safe. With
`--delete-branch` that idempotence holds only for the branch-name form, which
still deletes a leftover branch of that name: an already-removed *path* cannot
name a branch, so `remove <absent-path> --delete-branch` is an error telling you
to pass the branch name instead.

`unlock` is idempotent in the same way and one step further: a target that is
not locked, and a target that no longer exists at all, both exit 0 and report
`was_locked: false`. Crash recovery can call it unconditionally.

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

`--path` names the worktree directory itself, not a parent to allocate inside.
Missing parents are created. The directory may already exist if it is empty —
harnesses that pre-create one directory per job work — but a non-empty directory
is refused, and two concurrent allocations to the same path can never both
succeed. A destination inside the source repository is allowed but warned about
on stderr: it becomes untracked clutter in `git status` and `git clean` can
delete it, so pass a path outside the repository or set
`SIMGIT_WORKTREE_ROOT`.

`add` creates a branch, so it refuses one that already exists and points at the
command that does the right thing: `sg run <branch> -- <command>` attaches to
that branch's worktree, or creates one at its current commit when only the
branch survives — the situation you are in after a `remove` that retained the
branch.

Worktrees are deliberately **not** placed inside `.git`: agent harnesses and
editors treat everything under `.git/` as off-limits or invisible — Claude Code
refuses to edit files there — so a worktree nested in the git dir is unusable by
the tools this exists to serve. The worktree itself is still registered in Git's
normal `.git/worktrees/` registry, and the cached baseline stays internal in
`.git/simgit/baselines/`. Removing the last worktree also removes the empty
`.simgit` directory.

## What the agent sees

`sg run` does not emulate Git or intercept Git commands. It launches the child
with its working directory set to a real, registered linked worktree. From the
agent's perspective it is an ordinary repository: `git status`, `diff`, `add`,
`commit`, `restore`, `switch`, `merge`, `rebase`, `cherry-pick`, hooks,
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
sg gc --older-than 0s --delete-branches
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
`--require-cow` cannot be combined with `SIMGIT_POPULATE=checkout`.

`SIMGIT_POPULATE` forces the populate backend and accepts exactly one of
`reflink`, `overlay`, or `checkout`; anything else is rejected by name. That is
the *request* vocabulary, and it is deliberately not the *result* vocabulary
that `list` and `add --json` report in `mode`: `reflink` yields `cow-clone`,
`overlay` yields `overlay`, and `checkout` yields `git-checkout`. `overlay`
requires `fuse-overlayfs`, so it is Linux-only and fails outright on macOS
rather than falling back.

`--sparse` is rejected with the overlay backend, where the lower layer is the
whole baseline and narrowing the view saves nothing. Reusing a workspace keeps
whatever cone it was created with, and passing `--sparse` again on reuse is
rejected rather than silently ignored.

## Run locks and recovery

While a command launched by `run` is active, its linked worktree is locked
against removal and GC, including `--discard-dirty`. A second `run` in the same
workspace is refused until the first exits. The lock file records the launcher's
PID. If the launcher is killed the lock is deliberately retained; recover it
with `sg unlock [TARGET]`, where TARGET is a workspace path or branch that
defaults to the workspace containing the current directory.

It clears the lock wherever it lives: a linked worktree's lock is Git's own
`.git/worktrees/<slug>/locked`, while `.git/simgit-run.lock` is used only when
`run` targets the main checkout, which Git cannot lock. `unlock` refuses while
the recorded owner is still alive and names that PID — stop that process instead
of looking for an override flag, because there is none. A lock file written
before locks recorded a PID counts as unknown-owner and can be cleared.

Commands launched outside `sg run` are not tracked. Idle age uses index and
directory modification time, so it is only a cleanup heuristic for explicitly
disposable workspaces.

## Machine-readable output

The global `--json` flag gives orchestrators structured output from `doctor`,
`add`, `remove`, `list`, `unlock`, `gc`, `prune` and `repair`; it is accepted
before or after the command, and `run` is the one command that rejects it. A
`--json` failure emits no JSON: nonzero exit, empty stdout, one line of
diagnostic text on stderr.

Every worktree path simgit prints or returns is symlink-resolved and lexically
normalized, so one worktree always has exactly one string form and orchestrators
can compare those strings directly. That resolution applies whenever a target
resolves to a real worktree, so `remove`'s `removed` is the resolved absolute
path even when you named the worktree by branch or by a relative path. It is
only when nothing resolves — `remove` on an already-absent target, `unlock` on
one — that `removed` and `unlocked` repeat the target verbatim, because there is
no worktree left to resolve against and inventing a resolved form would imply
one exists.

| Command | Fields |
|---|---|
| `doctor` | `identity`, `product`, `version`, `filesystem`, `cow_supported`, `populate_mode`, `repository`, `repository_details`, `git_worktree_supported`, `default_worktree_root`, `default_worktree_root_inside_repository`, `baseline_cache`, `stale_worktree_registrations` |
| `add` | `worktree`, `path`, `cleanup_token`, `branch`, `base`, `mode`, `ephemeral` |
| `list` | array of `worktree`, `branch`, `HEAD`, `ephemeral`, `mode`, and `locked` while a command runs |
| `remove` | `removed`, `already_absent`, `branch_deleted`, `committed` |
| `unlock` | `unlocked`, `was_locked`, `owner_pid` |
| `gc` | `reaped`, `skipped`, `deleted_branches`, `retained_branches`, `dry_run` |
| `prune` | `pruned`, `pruned_registrations`, `retained`, `retained_bytes` |

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
reason: `persistent`, `dirty`, `locked`, or `recently-active` for one that is
simply younger than `--older-than`. An empty `reaped` list is therefore always
explained.

## Baselines and `prune`

`prune` does two things and reports both. It deregisters stale Git worktree
registrations — entries whose directory is gone — listing their paths in
`pruned_registrations` and printing `pruned N stale registration(s)`. And it
reclaims the baseline cache, reporting what that cache still costs in both
formats (`retained_bytes` in JSON).

One baseline is materialized per distinct base commit and kept for seven days,
so branching from several commits costs one full tree each until pruned;
`prune --all` drops every cached baseline immediately, including recently used
ones, and is the way to reclaim that space without waiting out the seven-day
window. A dropped baseline is rematerialized on the next `add` that needs it.
Compare the retained baseline cost directly with the marginal cost of each
additional worktree; the savings depend on repository shape, not a fixed
multiplier.

## Repository layout

```text
simgit/
├── sg/                 the CLI (`simgit`; `sg` is the short alias)
│   └── src/commands/worktree/   command launch/picker, CoW and overlay backends
├── tests/              CoW scaling benchmarks + overlay integration test
├── skills/             first-party `simgit-worktrees` agent skill
├── packaging/          Homebrew formula (prebuilt-binary install)
├── install.sh          checksummed installer for pinned release binaries
└── docs/               reference, agent integration, benchmark methodology
```
