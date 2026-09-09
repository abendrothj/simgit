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
| `sg worktree` | 640–650 MiB | 9.8–10.8 MiB after the baseline | 6.4–7.0 s |

The `sg` figure is one materialized baseline (553 MiB) plus ~10 MiB of
per-worktree metadata, so total disk is `tree + N × 10 MiB` against
`N × 567 MiB` here. That per-worktree constant is repository-specific — see
[what a worktree actually costs](#what-a-worktree-actually-costs) — and
setup is half of plain `git worktree`, not a tradeoff.

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

### What a worktree actually costs

The marginal worktree is filesystem and Git-index metadata. Measured end to
end (`sg worktree add`, warm baseline, `df` deltas over four worktrees, twice
for the real repositories):

| Repository | Entries | Avg path | Per worktree | B/entry | Index B/entry | % of tree |
|---|---:|---:|---:|---:|---:|---:|
| `microsoft/vscode` | 23,122 | 68.5 | 9.8–10.8 MiB | 446 | 116 | 1.1% |
| `git/git` | 5,075 | 27.2 | ~2.2 MiB | 456 | 91 | 3.1% |
| simgit itself | 43 | 20.7 | 57 KiB | 1357 | 71 | 0.5% |
| 100k × 4 KiB synthetic | 101,001 | 10.8 | 37.5–38.3 MiB | 389 | 79 | 9.4% |
| 200 × 8 MiB synthetic | 205 | 6.5 | 107 KiB | 534 | 71 | 0.007% |

Three things drive that, and one plausible candidate does not.

**Content size does not.** At a fixed 200 files, growing each file from 4 KiB
to 64 MiB — a 1 MiB tree to a 12.8 GiB tree — leaves the per-clone cost flat,
because `clonefile` shares the extent tree by reference rather than copying
extent records:

| 200 files × | Tree | Per clone | Per file |
|---:|---:|---:|---:|
| 4 KiB | 1 MiB | 57.8 KiB | 296 B |
| 256 KiB | 50 MiB | 45.0 KiB | 230 B |
| 8 MiB | 1600 MiB | 57.8 KiB | 296 B |
| 64 MiB | 12800 MiB | 56.5 KiB | 289 B |

**Path length does.** Directory entries and Git index entries both store the
name, so both grow with it. At a fixed 5,000 files:

| Shape | Filesystem clone | Git index | Total per entry |
|---|---:|---:|---:|
| 4-character names | 305 B | 80 B | 385 B |
| 40-character names | 382 B | 120 B | 502 B |
| 120-character names | 572 B | 199 B | 771 B |
| 12-character names, 4 levels deep | 334 B | 115 B | 449 B |

**Entry count** sets the multiplier, and **a fixed ~60 KiB** per worktree
covers the linked-worktree admin directory — measured at 67 KiB for a
single-file repository, which is why simgit's own 43-entry tree shows an
outlying 1357 B/entry.

So: `per worktree ≈ 60 KiB + entries × (dirent + index bytes)`, where the
per-entry term ran 389–456 B across the real repositories here and rises with
path length. As a *fraction* of the tree it is set by average file size, which
is why the same mechanism costs 0.007% on a large-file repository and 9.4% on
100k tiny files. A repository of many small, shortly-named files is where the
technique pays least.

> An earlier revision of this section reported ~0.30 KiB per path from
> `clonefile`-only measurements, which omitted the per-worktree Git index copy
> — 20–26% of the real cost — and did not test path length. The figures above
> measure `sg worktree add` end to end.

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

### Reducing the per-worktree cost

For the 100k × 4 KiB worst case, the 38 MiB splits into ~30 MiB of APFS
directory and inode records and 7.9 MiB of copied Git index — so any measure
that only attacks the index is capped at ~20%.

Evaluated:

| Approach | Result |
|---|---|
| Sparse (cone) checkout | **3.27 MiB per worktree vs 37.5 MiB**, and a 0.79 MiB sparse index vs 7.91 MiB dense. Not implemented. |
| `fuse-overlayfs` mode (Linux) | `upperdir` starts empty, so no per-file metadata at any entry count. Trades FUSE read overhead. Unmeasured. |
| `core.splitIndex` | No effect. The shared base is written into the *worktree's own* git dir (`.git/worktrees/<name>/sharedindex.*`), not the common dir, so nothing is shared between worktrees. |
| Index version 4 (path compression) | 7.91 → 6.64 MiB on short paths: 16% of the index, 3% of the total. Not worth teaching the stat patcher prefix-compressed paths. |
| Hard links instead of clones | Rejected: a write through a hard link mutates every worktree, which is the failure simgit exists to prevent. |

The sparse figure was measured by hand-populating one cone directory from a
baseline clone; that probe left the index inconsistent, so it establishes the
disk cost, not a working implementation. A real version has to populate only
the cone directories and handle sparse index entries (directories, mode
040000) in `adopt_stat_data`, which currently skips them on the size check —
safe, but it leaves their stat data stale.

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
