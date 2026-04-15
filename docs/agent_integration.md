# Agent Integration Guide

This guide shows how to integrate AI agent runners with simgit using either the Rust SDK or Python bindings.

## Prerequisites

- `simgitd` running and reachable through the control socket
- Repository configured for simgit sessions
- For Python flows: `simgit` package built from `simgit-py`

## Integration Pattern

A typical agent task lifecycle is:

1. Create a session for task isolation.
2. Perform file operations under the mounted session path.
3. Inspect diff for auditability.
4. Commit to a branch, or abort on failure.

## Python Example

```python
import simgit

session = simgit.Session.new(task_id="agent-task-123")
info = session.info()
print("mount:", info["mount_path"])

# Agent writes under info["mount_path"] here.

diff = session.diff()
print("changed paths:", diff["changed_paths"])

result = session.commit(
    branch_name="feat/agent-task-123",
    message="agent update",
    timeout_secs=5.0,
)

telemetry = result["telemetry"]
print("commit duration ms:", telemetry["total_duration_ms"])

# Contention-aware retry/backoff signal from commit scheduler wait.
if telemetry["scheduler_queue_wait_ms"] > 5000:
    print("high contention: consider backoff before next commit")
```

`Session.commit()` returns session fields plus a `telemetry` dictionary with per-stage timings (milliseconds), including scheduler wait and flatten sub-stages. This is intended for adaptive retries and throughput tuning in agent runners.

## Rust Example

```rust
use simgit_sdk::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::from_env();

    let session = client
        .session_create("agent-task-123", Some("agent-1".into()), None, false)
        .await?;

    // Agent writes under session.mount_path.

    let _diff = client.session_diff(session.session_id).await?;

    client
        .session_commit(
            session.session_id,
            Some("feat/agent-task-123".into()),
            Some("agent update".into()),
        )
        .await?;

    Ok(())
}
```

## Concurrency Stress Harness

A 50-agent deterministic/LLM harness is provided at:

- `tests/real_agent_harness.py`

Usage:

```bash
python3 tests/real_agent_harness.py --agents 50 --task-profile disjoint-files
```

Use `--mode commit` only when your mounted session paths are writable and your daemon is connected to a disposable test repository.

For overlap-focused benchmarking:

```bash
python3 tests/real_agent_harness.py \
    --agents 50 --workers 50 --task-profile hotspot-file --commit-barrier \
    --socket /tmp/simgit-stress-state/control.sock \
    --report-out /tmp/simgit-real-agent-hotspot.json
```

Then inspect:
- `simgit_session_commit_stage_duration_seconds{stage="scheduler_queue_wait|flatten|flatten_git_commit"}`
- `simgit_session_commit_conflicts_total{kind="active_session_overlap"}` (expected to remain low/zero with scheduler enabled)

## Operational Guidance

- Prefer one session per autonomous task.
- Use deterministic branch names (`feat/<task-id>`) for traceability.
- Prefer enabling path scheduling (`SIMGIT_COMMIT_WAIT_SECS > 0`) so overlapping commits queue by path instead of immediate overlap rejection.
- If scheduling is disabled (`SIMGIT_COMMIT_WAIT_SECS=0`), inspect peer/session/path conflict payloads and retry with a refreshed session.
- Flatten is gix-only by default; no engine switch is required in normal operation.
- Keep stress runs on test repos; avoid production branches.

## Timeout and Adaptive Retry Guidance

`session.commit` now supports request-level deadlines to prevent split-brain behavior between client and daemon. The Rust SDK method `session_commit_with_timeout` sends a `deadline_epoch_ms` to the daemon and applies the same timeout on the client transport.

If the daemon cannot safely start flatten before the deadline, it returns `ERR_DEADLINE_EXCEEDED` (`-32006`) instead of committing late.

```rust
use std::time::Duration;
use simgit_sdk::{Client, SdkError};

let client = Client::from_env();

let result = client
    .session_commit_with_timeout(
        session.session_id,
        Some("feat/agent-task-123".into()),
        Some("agent update".into()),
        Some(Duration::from_secs(5)),
    )
    .await;

match result {
    Ok(commit) => {
        let queue_wait = commit.telemetry.scheduler_queue_wait_ms;
        if queue_wait > 5_000.0 {
            // High contention signal: back off before next commit attempt.
        }
    }
    Err(SdkError::DeadlineExceeded(_)) => {
        // Deterministic timeout: create a new session or back off and retry.
    }
    Err(other) => return Err(other.into()),
}
```

Python bindings now expose the same per-call knob on `Session.commit(..., timeout_secs=...)`. Use this for adaptive retries instead of only relying on global `SIMGIT_RPC_TIMEOUT_SECS`.
