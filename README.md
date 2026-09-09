# simgit

[![crates.io](https://img.shields.io/crates/v/simgit-cli.svg)](https://crates.io/crates/simgit-cli)
[![CI](https://github.com/abendrothj/simgit/actions/workflows/ci.yml/badge.svg)](https://github.com/abendrothj/simgit/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/simgit-cli.svg)](LICENSE)

**Disk-efficient, separate Git worktrees for running many agents on one repository at once.**

<p align="center"><img src="assets/demo.gif" alt="sg run: launch agents in copy-on-write worktrees, merge their branches, and clean up" width="820"></p>

`sg worktree` creates real Git linked worktrees populated from a shared,
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

Each agent gets its own separate working tree, but unchanged files share
physical disk instead of being duplicated. There is no daemon and no server:
**Git owns the refs, the filesystem owns the data.**

> **Scope of isolation:** simgit separates Git branches, indexes, and working
> trees. It is not a security sandbox and does not isolate processes, network,
> credentials, environment variables, or files outside the worktree.

## Why

If you run several agents against one repository, plain `git worktree` gives
each a full copy of the working tree — N agents × repo size on disk. `sg
worktree` keeps the isolation but drops the duplication: every worktree is a
real `.git/worktrees` checkout whose unchanged data is CoW-shared with one
cached baseline (via reflink, or a fuse-overlayfs mount where reflink isn't
available).

Agents work in parallel on their own branches and integrate through normal Git
merges — no coordination layer, no conflict arbitration, no lock service.

### Measured

The number that matters is the **marginal cost of one more worktree**, not a
multiple: a multiple is just the worktree count restated. That marginal cost
is filesystem and index metadata, so it scales with **how many paths a
repository has and how long they are** — not with how much content it holds.
Measured on APFS, `df` deltas, four worktrees per run:

| Repository | Tracked paths | Tree | Each extra `git worktree` | Each extra `sg worktree` |
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
branching from several commits costs accordingly; `sg worktree prune` reports
and reclaims that cache.

> **Measure with `df`, not `du`.** `du` reports *logical* size and cannot see
> clonefile/reflink block-sharing, so a CoW worktree looks like a full copy to
> it. Only `df` (physical blocks consumed) shows what a worktree actually
> allocated.

## Install

```bash
# macOS / Linux — prebuilt binary, no toolchain required
curl -fsSL https://raw.githubusercontent.com/abendrothj/simgit/main/install.sh | sh

# Homebrew
brew install abendrothj/tap/simgit

# Cargo — from crates.io (`cargo install` compiles, `binstall` fetches a binary)
cargo install simgit-cli
cargo binstall simgit-cli
```

The script installs `sg` into `~/.local/bin`; set `SIMGIT_INSTALL_DIR` to
choose another directory or `SIMGIT_VERSION=v0.1.4` to pin a release. Verify
with `sg --version`.

On Linux without a reflink filesystem, install `fuse-overlayfs` to get the CoW
path (e.g. `apt-get install fuse-overlayfs`); otherwise `sg` falls back to a
plain checkout.

Prefer it as a Git subcommand? Add an alias:

```bash
git config --global alias.wt '!sg worktree'
git wt add feat/x
```

## Usage

```bash
# Create a CoW linked worktree on a new branch and cd into it
cd "$(sg worktree add feat/my-feature)"

# It's a standard linked checkout — every git command and hook just works
echo "hello" > README.md
git add README.md
git commit -m "work"

# List worktrees (add --json for machine-readable output)
sg worktree list

# Commit any leftover changes and remove the worktree (the branch is retained)
sg worktree remove --commit --message "clean up"

# Or, once a branch is merged while its worktree still exists, remove both
sg worktree remove feat/my-feature --delete-branch

# Or discard uncommitted changes explicitly
sg worktree remove --force

# Reap idle/abandoned worktrees (see "Running agents in parallel")
sg worktree gc --older-than 24h

# Remount overlay-backed Linux worktrees after a reboot/interrupted mount
sg worktree repair

# Prune stale registrations and old cached baselines
sg worktree prune
```

`remove` accepts either a path or a branch name. Plain `remove` refuses a dirty
worktree unless you pass `--force`.

New worktrees are created beside the repository, in
`../.simgit/<repo>/<branch>`, and `SIMGIT_WORKTREE_ROOT` or `--path` overrides
that. They are deliberately **not** placed inside `.git`: agent harnesses and
editors treat everything under `.git/` as off-limits or invisible — Claude Code
refuses to edit files there — so a worktree nested in the git dir is unusable
by the tools this exists to serve. The worktree itself is still registered in
Git's normal `.git/worktrees/` registry, and the cached baseline stays internal
in `.git/simgit/baselines/`. Removing the last worktree also removes the empty
`.simgit` directory.

### Running agents

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
sg worktree list

# Explicitly mark disposable automation for garbage collection:
sg run agent/test --ephemeral -- codex exec "implement the API"
git merge agent/test
sg worktree gc --older-than 1h --delete-branches

# Explicitly discard abandoned ephemeral work and its unmerged branch:
sg worktree gc --older-than 24h --delete-branches --force
```

`sg run [branch] -- <command>` is the short form of `sg worktree run`.
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
otherwise it creates both. `add` always requires a new branch and workspace.
Everything after `--` goes to the child command unchanged. The agent owns its
conversation history and resume behavior; simgit owns the workspace.

New `run` worktrees are persistent by default. Existing worktrees keep their
persistence setting unless you pass `--ephemeral` or `--persistent` explicitly.
GC selects only ephemeral worktrees by default, even with `--force`; use
`--include-persistent` to explicitly include persistent workspaces. This changes
the earlier defaults: automation that relied on `run` creating disposable
workspaces should now pass `--ephemeral`.

On reuse, `--path` must identify the existing workspace. `--base` and
`--require-cow` are creation-only options and are rejected on reuse; `--base`
is also rejected when attaching an existing branch. `--require-cow` cannot be
combined with `SIMGIT_POPULATE=checkout`.

While a command launched by `run` is active, its linked worktree is locked
against removal and GC, including `--force`. A second `run` in the same
workspace is refused until the first exits. If the launcher is killed, the
lock is deliberately retained: verify that its child has stopped, then use
`git worktree unlock <path>` to recover (for the main worktree, remove
`.git/simgit-run.lock` instead). Commands launched outside `sg run`
are not tracked. Idle age still uses index/directory modification time, so
it is only a cleanup heuristic for explicitly disposable workspaces.

`--json` on `add` / `remove` / `list` / `gc` / `prune` / `repair` gives
orchestrators structured output. `list` reports each worktree's `ephemeral`
flag, its `locked` state while a command runs, and the `mode` it was populated
with (`cow-clone`, `overlay`, `git-checkout`, or `null` for the main worktree)
— so you can confirm a workspace really is CoW-backed rather than a silent
full-copy fallback. The human `list` appends the same mode as a fourth
tab-separated field. GC skips uncommitted changes unless `--force`, and
accepts `--prefix <branch-prefix>`, `--older-than <90s|30m|24h|7d>`,
`--delete-branches`, and `--dry-run`. Safe branch deletion retains unmerged
work; combining it with `--force` explicitly discards unmerged branches.

`prune` reports what the baseline cache still costs, in both formats
(`retained_bytes` in JSON). One baseline is materialized per distinct base
commit and kept for seven days, so branching from several commits costs one
full tree each until pruned — worth checking if disk grows faster than the
7× worktree saving implies.

Each agent commits to its own branch; you integrate with `git merge`/`rebase`
as usual — there is no shared state to coordinate.

## Repository layout

```text
simgit/
├── sg/                 the CLI (`sg worktree`)
│   └── src/commands/worktree/   command launch/picker, CoW and overlay backends
├── tests/              CoW scaling benchmarks + overlay integration test
├── packaging/          Homebrew formula (prebuilt-binary install)
├── install.sh          curl | sh installer for the release binaries
└── docs/               scaling benchmark methodology
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
