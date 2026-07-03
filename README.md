# simgit

**Concurrent filesystem sessions for multi-agent coding pipelines.**

simgit is a daemon that lets many agents work against the same repository at the same time. Each agent gets an isolated copy-on-write overlay backed by a shared baseline. Path conflicts are detected and reported at commit time. Non-conflicting branches commit in parallel.

## Run many agents on many branches — simultaneously

The primary use case: three agents, three feature branches, one repository, zero coordination overhead.

```python
import simgit
import asyncio

async def run_agent(task_id, branch, label):
    session = simgit.Session.new(
        task_id=task_id,
        agent_label=label,
    )
    mount = session.info()["mount_path"]

    # Agent writes files under its private mount — fully isolated
    # from every other agent's changes

    return session.commit(branch_name=branch, message=f"{label} complete")

async def main():
    results = await asyncio.gather(
        run_agent("task-auth",  "feat/auth-refactor",  "agent-1"),
        run_agent("task-api",   "feat/api-v2",         "agent-2"),
        run_agent("task-ui",    "feat/ui-components",  "agent-3"),
    )
```

Each agent reads from the shared repository baseline and writes to its own overlay. Non-overlapping branches commit in parallel. Overlapping paths are caught at commit time with full context — no corruption, no silent data loss.

Works with Claude Code agents, custom orchestrators, or any tool that can write files to a path.

## Why simgit exists

If you run multiple agents against the same repository, `git worktree` gives
each one a full copy of the working tree.  That gets expensive fast —
N agents × repo size sitting on disk.

**simgit avoids duplicating the working tree.**  Every agent reads from
a shared baseline and writes to a private copy-on-write overlay.  The
working tree never touches disk.

| Property | How simgit achieves it |
|---|---|
| Shared reads | All agents read from one repository baseline |
| Isolated writes | Per-session copy-on-write overlays |
| Conflict safety | Borrow-checker style exclusivity at commit time |
| Observability | Structured conflict payloads, commit telemetry, Prometheus metrics |

### Measured

Standing up N isolated working trees over a 67 MB repo, real host disk consumed
(`git worktree` full checkouts vs `sg worktree` sessions):

| N sessions | `git worktree` | `sg worktree` |
|---:|---:|---:|
| 1  | 67.2 MB   | 0.11 MB |
| 8  | 537.5 MB  | 0.25 MB |
| 16 | 1075.1 MB | 0.41 MB |

Disk grows linearly with `git worktree` and stays flat with simgit — ~2,600×
less at N=16, and the gap widens with tree size. **At N=1–2, plain `git worktree`
is simpler and the right call; the win only shows up at high fan-out.** simgit
also trades local page-cache reads for a loopback FS layer, so it buys disk and
setup-time scaling, not raw single-file read latency. Full method, caveats, and a
reproduction script: [docs/scaling_benchmark.md](docs/scaling_benchmark.md).

## Features

- **Session lifecycle** — create, diff, commit, abort, status, garbage collection
- **Path-level conflict detection** — overlap-aware commit scheduling with structured conflict payloads
- **Commit telemetry** — per-stage breakdowns and scheduler queue-wait timing
- **Deadline and idempotency semantics** — safe retry behavior under transport ambiguity
- **Prometheus metrics** — full session, lock, and commit counter/histogram coverage
- **OTLP tracing** — optional distributed trace export for orchestrator-level visibility
- **Python bindings** — PyO3-backed, works with asyncio orchestrators
- **Rust SDK** — async JSON-RPC client with typed request/response models
- **`sg` CLI** — `sg worktree` creates per-agent sessions with zero-disk
  copy-on-write working trees.  Use it any time you'd otherwise duplicate
  the repo N times with `git worktree`.
- **`sg worktree`** — boots a minimal `.git/` so every git command works
  inside the session mount.  Auto-starts the daemon; no manual env var setup.
- **Chaos-validated** — SLO gate suite covering disjoint commits, hotspot contention, transport faults, and abandon storms

## Architecture

```text
Agent runner / CLI / SDK
          │
          ▼
  JSON-RPC over TCP loopback (port via control.port)
          │
          ▼
       simgitd
   ├── session manager
   ├── borrow registry
   ├── delta store
   ├── commit scheduler
   ├── git flatten path (gix)
   ├── metrics + tracing
   └── VFS backend abstraction
```

**Core guarantees:**

- Single-writer safety per conflicting path region at commit time
- Session isolation until commit finalization
- Idempotent commit resolution under ambiguous transport outcomes
- Explicit terminal states for all commit requests (pending → success | failed)

### Backends: native copy-on-write (default) vs. write-intercepting VFS

The **default** backend is a native copy-on-write working tree — no user-space
filesystem in the hot path. Each session is a real working tree over a shared
baseline, materialized per platform:

- **Linux**: `overlayfs` — `lowerdir` = shared baseline, per-session `upperdir`
  for writes. Only changed files land in the upper, so commit-time capture scans
  the upper (`O(changes)`); deletions are overlay whiteouts. Falls back to a
  reflink clone if the overlay mount isn't permitted.
- **macOS**: APFS `clonefile` (`cp -c`) — blocks shared until written.
- **Windows** (opt-in; default stays WinFSP): recursive copy for now.

Reads and writes are ordinary filesystem ops at **native latency**, while the
disk-scaling benefit (one shared baseline per `base_commit` + per-session
deltas) is preserved; idle baselines are garbage-collected. Conflict detection
happens **at commit time**: the working tree is diffed into the delta store and
the commit scheduler performs the same path-level overlap check. Same
no-corruption guarantee, detected at commit rather than rejected at write.

For workflows that need **synchronous write-time** borrow-checking (a write to a
path another session holds fails immediately with `EBUSY`), the write-
intercepting VFS backends remain available via `SIMGIT_BACKEND`:

| `SIMGIT_BACKEND` | Backend | Enforcement | Latency | Friction |
|---|---|---|---|---|
| _(default)_ | Native CoW (`clonefile`/reflink) | Commit-time overlap check | **Native** (page cache) | None |
| `fuse` | FUSE (`fuser`) | Write-time (`SessionFs` → `BorrowRegistry`) | User-space upcall per op | Linux built-in; macOS requires `--features macos-fuse` plus macFUSE/fuse-t |
| `nfs` | Embedded NFSv3 (`nfsserve`) | Write-time (`WRITE` RPC → `BorrowRegistry`) | Loopback RPC per op (slowest) | None (macOS built-in client) |
| `winfsp` | WinFSP (`winfsp_wrs`) | Write-time (write callback → `BorrowRegistry`) | User-space callback per op | WinFSP runtime install |

Measured hot-cache cost on macOS: CoW `stat` is ~42–46% slower than a Git
worktree (about 1.3 vs 0.9 µs/file), 4 KiB reads are ~15–21% slower (about
9.4 vs 8.0 µs/file), and durable first writes to shared extents cost ~1.6–2.3×
more. CoW remains orders of magnitude faster than the NFS-loopback path.
See [docs/scaling_benchmark.md](docs/scaling_benchmark.md).

Test coverage by platform:

| Platform | Backend | CI coverage |
|---|---|---|
| macOS | Native CoW | Nightly SLO/chaos suite (default exercised path) |
| Linux | FUSE | Mount integration tests — real create/rename/unlink through the FUSE mount (`.github/workflows/fuse-linux-integration.yml`) |
| Windows | WinFSP | Compile + link against the WinFSP runtime and cross-platform unit tests (`.github/workflows/windows-winfsp.yml`). Mount-level integration on Windows is **not yet covered** — the backend is build-verified, not mount-verified. |

The write-intercepting backends share the same borrow-checking and delta logic
through `SessionVfsOps` (FUSE and WinFSP additionally share `SessionFs`). The
default CoW backend reuses the same delta store and commit scheduler, but
populates deltas by diffing the working tree at commit (`capture_mount_delta`)
instead of intercepting each write.

## Repository layout

```text
simgit/
├── simgitd/          daemon implementation
├── simgit-sdk/       Rust SDK (JSON-RPC client + shared types)
├── simgit-py/        Python bindings (PyO3)
├── sg/               command-line interface
├── tests/            Rust integration tests + Python stress tools
├── docs/             integration and operational documentation
└── deploy/           local observability/dev deployment profiles
```

## Quick start

### 1. Build

```bash
cargo build --workspace
```

### 2. Create a worktree session

```bash
cd $(sg worktree add feat/my-feature)
```

The daemon auto-starts on first use. State is stored per-repo in
`.git/simgit/`. The session mount is fully isolated — every git command
works inside it as if it were a normal checkout.

### 3. Use git commands directly inside the session

`sg worktree` bootstraps a full `.git` directory inside the mount.
All standard git commands work without extra setup:

```bash
cd $(sg worktree add feat/demo)

# Edit files with any tool...
echo "hello" > README.md

# Standard git commands — commit is forwarded via pre-commit hook
git add README.md
git commit -m "demo"

# Inspect session state
sg status
sg diff

# Commit uncommitted changes and remove the session
sg worktree remove --commit --message "clean up"
```

For programmatic commit control:
```bash
sg commit --branch feat/demo --message "demo"
```

### 4. Run multiple agents in parallel

```bash
# Shell 1
cd $(sg worktree add feat/auth)
# ... agent writes files, runs git commit ...

# Shell 2
cd $(sg worktree add feat/api)
# ... agent writes files, runs git commit ...
```

For stress testing at scale:

```bash
source .venv/bin/activate
python3 tests/real_agent_harness.py --agents 5 --task-profile disjoint-files
```

Each agent gets its own session and commits to its own branch. Use `--task-profile hotspot-file` to observe conflict detection under contention.

## Observability

### Prometheus

```bash
export SIMGIT_METRICS_ENABLED=1
export SIMGIT_METRICS_ADDR=127.0.0.1:9100
./target/debug/simgitd

curl -s http://127.0.0.1:9100/metrics | head
```

Key series:

| Metric | Description |
|---|---|
| `simgit_rpc_requests_total` | RPC call volume by method |
| `simgit_rpc_duration_seconds` | RPC latency histogram |
| `simgit_active_sessions` | Live session count |
| `simgit_active_locks` | Active borrow locks |
| `simgit_session_commit_stage_duration_seconds` | Per-stage commit latency |
| `simgit_session_commit_conflicts_total` | Conflict event count |
| `simgit_session_commit_conflict_paths` | Conflicting paths per event |
| `simgit_session_commit_conflict_peers` | Peer sessions involved in conflicts |

### Tracing (OTLP)

```bash
export SIMGIT_OTLP_ENDPOINT=http://127.0.0.1:4317
./target/debug/simgitd
```

## Testing and validation

Three layers of test coverage are available:

| Script | Purpose |
|---|---|
| `tests/stress/agent_harness.py` | Control-plane stress — disjoint and hotspot modes; **used by CI** |
| `tests/real_agent_harness.py` | Real file writes + optional LLM backend; latency calibration |
| `tests/stress/drunk_agent.py` | Profile-based non-deterministic timing (TTFT, stalls, payload variance) |
| `tests/stress/swarm_runner.py` | Fault injection + SLO gate runner |
| `tests/stress/soak_runner.py` | Sustained-load soak with daemon RSS memory sampling |

The full chaos suite (Track 2) covers disjoint commits, hotspot contention, transport interruption, abandon storms, and duplicate submit behavior. All SLO gates pass at 20+ concurrent agents. See `docs/track2_chaos_validation.md` for methodology and results.

## Documentation

| Document | Contents |
|---|---|
| [`docs/agent_integration.md`](docs/agent_integration.md) | Concurrent workflow patterns, SDK usage, retries, deadlines, idempotency |
| [`TESTING.md`](TESTING.md) | Unit, integration, and stress test execution guide |
| [`docs/track2_chaos_validation.md`](docs/track2_chaos_validation.md) | Chaos run methodology, SLO definitions, and outcomes |

## Security notes

- Run stress tests against disposable repositories only — commits are real and irreversible.
- Set explicit timeout/deadline policies for commit-heavy orchestrators.
- Treat commit retries as semantic operations, not transparent transport retries.
- Apply queue-aware backoff when `scheduler_queue_wait_ms` rises under contention.

## License

MIT
