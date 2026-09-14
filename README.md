# simgit

[![crates.io](https://img.shields.io/crates/v/simgit-cli.svg)](https://crates.io/crates/simgit-cli)
[![CI](https://github.com/abendrothj/simgit/actions/workflows/ci.yml/badge.svg)](https://github.com/abendrothj/simgit/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/simgit-cli.svg)](LICENSE)
[![skills.sh](https://skills.sh/b/abendrothj/simgit)](https://skills.sh/abendrothj/simgit)

**Disk-efficient, separate Git worktrees for running many agents on one repository at once.**

Each agent gets its own separate working tree, but unchanged files share
physical disk instead of being duplicated. There is no daemon and no server:
**Git owns the refs, the filesystem owns the data.**

`simgit` is the canonical executable. Every installation also provides `sg` as
a fully equivalent short alias; examples below use the alias for brevity.

<p align="center"><img src="assets/demo.gif" alt="sg run: launch agents in copy-on-write worktrees, merge their branches, and clean up" width="820"></p>

## Quick start

```bash
# Install (macOS: Homebrew; anywhere Rust builds: cargo install simgit-cli)
brew install abendrothj/tap/simgit

# Create a CoW linked worktree on a new branch and cd into it
cd "$(sg add feat/my-feature)"

# It's a standard linked checkout — every git command and hook just works
git add -A
git commit -m "work"

# Back in the main checkout, integrate the branch with an ordinary merge
cd -
git merge feat/my-feature

# Remove the workspace, and the merged branch with it
sg remove feat/my-feature --delete-branch
```

That was a real `.git/worktrees` linked checkout on its own branch, whose
unchanged files shared physical disk with one cached baseline instead of being
copied.

[Usage](#usage) is the full command surface, [Why](#why) has the disk numbers,
and [Running agents](#running-agents) is the reason this exists.

## Install

On macOS, prefer the Homebrew formula: it uses a versioned release asset with a
pinned SHA-256 digest.

```bash
# macOS (preferred)
brew install abendrothj/tap/simgit

# From crates.io (either one)
cargo install simgit-cli --locked
cargo binstall simgit-cli
```

Verify the installed canonical command. `doctor` runs anywhere, so this works
before there is a repository to check:

```bash
simgit doctor --json
```

On Linux without a reflink filesystem, install `fuse-overlayfs` to get the CoW
path (e.g. `apt-get install fuse-overlayfs`); otherwise `sg` falls back to a
plain checkout.

Prefer it as a Git subcommand? Add an alias:

```bash
git config --global alias.wt '!simgit'
git wt add feat/x
```

Installing from a pinned release tag with the checksum-verified `install.sh`,
approval gating, and version pinning are operator and agent policy:
[docs/agent-integration.md](docs/agent-integration.md#installation-and-skill-discovery).

## Usage

```bash
# Create a CoW linked worktree on a new branch and cd into it
cd "$(sg add feat/my-feature)"

# It's a standard linked checkout — every git command and hook just works
echo "hello" > README.md
git add README.md
git commit -m "work"

# List worktrees (add the global --json for machine-readable output)
sg list

# Commit any leftover changes and remove the worktree (the branch is retained)
sg remove --commit -m "clean up"

# Or, once a branch is merged while its worktree still exists, remove both
sg remove feat/my-feature --delete-branch

# Only with the owner's explicit authority, discard uncommitted changes
sg remove --discard-dirty

# Clear a `run` lock left behind by a killed launcher
sg unlock feat/my-feature

# Reap idle/abandoned worktrees. `--older-than` is an idle-age filter whose
# default is 24h, so this reaps nothing that was used in the last day.
sg gc --older-than 24h

# Remount overlay-backed Linux worktrees after a reboot/interrupted mount
sg repair

# Prune stale Git registrations and cached baselines older than seven days
sg prune

# Reclaim every cached baseline now, including recently used ones
sg prune --all
```

## Running agents

Create a persistent workspace and launch your agent's normal terminal interface:

```bash
sg run chat/auth -- claude
sg run chat/api -- codex

# Return to the same workspace and let the agent resume its conversation:
sg run chat/auth -- claude --continue
sg run chat/api -- codex resume --last

# Or choose the workspace interactively, independently of the agent:
sg run -- claude --continue
sg run -- codex resume --last

# Find workspace branches, paths, and persistence/lock status:
sg list

# Explicitly mark disposable automation for garbage collection, then reap the
# workspace as soon as its branch is merged. `sg run` returns the child's exit
# status, so `&&` gates the merge on the agent actually succeeding:
sg run agent/test --ephemeral -- codex exec "implement the API" &&
  git merge agent/test &&
  sg gc --prefix agent/test --older-than 0s --delete-branches

# `--older-than` is an idle-age filter, not a delay: it selects workspaces
# whose files have been untouched for at least that long, and it defaults to
# 24h. A workspace used seconds ago is only reaped by an explicit small value
# such as `0s`, which is why the line above uses one. Larger values are for
# sweeping workspaces that really have been abandoned:
sg gc --older-than 24h --delete-branches

# Either way, without the merge, safe GC removes only the worktree and retains
# the unmerged branch for review.
```

`sg run [branch] -- <command>` launches a command inside a workspace.
Omit the branch for a numbered picker of existing workspaces, including the
main checkout and detached worktrees. The picker shows branch, path,
persistence, and lock status; type a number to select or `q` to cancel.
Selection requires an interactive terminal. Scripts must supply a branch.
Always put `--` before the child command.

Workspaces do not store a preferred agent or launch command: you can select
the same workspace for Claude, Codex, a shell, or any other command. Claude's
`--continue` resumes its most recent conversation in that directory;
`--resume` opens its conversation picker. Codex uses `resume --last`.
Those agent options select conversations; simgit's picker selects files and
branches. To create a workspace, supply a new branch name explicitly.

`run` reuses the branch's registered worktree, including uncommitted files. If
only the branch exists, it creates a worktree at that branch's current commit;
otherwise it creates both. `add` accepts an explicit new branch or
auto-generates a collision-resistant `agent/<uuid>` branch when it is omitted.
Everything after `--` goes to the child command unchanged. The agent owns its
conversation history and resume behavior; simgit owns the workspace.

`sg run` retains the worktree after the child exits, and exits with the child's
own status: the child's code for an ordinary exit, and `128 + signal` when a
signal killed it. `sg run … && git merge …` therefore behaves like any other
launcher, and `127` still means "command not found". The notice naming the
retained workspace is written to stderr, so it never contaminates the child's
stdout. `--ephemeral` only makes the worktree
eligible for GC. GC skips running or dirty worktrees. With
`--delete-branches`, normal safe branch deletion retains unmerged commits;
`--discard-dirty` and `--delete-unmerged` are the explicit paths that may
discard uncommitted changes or delete an unmerged branch, and must never be
used by automation without the owner's explicit authority. Workers
launched in an allocated checkout need no infrastructure prompt: they work and
commit normally while the capable harness creates, integrates, and cleans up
the workspace.

## For agents and harnesses

A harness owns discovery, allocation, launch, and cleanup. `doctor --json` is
the preflight: run it anywhere to gate acceptance on its `identity` field
before trusting a command merely named `simgit` or `sg`, then again inside the
source repository for the readiness diagnostics. `add --json` is the
allocation: launch the agent with its working directory set to the returned
absolute `path` and keep the returned `cleanup_token` for safe removal, and
mark disposable automation `--ephemeral` — which makes a workspace eligible for
GC, and is never authority to discard dirty files or unmerged commits.

The [canonical agent policy](docs/agent-integration.md#canonical-agent-policy),
the full [allocator/provider contract](docs/agent-integration.md#allocatorprovider-contract)
with its JSON field tables, installation policy, skill installation, and crash
recovery are in [docs/agent-integration.md](docs/agent-integration.md); the
model-facing version is
[skills/simgit-worktrees/SKILL.md](skills/simgit-worktrees/SKILL.md).

Agents that use the skills CLI can install the bundled skill directly, with the
user's approval:

```bash
npx skills add abendrothj/simgit --skill simgit-worktrees
```

## Platform support

`sg add` creates real Git linked worktrees populated from a shared,
immutable baseline using copy-on-write:

- **macOS (APFS)** — native `clonefile`, **zero dependencies**. This is the
  primary, best-supported path (APFS is the default on every Mac since 2017).
- **Linux with reflink** (btrfs, xfs) — native reflink, zero dependencies.
- **Linux without reflink** (ext4, …) — `fuse-overlayfs` (unprivileged, no
  root, no kernel module), so the disk win also lands on stock ext4 and CI.
- **No CoW available** — transparent fallback to a plain `git checkout` (pass
  `--require-cow` to fail instead).
- **Windows** — intentionally unsupported: ordinary NTFS does not provide the
  general reflink primitive that makes simgit useful. Use Git worktrees
  directly, or run simgit under WSL on a supported Linux filesystem.

No FUSE or kernel extension is ever used on macOS; `fuse-overlayfs` is a
Linux-only fallback.

> **Scope of isolation:** simgit separates Git branches, indexes, and working
> trees. It is not a security sandbox and does not isolate processes, network,
> credentials, environment variables, or files outside the worktree.

## Why

If you run several agents against one repository, plain `git worktree` gives
each a full copy of the working tree — N agents × repo size on disk. `sg add`
keeps the isolation but drops the duplication: every worktree is a real
`.git/worktrees` checkout whose unchanged data is CoW-shared with one cached
baseline (via reflink, or a fuse-overlayfs mount where reflink isn't
available).

Agents work in parallel on their own branches and integrate through normal Git
merges — no coordination layer, no conflict arbitration, no lock service.

### Measured

The number that matters is the **marginal cost of one more worktree**, not a
multiple: a multiple is just the worktree count restated. That marginal cost
is filesystem and index metadata, so it scales with **how many paths a
repository has and how long they are** — not with how much content it holds.
Measured on APFS, `df` deltas, four worktrees per run:

| Repository | Tracked paths | Tree | Each extra `git worktree` | Each extra simgit worktree |
|---|---:|---:|---:|---:|
| `microsoft/vscode` | 18,707 | 553 MiB | 567 MiB | **9.8–10.8 MiB** (1.1%) |
| `git/git` | 4,850 | 72 MiB | 74 MiB | **~2.2 MiB** (3.1%) |
| 100k × 4 KiB files | 100,000 | 390 MiB | 398 MiB | **37.5–38.3 MiB** (9.4%) |
| 200 × 8 MiB files | 200 | 1600 MiB | 1608 MiB | **107 KiB** (0.007%) |

That is **~0.4 KiB per tracked path** in these repositories, plus a fixed
~60 KiB per worktree. Content size does not enter into it: files from 4 KiB to
64 MiB cost the same per file, because `clonefile` shares the extent tree by
reference instead of copying it. Path length does — a 120-character path costs
771 B against 385 B for a 4-character one, split between filesystem directory
entries and the copied Git index.

So a repository of many small files with short names is where the technique
pays least (~10% of the tree per worktree), and one of large files is where it
approaches free. Cold setup for eight vscode worktrees: **6.4–7.0 s** against
**14.0–14.2 s** for eight plain `git worktree` — on macOS the whole tree is
cloned in one `clonefile(2)` call and the worktree adopts the baseline's index,
so a single worktree lands in 0.56 s (5.87 s on the per-file path Linux
reflink still uses). Hot read and metadata cost overlaps ordinary worktree
I/O; the first durable write is slower while the filesystem splits shared
extents. Full method:
[docs/scaling_benchmark.md](docs/scaling_benchmark.md).

One baseline is materialized per distinct base commit — a full tree each — so
branching from several commits costs accordingly; `sg prune` reports
and reclaims that cache.

> **Measure with `df`, not `du`.** `du` reports *logical* size and cannot see
> clonefile/reflink block-sharing, so a CoW worktree looks like a full copy to
> it. Only `df` (physical blocks consumed) shows what a worktree actually
> allocated.

### Estimating it for your repository

```bash
# entries × ~0.4 KiB, plus ~60 KiB, is what each extra worktree will cost
git ls-files | wc -l
```

Roughly `60 KiB + 0.4 KiB × tracked-paths` per worktree on APFS, so 5k paths
cost ~2 MiB, 20k cost ~9 MiB, 250k cost ~110 MiB. Compare that against your
working tree: the ratio is what you save per agent. Many small files is the
bad case — 100k × 4 KiB pays ~10% of the tree per worktree — and even there
eight worktrees cost about what one plain `git worktree` copy does.

If that fraction is too high for your repository, the options are:

- **Check out only what the agent needs:** `--sparse <dir>`, repeatable, using
  Git's cone-mode sparse checkout. The cost falls with the cone, because both
  the files on disk and the worktree's index shrink: on the 100k × 4 KiB tree,
  one directory of ten costs **3.8 MiB per worktree instead of 37.9 MiB**, and
  the index drops from 7.9 MiB to 0.79 MiB. Paths outside the cone are marked
  `skip-worktree`, so `git status` stays clean and commits and merges behave
  normally — but the agent cannot see or build against them, so it only suits
  work that is genuinely confined to a subtree.

  ```bash
  # Narrow a workspace when you create it …
  sg add agent/api --sparse services/api --sparse libs/shared

  # … or when `run` creates one for a branch that does not exist yet
  sg run agent/web --sparse apps/web -- claude

  # Reuse keeps the cone the workspace was created with
  sg run agent/api -- claude
  ```

  `--sparse` applies only when creating a workspace, like `--base`; reusing an
  existing one keeps whatever cone it was created with, and passing `--sparse`
  again on reuse is rejected rather than silently ignored.
- **On Linux with `fuse-overlayfs`, prefer the overlay mode:**
  `SIMGIT_POPULATE=overlay sg add …`. An overlay worktree's
  `upperdir` starts empty, so it pays no per-file metadata at all regardless
  of entry count — it trades that for FUSE overhead on every read. simgit
  otherwise prefers reflink when both are available, which is the wrong
  default for entry-heavy repositories. Neither the metadata saving nor the
  read overhead has been measured yet; treat the direction as sound and the
  magnitude as unknown. `--sparse` is rejected with this backend, where the
  lower layer is the whole baseline and narrowing the view saves nothing.
- **Keep worktrees on one base commit.** The per-worktree cost is small; a
  second *baseline* is a whole tree. `sg prune` reports the cache.
- **Reuse workspaces instead of creating them.** `sg run <branch>` attaches to
  an existing worktree, so a stable set of agent workspaces costs a stable
  amount of disk.

## Reference

The rest of the surface in detail: cleanup semantics, where worktrees live,
what an agent sees inside one, populate backends, `run` locks, and the
machine-readable output.

### Removal and unlock semantics

`remove` accepts either a path or a branch name. Plain `remove` refuses a dirty
worktree; use `--discard-dirty` only with the owner's explicit authority to
discard it. Deleting a branch that is not merged additionally requires
`--delete-unmerged` next to `--delete-branch`. Removing a target that no longer
has a worktree succeeds and reports `already_absent: true`, so a retried or
at-least-once cleanup is safe. With `--delete-branch`, that idempotence holds
only for the branch-name form, which still deletes a leftover branch of that
name: an already-removed *path* cannot name a branch, so
`remove <absent-path> --delete-branch` is an error telling you to pass the
branch name instead.

`unlock` is idempotent in the same way and one step further: a target that is
not locked, and a target that no longer exists at all, both exit 0 and report
`was_locked: false`. Crash recovery can call it unconditionally.

### Where worktrees live

New worktrees are created beside the repository, under
`../.simgit/<repo>/`, and `SIMGIT_WORKTREE_ROOT` or `--path` overrides that.
The directory name is not the branch name: characters Git allows in a branch
but a single path component does not — `/` above all — are replaced with `-`,
and whenever that rewriting changes the name an 8-hex digest of the original
branch is appended, so distinct branches that flatten to the same string still
get distinct directories. `feat/my-feature` becomes
`../.simgit/<repo>/feat-my-feature-c75e230f`, while an already-safe name like
`feature-x` is used as-is. Script against the `worktree`/`path` field that
`add --json` returns, not against a formula.

`--path` names the worktree directory itself, not a parent to allocate inside.
Missing parents are created. The directory may already exist if it is empty —
harnesses that pre-create one directory per job work — but a non-empty
directory is refused, and two concurrent allocations to the same path can never
both succeed. A destination inside the source repository is allowed but warned
about on stderr: it becomes untracked clutter in `git status` and `git clean`
can delete it, so pass a path outside the repository or set
`SIMGIT_WORKTREE_ROOT`.

`add` creates a branch, so it refuses one that already exists and points at the
command that does the right thing: `sg run <branch> -- <command>` attaches to
that branch's worktree, or creates one at its current commit when only the
branch survives — the situation you are in after a `remove` that retained the
branch.

Worktrees are deliberately **not** placed inside `.git`: agent harnesses and
editors treat everything under `.git/` as off-limits or invisible — Claude Code
refuses to edit files there — so a worktree nested in the git dir is unusable
by the tools this exists to serve. The worktree itself is still registered in
Git's normal `.git/worktrees/` registry, and the cached baseline stays internal
in `.git/simgit/baselines/`. Removing the last worktree also removes the empty
`.simgit` directory.

### What the agent sees

`sg run` does not emulate Git or intercept Git commands. It launches the child
with its working directory set to a real, registered linked worktree. From the
agent's perspective it is an ordinary repository: `git status`, `diff`, `add`,
`commit`, `restore`, `switch`, `merge`, `rebase`, `cherry-pick`, hooks,
attributes and ignores work through Git itself. The files are ordinary local
files; copy-on-write is handled below Git by the filesystem.

Each worktree has its own files, index, `HEAD`, current branch and in-progress
merge/rebase state. Git objects, refs, remotes, configuration, hooks and
stashes are shared with the main repository, exactly as with `git worktree`.
That means Git will not let two worktrees check out the same branch, and
repository-wide operations can affect the other worktrees. simgit provides
workspace isolation, not a security boundary.

When an agent commits, Git writes the commit to the shared object database and
advances only that agent's branch. The commit is immediately visible from the
main checkout, but the main branch and its files do not change until you merge
or rebase it:

```bash
git log agent/auth
git merge agent/auth
sg gc --older-than 0s --delete-branches
```

### Reuse, persistence, and populate backends

New `run` worktrees are persistent by default. Existing worktrees keep their
persistence setting unless you pass `--ephemeral` or `--persistent` explicitly.
GC selects only ephemeral worktrees by default, even with `--discard-dirty`;
use `--include-persistent` to explicitly include persistent workspaces. This
changes the earlier defaults: automation that relied on `run` creating
disposable workspaces should now pass `--ephemeral`.

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

### Run locks and recovery

While a command launched by `run` is active, its linked worktree is locked
against removal and GC, including `--discard-dirty`. A second `run` in the same
workspace is refused until the first exits. The lock file records the
launcher's PID. If the launcher is killed the lock is deliberately retained;
recover it with `sg unlock [TARGET]`, where TARGET is a workspace path or
branch that defaults to the workspace containing the current directory. It
clears the lock wherever it lives: a linked worktree's lock is Git's own
`.git/worktrees/<slug>/locked`, while `.git/simgit-run.lock` is used only when
`run` targets the main checkout, which Git cannot lock. `unlock` refuses while
the recorded owner is still alive and names that PID — stop that process
instead of looking for an override flag, because there is none. Unlocking a
workspace that is not locked is a success, and so is unlocking a target that no
longer exists at all: both exit 0 with `was_locked: false`, so crash recovery
can unlock unconditionally before removal. A lock file written before locks
recorded a PID counts as unknown-owner and can be cleared. With `--json`,
`unlock` reports `unlocked`,
`was_locked`, and `owner_pid`. Commands launched outside `sg run` are not
tracked. Idle age still uses index/directory modification time, so it is only a
cleanup heuristic for explicitly disposable workspaces.

### Machine-readable output and GC

The global `--json` flag gives orchestrators structured output from `doctor`,
`add`, `remove`, `list`, `unlock`, `gc`, `prune` and `repair`; it is accepted
before or after the command, and `run` is the one command that rejects it.
`list` reports each worktree's `ephemeral` flag, its `locked` state while a
command runs, and the `mode` it was populated with (`cow-clone`, `overlay`,
`git-checkout`, or `null` for the main worktree) — so you can confirm a
workspace really is CoW-backed rather than a silent full-copy fallback. The
human `list` appends the same mode as a fourth tab-separated field. Every
worktree path simgit prints or returns is symlink-resolved and lexically
normalized, so one worktree always has exactly one string form and
orchestrators can compare those strings directly. That resolution applies
whenever a target resolves to a real worktree, so `remove`'s `removed` is the
resolved absolute path even when you named the worktree by branch or by a
relative path. It is only when nothing resolves — `remove` on an already-absent
target, `unlock` on one — that `removed` and `unlocked` repeat the target
verbatim, because there is no worktree left to resolve against and inventing a
resolved form would imply one exists. GC selects
ephemeral worktrees only unless `--include-persistent`, skips uncommitted
changes unless `--discard-dirty`, and accepts `--prefix <branch-prefix>`,
`--older-than <90s|30m|24h|7d>`, `--delete-branches`, and `--dry-run`. Every
worktree GC considered but did not reap appears in `skipped` with a reason:
`persistent`, `dirty`, `locked`, or `recently-active` for one that is simply
younger than `--older-than`. An empty `reaped` list is therefore always
explained. Safe
branch deletion retains unmerged work; `--delete-unmerged` next to
`--delete-branches` explicitly discards unmerged branches.

### Baselines and `prune`

`prune` does two things and reports both. It deregisters stale Git worktree
registrations — entries whose directory is gone — listing their paths in
`pruned_registrations` and printing `pruned N stale registration(s)`. And it
reclaims the baseline cache, reporting what that cache still costs in both
formats (`retained_bytes` in JSON). One baseline is materialized per distinct
base commit and kept for seven days, so branching from several commits costs
one full tree each until pruned; `prune --all` drops every cached baseline
immediately, including recently used ones, and is the way to reclaim that space
without waiting out the seven-day window. A dropped baseline is rematerialized
on the next `add` that needs it. Compare the retained baseline cost directly
with the marginal cost of each additional worktree; the savings depend on
repository shape, not a fixed multiplier.

## Repository layout

```text
simgit/
├── sg/                 the CLI (`simgit`; `sg` is the short alias)
│   └── src/commands/worktree/   command launch/picker, CoW and overlay backends
├── tests/              CoW scaling benchmarks + overlay integration test
├── skills/             first-party `simgit-worktrees` agent skill
├── packaging/          Homebrew formula (prebuilt-binary install)
├── install.sh          checksummed installer for pinned release binaries
└── docs/               agent integration and scaling benchmark methodology
```

## History

simgit began as a daemon ("a borrow checker for filesystems") that mounted a
virtual filesystem per agent session and arbitrated writes in real time, with a
Rust SDK and Python bindings. That approach — FUSE/NFS/WinFSP VFS backends, a
session daemon, path leases, and commit scheduling — was retired in favor of
standing on the lean native-CoW worktree path, which delivers the same disk and
I/O properties for isolated parallel agents with none of the moving parts. The
full daemon implementation remains in this repository's Git history.

## License

MIT
