# Testing Guide

This guide outlines the testing strategy for simgit and explains how to run, write, and validate tests.

## Overview

simgit uses a **three-tier testing strategy**:

1. **Unit Tests** — Fast, isolated tests of individual functions/modules (within each crate)
2. **Integration Tests** — Tests of subsystem interactions (VFS + borrow registry, delta store + session manager)
3. **E2E Tests** — Full daemon + multi-agent scenarios (Phase 2+)

## Running Tests

### Run All Tests
```bash
cargo test --workspace
```

### Run Tests for a Specific Crate
```bash
# SDK tests
cargo test -p simgit-sdk

# Daemon tests
cargo test -p simgitd

# CLI tests
cargo test -p sg
```

### Run Integration Tests Only
```bash
cargo test --test '*' --lib
```

### Run a Specific Test
```bash
cargo test test_borrow_semantics_exclusive_write
```

### Run Range-Aware Commit Tests
```bash
cargo test -p simgitd session_commit_succeeds_for_non_overlapping_byte_ranges
cargo test -p simgitd session_commit_conflicts_for_overlapping_byte_ranges
```

### Run with Logging
```bash
RUST_LOG=debug cargo test -- --nocapture
```

### Run Single-Threaded (for debugging)
```bash
cargo test -- --test-threads=1 --nocapture
```

## Test Organization

### Module-Level Unit Tests

Unit tests live within each module via `#[cfg(test)]` blocks.

**Location:** `simgitd/src/borrow/registry.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acquire_write_succeeds_when_free() {
        // Inline test of registry.acquire_write()
    }
}
```

**Run:**
```bash
cargo test --lib registry
```

### Integration Tests

Integration tests live in `tests/` at the workspace root and test complete workflows.

**Location:** `tests/borrow_checker_tests.rs`

```rust
#[test]
fn test_exclusive_write_enforcement() {
    // Multi-step scenario:
    // 1. Start daemon
    // 2. Create sessions
    // 3. Verify locking behavior
}
```

**Run:**
```bash
cargo test --test borrow_checker_tests
```

## Writing Tests

### Unit Test Template

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Test should have a descriptive name: test_<component>_<scenario>_<expected_behavior>
    #[test]
    fn test_component_scenario_succeeds() {
        // Arrange
        let input = /* ... */;
        
        // Act
        let result = component.operation(input);
        
        // Assert
        assert!(result.is_ok(), "Expected success but got: {:?}", result);
        assert_eq!(result.unwrap().field, expected_value);
    }

    /// Doc comment explains what this tests.
    #[test]
    fn test_error_case_returns_expected_error() {
        let invalid_input = /* ... */;
        let result = component.operation(invalid_input);
        
        assert!(result.is_err());
        match result.unwrap_err() {
            SpecificError::Kind => { /* OK */ }
            _ => panic!("Wrong error type"),
        }
    }
}
```

### Integration Test Template

```rust
//! Integration test for <subsystem>.
//!
//! Validates behavior across multiple components.

#[test]
fn test_subsystem_workflow_end_to_end() {
    // 1. Setup: Initialize daemon, create temp repo, etc.
    let daemon = DaemonHandle::start();
    let session = daemon.create_session("task-id").unwrap();

    // 2. Execute: Run the workflow
    session.write_file("/src/main.rs", b"code").unwrap();
    let diff = session.diff_vs_head().unwrap();

    // 3. Verify: Assertions on outcome
    assert!(!diff.unified_diff.is_empty());
    session.commit("branch-name", "message").unwrap();

    // 4. Cleanup
    daemon.shutdown();
}
```

## Test Documentation Requirements

Every test **must have a doc comment** explaining:

1. **What it tests** — The component/method
2. **Scenario** — The specific situation (happy path, error case, edge case)
3. **Expected behavior** — What should happen

```rust
/// Test that the borrow registry prevents two sessions from writing the same path.
///
/// Scenario: Session A holds write lock on /src/config.rs.
///           Session B attempts to acquire write lock on the same path.
///
/// Expected: Session B receives BorrowError immediately (no blocking).
#[test]
fn test_exclusive_write_prevents_concurrent_writes() {
    // Arrange & Act & Assert
}
```

## Naming Conventions

### Test Names
- **Pattern:** `test_<component>_<scenario>`
- **Examples:**
  - `test_borrow_registry_exclusive_write_enforced`
  - `test_session_manager_creates_and_persists`
  - `test_delta_store_handles_collisions`
  - `test_rpc_server_parses_valid_json_rpc`

### Test Function Names
- Use `assert_*` for assertions (provided by `assert!`, `assert_eq!`, etc.)
- Use `should_` prefix for helpers that validate preconditions
- Example:
  ```rust
  #[test]
  fn test_borrow_registry_reader_not_blocked_by_writer() {
      // ...
      should_have_acquired_lock(&reg, session1, path);
      assert!(reg.acquire_read(session2, path).is_ok());
  }

  fn should_have_acquired_lock(reg: &BorrowRegistry, session: Uuid, path: &Path) {
      let locks = reg.list(None);
      assert!(locks.iter().any(|l| l.path == path && l.writer_session == Some(session)));
  }
  ```

## Coverage Goals

- **Borrow Registry:** 90%+ coverage (core invariant validation)
- **Delta Store:** 85%+ coverage (content integrity, atomicity)
- **Session Manager:** 80%+ coverage (CRUD, recovery)
- **RPC Methods:** 75%+ coverage (happy + error paths)
- **VFS Router:** 70%+ coverage (dispatch logic)

Check coverage with:
```bash
# Install tarpaulin (Rust coverage tool)
cargo install cargo-tarpaulin

# Run with coverage
cargo tarpaulin --workspace --out Html
```

## Common Assertions

```rust
// Basic assertions
assert!(condition);
assert_eq!(actual, expected);
assert_ne!(actual, unexpected);

// Option assertions
assert!(option.is_some());
assert!(option.is_none());

// Result assertions
assert!(result.is_ok());
assert!(result.is_err());

// Error handling
assert_eq!(error_result.unwrap_err().kind, ExpectedErrorKind::Something);

// Collection assertions
assert_eq!(vec.len(), 3);
assert!(vec.contains(&item));
```

## Debugging Tests

### Print During Test Execution
```rust
#[test]
fn test_something() {
    eprintln!("Debug info: {:?}", value);  // Use eprintln!, not println!
    assert!(condition);
}
```

## Stress Benchmarks

Use the stress harness for control-plane and conflict behavior validation under load.

```bash
# Start daemon in a disposable repo
rm -rf /tmp/simgit-stress-state && mkdir -p /tmp/simgit-stress-state
cd /tmp/simgit-disposable-repo
SIMGIT_REPO=/tmp/simgit-disposable-repo \
SIMGIT_STATE_DIR=/tmp/simgit-stress-state \
/Users/ja/Desktop/projects/simgit/target/debug/simgitd

# In another shell
cd /Users/ja/Desktop/projects/simgit
source .venv/bin/activate
python tests/stress/agent_harness.py \
    --agents 50 --workers 50 --mode commit \
    --overlap-path hotspot/shared.txt --two-phase-barrier \
    --socket /tmp/simgit-stress-state/control.sock \
    --json --report-out /tmp/simgit-stress-report.json
```

Retry-focused benchmark (long-lived session commit attempts):

```bash
cd /Users/ja/Desktop/projects/simgit
source .venv/bin/activate
python tests/stress/agent_harness.py \
    --agents 20 --workers 10 --execution-mode phased \
    --stress-mode disjoint-range --commit-workers 10 \
    --retry-attempts 3 --retry-failure-mode invalid-branch \
    --socket /tmp/simgit-stress-state/control.sock \
    --report-out /tmp/simgit-retry-report.json
```

The JSON report includes `retry` fields with attempt counts and per-attempt
latency percentiles (`retry.attempt_latency.p50_ms`, `p95_ms`, `p99_ms`).

Key Prometheus series to capture during stress runs:
- `simgit_session_commit_stage_duration_seconds{stage="capture_self|capture_peers|conflict_scan|flatten"}`
- `simgit_session_commit_conflicts_total{kind="active_session_overlap"}`
- `simgit_session_commit_conflict_paths`
- `simgit_session_commit_conflict_peers`
- `simgit_peer_capture_skip_total{result="hit|miss"}`

### Incremental Peer-Capture A/B (Validated)

Observed on `20 agents x 5 retry attempts` (`invalid-branch` early failures, disjoint-range workload):
- `capture_peers_execution` average/event: `242.432ms -> 7.408ms` (96.9% reduction)
- End-to-end latency: `p95 10688ms -> 3771ms` (64.7% reduction)
- Fingerprint skip efficiency:
  - `hit=1344` (skip mount walk)
  - `miss=20` (re-scan mount)
  - hit rate: `98.5%`

Interpretation:
- The dominant retry-path tax moved out of the commit critical path.
- The `miss` population confirms invalidation still triggers when peer state changes.

### False-Positive Guardrail Checklist

Use this checklist to avoid over-claiming wins from measurement artifacts:
- Run control and treatment with identical workload shape (agent count, retries, commit workers, socket timeout, state isolation).
- Capture metrics snapshots before and after each run, then diff only the window (`after - before`).
- Verify `simgit_peer_capture_skip_total{result="hit|miss"}` and stage histograms move in the same direction.
- Confirm `SIMGIT_NFS_INCREMENTAL_CAPTURE=0` materially degrades `capture_peers_execution` vs `=1`.
- Ensure correctness invariants stay green (`successes=agents`, no unexpected conflict taxonomy drift).

Minimum anti-false-positive command pair:

```bash
# Control (incremental OFF)
SIMGIT_NFS_INCREMENTAL_CAPTURE=0 ... simgitd ...

# Treatment (incremental ON)
SIMGIT_NFS_INCREMENTAL_CAPTURE=1 ... simgitd ...
```

### Fingerprinting Hardening Roadmap

Current fingerprint inputs: `(relative_path, size, mtime_ns)`.

Hardening candidates for high-frequency rewrite loops:
- Add `ctime_ns` when available.
- Include inode identity/generation when platform support is reliable.
- Optional fast content sentinel (e.g., hash of first 4KB) on suspiciously stable metadata.

Policy:
- Keep commit-time fingerprinting deterministic by default.
- Gate stronger probes behind env flags to preserve baseline throughput when not needed.

### Lessons Learned

1. The real scaling wall was algorithmic, not scheduler-related:
    - Without skip logic, peer capture behaves like repeated full scans across active peers, which trends toward an $O(N^2)$ tax as active sessions rise.
2. Backend behavior matters for optimization shape:
    - macOS NFS-loopback (Phase 0 plain-directory capture at commit time) benefits immediately from fingerprint skips.
    - Linux FUSE path already intercepts writes in-kernel, so this specific optimization primarily targets the macOS commit path.
3. Deterministic commit-time checks are operationally simpler than event-driven invalidation:
    - Fewer moving parts, easier incident replay, lower maintenance burden while throughput is already acceptable.

## SLO Definition & Nightly Gate

Mock swarm stress testing is the primary hardening lever: a deterministic, reproducible baseline that detects regressions before real-agent canary runs.

**Baseline Established:** April 2026, 60-agent swarm validation (commit a104774).

### SLO Table

| Scenario | Success Rate | p95 Latency | p99 Latency | Peer-Capture Hit Rate | Failure Taxonomy | Notes |
|----------|-------------|------------|------------|---------------------|------------------|-------|
| **Disjoint-Range (60 agents)** | 100% | < 8,000ms | < 8,500ms | > 90% | None | Workers write to non-overlapping paths. This is the happy path. |
| **Hotspot (60 agents)** | lock_conflict only | < 6,500ms | < 7,000ms | > 85% | All failures must be `lock_conflict` only. Zero other failure types tolerated. | Multiple workers contend on shared path(s). Conflicts must be explicit, not silent. |

### Interpretation & Escalation

- **Disjoint-Range Regression:** If success rate drops below 100%, or p95/p99 exceed thresholds, investigate:
  - New panic/unwind in commit path
  - RPC timeout misconfiguration
  - Daemon stability under load
  - **Escalation:** Block deployment, run bisect against main
  
- **Hotspot Regression:** If non-lock_conflict failures appear (e.g., `silent_corruption`, `merge_conflict`), investigate:
  - Incorrect conflict detection (false negatives)
  - Data corruption in delta store
  - Fingerprinting skipping legitimate invalidation
  - **Escalation:** Block deployment, run control/treatment A/B to isolate root cause
  
- **Peer-Capture Regression:** If hit rate drops below target (90%), investigate:
  - Fingerprint invalidation logic (too conservative or too aggressive)
  - Mount walk performance degradation
  - **Action:** Re-baseline using control/treatment pair (SIMGIT_NFS_INCREMENTAL_CAPTURE=0/1)

### Running the SLO Gate

**Automated (Nightly CI):**
The gate runs on all commits via `.github/workflows/nightly-slo.yml` (see CI section below).

**Manual Validation:**
```bash
# Clean state and run the gate locally
cd /Users/ja/Desktop/projects/simgit
./tests/nightly-slo-gate.sh
```

Output structure:
```
===== NIGHTLY SLO GATE =====
Loading release binary...
Starting daemon...
Running disjoint-range (60 agents, 24 workers)...
Running hotspot (60 agents, 24 workers)...
Extracting metrics...

DISJOINT-RANGE RESULTS:
  Success: 60/60 (PASS)
  p95: 6708ms (PASS, threshold 8000ms)
  p99: 7068ms (PASS, threshold 8500ms)
  Peer-Capture Hit Rate: 97.1% (PASS, threshold 90%)

HOTSPOT RESULTS:
  Success: 0/60 (expected lock_conflict)
  Failure Taxonomy: lock_conflict=60 (PASS, zero other types)
  p95: 5401ms (PASS, threshold 6500ms)
  p99: 5603ms (PASS, threshold 7000ms)

OVERALL: PASS ✓
```

### Archiving Results

All SLO gate runs are archived with:
- JSON reports (`/tmp/simgit-slo-*-{disjoint,hotspot}.json`)
- Prometheus metrics snapshots (`/tmp/simgit-slo-*-{before,after}.prom`)
- Human-readable summary (logged to stdout and saved to `/tmp/simgit-slo-latest.log`)

Trend analysis: Extract peer_capture_skip deltas across weekly/monthly runs to plot efficiency degradation and set ceiling.

## Track 2: Mock-to-Reality Calibration

Once mock swarm SLOs are stable (2+ weeks), the next phase validates system behavior against real agents.
Real-agent scenarios capture:
- **Conflict patterns:** Real agents produce diverse, irregular edits (not repeated hotspot contention like mock).
- **Tool-call patterns:** Retries are erratic and influenced by LLM latency, not deterministic.
- **File churn:** Build artifacts, cache files, temporary outputs that mock doesn't model.
- **Latency tails:** Model inference time dominates (not just RPC round-trip).

### Using the Real-Agent Harness

**Harness Status:** Implemented in `tests/real_agent_harness.py` with real mount-path writes, concurrent execution, commit retries, and weakness-focused reporting.

**Current Capability:**
- Deterministic overlap profiles: `hotspot-file`, `disjoint-files`, `mixed`, `sharded-hotspot`
- OpenAI-compatible LLM agent via `SIMGIT_LLM_API_KEY` or `OPENAI_API_KEY`
- Weakness summary with dominant failure type, top paths, retry rate, and commit latency percentiles

**Track 3 Status (Validated):**
- Path-level commit scheduling is active when `SIMGIT_COMMIT_WAIT_SECS > 0`.
- Latest smoke validation (`8 agents`) with scheduling enabled:
  - `hotspot-file`: `success_rate=1.0`, `total_conflicts=0`
  - `sharded-hotspot`: `success_rate=1.0`, `total_conflicts=0`
  - `disjoint-files`: `success_rate=1.0`, `total_conflicts=0`

**Session Management:** RPC client handles session lifecycle; agent implementations focus on edit strategy and return real file edits that are applied in the session mount before commit.

```bash
# Deterministic hotspot run (good for exposing contention)
python3 tests/real_agent_harness.py \
  --agents 5 \
  --task-profile hotspot-file \
  --commit-barrier \
  --socket /tmp/simgit-slo-latest/control.sock \
  --report-out /tmp/real_test_results.json

# Deterministic disjoint run (good control group)
python3 tests/real_agent_harness.py \
  --agents 5 \
  --task-profile disjoint-files \
  --socket /tmp/simgit-slo-latest/control.sock \
  --report-out /tmp/real_disjoint_results.json

# Deterministic sharded hotspot run (same directory family, per-agent shard files)
python3 tests/real_agent_harness.py \
  --agents 5 \
  --task-profile sharded-hotspot \
  --socket /tmp/simgit-slo-latest/control.sock \
  --report-out /tmp/real_sharded_results.json

# LLM-driven run using an OpenAI-compatible endpoint
SIMGIT_LLM_API_KEY=... python3 tests/real_agent_harness.py \
  --agent-type llm \
  --model gpt-4.1 \
  --agents 5 \
  --task-profile mixed \
  --task "Refine the benchmark text files clearly and consistently" \
  --report-out /tmp/real_llm_results.json
```

**LLM Environment Variables:**
- `SIMGIT_LLM_API_KEY` or `OPENAI_API_KEY`: required for `--agent-type llm`
- `SIMGIT_LLM_BASE_URL` or `OPENAI_BASE_URL`: optional OpenAI-compatible base URL
- `SIMGIT_LLM_TIMEOUT_SECS`: optional HTTP timeout override

**Commit Scheduler Environment Variable:**
- `SIMGIT_COMMIT_WAIT_SECS`: enables path-level commit scheduling when `> 0` (default `30`)
- Set to `0` to disable scheduling and keep immediate overlap rejection semantics

**Report Structure:**
```json
{
  "timestamp": "2026-04-14T...",
  "total_agents": 5,
  "successful_agents": 3,
  "sessions": [
    {
      "agent_id": "agent-abc123",
      "session_id": "sess-xyz789",
      "success": true,
      "total_duration_s": 2.345,
      "file_edits": [
        {"file_path": "main.py", "action": "create", "size_bytes": 150},
        {"file_path": "main.py", "action": "modify", "size_bytes": 250}
      ],
      "commit_attempts": [
        {
          "attempt_num": 1,
          "duration_ms": 234.5,
          "success": true,
          "edits_count": 2
        }
      ],
      "conflicts_encountered": 0,
      "model_used": null
    }
  ],
  "summary": {
    "success_rate": 0.6,
    "avg_duration_s": 2.1,
    "total_commit_attempts": 8,
    "total_conflicts": 5
  },
  "weakness_summary": {
    "dominant_failure_type": "lock_conflict",
    "conflict_rate": 0.625,
    "commit_latency_ms": {
      "p50": 184.0,
      "p95": 712.3,
      "p99": 801.0
    },
    "top_paths": [["bench/shared_hotspot.txt", 10]],
    "recommendation": "edit_isolation_or_ast_locking"
  }
}
```

### Extending with LLM Agents

The built-in `LLMAgent` already uses an OpenAI-compatible `chat/completions` endpoint and asks the model to emit a JSON edit plan constrained to the allowed benchmark paths. If you extend it further, preserve these invariants:

1. Keep path selection constrained so overlap is intentional and measurable.
2. Record `model_tokens_used` and `tool_calls_made` for cost analysis.
3. Treat malformed model output as harness signal, not silent success.
4. Prefer deterministic benchmark tasks before repo-wide open-ended tasks.

### Comparison Methodology

After running real agents:

1. **Conflict taxonomy comparison:**
   - Extract lock_conflict rate from mock hotspot baseline
   - Compare to real-agent failure breakdown
   - If real shows new types (e.g., merge_conflict), investigate root cause

2. **Latency distribution:**
   - Compare p50/p95/p99 real vs mock (expect real to be longer due to model inference)
   - Confirm tail doesn't regress by >10% vs baseline

3. **File churn analysis:**
   - Real agents typically generate more files (build artifacts, cache scripts)
   - Mock harness can be updated with observed patterns for improved realism

4. **Retry behavior:**
   - Real agents show more irregular retry patterns (LLM-driven, not phased)
   - Compare conflict resolution rates: mock (explicit blocking) vs real (potential deadlocks)

5. **Weakness selection:**
  - `edit_isolation_or_ast_locking`: repeated overlap on the same paths with high conflict rate
  - `scheduler_or_backpressure`: high tail latency without dominant conflict pressure
  - `task_routing_or_overlap_policy`: moderate conflict rate with avoidable overlap patterns
  - `harness_or_protocol_hardening`: non-conflict infrastructure failures still dominate

### Run with Log Output
```bash
RUST_LOG=debug cargo test test_name -- --nocapture
```

### Single-Threaded Execution (prevents race conditions from masking bugs)
```bash
cargo test -- --test-threads=1
```

### gdb Integration
```bash
rust-gdb ./target/debug/deps/simgitd-<hash>
```

## Continuous Integration

CI coverage is active and includes Linux FUSE integration workflows in addition to standard crate tests.
Example baseline workflow shape:

```yaml
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --workspace
      - run: cargo test --doc
```

## Phases & Extended Testing

### Phase 0 (Complete)
- ✅ Module-level documentation tests
- ✅ Basic borrow registry semantics
- ✅ Foundation integration test scaffolding established; deeper end-to-end coverage continues in later phases

### Phase 1
- Add full integration tests for read-only VFS
- Add daemon startup/shutdown test harness
- Add multi-agent concurrency tests
- Current status:
    - Unit-level VFS coverage is active for git tree traversal, nested lookup, read/readlink behavior, and diff helpers.
    - Full end-to-end FUSE mount integration remains pending.

### Phase 2
- Add delta store integrity tests
- Add session persistence + recovery tests
- Add RPC method tests (all 7 methods)
- Current status:
    - Delta-aware VFS unit tests cover write offset behavior and path utilities in `vfs::fuse_backend`.
    - Synthetic inode mapping tests cover delta-only file metadata flow in `vfs::git_resolver`.
    - Delta-aware `getattr` metadata updates are implemented for session-modified files.
    - Delta store recovery tests validate session discovery via `delta::store::list_sessions`.
    - Session manager tests validate ACTIVE/COMMITTED persistence across reopen for crash recovery bootstrap.
    - Session recovery tests validate ACTIVE session mount re-attachment via the NFS-loopback backend.
    - Linux-only ignored FUSE integration harness covers mount-level `create`/`unlink`/`rename` plus remount scenarios.
    - RPC diff regression tests for add/modify/delete are green.
    - Next target is integration tests for `unlink`/`rename`/`create` flows through mounted sessions.

### Phase 3+
- Add E2E multi-agent scenarios
- Add performance / stress tests
- Add chaos engineering tests (daemon crashes, network failures)
- Current status:
    - Phase 3 lock semantics are covered by borrow registry tests and lock wait/acquire RPC logic.
    - Phase 4 helper coverage includes changed-path and overlap detection for pre-commit conflict checks.
    - Phase 4 integration tests now cover commit overlap blocking and non-overlap commit success across active sessions.
    - Phase 4 RPC coverage now validates per-path overlap operation details and flatten error taxonomy mapping.
    - Phase 4 auto-merge behavior is still partial; scheduler-enabled overlap handling now queues overlapping commits by path instead of immediately rejecting active peers.
    - Phase 5 bootstrap validation: `cargo check -p simgit-py` is green with ABI3 compatibility enabled.
    - Phase 5 packaging flow is configured via `simgit-py/pyproject.toml` (maturin backend).
    - Wheel build/publish commands are documented; runtime wheel build was not executed locally because `maturin` is not installed in this environment.
    - Phase 5 stress harness scaffold (`tests/stress/agent_harness.py`) is syntax-validated via `python3 -m py_compile`.
    - Full 50-agent runtime execution requires installed Python bindings and a running daemon-backed test repository.
    - Phase 6 CLI surface started with `sg peer diff <session-id>` (build-level validation in `cargo test -p sg`).
    - Phase 6 event polling surface (`event.list` + `sg peer events`) is validated by daemon unit tests and crate builds.
    - Phase 6 event streaming surface (`event.subscribe` + `sg peer events --stream`) is compile-validated through `cargo test -p simgitd` and `cargo test -p sg`.
    - Phase 6 peer snapshot VFS path parsing is unit-tested in `vfs::fuse_backend::tests`.
    - Linux-only FUSE integration tests remain unexecuted on this macOS host (`cargo test -p simgitd linux_integration_tests -- --ignored` runs 0 tests).
    - Linux FUSE integration tests are now wired to CI in `.github/workflows/fuse-linux-integration.yml` (manual + nightly); job skips when `/dev/fuse` is unavailable on runner.
    - Phase 7 metrics instrumentation compiles and is covered by daemon regression suite (`cargo test -p simgitd`).
    - Phase 7 includes an HTTP scrape test for `GET /metrics` in `metrics::tests::metrics_endpoint_exposes_key_series`.
    - Lock persistence fault-injection coverage validates in-memory lock invariants under simulated SQLite failures (`acquire_write_still_grants_when_sqlite_persist_fails`, `release_session_still_clears_in_memory_when_sqlite_remove_fails`).
    - RPC lock contention reporting is validated by `rpc::methods::tests::lock_contention_reports_top_paths`.
    - Phase 7 adds a local container profile for observability smoke checks in `deploy/dev/` (daemon + Prometheus).
    - Stress harness now emits percentile latency and failure taxonomy in JSON (`tests/stress/agent_harness.py --report-out <path>`).

### Next Todo (1-3)
1. Implement auto three-way merge attempt for non-conflicting overlaps in `session.commit` path.
2. Add flatten E2E integration test coverage (delta to branch commit verification).
3. Prepare pure-gix flatten migration scaffold and test plan.

## FAQ

**Q: Should I test private functions?**
A: No. Test the public API contract. Private functions are implementation details.

**Q: How do I handle non-deterministic tests (e.g., timing)?**
A: Avoid them. Use mock clocks or inject time sources. For unavoidable timing tests, add retries:
```rust
let attempts = 3;
for attempt in 0..attempts {
    if test_passes() { return; }
    if attempt < attempts - 1 { std::thread::sleep(Duration::from_millis(100)); }
}
panic!("Test failed after {} retries", attempts);
```

**Q: How do I test async code?**
A: Use `#[tokio::test]` macro:
```rust
#[tokio::test]
async fn test_async_operation() {
    let result = async_fn().await;
    assert!(result.is_ok());
}
```

**Q: What about external dependencies?**
A: Mock them using `mockall` or similar:
```rust
#[cfg(test)]
mod tests {
    use mockall::mock;
    mock! {
        Git {}
        impl GitService for Git {
            fn read_blob(&self, oid: &str) -> Result<Vec<u8>>;
        }
    }
}
```

## References

- [Rust Testing Book](https://doc.rust-lang.org/book/ch11-00-testing.html)
- [Rust Test Driven Development](https://docs.rust-embedded.org/book/intro.html)
- [Criterion.rs](https://criterion.rs/) for benchmarking
- [Proptest](https://docs.rs/proptest/) for property-based testing

---

**Last updated:** 2026-04-12
