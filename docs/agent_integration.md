# Agent Integration Guide

This guide is for building production-grade agent runners on top of simgit using either Python bindings or the Rust SDK.

## Drop-in worktree replacement — using simgit sessions like `git worktree add`

simgit sessions ship a synthetic `.git` directory at the mount root so that
existing LLM coding agents that shell out to `git` work **without
modification** inside a session mount.  This means you can replace:

```bash
git worktree add ../agent-1-session main
cd ../agent-1-session
```

with:

```bash
sg new --task "agent-1" --label "agent-1"
cd /vdev/<session-uuid>
```

and every subsequent `git` command the agent runs behaves correctly.

### Git commands that work transparently

| Command | What the agent sees |
|---|---|
| `git status` | Modified files = session delta vs baseline. Already-staged writes and deletes appear. |
| `git diff` | Working tree vs index. Shows the session's uncommitted delta. |
| `git diff --cached` | Index vs HEAD. Empty unless the agent explicitly runs `git add` on a file already in the index. |
| `git log` | Full history up to `base_commit` (objects reachable via `objects/info/alternates`). |
| `git blame <file>` | Line-level history from the real repository. |
| `git show <commit>` | Any commit reachable from `base_commit`. |
| `git add <file>` | **Idempotent.**  The file is already in the index after a write.  `git add` is a no-op. |

### Git commands that are intercepted and rerouted

| Command | What happens |
|---|---|
| `git commit -m "..."` | The `.git/hooks/pre-commit` hook fires, forwards the commit to the daemon via `sg commit --session <uuid> --branch <branch> --message "..."`, and prevents git from creating a local commit.  The agent's changes are committed through the simgit commit scheduler (conflict-aware, idempotent). |

### Git commands that are NOT supported (by design)

| Command | Why | What to do instead |
|---|---|---|
| `git checkout <branch>` | Sessions are pinned to `base_commit`. Switching branches would invalidate the delta overlay. | Create a new session (`sg new`) with the desired branch. |
| `git push` / `git fetch` | The synthetic `.git` has no remotes configured. | The orchestrator handles push/fetch outside the session. |
| `git merge` / `git rebase` | Conflict resolution is the orchestrator's concern, not the agent's. | Use simgit's structured merge at commit time; if auto-merge fails, the orchestrator receives a conflict payload and can plan a resolution. |
| `git stash` | No git stash state. The session's delta IS the working state. | N/A — there's nothing to stash; all changes are tracked in the delta store. |

### How it works

At session creation (`sg new` or `session.create` RPC), the daemon bootstraps a
synthetic `.git` directory at the mount root:

- `HEAD` → points to `base_commit` (detached or branch-ref).
- `objects/info/alternates` → path to the real repo's object store, so all
  blobs, trees, and commits are reachable without copying data.
- `index` → a sparse index listing every file in `base_commit`'s tree as
  tracked (stage 0).  Updated in real time on every write, delete, or rename
  via the FUSE/NFS handler.
- `hooks/pre-commit` → intercepts `git commit` and forwards to `sg commit`.

### Disabling the git proxy

If agents don't need `git` subprocess support, set:

```python
session = simgit.Session.new(
    task_id="agent-task-123",
    socket_path=SOCKET,
    agent_label="planner-1",
    git_proxy_enabled=False,   # skips .git/ bootstrap
)
```

Or via the Rust SDK:

```rust
client.session_create_with_opts("task", Some("label".into()), None, false, false)
                                                                           // ^ git_proxy_enabled
```

This reduces mount-time overhead when the agent only reads/writes files and
commits via the simgit SDK directly.

## Concurrent multi-agent workflows

simgit's primary design goal is letting multiple agents work against the same repository at the same time — each on its own branch, without coordination or locking between them.

### How it works

1. Each agent creates its own **session**. The session returns a private mount path.
2. The agent reads and writes files exclusively under that mount path.
3. All reads reflect the shared repository baseline. All writes go to an isolated copy-on-write overlay.
4. At commit time, the daemon checks whether any two sessions' changed paths overlap:
   - **No overlap** → both commits proceed in parallel.
   - **Overlap** → the conflicting session receives a structured conflict payload describing which paths collide and which peer session holds them. No data is corrupted.

### Python: three agents, three branches

```python
import simgit
import asyncio

SOCKET = "/tmp/simgit-dev/control.sock"

async def agent_task(task_id: str, branch: str, label: str, write_fn):
    session = simgit.Session.new(
        task_id=task_id,
        socket_path=SOCKET,
        agent_label=label,
    )
    mount = session.info()["mount_path"]

    # Each agent writes only under its own mount_path.
    write_fn(mount)

    result = session.commit(
        branch_name=branch,
        message=f"[{label}] automated change",
        timeout_secs=10.0,
    )
    return result

async def main():
    results = await asyncio.gather(
        agent_task("task-1", "feat/auth-refactor",  "agent-auth",  write_auth_changes),
        agent_task("task-2", "feat/api-v2",          "agent-api",   write_api_changes),
        agent_task("task-3", "feat/ui-components",   "agent-ui",    write_ui_changes),
    )
    for r in results:
        print(r["telemetry"]["total_duration_ms"], "ms")
```

All three agents run concurrently. Non-overlapping branches commit in parallel; if two agents happen to touch the same file, only the conflicting session is blocked — not the others.

### Rust: concurrent sessions with Tokio

```rust
use std::time::Duration;
use simgit_sdk::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::new("/tmp/simgit-dev/control.sock");

    // Spawn sessions concurrently.
    let (s1, s2, s3) = tokio::try_join!(
        client.session_create("task-auth", Some("agent-1".into()), None, false),
        client.session_create("task-api",  Some("agent-2".into()), None, false),
        client.session_create("task-ui",   Some("agent-3".into()), None, false),
    )?;

    // Each agent writes under its own mount_path independently.
    // write_files(&s1.mount_path, ...) etc.

    // Commit all three branches concurrently.
    let (r1, r2, r3) = tokio::try_join!(
        client.session_commit_with_timeout(
            s1.session_id, Some("feat/auth-refactor".into()),
            Some("auth refactor".into()), Some(Duration::from_secs(10)),
        ),
        client.session_commit_with_timeout(
            s2.session_id, Some("feat/api-v2".into()),
            Some("api v2".into()), Some(Duration::from_secs(10)),
        ),
        client.session_commit_with_timeout(
            s3.session_id, Some("feat/ui-components".into()),
            Some("ui components".into()), Some(Duration::from_secs(10)),
        ),
    )?;

    println!("auth:  {}ms", r1.telemetry.total_duration_ms);
    println!("api:   {}ms", r2.telemetry.total_duration_ms);
    println!("ui:    {}ms", r3.telemetry.total_duration_ms);

    Ok(())
}
```

### Using with Claude Code agents

Assign each Claude Code agent instance a distinct session mount path via `SIMGIT_SOCKET` and session creation. The agents never need to know about each other — isolation and conflict detection are handled entirely by the daemon.

```bash
# Agent 1 shell
export SIMGIT_SOCKET=/tmp/simgit-dev/control.sock
sg new --task "auth-refactor" --label "claude-agent-1"
# work under the printed mount path, then:
sg commit --session <uuid> --branch feat/auth-refactor --message "auth refactor"

# Agent 2 shell (runs simultaneously)
sg new --task "api-v2" --label "claude-agent-2"
sg commit --session <uuid> --branch feat/api-v2 --message "api v2"

# Agent 3 shell (runs simultaneously)
sg new --task "ui-components" --label "claude-agent-3"
sg commit --session <uuid> --branch feat/ui-components --message "ui components"
```

### Scalability reference

The stress harness validates this at scale. For a quick local check:

```bash
source .venv/bin/activate
python3 tests/real_agent_harness.py --agents 20 --task-profile disjoint-files
```

Track 2 chaos validation ran 20 concurrent disjoint agents and 20 concurrent hotspot agents with 100% commit success rate and p95 commit latency well within SLO thresholds. See `docs/track2_chaos_validation.md` for full results.

## Integration model

An agent task should follow this lifecycle:

1. Create one session per autonomous task.
2. Write only under the returned session mount path.
3. Inspect diff for traceability.
4. Commit with explicit timeout/deadline policy.
5. Resolve ambiguous outcomes through commit status polling.
6. Abort stale or failed sessions.

This pattern keeps correctness decisions explicit and observable.

## Transport and socket discipline

Always ensure daemon and client agree on socket path.

Recommended approach:

```bash
export SIMGIT_STATE_DIR=/tmp/simgit-dev
export SIMGIT_SOCKET=/tmp/simgit-dev/control.sock
```

Pass socket path explicitly from orchestrator config rather than relying on default heuristics.

## Python usage

### Baseline flow

```python
import simgit

session = simgit.Session.new(
    task_id="agent-task-123",
    socket_path="/tmp/simgit-dev/control.sock",
    agent_label="planner-1",
)

info = session.info()
mount_path = info["mount_path"]

# Agent writes under mount_path

diff = session.diff()
changed_paths = diff["changed_paths"]

result = session.commit(
    branch_name="feat/agent-task-123",
    message="agent update",
    timeout_secs=5.0,
)

telemetry = result["telemetry"]
print("total ms:", telemetry["total_duration_ms"])
print("scheduler wait ms:", telemetry["scheduler_queue_wait_ms"])
```

### Conflict and retry posture

If commit fails due to overlap, treat it as a semantic conflict, not a transport failure. Re-plan using returned conflict context, then retry with a fresh session.

If scheduler queue wait is persistently high, apply orchestrator-level backoff instead of increasing commit concurrency.

## Rust usage

### Baseline flow with timeout

```rust
use std::time::Duration;
use simgit_sdk::{Client, SdkError};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::new("/tmp/simgit-dev/control.sock");

    let session = client
        .session_create("agent-task-123", Some("planner-1".into()), None, false)
        .await?;

    // Agent writes under session.mount_path

    let _diff = client.session_diff(session.session_id).await?;

    let commit = client
        .session_commit_with_timeout(
            session.session_id,
            Some("feat/agent-task-123".into()),
            Some("agent update".into()),
            Some(Duration::from_secs(5)),
        )
        .await;

    match commit {
        Ok(ok) => {
            let wait = ok.telemetry.scheduler_queue_wait_ms;
            if wait > 5000.0 {
                // Backoff signal for next attempt under high contention.
            }
        }
        Err(SdkError::DeadlineExceeded(_)) => {
            // Deterministic timeout; re-run strategy with fresh session.
        }
        Err(other) => return Err(other.into()),
    }

    Ok(())
}
```

## Deadlines and idempotency semantics

simgit commit behavior is built for transport ambiguity:

- Client sends request_id and deadline_epoch_ms.
- Daemon persists request state.
- If transport drops after server acceptance, client can query commit.status.
- Terminal commit state is authoritative; blind duplicate commit submission is avoided.

Operational implication:

- Separate retry policy into two paths:
  - Transport retry with status lookup
  - Semantic retry with fresh task/session planning

## Telemetry-driven orchestration

Commit telemetry fields support adaptive control loops:

- total_duration_ms
- scheduler_queue_wait_ms
- conflict_scan_ms
- flatten_ms
- flatten_write_tree_ms
- flatten_commit_object_ms

Recommended controls:

- If scheduler_queue_wait_ms grows, reduce concurrency for overlapping files.
- If flatten_ms dominates, reduce per-session payload size and branch fan-out.
- If conflict metrics rise, rebalance workload partitioning from hotspot toward disjoint shards.

## Stress toolchain for agent teams

Use these scripts to validate orchestrator behavior before production rollout:

- tests/real_agent_harness.py: deterministic workload baseline
- tests/stress/drunk_agent.py: profile-based non-deterministic timing
- tests/stress/fault_injector.py: transport and resilience faults
- tests/stress/swarm_runner.py: end-to-end SLO gate runner

## Production hardening checklist

1. Enforce per-task session lifecycle ownership in orchestrator code.
2. Set commit timeout policy by task class and SLO budget.
3. Use request-id aware commit resolution before any retry.
4. Export commit telemetry and lock/conflict counters to central monitoring.
5. Keep chaos runner as nightly gate for regression detection.
6. Run stress against disposable repositories only.

## Anti-patterns

- Sharing one session among independent tasks.
- Retrying commits without checking commit.status.
- Inferring daemon health from one latency metric.
- Running hotspot-heavy workloads without scheduler-aware backoff.
- Interpreting harness end-to-end p95 as pure daemon internal latency.
