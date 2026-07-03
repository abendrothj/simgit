# Scaling benchmark — `sg worktree` vs `git worktree`

simgit's default CoW backend avoids **physical** N-way duplication of the
working tree. On macOS it keeps one materialized baseline and creates APFS
`clonefile` sessions whose file extents remain shared until written. Git
worktrees independently materialize every checkout.

Physical allocation, path-level disk accounting, setup latency, and file-I/O
latency are different measurements. This page reports them separately.

## Method

`tests/bench_scaling.sh` creates a synthetic repository, then creates N Git
worktrees and N simgit CoW sessions in separate runs. It records:

- **`du`-accounted disk** using `du -sxk`. `-x` excludes NFS/FUSE mounts, but
  APFS and reflink clones are ordinary directories on the same filesystem.
  `du` charges their shared extents to every path, so this is not unique
  physical space for CoW sessions.
- **Physical allocation delta** using the filesystem's used-block count before
  and after setup (`df -Pk`), with `sync` and a short settling interval. This
  observes clone sharing, but can be noisy if unrelated processes write to the
  same volume during a run.
- **Cold batch setup time**, including simgit's first daemon startup and shared
  baseline materialization.

Both disk figures exclude the source repository, which exists before each
measurement.

## Setup and `du` accounting

macOS/APFS, 200 files × 256 KiB after generation = **67.2 MiB per tree**:

| Sessions | Git `du` | simgit `du` | Git setup | simgit setup |
|---:|---:|---:|---:|---:|
| 1  | 67.2 MiB | 134.5 MiB | 0.20 s | 0.91 s |
| 4  | 268.8 MiB | 336.3 MiB | 0.87 s | 1.01 s |
| 8  | 537.5 MiB | 605.4 MiB | 1.55 s | 1.60 s |
| 16 | 1075.1 MiB | 1143.4 MiB | 3.13 s | 2.70 s |

The simgit `du` result is almost exactly `(N + 1) × tree size`: N session
directories plus one shared baseline. That does **not** mean APFS allocated N+1
physical copies; it demonstrates why `du` alone cannot measure clone savings.

Setup has a fixed simgit cost for daemon startup and baseline extraction. Git is
faster at low fan-out. Once that fixed cost is amortized, clone creation is
cheaper than repeated checkout and simgit crosses over around 8–16 sessions for
this tree shape.

## Physical allocation

A dedicated larger run reduces `df` noise: **300 MiB tree, 8 sessions**, APFS.

| Path | `du`-accounted | Physical allocation delta | Setup |
|---|---:|---:|---:|
| Git worktrees | 2400.0 MiB | 2401.5 MiB | 1.38 s |
| simgit CoW | 2700.7 MiB | 327.3 MiB | 2.34 s |

For eight untouched sessions, simgit used **7.3× less physical disk** while
`du` misleadingly reported more. The physical result is approximately one
300 MiB baseline plus metadata; the eight session trees share its extents.

## File-I/O latency

Hot-cache microbenchmark, 1,000 files × 16 KiB, repeated in both execution
orders to control for cache warming:

| Operation | Git worktree | simgit CoW | simgit overhead |
|---|---:|---:|---:|
| `stat()` | 0.83–0.96 µs/file | 1.21–1.36 µs/file | 42–46% |
| open + read 4 KiB | 7.84–8.10 µs/file | 9.32–9.51 µs/file | 15–21% |
| cached full reads | 1.63–1.73 GiB/s | 1.41–1.45 GiB/s | 11–18% lower |
| overwrite 4 KiB + `fsync` | 0.050–0.051 ms/file | 0.083–0.117 ms/file | 1.6–2.3× |

CoW stays in the native filesystem hot path—these costs are microseconds, not
the millisecond-scale overhead of NFS-loopback. It is nevertheless not free:
clone metadata is slightly slower to traverse, and the first durable write to a
shared extent must split that extent.

## Effect of edits

The untouched-session result is the best case. A useful steady-state model is:

```text
Git physical disk    ≈ N × full tree
simgit physical disk ≈ one baseline
                       + private blocks changed in each session
                       + captured full-file delta blobs
                       + metadata
```

CoW allocates only blocks dirtied in each session, but commit capture currently
stores the full contents of every changed file. Committed sessions remain
mounted for inspection and retain their captured delta until abort/cleanup.
Workloads that rewrite most of every tree therefore erode the disk advantage;
the strongest win is many mostly-read sessions with sparse edits.

## Practical conclusion

- At low fan-out, use Git worktrees unless isolation disk or automation
  semantics justify the daemon.
- At high fan-out with sparse edits, simgit trades modest native-I/O overhead
  and a cold baseline cost for close to N-fold physical disk savings.
- `du -x` remains correct for excluding NFS/FUSE mounts, but cannot measure
  shared extents for the default CoW backend. Use the physical allocation delta
  as the primary disk result.
- Use `SIMGIT_BACKEND=nfs` or Linux FUSE only when synchronous write-time
  conflict rejection is worth much higher per-operation latency.

## Architecture tradeoffs

| Approach | Read/metadata I/O | First-write I/O | Physical disk | Conflict timing | Main cost |
|---|---|---|---|---|---|
| Plain Git worktrees | Baseline | Baseline | `N × tree` | Git/ref conflicts | Full checkout per session |
| Current native CoW | Near baseline | CoW extent split | `1 × baseline + changed extents + captured files` | Commit time | Baseline creation and duplicate capture |
| Linux overlayfs | Small lookup/overlay tax | Whole-file copy-up can dominate | One lower + upper changes | Commit time | [Overlay semantics and copy-up](https://docs.kernel.org/filesystems/overlayfs.html) |
| NFS/FUSE/WinFSP VFS | Highest per-op tax | Intercepted RPC/upcall | Git objects + deltas | Write time | Every filesystem operation crosses userspace |
| Hardlink farm | Baseline | Unsafe without interception | Near one tree | None by itself | One writer can mutate every session |
| Sparse/partial checkout | Baseline for present files | Baseline | Selected paths only | Git semantics | Requires knowing the working set; absent paths break general agents |

The current CoW data path is already close to the useful lower bound for a
general, fully materialized tree: files are ordinary local filesystem objects.
The remaining first-write penalty is fundamental to block sharing—Apple
[documents that clone modifications are written elsewhere while unchanged
blocks remain shared](https://developer.apple.com/documentation/foundation/about-apple-file-system).
Removing that penalty for every file requires allocating every file privately,
which is the disk behavior simgit is intended to avoid.

### Strongest next design: CoW-backed native Git worktrees

A better commit/control plane can retain the same disk model while removing
most simgit-specific commit I/O:

1. Create a real Git linked worktree with `git worktree add --no-checkout`.
2. Initialize its private index with `git read-tree HEAD`.
3. Populate it from an immutable cached baseline with APFS clones/reflinks,
   rather than asking Git to inflate every blob again.
4. Keep Git's normal per-worktree `HEAD` and index, and share the object database
   using Git's native common-dir mechanism (or alternates for standalone trees).
5. In a pre-commit reservation, compute changed paths from the index/worktree,
   fingerprint peers, and acquire simgit's path leases.
6. Let native Git write blobs, trees, commits, and the unique branch ref. Avoid
   copying changed files into the delta store and avoid the second flattening
   worktree.
7. Verify the pre/post fingerprint and release the lease; retain the existing
   request-id recovery record for ambiguous outcomes.

[Git documents linked worktrees](https://git-scm.com/docs/git-worktree.html) as
sharing repository data while keeping per-worktree `HEAD` and index state, and
[documents object alternates](https://git-scm.com/docs/gitrepository-layout) for
borrowing immutable objects. This makes native Git the commit engine while
simgit remains the isolation, conflict-reservation, lifecycle, and observability
layer.

Expected effects:

- **Reads/stats:** should approach ordinary Git-worktree performance because the
  paths are still normal local files. Matching Git is a realistic goal; beating
  it consistently is not.
- **Writes:** unmodified cloned files still pay the unavoidable first-write CoW
  split. An optional `sg prepare-write <paths...>` can proactively make likely
  write targets private in the background. That moves latency off the agent's
  hot path while spending disk only on the predicted write set.
- **Commits:** should improve materially by eliminating full-file delta copies,
  duplicate local/daemon commits, and flatten worktrees.
- **Disk:** remains approximately one baseline plus changed private extents and
  Git's compressed objects, rather than N complete trees.

The critical prototype gates are: native Git-command parity, safe lease cleanup
when a hook fails, protection against writes racing the pre/post fingerprint,
atomic unique-branch publication, and physical-disk measurements after sparse
and dense edits.

The basic filesystem/Git sequence has been smoke-tested locally: a no-checkout
linked worktree plus `read-tree`, cloned baseline population, ordinary `git
status`, native commit, and shared branch visibility all complete with a clean
index before and after the commit.

## Reproduce

```bash
cargo build -p simgitd -p simgit-cli
NFILES=400 FSIZE_KB=128 NS="1 2 4 8 16" bash tests/bench_scaling.sh
```

Use a tree of several hundred MiB for meaningful physical allocation deltas,
and minimize unrelated writes to the measured filesystem during the run.
