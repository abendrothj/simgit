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
        socket_path="/tmp/simgit-dev/control.sock",
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

Classic multi-agent pipelines usually choose one of two expensive options:

- **One full git worktree per agent** — high disk and I/O overhead that scales poorly
- **Shared checkout without write isolation** — race-prone, produces corrupted or lost changes

simgit targets a third design point:

| Property | How simgit achieves it |
|---|---|
| Shared reads | All agents read from one repository baseline |
| Isolated writes | Per-session copy-on-write overlays |
| Conflict safety | Borrow-checker style exclusivity at commit time |
| Observability | Structured conflict payloads, commit telemetry, Prometheus metrics |

## Features

- **Session lifecycle** — create, diff, commit, abort, status, garbage collection
- **Path-level conflict detection** — overlap-aware commit scheduling with structured conflict payloads
- **Commit telemetry** — per-stage breakdowns and scheduler queue-wait timing
- **Deadline and idempotency semantics** — safe retry behavior under transport ambiguity
- **Prometheus metrics** — full session, lock, and commit counter/histogram coverage
- **OTLP tracing** — optional distributed trace export for orchestrator-level visibility
- **Python bindings** — PyO3-backed, works with asyncio orchestrators
- **Rust SDK** — async JSON-RPC client with typed request/response models
- **`sg` CLI** — interactive session management and inspection
- **Chaos-validated** — SLO gate suite covering disjoint commits, hotspot contention, transport faults, and abandon storms

## Architecture

```text
Agent runner / CLI / SDK
          │
          ▼
  JSON-RPC over Unix socket
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

## Repository layout

```text
simgit/
├── simgitd/          daemon implementation
├── simgit-sdk/       Rust SDK (JSON-RPC client + shared types)
├── simgit-py/        Python bindings (PyO3)
├── sg/               command-line interface
├── tests/            Rust integration tests + Python stress tools
├── docs/             integration and operational documentation
├── deploy/           local observability/dev deployment profiles
└── spike/            focused technical spikes and prototypes
```

## Quick start

### 1. Build

```bash
cargo build --workspace
```

### 2. Start the daemon

```bash
export SIMGIT_REPO=$(pwd)
export SIMGIT_STATE_DIR=/tmp/simgit-dev
mkdir -p "$SIMGIT_STATE_DIR"

./target/debug/simgitd
```

The control socket is created at `/tmp/simgit-dev/control.sock`.

### 3. Create and use a session

`sg` is the simgit CLI. In a second shell:

```bash
export SIMGIT_SOCKET=/tmp/simgit-dev/control.sock

sg new --task "demo-task" --label "agent-1"
# write files under the printed mount path, then:

sg status
sg diff --session <session-uuid>
sg commit --session <session-uuid> --branch feat/demo --message "demo"
```

### 4. Run multiple agents in parallel

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
