# simgit

**Cheap, isolated Git worktrees for running many agents on one repository at once.**

`sg worktree` creates real Git linked worktrees populated from a shared,
immutable baseline using copy-on-write:

- **macOS**: APFS `clonefile`.
- **Linux with reflink** (btrfs, xfs): reflink clone.
- **Linux without reflink** (ext4, …): `fuse-overlayfs` — unprivileged, no
  root, no kernel mount — so the disk win also lands on stock ext4 and CI.
- **Anywhere else / no CoW available**: transparent fallback to an ordinary
  `git checkout` (pass `--require-cow` to fail instead).

Each agent gets its own fully isolated working tree, but unchanged files share
physical disk instead of being duplicated. There is no daemon and no server:
**Git owns the refs, the filesystem owns the data.**

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

Standing up eight isolated views of a 300 MiB tree on APFS:

| Path | Physical disk added | Cold setup |
|---|---:|---:|
| 8 × `git worktree` | 2405.9–2583.4 MiB | 6.32–6.66 s |
| 8 × `sg worktree` | 301.2 MiB | 14.10–14.15 s |

At least **8.0× less physical disk** at **2.1–2.2× the cold setup time**. Hot
read and metadata cost overlaps ordinary worktree I/O; the first durable write
is slower while the filesystem splits shared extents. Full method:
[docs/scaling_benchmark.md](docs/scaling_benchmark.md).

## Install

```bash
# From crates.io
cargo install simgit-cli

# Prebuilt binary (no compile), once releases are published
cargo binstall simgit-cli

# Homebrew (tap)
brew install abendrothj/tap/simgit

# From source
cargo build --release   # binary at target/release/sg
```

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
cd $(sg worktree add feat/my-feature)

# It's a standard linked checkout — every git command and hook just works
echo "hello" > README.md
git add README.md
git commit -m "work"

# List worktrees (add --json for machine-readable output)
sg worktree list

# Commit any leftover changes and remove the worktree
sg worktree remove --commit --message "clean up"

# Or discard uncommitted changes explicitly
sg worktree remove --force

# Reap idle/abandoned worktrees (see "Running agents in parallel")
sg worktree gc --older-than 24h

# Prune stale registrations and old cached baselines
sg worktree prune
```

`remove` accepts either a path or a branch name. The worktree is registered in
Git's normal `.git/worktrees/` registry; the cached baseline lives under
`.git/simgit/baselines/`. Plain `remove` refuses a dirty worktree unless you
pass `--force`.

### Running agents in parallel

Give each agent its own ephemeral worktree, then reap abandoned ones in bulk:

```bash
# Spawn N isolated agent sandboxes (each on its own branch)
for i in 1 2 3; do
  dir=$(sg worktree add "agent/$i" --ephemeral --json | jq -r .worktree)
  run_my_agent --cwd "$dir" &   # your orchestrator; agent commits to agent/$i
done
wait

# Integrate whichever branches you want with normal git, then bulk-clean:
sg worktree gc --ephemeral --older-than 1h     # reap idle agent sandboxes
```

`--json` on `add` / `remove` / `gc` gives orchestrators structured output.
`gc` skips worktrees with uncommitted changes unless `--force`, and takes
`--prefix <branch-prefix>`, `--older-than <90s|30m|24h|7d>`, and `--dry-run`.

Each agent commits to its own branch; you integrate with `git merge`/`rebase`
as usual — there is no shared state to coordinate.

## Repository layout

```text
simgit/
├── sg/                 the CLI (`sg worktree`)
├── tests/              CoW scaling benchmarks + overlay integration test
├── packaging/          Homebrew formula
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
