# Track 2 Chaos Validation Report

Date: 2026-04-16

## Objective

Validate that simgitd remains correct and recoverable under production-like multi-agent timing jitter and transport faults, with explicit SLO gates.

## Test method

Runner:

- tests/stress/swarm_runner.py

Phases:

1. Disjoint mixed-profile swarm
2. Hotspot mixed-profile swarm
3. Fault battery
4. SLO gate evaluation

Canonical command used:

```bash
source .venv/bin/activate
python3 tests/stress/swarm_runner.py \
  --socket /tmp/simgit-track2/control.sock \
  --disjoint-agents 20 \
  --hotspot-agents 20 \
  --profile-mix '{"fast_coder":10,"reasoning":5,"unstable":3,"overthinker":2}' \
  --fault-scenarios all \
  --report-out /tmp/swarm_track2_final_postfix.json
```

## Profile model

Synthetic profiles approximate realistic LLM behavior without coupling tests to provider latency variance.

Profiles used:

- fast_coder
- reasoning
- unstable
- overthinker

The profile model includes:

- Lognormal TTFT draws
- Exponential write duration
- Pareto stall events
- Bimodal payload sizes
- Abandon and double-commit probabilities

## Fault scenarios

- post_commit_pre_ack_socket_drop
- session_abandon_storm
- double_submit_idempotency

Each scenario validates terminal-state correctness and daemon survivability, not merely throughput.

## Final SLO definitions

- disjoint_success_rate_pct: threshold 100.0
- disjoint_commit_p95_ms: threshold 12000.0
- hotspot_commit_p95_ms: threshold 8500.0
- fault_pass_rate_pct: threshold 66.7
- abandon_follow_up_success_rate_pct: threshold 100.0

## Final clean-run outcomes

All gates passed (5/5):

- disjoint_success_rate_pct: actual 100.0, passed
- disjoint_commit_p95_ms: actual 7356, passed
- hotspot_commit_p95_ms: actual 6078, passed
- fault_pass_rate_pct: actual 100.0, passed
- abandon_follow_up_success_rate_pct: actual 100.0, passed

## Interpretation

1. Correctness under jitter
- Disjoint commits remained fully successful under mixed TTFT/write/stall distributions.

2. Contention behavior
- Hotspot latency remained within calibrated thresholds without correctness regressions.

3. Fault tolerance
- Transport interruption and abandon storms did not induce stuck pending states or session-table exhaustion.

4. Idempotency
- Double-submit behavior resolved cleanly without crash or duplicate corruption path.

## Known measurement nuance

Harness p95 values represent end-to-end transaction windows, including simulated client think time (TTFT and stalls), not only daemon-internal processing latency.

## Operational recommendations

1. Keep Track 2 runner as nightly regression gate.
2. Persist JSON report artifacts for trend analysis.
3. Alert on drift of p95 and fault pass rate before hard gate failures.
4. Correlate harness p95 with daemon-stage metrics for root-cause localization.
