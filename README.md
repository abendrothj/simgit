# simgit

**Concurrent filesystem sessions for multi-agent coding pipelines.**

simgit provides two concurrency layers: a lean `sg worktree` path built from
native Git linked worktrees and filesystem CoW clones, plus daemon-managed
sessions for workflows that need path leases, RPC lifecycle control, and
commit scheduling.

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

**`sg worktree` avoids duplicating the working tree without replacing Git.**
Every agent gets a real linked worktree populated from a shared immutable
baseline using APFS clones or Linux reflinks. Git still owns HEAD, the index,
refs, commits, hooks, and worktree registration. On unsupported filesystems the
command falls back to an ordinary Git checkout.

| Property | How simgit achieves it |
|---|---|
| Shared reads | All agents read from one repository baseline |
| Isolated writes | Per-session copy-on-write overlays |
| Git integration | Real `.git/worktrees` registration and native Git behavior |
| Optional coordination | Daemon sessions provide path leases and commit scheduling |

### Measured

Standing up eight isolated views of a 300 MiB tree on APFS:

| Path | Physical disk added | Cold setup |
|---|---:|---:|
| 8 × `git worktree` | 2405.9–2583.4 MiB | 6.32–6.66 s |
| 8 × `sg worktree` | 301.2 MiB | 14.10–14.15 s |

The native CoW path used **at least 8.0× less physical disk** at **2.1–2.2× the sequential
cold setup time**. Hot read and metadata ranges overlap ordinary worktree I/O;
the first durable write is slower while the filesystem splits shared extents.
Full method: [docs/scaling_benchmark.md](docs/scaling_benchmark.md).

## Features

- **Session lifecycle** — create, diff, commit, abort, status, garbage collection
- **Path-level conflict detection** — overlap-aware commit scheduling with structured conflict payloads
- **Commit telemetry** — per-stage breakdowns and scheduler queue-wait timing
- **Deadline and idempotency semantics** — safe retry behavior under transport ambiguity
- **Prometheus metrics** — full session, lock, and commit counter/histogram coverage
- **OTLP tracing** — optional distributed trace export for orchestrator-level visibility
- **Python bindings** — PyO3-backed, works with asyncio orchestrators
- **Rust SDK** — async JSON-RPC client with typed request/response models
- **`sg worktree`** — creates real linked worktrees backed by one physical CoW
  baseline; no daemon, mount, synthetic repository, or agent integration layer.
- **Safe fallback** — uses an ordinary Git checkout when clone/reflink support
  is absent; `--require-cow` makes disk-saving support mandatory.
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

### Daemon-session backend: native copy-on-write

For `sg new`/SDK sessions, the daemon gives each session a native copy-on-write
working tree — no user-space filesystem in the hot path. Each session is a real
working tree over a shared baseline, materialized per platform:

- **Linux**: `overlayfs` — `lowerdir` = shared baseline, per-session `upperdir`
  for writes. Only changed files land in the upper, so commit-time capture scans
  the upper (`O(changes)`); deletions are overlay whiteouts. Falls back to a
  reflink clone if the overlay mount isn't permitted.
- **macOS**: APFS `clonefile` (`cp -c`) — blocks shared until written.

Reads and writes are ordinary filesystem ops at **native latency**, while the
disk-scaling benefit (one shared baseline per `base_commit` + per-session
deltas) is preserved; idle baselines are garbage-collected. Conflict detection
happens **at commit time**: the working tree is diffed into the delta store
(`capture_mount_delta`) and the commit scheduler performs a path-level overlap
check. Same no-corruption guarantee, detected at commit rather than rejected at
each write.

Measured hot-cache cost on macOS: CoW `stat` is ~42–46% slower than a Git
worktree (about 1.3 vs 0.9 µs/file), 4 KiB reads are ~15–21% slower (about
9.4 vs 8.0 µs/file), and durable first writes to shared extents cost ~1.6–2.3×
more — all still native filesystem latency.
See [docs/scaling_benchmark.md](docs/scaling_benchmark.md).

macOS is the primary supported platform (nightly SLO/chaos suite exercises the
CoW path); Linux overlayfs shares the same delta store and commit scheduler.
Earlier prototypes explored write-intercepting VFS backends (FUSE, embedded
NFSv3, WinFSP) that rejected conflicting writes synchronously; those were
removed in favor of standing on the faster CoW path, and remain in git history
for reference.

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

### 2. Create a native CoW worktree

```bash
cd $(sg worktree add feat/my-feature)
```

The worktree is registered in Git's normal `.git/worktrees/` registry. A cached
baseline is stored under `.git/simgit/baselines/`; unchanged extents remain
shared on APFS and reflink-capable Linux filesystems. No daemon starts.

### 3. Use git commands directly inside the session

The worktree is a standard linked checkout, so all Git commands and existing
hooks work without a proxy:

```bash
cd $(sg worktree add feat/demo)

# Edit files with any tool...
echo "hello" > README.md

# Standard git commands commit directly to the linked branch
git add README.md
git commit -m "demo"

# Inspect native worktree state
git status
git diff

# Commit uncommitted changes and remove the session
sg worktree remove --commit --message "clean up"
```

Use `sg worktree remove --force` only when changes should be discarded. Plain
removal delegates to Git and refuses a dirty worktree.

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
