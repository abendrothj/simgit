# Scaling benchmark — `sg worktree` vs `git worktree`

simgit's central claim is that it **avoids duplicating the working tree** when
you run many agents against one repo. `git worktree` materializes a full
checkout per session (tree size × N on disk); simgit serves reads from a shared
baseline and stores only per-session deltas, so real disk stays flat.

This page records a reproducible measurement of that claim.

## Method

`tests/bench_scaling.sh` builds a synthetic repo with a fixed working-tree size,
then for each session count N creates N `git worktree` checkouts and, separately,
N `sg worktree` sessions. It measures:

- **Real host disk consumed** via `du -sxk`. The `-x` flag stays on one
  filesystem, so it counts the on-disk backing store and *excludes* the virtual
  NFS mount points simgit serves — i.e. it measures bytes actually written to
  disk, not the files an agent can see.
- **Time-to-ready** — wall-clock to create all N isolated working trees.

Both numbers are the baseline cost *before any agent writes*: what it costs just
to stand up N isolated views of the repo.

## Results

Working tree: 400 files × 128 KB = **67.2 MB per checkout**. macOS, NFSv3 backend.

| N (sessions) | git worktree disk | simgit disk | git worktree time | simgit time |
|---:|---:|---:|---:|---:|
| 1  | 67.2 MB   | 0.11 MB | 0.21 s | 0.25 s |
| 2  | 134.4 MB  | 0.13 MB | 0.42 s | 0.25 s |
| 4  | 268.8 MB  | 0.17 MB | 0.94 s | 0.32 s |
| 8  | 537.5 MB  | 0.25 MB | 1.77 s | 0.34 s |
| 16 | 1075.1 MB | 0.41 MB | 3.36 s | 0.50 s |

**Disk** grows linearly for `git worktree` (~67 MB per session) and stays
effectively flat for simgit (~0.1–0.4 MB of metadata). At N=16 that is **1075 MB
vs 0.41 MB — roughly 2,600× less disk.** The gap scales with working-tree size:
a larger repo widens it, a tiny repo shrinks it.

**Time-to-ready** grows linearly for `git worktree` (each checkout writes the
whole tree) and stays roughly flat for simgit (no tree is written).

## Honest reading of the result

- **The claim holds, and the effect is large at scale.** The more agents and the
  bigger the tree, the more decisively simgit wins on disk and setup time.
- **At N=1–2, `git worktree` is the simpler choice.** simgit's per-session disk
  is smaller even at N=1, but a daemon + loopback filesystem is a lot of
  machinery to justify for one or two checkouts. The operational win only shows
  up at high fan-out.
- **This is the pre-write baseline.** Once agents edit files, simgit's delta
  store grows — but only by what actually changed, still far below a full copy.
- **simgit trades disk for read indirection.** Reads are served over a loopback
  NFSv3 (macOS) / FUSE (Linux) layer rather than the local page cache, so
  individual file reads carry more per-op overhead than a plain checkout. The
  win is disk and setup-time scaling, not raw single-file read latency.

## Per-operation latency: CoW default vs. write-intercepting VFS

The disk win above is independent of backend. Where backends differ sharply is
**per-file-operation latency**, because the VFS backends route every read/write
through a user-space filesystem while the default CoW backend serves them from
the kernel page cache.

Measured on macOS, 400 small files, warm (per-op, averaged):

| Path | `stat()` | `open()`+`read()` |
|---|--:|--:|
| Plain `git` checkout | 1.2 µs | 10.7 µs |
| **CoW backend (default)** | **1.6 µs** | **11.4 µs** |
| NFS-loopback VFS (`SIMGIT_BACKEND=nfs`) | 792 µs | 1961 µs |

The CoW backend is within noise of a plain checkout and ~500×/170× faster per
op than the NFS-loopback VFS. The NFS cost is dominated by `actimeo=0` (no
client attribute caching → an RPC round trip per `stat`, and `LOOKUP`+`GETATTR`+
`READ` per read) — the price of synchronous write-time borrow-checking.

**When to use which:** the CoW default detects conflicts at commit (optimistic).
Use a write-intercepting backend (`SIMGIT_BACKEND=fuse|nfs`) only if you need a
write to a path another session holds to fail *immediately* rather than at
commit — and accept the per-op latency that requires.

## Reproduce

```bash
# Build first: cargo build --workspace
NFILES=400 FSIZE_KB=128 NS="1 2 4 8 16" bash tests/bench_scaling.sh
```

Tune `NFILES`/`FSIZE_KB` to model your repo's working-tree size and `NS` to the
session counts you care about.
