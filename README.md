# simgit

**Cheap, isolated Git worktrees for running many agents on one repository at once.**

`sg worktree` creates real Git linked worktrees populated from a shared,
immutable baseline using filesystem copy-on-write (APFS `clonefile` on macOS,
reflink on Linux). Each agent gets its own fully isolated working tree, but
unchanged files share physical disk instead of being duplicated. There is no
daemon and no server: **Git owns the refs, the filesystem owns the data.**

## Why

If you run several agents against one repository, plain `git worktree` gives
each a full copy of the working tree — N agents × repo size on disk. `sg
worktree` keeps the isolation but drops the duplication: every worktree is a
real `.git/worktrees` checkout whose unchanged extents are CoW-shared with one
cached baseline. On filesystems without clone support it transparently falls
back to an ordinary `git checkout` (use `--require-cow` to make CoW mandatory).

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
cargo build --release   # binary at target/release/sg
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

# Prune stale registrations and old cached baselines
sg worktree prune
```

The worktree is registered in Git's normal `.git/worktrees/` registry; the
cached baseline lives under `.git/simgit/baselines/`. Plain `remove` delegates
to Git and refuses a dirty worktree unless you pass `--force`.

### Running agents in parallel

```bash
# Shell 1
cd $(sg worktree add feat/auth)     # agent writes files, runs git commit

# Shell 2
cd $(sg worktree add feat/api)      # another agent, fully isolated
```

Each agent commits to its own branch; you integrate with `git merge`/`rebase`
as usual.

## Repository layout

```text
simgit/
├── sg/                 the CLI (`sg worktree`)
├── tests/              CoW scaling benchmarks
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
