# Scaling benchmark — native CoW `sg worktree` vs `git worktree`

`sg worktree` is now a thin wrapper around real Git linked worktrees. It uses
the same local filesystem and Git code paths as `git worktree`, but populates
each checkout from one immutable baseline using APFS clones or Linux reflinks.
There is no daemon, mount, synthetic `.git`, delta capture, or second commit.

Disk accounting and I/O latency must be measured separately. `du` reports
blocks reachable through each path and double-counts shared clone extents;
filesystem used-block deltas measure physical allocation but can be noisy when
other processes write to the same volume.

## Method

`tests/bench_scaling.sh` creates equivalent synthetic repositories and records:

- path-accounted disk with `du -sxk`; `-x` excludes other mounted filesystems
  such as historical NFS/FUSE backends;
- physical allocation from the `df -Pk` used-block delta after `sync` and a
  settling interval;
- cold sequential creation time for N worktrees.

The source repository exists before each measurement and is excluded from both
disk figures. The simgit figure includes its one cached baseline and all linked
worktrees. `tests/bench_worktree_io.py` separately warms both trees, alternates
measurement order, and compares ordinary file operations.

## Current native-worktree results

macOS/APFS, 400 files, **67.2 MiB per checkout**, July 2, 2026:

| Worktrees | Git `du` | `sg` `du` | Git setup | `sg` setup |
|---:|---:|---:|---:|---:|
| 1 | 67.2 MiB | 134.4 MiB | 0.25 s | 0.74 s |
| 2 | 134.4 MiB | 201.6 MiB | 0.45 s | 1.23 s |
| 4 | 268.8 MiB | 336.0 MiB | 0.86 s | 2.24 s |
| 8 | 537.5 MiB | 604.7 MiB | 1.66 s | 4.06 s |

The `sg` `du` total is approximately `(N + 1) × tree`: N worktree paths plus
one baseline path. That is expected clone accounting, not N+1 physical copies.
Cold creation is slower because the first add creates one native checkout for
the baseline and each add performs both Git registration and a recursive clone
operation. The trade is setup latency for steady-state disk reduction; agents
do not pay an I/O virtualization tax after creation.

The same run's `df` deltas ranged from 46–51 MiB for `sg` and 46–530 MiB for
Git. The non-monotonic `sg` values demonstrate the noise floor of volume-wide
accounting; use the larger dedicated run below for the disk ratio.

### Physical allocation

Two dedicated APFS runs, **300 MiB tree and 8 worktrees**:

| Path | `du`-accounted | Physical allocation delta | Setup |
|---|---:|---:|---:|
| Git worktrees | 2404.7 MiB | 2405.9–2583.4 MiB | 6.32–6.66 s |
| native CoW `sg worktree` | 2705.3 MiB | 301.2 MiB | 14.10–14.15 s |

`du` remains intentionally shown because it catches accidental extra trees,
but the physical allocation delta is the result that tests extent sharing.
The native CoW path used **at least 8.0× less physical disk** for eight
untouched worktrees, at **2.1–2.2× the cold setup time** in these sequential
runs.

### Native file-I/O latency

Hot-cache microbenchmark, 1,000 files × 16 KiB, six alternating rounds:

| Operation | Git worktree | native CoW `sg` | Interpretation |
|---|---:|---:|---|
| `stat()` | 1.45–2.03 µs/file | 1.66–2.05 µs/file | overlapping ranges |
| open + read 4 KiB | 10.81–12.25 µs/file | 10.91–12.56 µs/file | overlapping ranges |
| cached full reads | 1.30–1.50 GiB/s | 1.25–1.48 GiB/s | overlapping ranges |
| first overwrite 4 KiB + `fsync` | 0.038 ms/file | 0.077 ms/file | 2.0× for extent split |

Read and metadata behavior is effectively ordinary worktree I/O because these
are ordinary local files. The durable first write still pays the fundamental
CoW extent-split cost. Subsequent writes to already-private blocks should
converge toward the Git worktree result.

## Disk model after edits

```text
Git physical disk ≈ N × full tree
sg physical disk  ≈ one cached baseline
                    + private blocks changed in each worktree
                    + compressed Git objects created by commits
```

There is no daemon delta store and no full-file commit capture in the native
worktree path. Dense rewrites still erode the disk advantage because each
worktree eventually owns the blocks it changes; sparse agent edits retain most
of the sharing.

## Architecture tradeoffs

| Approach | Agent I/O | Physical disk | Git/tool integration | Operational complexity | Best fit |
|---|---|---|---|---|---|
| Plain Git worktrees | Native; no first-write split | `N × tree` | Exact | Lowest | Few worktrees or small repos |
| **Native CoW linked worktrees (`sg worktree`)** | Native reads; first write splits extents | `1 × baseline + changed extents` | Exact; real `.git/worktrees` entries | Low | Default for many local agents |
| Daemon native-CoW sessions | Native reads; capture/commit overhead | Baseline + changed extents + captured deltas | Synthetic Git proxy | Medium | Path leases, RPC lifecycle, telemetry |
| Linux overlayfs | Lookup/overlay tax; whole-file copy-up | One lower + changed upper files | Good but mount-sensitive | Medium/high; privileges and whiteouts | Controlled Linux hosts |
| FUSE/NFS/WinFSP VFS | Every operation crosses userspace/RPC | Git objects + deltas | Requires proxy behavior | Highest | Synchronous write-time rejection |
| Hardlink farm | Native until a writer mutates shared inode | Near one tree | Unsafe without interception | Deceptively low | Never for writable agent trees |
| Sparse checkout | Native for present files | Selected paths only | Native but incomplete tree | Low/medium | Known working sets, not general agents |

The implemented native CoW linked-worktree design is the lean default: it
keeps the disk property that matters while deleting the custom filesystem and
commit machinery from the agent hot path. VFS is not the general winner; it is
only justified when rejecting conflicting writes synchronously is worth its
platform and latency costs.

## Platform behavior

| OS/filesystem | `sg worktree` population | Result |
|---|---|---|
| macOS on APFS | `cp -c` clonefile | CoW disk sharing |
| Linux on reflink-capable Btrfs/XFS | `cp --reflink=always` | CoW disk sharing |
| Linux without reflinks | capability probe fails | normal Git checkout fallback |
| Windows | no clone implementation yet | normal Git checkout fallback |

Use `sg worktree add --require-cow ...` in automation when falling back to N
full checkouts would violate a disk budget. Baselines unused for seven days are
pruned opportunistically; `sg worktree prune --all` removes them immediately
without invalidating existing worktrees.

## Historical benchmark note

Measurements recorded before July 2, 2026 exercised the daemon session/VFS or
daemon-managed CoW architecture. Those results remain useful for comparing
filesystem approaches, but they are not evidence for the current native
linked-worktree control plane. In particular, old 15–46% read/metadata
overheads and duplicate commit-capture costs do not apply to `sg worktree` now.

## Reproduce

```bash
cargo build -p simgit-cli
bash tests/bench_scaling.sh

# For the I/O comparison, create equivalent untouched Git and sg worktrees:
python3 tests/bench_worktree_io.py /path/to/git-wt /path/to/sg-wt
```

Use a tree of several hundred MiB for meaningful physical-allocation deltas and
minimize unrelated writes on the measured volume.
