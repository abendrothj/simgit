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
Eight untouched worktrees cost roughly one tree, at **2.1–2.2× the cold setup
time** in these sequential runs.

> **Report the marginal cost, not a multiple.** "8× less disk with 8
> worktrees" is the worktree count restated — the same run yields 12× at
> twelve and 50× at fifty, which says nothing new. The invariant is that the
> CoW path pays for one tree plus a small per-worktree constant, while plain
> `git worktree` pays for a tree every time.

### Real repository: microsoft/vscode

Synthetic trees understate per-file cost, so the same comparison was run on a
`--depth 1` clone of `microsoft/vscode` — **18,707 tracked files, 553 MiB of
tracked content** — with 8 worktrees on APFS, two runs each, September 8, 2026:

| Path | Physical allocation delta | Per worktree | Cold setup |
|---|---:|---:|---:|
| Git worktrees | 4534–4536 MiB | 567 MiB | 14.0–14.2 s |
| `sg worktree` | 640–650 MiB | ~11 MiB after the baseline | 6.4–7.0 s |

The `sg` figure is one materialized baseline (553 MiB) plus ~11 MiB of
per-worktree filesystem metadata, so total disk is `tree + N × 11 MiB` against
`N × 567 MiB`. Setup is half of plain `git worktree`, not a tradeoff.

### Whole-tree cloning

Cloning file by file made setup scale with file count rather than content
size. macOS `clonefile(2)` clones a directory hierarchy recursively in one
syscall, which removes the walk. Isolated on the same 23k-entry baseline:

| Materialization | Time |
|---|---:|
| `cp -c -R baseline/. target` (per file) | 2.50 s |
| `clonefile(baseline, target)` (whole tree) | 0.20 s |

### Adopting the baseline index

A clone carries the baseline's content, mode, size and mtime but its own inode
and ctime — exactly the fields Git compares before trusting an index entry. So
a copied baseline index is worse than useless: Git rehashes every file on
first use. Measured on vscode, each variant from a fresh clone:

| Worktree index | Setup | First `git status` |
|---|---:|---:|
| `read-tree HEAD` | 0.00 s | 3.02 s |
| copied baseline index, untouched | 0.00 s | 3.33 s |
| copied index + `update-index --refresh` under relaxed stat checks | 0.13 s | 3.05 s |
| copied index + strict `update-index --really-refresh` | 3.34 s | 0.15 s |
| copied index + stat adoption | 0.16 s | 0.13 s |

Nothing Git offers under relaxed stat settings persists strict-usable stat
data: it only records stat information for entries it decides to inspect.
`--really-refresh` does record it, but pays the full rehash to get there.

So simgit rewrites the stat fields itself. `checkout-index -u` records the
baseline's stat data when the baseline is materialized; copying that index and
re-recording ctime, mtime, dev, ino, uid and gid from the clone costs one
`lstat` per entry and no content reads. The result is **byte-for-byte
identical to the index `git update-index --really-refresh` writes** — pinned
by `adopted_stat_data_equals_gits_own_refresh` — and Git's strict staleness
checks are left untouched. Per worktree on vscode, warm baseline:

| Path | `sg worktree add` | First `git status` |
|---|---:|---:|
| per-file clone + `read-tree` | 5.87 s | 0.10 s |
| whole-tree clone + adopted index | 0.56 s | 0.11 s |

### What the disk overhead scales with

Each worktree costs filesystem metadata proportional to **entry count**, not
repository size, and the two clone paths cost the same. Four clones of each
synthetic tree, `df` deltas per worktree:

| Entries | Content | Whole-tree clone | Per-file clone |
|---:|---:|---:|---:|
| 1,011 | 3 MiB | 304 KiB | 325 KiB |
| 10,101 | 39 MiB | 3,095 KiB | 3,128 KiB |
| 101,001 | 390 MiB | 31,203 KiB | 31,222 KiB |
| 10,101 | 2,500 MiB | 3,087 KiB | 3,106 KiB |

That is ~0.30 KiB per tracked path, unchanged when content grows 64×. So N
worktrees of a tree with `bytes` of content and `entries` paths cost
`bytes + N × 0.30 KiB × entries`, and the marginal worktree costs
`0.30 KiB × entries` however large N gets. What varies between repositories
is that constant as a fraction of the tree — set by average file size, not
repository size: ~2% at vscode's 24 KiB average, ~8% for a tree of 4 KiB
files, negligible when files are large. A repository of many tiny files is
where the technique pays least.

Baselines published before stat adoption carry no index and fall back to the
per-file path, as does Linux, which has no directory-level reflink.

Thanks to @pasteley ([#20](https://github.com/abendrothj/simgit/issues/20))
for measuring this on a 256,886-path monorepo and identifying both halves.

> **Correction.** An earlier revision of this document claimed 1.2 s per
> worktree and a 13% disk penalty for whole-tree cloning. Both were wrong: the
> timings came from a repository where a benchmark had left
> `core.checkStat=minimal` and `core.trustctime=false` in `.git/config`, and
> the disk penalty was run-to-run `df` variance between two different clones.
> The tables above were re-measured on an untouched repository.

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
| Linux without reflinks + `fuse-overlayfs` | shared lowerdir + per-worktree upperdir | CoW disk sharing |
| Linux without reflinks or `fuse-overlayfs` | capability probes fail | normal Git checkout fallback |
| Windows | intentionally unsupported (ordinary NTFS lacks the required general reflink primitive) | use Git worktrees or WSL |

Use `sg worktree add --require-cow ...` in automation when falling back to N
full checkouts would violate a disk budget. Baselines unused for seven days are
removed by `sg worktree prune`; active overlay lowerdirs remain protected even
with `--all`.

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
