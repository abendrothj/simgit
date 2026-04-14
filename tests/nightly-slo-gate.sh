#!/bin/bash

# Nightly SLO Gate for Mock Swarm Validation
# 
# Validates that disjoint-range and hotspot stress scenarios stay within defined SLOs.
# Runs on every commit via CI; can also be invoked locally for validation.
#
# Exit codes:
#   0: All SLOs passed
#   1: SLO violation detected
#   2: Infrastructure error (build, daemon, harness)

set -e  # Exit on any error during setup phase
trap 'cleanup' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
VENV_DIR="$ROOT_DIR/.venv"
STATE_DIR="/tmp/simgit-slo-$(date +%s)"
METRICS_PORT=9125

# SLO thresholds (from TESTING.md)
DISJOINT_P95_MAX_MS=8000
DISJOINT_P99_MAX_MS=8500
HOTSPOT_P95_MAX_MS=6500
HOTSPOT_P99_MAX_MS=7000

echo "===== NIGHTLY SLO GATE ====="
echo "State directory: $STATE_DIR"
echo "Timestamp: $(date)"
echo

# Cleanup function
cleanup() {
    local exit_code=$?
    echo "[cleanup] Terminating daemon..."
    pkill -x simgitd 2>/dev/null || true
    if [ "$exit_code" -eq 0 ]; then
        # Keep state on success for analysis
        echo "[cleanup] State directory: $STATE_DIR (keep for trends analysis)"
        ln -sf "$STATE_DIR" /tmp/simgit-slo-latest
    else
        echo "[cleanup] Removing state directory due to error..."
        rm -rf "$STATE_DIR"
    fi
    return "$exit_code"
}

# Step 1: Build release binary
echo "[step 1] Building simgitd release binary..."
cd "$ROOT_DIR"
if ! cargo build -p simgitd --release 2>&1 | grep -E "(Finished|error)" | tail -3; then
    echo "[error] Build failed"
    exit 2
fi
DAEMON_BIN="$ROOT_DIR/target/release/simgitd"
echo "[step 1] Build successful"
echo

# Step 2: Activate venv
echo "[step 2] Activating Python venv..."
if [ ! -f "$VENV_DIR/bin/activate" ]; then
    echo "[error] Venv not found at $VENV_DIR"
    exit 2
fi
source "$VENV_DIR/bin/activate"
echo "[step 2] Venv activated"
echo

# Step 3: Start daemon
echo "[step 3] Starting simgitd daemon..."
mkdir -p "$STATE_DIR"
if ! SIMGIT_STATE_DIR="$STATE_DIR" \
     SIMGIT_COMMIT_PEER_CAPTURE_CONCURRENCY=8 \
     SIMGIT_METRICS_ADDR="127.0.0.1:$METRICS_PORT" \
     "$DAEMON_BIN" --repo . &> "$STATE_DIR/daemon.log" &
then
    echo "[error] Failed to start daemon"
    cat "$STATE_DIR/daemon.log"
    exit 2
fi
DAEMON_PID=$!
echo "[step 3] Daemon started (PID: $DAEMON_PID)"

# Wait for daemon to be ready
echo "[step 3] Waiting for daemon to be ready..."
for i in {1..100}; do
    # Check if process is still running (macOS compatible)
    if kill -0 "$DAEMON_PID" 2>/dev/null && \
       curl -s "http://127.0.0.1:$METRICS_PORT/metrics" > /dev/null 2>&1; then
        echo "[step 3] Daemon is ready"
        break
    fi
    if [ $i -eq 100 ]; then
        echo "[error] Daemon failed to become ready within 10 seconds"
        exit 2
    fi
    sleep 0.1
done

# Capture baseline metrics
curl -s "http://127.0.0.1:$METRICS_PORT/metrics" > "$STATE_DIR/metrics_before.prom"
echo

# Step 4: Run disjoint-range stress
echo "[step 4a] Running disjoint-range stress (60 agents, 24 workers)..."
set +e
SIMGIT_SOCKET="$STATE_DIR/control.sock" \
python3 "$SCRIPT_DIR/stress/agent_harness.py" \
    --agents 60 --workers 24 --execution-mode phased \
    --stress-mode disjoint-range --commit-workers 24 \
    --report-out "$STATE_DIR/disjoint_60.json" 2>&1 | tail -5
DISJOINT_EXIT=$?
set -e

if [ ! -f "$STATE_DIR/disjoint_60.json" ]; then
    echo "[error] Disjoint-range harness failed"
    exit 2
fi
echo "[step 4a] Disjoint-range completed"
echo

# Step 5: Run hotspot stress
echo "[step 4b] Running hotspot stress (60 agents, 24 workers)..."
set +e
SIMGIT_SOCKET="$STATE_DIR/control.sock" \
python3 "$SCRIPT_DIR/stress/agent_harness.py" \
    --agents 60 --workers 24 --execution-mode phased \
    --stress-mode hotspot --commit-workers 24 \
    --report-out "$STATE_DIR/hotspot_60.json" 2>&1 | tail -5
HOTSPOT_EXIT=$?
set -e

if [ ! -f "$STATE_DIR/hotspot_60.json" ]; then
    echo "[error] Hotspot harness failed"
    exit 2
fi
echo "[step 4b] Hotspot completed"
echo

# Capture post-run metrics
curl -s "http://127.0.0.1:$METRICS_PORT/metrics" > "$STATE_DIR/metrics_after.prom"
echo "[step 5] Captured post-run metrics"
echo

# Step 6: Validate SLO results
echo "[step 6] Extracting and validating SLO results..."
echo

export STATE_DIR DISJOINT_P95_MAX_MS DISJOINT_P99_MAX_MS HOTSPOT_P95_MAX_MS HOTSPOT_P99_MAX_MS
python3 <<'EOFPARSE'
import json, os, sys

state_dir = os.environ['STATE_DIR']
d_p95_max = float(os.environ['DISJOINT_P95_MAX_MS'])
d_p99_max = float(os.environ['DISJOINT_P99_MAX_MS'])
h_p95_max = float(os.environ['HOTSPOT_P95_MAX_MS'])
h_p99_max = float(os.environ['HOTSPOT_P99_MAX_MS'])

exit_code = 0

# Load reports
with open(f"{state_dir}/disjoint_60.json") as f:
    disjoint = json.load(f)
with open(f"{state_dir}/hotspot_60.json") as f:
    hotspot = json.load(f)

# Validate disjoint-range
print("DISJOINT-RANGE RESULTS:")
print(f"  Success: {disjoint['successes']}/60", end="")
if disjoint['successes'] == 60:
    print(" ✓ PASS")
else:
    print(" ❌ FAIL")
    exit_code = 1

d_p95, d_p99 = disjoint['latency']['p95_ms'], disjoint['latency']['p99_ms']
print(f"  p95: {d_p95:.0f}ms (max: {d_p95_max}ms)", end="")
if d_p95 <= d_p95_max:
    print(" ✓ PASS")
else:
    print(" ❌ FAIL")
    exit_code = 1

print(f"  p99: {d_p99:.0f}ms (max: {d_p99_max}ms)", end="")
if d_p99 <= d_p99_max:
    print(" ✓ PASS")
else:
    print(" ❌ FAIL")
    exit_code = 1

print()
print("HOTSPOT RESULTS:")
print(f"  Failures: {hotspot['failures']}/60 (expected: lock_conflict only)")

fb = hotspot.get('failure_breakdown', {})
has_other = any(k != 'lock_conflict' for k in fb.keys())
if not has_other and hotspot['failures'] > 0:
    print(f"  Failure Types: lock_conflict={fb.get('lock_conflict', 0)} ✓ PASS")
else:
    if has_other:
        print(f"  Failure Types: {fb} ❌ FAIL (unexpected failure type)")
        exit_code = 1
    else:
        print(f"  Failure Types: none (unexpected for hotspot)")

h_p95, h_p99 = hotspot['latency']['p95_ms'], hotspot['latency']['p99_ms']
print(f"  p95: {h_p95:.0f}ms (max: {h_p95_max}ms)", end="")
if h_p95 <= h_p95_max:
    print(" ✓ PASS")
else:
    print(" ❌ FAIL")
    exit_code = 1

print(f"  p99: {h_p99:.0f}ms (max: {h_p99_max}ms)", end="")
if h_p99 <= h_p99_max:
    print(" ✓ PASS")
else:
    print(" ❌ FAIL")
    exit_code = 1

print()
print(f"OVERALL: {'✓ PASS' if exit_code == 0 else '❌ FAIL'}")
print(f"Results: {state_dir}")
sys.exit(exit_code)
EOFPARSE
