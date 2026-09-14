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

## What a worktree costs

Plain `git worktree` gives each agent a full copy of the working tree — N
agents × repo size on disk. `sg add` keeps the isolation and drops the
duplication: every worktree is a real `.git/worktrees` checkout whose unchanged
data is copy-on-write shared with one cached baseline (reflink, or a
fuse-overlayfs mount where reflink is unavailable).

The number that matters is the **marginal cost of one more worktree**, not a
multiple — a multiple is just the worktree count restated. That cost is
filesystem and index metadata, so it scales with **how many paths a repository
has and how long they are**, not with how much content it holds. Measured on
APFS, `df` deltas, four worktrees per run:

| Repository | Tracked paths | Tree | Each extra `git worktree` | Each extra simgit worktree |
|---|---:|---:|---:|---:|
| `microsoft/vscode` | 18,707 | 553 MiB | 567 MiB | **9.8–10.8 MiB** (1.1%) |
| `git/git` | 4,850 | 72 MiB | 74 MiB | **~2.2 MiB** (3.1%) |
| 100k × 4 KiB files | 100,000 | 390 MiB | 398 MiB | **37.5–38.3 MiB** (9.4%) |
| 200 × 8 MiB files | 200 | 1600 MiB | 1608 MiB | **107 KiB** (0.007%) |

Roughly `60 KiB + 0.4 KiB × tracked-paths` per worktree, so 5k paths cost
~2 MiB and 250k cost ~110 MiB. Estimate yours:

```bash
git ls-files | wc -l
```

Content size does not enter into it: a 4 KiB file and a 64 MiB file cost the
same, because `clonefile` shares the extent tree by reference. Many small files
with short names is the worst case (~10% of the tree per worktree, where eight
simgit worktrees still cost about what one plain `git worktree` copy does);
large files approach free. Creation is also faster: eight vscode worktrees in
**6.4–7.0 s** against **14.0–14.2 s**, because macOS clones the whole tree in
one `clonefile(2)` call and the worktree adopts the baseline's index.

> **Measure with `df`, not `du`.** `du` reports *logical* size and cannot see
> clonefile/reflink block sharing, so a CoW worktree looks like a full copy to
> it. Only `df` shows what a worktree actually allocated.

Method, hardware, and full results:
[docs/scaling_benchmark.md](docs/scaling_benchmark.md).

## Install

On macOS, prefer the Homebrew formula: it uses a versioned release asset with a
pinned SHA-256 digest.

```bash
# macOS (preferred)
brew install abendrothj/tap/simgit

# From crates.io (either one)
cargo install simgit-cli --locked
cargo binstall simgit-cli

# Verify the install. `doctor` runs anywhere, so this works before there is a
# repository to check.
simgit doctor --json
```

On Linux without a reflink filesystem, install `fuse-overlayfs` to get the CoW
path (e.g. `apt-get install fuse-overlayfs`); otherwise simgit falls back to a
plain checkout. Prefer it as a Git subcommand? `git config --global alias.wt
'!simgit'`, then `git wt add feat/x`.

Pinned-tag installs with the checksum-verified `install.sh`, approval gating,
and version pinning are operator and agent policy:
[docs/agent-integration.md](docs/agent-integration.md#installation-and-skill-discovery).

## Commands

The surface is flat — nine verbs, no `worktree` namespace. `--json` is a global
flag accepted before or after the command; `run` is the only command that
rejects it, because it owns the child's stdout. Location is always `--path`.

| Command | What it does |
|---|---|
| `sg doctor` | Identity, version, filesystem, `cow_supported`, worktree root, baseline cache, stale registrations. Exits 0 outside a repository, reporting every repository-dependent field as `null`. |
| `sg add [BRANCH]` | Create a linked worktree on a new branch and print its path. Omit `BRANCH` for a generated `agent/<uuid>`; `--detach` for a branchless one. `--path`, `--base`, `--sparse <DIR>`, `--require-cow`, `--ephemeral`. |
| `sg list` | Every linked worktree with branch, path, persistence, lock state, and populate `mode`. |
| `sg run [BRANCH] -- <cmd>` | Run a command inside a workspace, creating or reusing it. Omit `BRANCH` for an interactive picker. `--persistent`/`--ephemeral`; `--path`, `--base`, `--sparse`, `--require-cow` apply only when creating. |
| `sg remove [TARGET]` | Remove a worktree by path or branch. `--commit -m <msg>`, `--delete-branch`, and the authority-gated `--discard-dirty` / `--delete-unmerged`. |
| `sg unlock [TARGET]` | Clear a `run` lock stranded by a killed launcher. |
| `sg gc` | Reap idle ephemeral worktrees. `--older-than <90s\|30m\|24h\|7d>` (default 24h), `--prefix`, `--delete-branches`, `--include-persistent`, `--discard-dirty`, `--delete-unmerged`, `--dry-run`. |
| `sg prune` | Drop stale Git registrations and expired cached baselines; `--all` reclaims every baseline now. |
| `sg repair` | Remount overlay-backed worktrees after a reboot or interrupted mount (Linux). |

There is no `--force`: the two operations that can destroy work name what they
destroy, and `--delete-unmerged` requires `--delete-branch`/`--delete-branches`
beside it.

## Everyday use

```bash
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
copied. `sg run` does not emulate Git or intercept commands: it sets the child's
working directory to that checkout, and Git does the rest.

## Running agents

```bash
# Launch an agent's normal terminal interface in its own workspace
sg run chat/auth -- claude
sg run chat/api -- codex

# Return to the same workspace and let the agent resume its conversation
sg run chat/auth -- claude --continue
sg run chat/api -- codex resume --last

# Or pick the workspace interactively, independently of the agent
sg run -- claude --continue

# Disposable automation: `run` exits with the child's status, so `&&` gates the
# merge on the agent actually succeeding, and `0s` reaps a just-used workspace
sg run agent/test --ephemeral -- codex exec "implement the API" &&
  git merge agent/test &&
  sg gc --prefix agent/test --older-than 0s --delete-branches
```

Omitting the branch opens a numbered picker over existing workspaces, including
the main checkout and detached worktrees, showing branch, path, persistence and
lock status. It needs an interactive terminal; scripts must supply a branch, and
a new branch name is how you create a workspace. Everything after `--` goes to
the child unchanged.

A workspace stores no preferred agent: the same one can be selected for Claude,
Codex, a shell, or anything else. Those tools' `--continue` and `resume --last`
select *conversations*; simgit's picker selects *files and branches*.

`run` retains the worktree after the child exits and exits with the child's own
status — the child's code normally, `128 + signal` when a signal killed it, so
`127` still means "command not found". The notice naming the retained workspace
goes to stderr, never into the child's stdout. `--ephemeral` only makes a
workspace *eligible* for GC; it is never authority to discard dirty files or
unmerged commits.

## Sharp edges

Everything here is deliberate, and none of it is guessable:

- **`run` workspaces are persistent by default.** GC reaps only ephemeral ones
  unless you pass `--include-persistent`. Automation that relied on `run`
  creating disposable workspaces must now pass `--ephemeral`.
- **`--older-than` is an idle-age filter, not a delay.** It selects workspaces
  untouched for at least that long and defaults to 24h, so reaping a workspace
  you just used takes an explicit `0s`.
- **A killed launcher keeps its lock on purpose.** `sg unlock [TARGET]` clears
  it, refuses while the recorded owner PID is alive and names that PID, and
  there is no override flag.
- **Cleanup is idempotent.** `remove` on an absent target exits 0 with
  `already_absent: true`, and `unlock` on an unlocked or absent target exits 0
  with `was_locked: false`, so retried cleanup is safe. The one error case is
  `remove <absent-path> --delete-branch`: a path that is gone cannot name the
  branch it once held.
- **Directory names are slugs, not branch names.** `feat/my-feature` becomes
  `../.simgit/<repo>/feat-my-feature-c75e230f`. Script against the `path` that
  `add --json` returns, never against a formula.
- **`--base`, `--sparse` and `--require-cow` are creation-only** and are
  rejected on reuse rather than silently ignored.

> **Scope of isolation:** simgit separates Git branches, indexes, and working
> trees. It is not a security sandbox and does not isolate processes, network,
> credentials, environment variables, or files outside the worktree.

## Keeping the cost down

- **Check out only what the agent needs:** `sg add agent/api --sparse
  services/api --sparse libs/shared`, repeatable, Git cone-mode. Both the files
  and the index shrink with the cone — on the 100k × 4 KiB tree, one directory
  of ten costs **3.8 MiB per worktree instead of 37.9 MiB**. Paths outside the
  cone are `skip-worktree`, so status, commits and merges behave normally, but
  the agent cannot see or build against them.
- **On Linux with `fuse-overlayfs`, prefer overlay mode:**
  `SIMGIT_POPULATE=overlay sg add …`. Its `upperdir` starts empty, so it pays no
  per-file metadata regardless of entry count, trading that for FUSE overhead on
  every read. The magnitude is unmeasured; the direction is sound.
- **Keep worktrees on one base commit.** A second *baseline* is a whole tree;
  `sg prune` reports and reclaims that cache.
- **Reuse workspaces instead of creating them.** `sg run <branch>` attaches to
  an existing worktree, so a stable set of agent workspaces costs a stable
  amount of disk.

## Platform support

- **macOS (APFS)** — native `clonefile`, **zero dependencies**. The primary,
  best-supported path (APFS is the default on every Mac since 2017).
- **Linux with reflink** (btrfs, xfs) — native reflink, zero dependencies.
- **Linux without reflink** (ext4, …) — `fuse-overlayfs` (unprivileged, no root,
  no kernel module), so the disk win also lands on stock ext4 and CI.
- **No CoW available** — transparent fallback to a plain `git checkout`; pass
  `--require-cow` to fail instead, and check `mode` in `list`/`add --json` to
  confirm what you actually got.
- **Windows** — intentionally unsupported: ordinary NTFS has no general reflink
  primitive. Use Git worktrees directly, or run simgit under WSL.

No FUSE or kernel extension is ever used on macOS; `fuse-overlayfs` is a
Linux-only fallback.

## For agents and harnesses

A harness owns discovery, allocation, launch, and cleanup. `doctor --json` is
the preflight: run it anywhere to gate acceptance on its `identity` field before
trusting a command merely named `simgit` or `sg`, then again inside the source
repository for the readiness diagnostics. `add --json` is the allocation: launch
the agent with its working directory set to the returned absolute `path`, keep
the returned `cleanup_token` for safe removal, and mark disposable automation
`--ephemeral`.

Agents that use the skills CLI can install the bundled skill directly, with the
user's approval:

```bash
npx skills add abendrothj/simgit --skill simgit-worktrees
```

## Documentation

- [docs/reference.md](docs/reference.md) — the detailed semantics: removal and
  unlock, where worktrees live, what an agent sees, populate backends, run
  locks, JSON fields, GC, and baselines.
- [docs/agent-integration.md](docs/agent-integration.md) — canonical agent
  policy, the allocator/provider contract, installation policy, crash recovery.
- [skills/simgit-worktrees/SKILL.md](skills/simgit-worktrees/SKILL.md) — the
  model-facing version of that policy.
- [docs/scaling_benchmark.md](docs/scaling_benchmark.md) — benchmark method.
- [CONTRIBUTING.md](CONTRIBUTING.md) and [TESTING.md](TESTING.md) — building,
  gates, and the test layout.

## History

simgit began as a daemon ("a borrow checker for filesystems") that mounted a
virtual filesystem per agent session and arbitrated writes in real time. That
approach — FUSE/NFS/WinFSP backends, a session daemon, path leases, commit
scheduling — was retired in favor of the lean native-CoW worktree path, which
delivers the same disk and I/O properties with none of the moving parts. The
full daemon implementation remains in this repository's Git history.

## License

MIT
