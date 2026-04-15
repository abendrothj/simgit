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

session.commit(branch_name="feat/agent-task-123", message="agent update")
```

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
