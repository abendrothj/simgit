# Testing Guide

This document defines the authoritative test strategy for simgit across Rust crates, daemon integration, and multi-agent stress workflows.

## Test taxonomy

simgit uses four test layers with increasing system realism.

1. Unit tests
- Scope: isolated module invariants
- Goal: deterministic correctness for core logic

2. Integration tests
- Scope: crate boundaries and daemon-facing flows
- Goal: verify subsystem composition (sessions, locks, commit semantics)

3. Deterministic stress tests
- Scope: fixed workload profiles and reproducible concurrency
- Goal: establish latency/correctness baselines and prevent regressions

4. Chaos/fault-injection tests
- Scope: transport faults, abandon storms, duplicate submit behavior
- Goal: validate resilience and idempotent commit behavior

## Prerequisites

- Rust stable toolchain
- Python virtual environment with simgit bindings installed
- A disposable repository for destructive commit testing

## Rust test execution

### Run full workspace tests

```bash
cargo test --workspace
```

### Run daemon crate tests only

```bash
cargo test -p simgitd
```

### Run targeted integration tests

```bash
cargo test --test borrow_checker_tests
```

### Debug mode with logs

```bash
RUST_LOG=debug cargo test -- --nocapture --test-threads=1
```

## Python and harness setup

Build/install Python bindings into the active venv:

```bash
source .venv/bin/activate
maturin develop -m simgit-py/Cargo.toml
```

If bindings cannot be imported, stress scripts will fail early.

## Control-plane stress harness (used by CI)

Script: `tests/stress/agent_harness.py`

This is the harness the nightly SLO gate (`tests/nightly-slo-gate.sh`) runs. It stress-tests session lifecycle and commit scheduling directly via Python bindings — no LLM or external API required.

Two stress modes:

- `disjoint-range` — agents write non-overlapping byte ranges; expects zero conflicts
- `hotspot` — all agents write the same file; exercises conflict detection and scheduler contention

```bash
source .venv/bin/activate

# Disjoint (CI default: 60 agents)
python3 tests/stress/agent_harness.py \
  --agents 20 --workers 8 \
  --stress-mode disjoint-range \
  --report-out /tmp/simgit-disjoint.json

# Hotspot
python3 tests/stress/agent_harness.py \
  --agents 20 --workers 8 \
  --stress-mode hotspot \
  --report-out /tmp/simgit-hotspot.json
```

## Real-agent harness (file writes + optional LLM)

Script: `tests/real_agent_harness.py`

Writes real files inside each session mount before committing, making latency and conflict numbers reflect actual VFS behavior. Can run fully deterministic (no API key) or with a live LLM backend for calibration.

```bash
source .venv/bin/activate

# Deterministic (no API key needed)
python3 tests/real_agent_harness.py \
  --agents 50 \
  --task-profile disjoint-files \
  --report-out /tmp/simgit-real-disjoint.json

# Hotspot profile
python3 tests/real_agent_harness.py \
  --agents 50 \
  --task-profile hotspot-file \
  --commit-barrier \
  --report-out /tmp/simgit-real-hotspot.json
```

Set `SIMGIT_LLM_API_KEY` to enable real LLM-backed agents (see `.env.example`).

## Track 2 chaos validation

Primary orchestrator: tests/stress/swarm_runner.py

This runner executes:

- Phase 1: profile-mixed disjoint wave
- Phase 2: profile-mixed hotspot wave
- Phase 3: fault battery (socket drop, abandon storm, double submit)
- Phase 4: SLO gate evaluation

### Canonical invocation

```bash
source .venv/bin/activate
python3 tests/stress/swarm_runner.py \
  --socket /tmp/simgit-track2/control.sock \
  --disjoint-agents 20 \
  --hotspot-agents 20 \
  --profile-mix '{"fast_coder":10,"reasoning":5,"unstable":3,"overthinker":2}' \
  --fault-scenarios all \
  --report-out /tmp/swarm_track2_final.json
```

### Exit semantics

- Exit code 0: all configured SLO gates passed
- Non-zero exit code: at least one gate failed or execution error occurred

### Final validated gate outcomes

Latest clean run achieved 5/5 passing:

- disjoint_success_rate_pct: 100.0 (threshold 100.0)
- disjoint_commit_p95_ms: 7356 (threshold 12000.0)
- hotspot_commit_p95_ms: 6078 (threshold 8500.0)
- fault_pass_rate_pct: 100.0 (threshold 66.7)
- abandon_follow_up_success_rate_pct: 100.0 (threshold 100.0)

See docs/track2_chaos_validation.md for interpretation and constraints.

## Fault scenario contract

Script: tests/stress/fault_injector.py

Scenarios:

- post_commit_pre_ack_socket_drop
- session_abandon_storm
- double_submit_idempotency

Pass criteria:

- No stuck pending commit states
- No daemon crash
- No leaked-session saturation after abandon storm
- Double-submit behavior resolves to success or clean conflict, not corruption

## Metrics to monitor during stress

Prometheus endpoint should be scraped during load runs.

High-value metrics:

- simgit_session_commit_stage_duration_seconds
- simgit_session_commit_conflicts_total
- simgit_session_commit_conflict_paths
- simgit_session_commit_conflict_peers
- simgit_active_sessions
- simgit_active_locks

## Reproducibility guidance

- Pin profile mix and agent counts in CI jobs.
- Capture JSON reports for every stress run.
- Keep daemon state directory ephemeral per run.
- Use explicit socket paths in scripts and env variables.

## CI recommendations

Minimum CI gate set:

1. cargo test --workspace
2. smoke run of tests/real_agent_harness.py with reduced agents
3. nightly Track 2 swarm_runner execution with artifact upload
4. threshold regression check against prior p95 baselines

## Failure triage checklist

1. Confirm daemon process and control socket exist.
2. Verify SIMGIT_SOCKET and runner --socket point to the same path.
3. Validate venv contains current simgit bindings.
4. Inspect stress JSON report first, daemon logs second.
5. Differentiate transport-level failure from semantic commit failure.

## Common pitfalls

- Running stress against non-disposable repos.
- Mixing stale daemon process with new harness code.
- Comparing harness-level end-to-end latency to internal daemon-stage latency without context.
- Retrying commit blindly without request-id aware status polling.
