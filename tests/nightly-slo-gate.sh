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
REPORT_DIR="/tmp"

# SLO thresholds (from TESTING.md)
DISJOINT_SUCCESS_TARGET=100
DISJOINT_P95_MAX_MS=8000
DISJOINT_P99_MAX_MS=8500
DISJOINT_HIT_RATE_MIN=90

HOTSPOT_P95_MAX_MS=6500
HOTSPOT_P99_MAX_MS=7000
HOTSPOT_HIT_RATE_MIN=85

echo "===== NIGHTLY SLO GATE ====="
echo "State directory: $STATE_DIR"
echo "Timestamp: $(date)"
echo

# Cleanup function
cleanup() {
    local exit_code=$?
    echo "[cleanup] Terminating daemon..."
    pkill -x simgitd 2>/dev/null || true
    if [ "$exit_code" -ne 0 ] && [ "$exit_code" -ne 1 ]; then
        echo "[cleanup] Removing state directory due to error..."
        rm -rf "$STATE_DIR"
    else
        echo "[cleanup] Archiving results to $STATE_DIR (keep for trends analysis)"
    fi
    return "$exit_code"
}

# Step 1: Build release binary
echo "[step 1] Building simgitd release binary..."
cd "$ROOT_DIR"
if ! cargo build -p simgitd --release 2>&1 | tail -20; then
    echo "[error] Build failed"
    exit 2
fi
DAEMON_BIN="$ROOT_DIR/target/release/simgitd"
echo "[step 1] Build successful: $DAEMON_BIN"
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
    # Check if process is still running (macOS compatible: kill -0 returns 0 if process exists)
    if kill -0 "$DAEMON_PID" 2>/dev/null && \
       curl -s "http://127.0.0.1:$METRICS_PORT/metrics" > /dev/null 2>&1; then
        echo "[step 3] Daemon is ready"
        break
    fi
    if [ $i -eq 100 ]; then
        echo "[error] Daemon failed to become ready within 10 seconds"
        cat "$STATE_DIR/daemon.log"
        exit 2
    fi
    sleep 0.1
done

# Capture baseline metrics
echo "[step 3] Capturing baseline metrics..."
curl -s "http://127.0.0.1:$METRICS_PORT/metrics" > "$STATE_DIR/metrics_before.prom"
echo

# Step 4: Run disjoint-range stress
echo "[step 4a] Running disjoint-range stress (60 agents, 24 workers)..."
set +e  # Don't exit on harness exit code; we check it explicitly
SIMGIT_SOCKET="$STATE_DIR/control.sock" \
python3 "$SCRIPT_DIR/stress/agent_harness.py" \
    --agents 60 \
    --workers 24 \
    --execution-mode phased \
    --stress-mode disjoint-range \
    --commit-workers 24 \
    --report-out "$STATE_DIR/disjoint_60.json" \
    2>&1 | tail -30
DISJOINT_EXIT=$?
set -e

if [ ! -f "$STATE_DIR/disjoint_60.json" ]; then
    echo "[error] Disjoint-range harness failed (exit: $DISJOINT_EXIT)"
    exit 2
fi
echo "[step 4a] Disjoint-range completed"
echo

# Step 5: Run hotspot stress
echo "[step 4b] Running hotspot stress (60 agents, 24 workers)..."
set +e
SIMGIT_SOCKET="$STATE_DIR/control.sock" \
python3 "$SCRIPT_DIR/stress/agent_harness.py" \
    --agents 60 \
    --workers 24 \
    --execution-mode phased \
    --stress-mode hotspot \
    --commit-workers 24 \
    --report-out "$STATE_DIR/hotspot_60.json" \
    2>&1 | tail -30
HOTSPOT_EXIT=$?
set -e

if [ ! -f "$STATE_DIR/hotspot_60.json" ]; then
    echo "[error] Hotspot harness failed (exit: $HOTSPOT_EXIT)"
    exit 2
fi
echo "[step 4b] Hotspot completed"
echo

# Step 6: Capture post-run metrics
echo "[step 5] Capturing post-run metrics..."
curl -s "http://127.0.0.1:$METRICS_PORT/metrics" > "$STATE_DIR/metrics_after.prom"
echo

# Step 7: Extract and validate results
echo "[step 6] Extracting and validating SLO results..."
echo

# Parse disjoint-range results
disjoint_report=$(python3 - <<'EOF'
import json, sys
try:
    with open(sys.argv[1]) as f:
        data = json.load(f)
    print(f"{data['successes']},{data['failures']},{data['latency']['p95_ms']:.0f},{data['latency']['p99_ms']:.0f}")
except Exception as e:
    print(f"ERROR:{e}", file=sys.stderr)
    sys.exit(1)
EOF
"$STATE_DIR/disjoint_60.json")

if [[ $disjoint_report == ERROR* ]]; then
    echo "[error] Failed to parse disjoint report: $disjoint_report"
    exit 2
fi

read -r disjoint_success disjoint_failures disjoint_p95 disjoint_p99 <<< "${disjoint_report/,/ }"

# Parse hotspot results
hotspot_report=$(python3 - <<'EOF'
import json, sys
try:
    with open(sys.argv[1]) as f:
        data = json.load(f)
    fb = json.dumps(data.get('failure_breakdown', {}))
    print(f"{data['successes']},{data['failures']},{data['latency']['p95_ms']:.0f},{data['latency']['p99_ms']:.0f},{fb}")
except Exception as e:
    print(f"ERROR:{e}", file=sys.stderr)
    sys.exit(1)
EOF
"$STATE_DIR/hotspot_60.json")

if [[ $hotspot_report == ERROR* ]]; then
    echo "[error] Failed to parse hotspot report: $hotspot_report"
    exit 2
fi

read -r hotspot_success hotspot_failures hotspot_p95 hotspot_p99 hotspot_fb <<< "${hotspot_report/,/ }"

# Parse peer-capture metrics
disjoint_hit_rate=$(python3 - <<'EOF'
import re, sys
def extract_peer_capture(path):
    hit, miss = 0, 0
    try:
        with open(path) as f:
            pat = re.compile(r'^simgit_peer_capture_skip_total\{result="([^"]+)"\}\s+([0-9]+)$')
            for line in f:
                m = pat.match(line.strip())
                if m:
                    if m.group(1) == 'hit':
                        hit += int(m.group(2))
                    elif m.group(1) == 'miss':
                        miss += int(m.group(2))
    except:
        return -1
    if hit + miss == 0:
        return -1
    return 100.0 * hit / (hit + miss)

before = extract_peer_capture(sys.argv[1])
after = extract_peer_capture(sys.argv[2])

if before < 0 or after < 0:
    print("UNKNOWN")
    sys.exit(0)

delta_before_dict = {}
delta_after_dict = {}
try:
    pat = re.compile(r'^simgit_peer_capture_skip_total\{result="([^"]+)"\}\s+([0-9]+)$')
    with open(sys.argv[1]) as f:
        for line in f:
            m = pat.match(line.strip())
            if m:
                delta_before_dict[m.group(1)] = int(m.group(2))
    with open(sys.argv[2]) as f:
        for line in f:
            m = pat.match(line.strip())
            if m:
                delta_after_dict[m.group(1)] = int(m.group(2))
except:
    pass

hit_delta = delta_after_dict.get('hit', 0) - delta_before_dict.get('hit', 0)
miss_delta = delta_after_dict.get('miss', 0) - delta_before_dict.get('miss', 0)
total_delta = hit_delta + miss_delta
if total_delta == 0:
    print("UNKNOWN")
else:
    hit_rate_delta = 100.0 * hit_delta / total_delta
    print(f"{hit_rate_delta:.1f}")
EOF
"$STATE_DIR/metrics_before.prom" "$STATE_DIR/metrics_after.prom")

# Validate SLOs
disjoint_pass=true
hotspot_pass=true
exit_code=0

echo "DISJOINT-RANGE RESULTS:"
echo "  Success: $disjoint_success/60 (threshold: ${DISJOINT_SUCCESS_TARGET}%)"
if [ "$disjoint_success" -ne 60 ]; then
    echo "    ❌ FAIL: Expected 60/60 successes"
    disjoint_pass=false
    exit_code=1
else
    echo "    ✓ PASS"
fi

echo "  p95: ${disjoint_p95}ms (threshold: ${DISJOINT_P95_MAX_MS}ms)"
if (( $(echo "$disjoint_p95 > $DISJOINT_P95_MAX_MS" | bc -l) )); then
    echo "    ❌ FAIL: Exceeded threshold"
    disjoint_pass=false
    exit_code=1
else
    echo "    ✓ PASS"
fi

echo "  p99: ${disjoint_p99}ms (threshold: ${DISJOINT_P99_MAX_MS}ms)"
if (( $(echo "$disjoint_p99 > $DISJOINT_P99_MAX_MS" | bc -l) )); then
    echo "    ❌ FAIL: Exceeded threshold"
    disjoint_pass=false
    exit_code=1
else
    echo "    ✓ PASS"
fi

if [[ "$disjoint_hit_rate" == "UNKNOWN" ]]; then
    echo "  Peer-Capture Hit Rate: UNKNOWN (metrics unavailable)"
else
    echo "  Peer-Capture Hit Rate: ${disjoint_hit_rate}% (threshold: ${DISJOINT_HIT_RATE_MIN}%)"
    if (( $(echo "$disjoint_hit_rate < $DISJOINT_HIT_RATE_MIN" | bc -l) )); then
        echo "    ❌ FAIL: Below threshold"
        disjoint_pass=false
        exit_code=1
    else
        echo "    ✓ PASS"
    fi
fi

echo
echo "HOTSPOT RESULTS:"
echo "  Success: $hotspot_success/60 (expected: contention with lock_conflict only)"
echo "  Failures: $hotspot_failures"

# Parse failure breakdown
has_non_lock_conflict=false
lock_conflict_count=0

if [[ "$hotspot_fb" != "{}" ]]; then
    fb_check=$(python3 - <<'EOF'
import json, sys
try:
    fb = json.loads(sys.argv[1])
    has_other = any(k != 'lock_conflict' for k in fb.keys())
    lock_count = fb.get('lock_conflict', 0)
    print(f"{has_other},{lock_count}")
except:
    print("PARSE_ERROR,0")
EOF
"$hotspot_fb")
    
    read -r has_other lock_count <<< "${fb_check/,/ }"
    lock_conflict_count=$lock_count
    
    if [[ $has_other == "true" ]]; then
        has_non_lock_conflict=true
    fi
fi

if $has_non_lock_conflict; then
    echo "  Failure Taxonomy:"
    echo "$hotspot_fb" | python3 -m json.tool | sed 's/^/    /'
    echo "    ❌ FAIL: Non-lock_conflict failures detected"
    hotspot_pass=false
    exit_code=1
else
    echo "  Failure Taxonomy: lock_conflict=$lock_conflict_count (PASS, zero other types)"
    echo "    ✓ PASS"
fi

echo "  p95: ${hotspot_p95}ms (threshold: ${HOTSPOT_P95_MAX_MS}ms)"
if (( $(echo "$hotspot_p95 > $HOTSPOT_P95_MAX_MS" | bc -l) )); then
    echo "    ❌ FAIL: Exceeded threshold"
    hotspot_pass=false
    exit_code=1
else
    echo "    ✓ PASS"
fi

echo "  p99: ${hotspot_p99}ms (threshold: ${HOTSPOT_P99_MAX_MS}ms)"
if (( $(echo "$hotspot_p99 > $HOTSPOT_P99_MAX_MS" | bc -l) )); then
    echo "    ❌ FAIL: Exceeded threshold"
    hotspot_pass=false
    exit_code=1
else
    echo "    ✓ PASS"
fi

echo
if $disjoint_pass && $hotspot_pass; then
    echo "OVERALL: ✓ PASS"
    echo "All SLOs satisfied. Ready for deployment."
    exit_code=0
else
    echo "OVERALL: ❌ FAIL"
    echo "SLO violations detected. Block deployment."
    exit_code=1
fi

echo
echo "Results archived to:"
echo "  Disjoint report: $STATE_DIR/disjoint_60.json"
echo "  Hotspot report: $STATE_DIR/hotspot_60.json"
echo "  Metrics before: $STATE_DIR/metrics_before.prom"
echo "  Metrics after: $STATE_DIR/metrics_after.prom"
echo "  Daemon log: $STATE_DIR/daemon.log"

# Create summary file for CI/trend tracking
cat > "$STATE_DIR/slo-summary.txt" <<SUMMARY
timestamp: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
disjoint_success: $disjoint_success/60
disjoint_p95_ms: $disjoint_p95
disjoint_p99_ms: $disjoint_p99
disjoint_hit_rate: $disjoint_hit_rate
hotspot_p95_ms: $hotspot_p95
hotspot_p99_ms: $hotspot_p99
hotspot_failures: $hotspot_failures
hotspot_lock_conflict: $lock_conflict_count
overall_result: $([ "$exit_code" -eq 0 ] && echo "PASS" || echo "FAIL")
SUMMARY

echo "Summary: $STATE_DIR/slo-summary.txt"

# Create symlink to latest for easy downstream access
ln -sf "$STATE_DIR" /tmp/simgit-slo-latest

exit "$exit_code"
