# Agent Integration Guide

This guide is for building production-grade agent runners on top of simgit using either Python bindings or the Rust SDK.

## Lean agent worktrees — native Git plus filesystem CoW

Ordinary `git worktree add` inflates every tracked file for every agent.
`sg worktree add` keeps Git's linked-worktree model but populates the checkout
from one immutable cached baseline using filesystem CoW clones. Existing agents
still use Git without modification:

```bash
git worktree add ../agent-1-session main
cd ../agent-1-session
```

with:

```bash
cd $(sg worktree add feat/my-feature)
```

The printed path is a real Git worktree. It appears in both `git worktree list`
and `sg worktree list`; its `.git` file points at the common repository, and
its branch, index, reflog, hooks, remotes, config, and object database all use
Git's standard implementation.

### How it works

Creation is intentionally small:

| Step | What it does |
|---|---|
| `git worktree add --no-checkout -b ...` | Registers the worktree and creates its private HEAD/index administration. |
| `git read-tree HEAD` | Initializes that private index before files are populated. |
| Cached native checkout | `git checkout-index` materializes each commit once under `.git/simgit/baselines/<commit>/tree`, preserving checkout filters and attributes without running hooks. |
| APFS clone / Linux reflink | Populates ordinary files whose unchanged extents remain physically shared. |
| `git status --porcelain` | Verifies the populated tree exactly matches its index before returning it. |

If the capability probe cannot clone a file, `sg` performs a normal Git
checkout. Pass `--require-cow` when silently consuming full checkout disk would
be unacceptable. Failed setup is rolled back, including its newly created
branch.

### Every git command works

| Command | Status | Notes |
|---|---|---|
| `git status` | ✓ | Standard linked-worktree index and ordinary local files. |
| `git diff` | ✓ | Same comparison. |
| `git diff --cached` | ✓ | Staging area vs HEAD. |
| `git log` / `git blame` | ✓ | Shared Git object database and refs. |
| `git show` | ✓ | Any commit reachable from the real repo. |
| `git add` | ✓ | Native Git index. |
| `git commit` | ✓ | Native commit and branch-ref update; repository hooks run normally. |
| `git checkout <branch>` | ✓ | Native Git behavior and linked-worktree branch exclusivity. |
| `git checkout <file>` | ✓ | Restores the file from the index in the private working tree. |
| `git cherry-pick` | ✓ | Applies commits via checkout + write path. |
| `git push` / `git fetch` | ✓ | Shared repository config, remotes, and refs. |
| `git merge` / `git rebase` | ✓ | Native Git behavior. |
| `git commit --amend` / `git stash` / `git bisect` | ✓ | Native Git behavior. |
| `git clean` | ✓ | Native filesystem and Git behavior. |

### Session isolation

Each agent writes to a distinct directory and a distinct linked-worktree index.
CoW shares physical extents, not mutable file identity: writing one clone does
not change another worktree. Git prevents the same branch from being checked
out in multiple linked worktrees. Cross-branch semantic conflicts remain Git's
normal merge/rebase concern; use daemon-managed `sg new` sessions when
simgit's path-lease and commit-scheduler semantics are required.

### When to use simgit vs git worktree

- **`git worktree`** → no baseline cache and a full checkout per worktree.
- **`sg worktree`** → same Git integration and local-I/O path, with one cached
  baseline plus private changed extents. Best for many mostly-read, sparsely
  edited agents.
- **Daemon sessions (`sg new`, SDK, Python)** → stronger coordinated commit and
  observability semantics, with more machinery and a non-native lifecycle.

### Lifecycle and cleanup

`sg worktree remove` delegates to `git worktree remove` and therefore refuses
to delete dirty state. `--commit` stages and commits all changes first;
`--force` explicitly discards them. `sg worktree prune` removes stale Git
registrations and cached baselines unused for seven days, while
`sg worktree prune --all` drops the entire baseline cache. Existing worktrees
remain valid after cache pruning because cloned extents have independent
filesystem references.

### Disabling the git proxy

If agents don't need `git` subprocess support, set:

```python
session = simgit.Session.new(
    task_id="agent-task-123",
    agent_label="planner-1",
    git_proxy_enabled=False,   # skips .git/ bootstrap
)
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

PORT_FILE = "/tmp/simgit-dev/control.port"

async def agent_task(task_id: str, branch: str, label: str, write_fn):
    session = simgit.Session.new(
        task_id=task_id,
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
    let client = Client::new("/tmp/simgit-dev/control.port");

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

Assign each Claude Code agent instance a distinct linked worktree via
`sg worktree`.
The agents get separate native branches, indexes, and working directories. No
daemon or environment setup is involved.

```bash
# Agent 1 shell
cd $(sg worktree add feat/auth-refactor)
# agent writes files and commits with native Git
sg worktree remove --commit --message "auth refactor"

# Agent 2 shell (runs simultaneously)
cd $(sg worktree add feat/api-v2)
# agent writes files, runs `git commit`
sg worktree remove --commit --message "api v2"

# Agent 3 shell (runs simultaneously)
cd $(sg worktree add feat/ui-components)
# agent writes files, runs `git commit`
sg worktree remove --commit --message "ui components"
```

For programmatic commit control without git subprocesses:
```bash
sg new --task "auth-refactor" --label "agent-1"
# ... write files ...
sg commit --branch feat/auth-refactor --message "auth refactor"
```

### Scalability reference

The stress harness validates this at scale. For a quick local check:

```bash
source .venv/bin/activate
python3 tests/real_agent_harness.py --agents 20 --task-profile disjoint-files
```

The original Track 2 chaos validation ran against the VFS-era backend and is
retained as historical correctness/fault evidence, not a CoW latency baseline.
The current default-CoW nightly gate ran 60 disjoint and 60 hotspot agents with
100% success; p95 was 3989 ms and 4338 ms respectively. See
`docs/track2_chaos_validation.md` and `docs/scaling_benchmark.md`.

## Integration model

An agent task should follow this lifecycle:

1. Create one session per autonomous task.
2. Write only under the returned session mount path.
3. Inspect diff for traceability.
4. Commit with explicit timeout/deadline policy.
5. Resolve ambiguous outcomes through commit status polling.
6. Abort stale or failed sessions.

This pattern keeps correctness decisions explicit and observable.

## Transport and port discovery

The daemon binds a TCP listener on `127.0.0.1:0` (random port) and writes the
assigned port number to a discovery file.  Clients read this file to connect.
No platform-specific IPC primitives — works identically on Linux, macOS, and
Windows.

**With `sg worktree` (recommended for lean agent workflows):** No daemon,
socket, mount, or environment variable is involved.

```bash
cd $(sg worktree add feat/my-branch)
```

**For daemon-managed SDK/Python sessions:**

```bash
export SIMGIT_STATE_DIR=/tmp/simgit-dev
export SIMGIT_PORT_FILE=/tmp/simgit-dev/control.port
```

Pass port-file path explicitly from orchestrator config rather than relying on default heuristics.

## Python usage

### Baseline flow

```python
import simgit

session = simgit.Session.new(
    task_id="agent-task-123",
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
    let client = Client::new("/tmp/simgit-dev/control.port");

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
