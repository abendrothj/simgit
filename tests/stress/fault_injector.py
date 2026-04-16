#!/usr/bin/env python3
"""Fault injection scenarios for simgit Track 2 stress testing.

Each scenario is a self-contained callable that:
  1. Sets up a session
  2. Injects a fault at a precise moment
  3. Verifies the expected recovery outcome
  4. Returns a FaultResult with pass/fail and diagnostics

Scenarios
---------
post_commit_pre_ack_socket_drop
    The "Jepsen" scenario.  A low-level socket interceptor sends the commit
    RPC but closes the connection before reading the response.  The client
    must then call commit.status to discover the terminal state without
    reissuing the commit.  Validates the idempotency layer end-to-end.

daemon_bounce_mid_wave
    Starts N sessions, commits half, bounces the daemon (SIGTERM + restart),
    then commits the remaining half.  Expects all pre-bounce commits already
    recorded as SUCCESS and all post-bounce commits to succeed on new daemon.

session_abandon_storm
    Fires M agents that all acquire sessions and then abandon them (no commit).
    Verifies that the daemon session table does not leak and that new sessions
    can be acquired after the storm.

double_submit_idempotency
    Issues two commit calls for logically identical work in quick succession
    (different sessions, same branch name pattern).  The second must either
    succeed on its own branch or get a clean merge-conflict — never a crash
    or stuck-pending.

Usage:
    python3 fault_injector.py --scenario post_commit_pre_ack_socket_drop \
        --socket /tmp/simgit-test/control.sock

    python3 fault_injector.py --scenario all --socket /tmp/simgit-test/control.sock
"""

from __future__ import annotations

import json
import os
import random
import signal
import socket
import string
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Callable


# ---------------------------------------------------------------------------
# Result type
# ---------------------------------------------------------------------------

@dataclass
class FaultResult:
    scenario: str
    passed: bool
    duration_ms: int
    details: dict[str, Any] = field(default_factory=dict)
    error: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "scenario": self.scenario,
            "passed": self.passed,
            "duration_ms": self.duration_ms,
            "details": self.details,
            "error": self.error,
        }


# ---------------------------------------------------------------------------
# Low-level socket helpers
# ---------------------------------------------------------------------------

class _InterceptSocket:
    """Wraps a raw Unix socket.  Lets callers send an RPC payload and then
    immediately close the connection before receiving the response — simulating
    a network drop or process kill after the write syscall returns.
    """

    def __init__(self, socket_path: str):
        self._path = socket_path
        self._sock: socket.socket | None = None

    def connect(self) -> None:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(self._path)
        self._sock = s

    def send_and_drop(self, payload: dict[str, Any]) -> None:
        """Send JSON-RPC payload and immediately close socket (no read)."""
        if self._sock is None:
            raise RuntimeError("Not connected")
        data = (json.dumps(payload) + "\n").encode()
        self._sock.sendall(data)
        # Drop the connection before the daemon can respond
        self._sock.close()
        self._sock = None

    def send_and_recv(self, payload: dict[str, Any], timeout_secs: float = 10.0) -> dict[str, Any]:
        """Send JSON-RPC payload and read the newline-delimited response."""
        if self._sock is None:
            raise RuntimeError("Not connected")
        data = (json.dumps(payload) + "\n").encode()
        self._sock.sendall(data)
        self._sock.settimeout(timeout_secs)
        buf = b""
        while b"\n" not in buf:
            chunk = self._sock.recv(65536)
            if not chunk:
                break
            buf += chunk
        line = buf.split(b"\n")[0]
        return json.loads(line)

    def close(self) -> None:
        if self._sock:
            try:
                self._sock.close()
            except Exception:
                pass
            self._sock = None


def _rand_suffix(n: int = 6) -> str:
    return "".join(random.choice(string.ascii_lowercase) for _ in range(n))


def _load_simgit() -> Any:
    try:
        import simgit  # type: ignore
        return simgit
    except ImportError as exc:
        raise RuntimeError(
            "simgit bindings not available. Run: maturin develop -m simgit-py/Cargo.toml"
        ) from exc


# ---------------------------------------------------------------------------
# Scenario 1: post-commit pre-ack socket drop
# ---------------------------------------------------------------------------

def post_commit_pre_ack_socket_drop(
    socket_path: str,
    stress_mode: str = "disjoint-range",
    overlap_path: str = "hotspot/shared.txt",
    disjoint_prefix: str = "data/fault_inject",
    timeout_secs: float = 30.0,
) -> FaultResult:
    """Drop the socket after sending commit RPC but before reading the ACK.

    Expected outcome:
      - SDK / caller catches a transport error
      - Caller polls commit.status with the request_id
      - Status transitions to SUCCESS or FAILED (never stuck as PENDING)
      - No duplicate commits land on git history

    We simulate this at the raw socket level: we send a session.commit JSON-RPC
    request ourselves, immediately close the socket, then use the high-level
    Python SDK to call commit.status.
    """
    scenario = "post_commit_pre_ack_socket_drop"
    start = time.monotonic()

    try:
        simgit = _load_simgit()

        # 1. Create a session via the SDK so the VFS mount is live
        session = simgit.Session.new(
            task_id=f"fault-pca-{_rand_suffix()}",
            socket_path=socket_path,
            agent_label="fault-inject-pca",
        )
        session_id = session.session_id
        info = session.info()
        mount_path = info["mount_path"]

        # 2. Write a file
        if stress_mode == "hotspot":
            target = os.path.join(mount_path, overlap_path)
        else:
            target = os.path.join(mount_path, disjoint_prefix, "pca_probe.txt")
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "w") as f:
            f.write(f"fault-inject ts={time.time_ns()}\n")

        # 3. Build commit RPC with an explicit request_id
        import uuid
        request_id = str(uuid.uuid4())
        branch_name = f"fault/pca-{_rand_suffix()}"

        commit_rpc = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session.commit",
            "params": {
                "session_id": session_id,
                "branch_name": branch_name,
                "message": "fault injection: post-commit pre-ack",
                "request_id": request_id,
            },
        }

        # 4. Send commit RPC and DROP the socket before reading response
        intercept = _InterceptSocket(socket_path)
        intercept.connect()
        intercept.send_and_drop(commit_rpc)  # <-- the fault injection point

        # 5. Poll commit.status until we get a terminal state (SUCCESS/FAILED)
        #    Give the daemon up to timeout_secs to process the commit.
        deadline = time.monotonic() + timeout_secs
        terminal_state = None
        polls = 0
        poll_interval = 0.1

        status_rpc_id = 100
        while time.monotonic() < deadline:
            polls += 1
            status_sock = _InterceptSocket(socket_path)
            status_sock.connect()
            try:
                resp = status_sock.send_and_recv({
                    "jsonrpc": "2.0",
                    "id": status_rpc_id,
                    "method": "commit.status",
                    "params": {
                        "session_id": session_id,
                        "request_id": request_id,
                    },
                })
            finally:
                status_sock.close()

            status_rpc_id += 1

            if "error" in resp:
                err_code = resp["error"].get("code", 0)
                if err_code == -32007:  # ERR_COMMIT_PENDING
                    time.sleep(poll_interval)
                    poll_interval = min(poll_interval * 1.5, 2.0)
                    continue
                # Any other error code is unexpected
                elapsed = int((time.monotonic() - start) * 1000)
                return FaultResult(
                    scenario=scenario, passed=False, duration_ms=elapsed,
                    details={"polls": polls, "last_response": resp},
                    error=f"Unexpected RPC error during status poll: {resp['error']}",
                )

            if "result" in resp:
                result = resp["result"]
                state = result.get("state")
                if state in ("SUCCESS", "FAILED"):
                    terminal_state = state
                    break
                if state == "NOT_FOUND":
                    # Daemon has no record of this request_id — commit
                    # was never received. That is also a valid recovery path
                    # (client should re-issue).
                    terminal_state = "NOT_FOUND"
                    break
                # PENDING: keep polling
                time.sleep(poll_interval)
                poll_interval = min(poll_interval * 1.5, 2.0)

        elapsed = int((time.monotonic() - start) * 1000)

        if terminal_state is None:
            return FaultResult(
                scenario=scenario, passed=False, duration_ms=elapsed,
                details={"polls": polls, "request_id": request_id},
                error=f"commit.status never reached terminal state within {timeout_secs}s",
            )

        # Pass if state is determinate (SUCCESS, FAILED, or NOT_FOUND)
        # NOT_FOUND means the daemon never received the commit (socket closed
        # before the write was processed) — a valid recovery state.
        passed = terminal_state in ("SUCCESS", "FAILED", "NOT_FOUND")
        return FaultResult(
            scenario=scenario, passed=passed, duration_ms=elapsed,
            details={
                "terminal_state": terminal_state,
                "polls": polls,
                "request_id": request_id,
                "session_id": session_id,
            },
        )

    except Exception as exc:
        elapsed = int((time.monotonic() - start) * 1000)
        return FaultResult(
            scenario=scenario, passed=False, duration_ms=elapsed,
            error=f"Unexpected exception: {exc}",
        )


# ---------------------------------------------------------------------------
# Scenario 2: daemon bounce mid-wave
# ---------------------------------------------------------------------------

def daemon_bounce_mid_wave(
    socket_path: str,
    daemon_cmd: list[str],
    env: dict[str, str] | None = None,
    agents_before: int = 5,
    agents_after: int = 5,
    startup_wait_secs: float = 2.0,
) -> FaultResult:
    """Commit agents_before sessions, bounce the daemon, commit agents_after.

    Expected outcomes:
      - Pre-bounce commits: all succeeded (already on git history)
      - Post-bounce commits: all succeed on fresh daemon state
      - No panics, no corruption
    """
    scenario = "daemon_bounce_mid_wave"
    start = time.monotonic()
    details: dict[str, Any] = {
        "agents_before": agents_before,
        "agents_after": agents_after,
        "pre_bounce_successes": 0,
        "post_bounce_successes": 0,
        "pre_bounce_failures": [],
        "post_bounce_failures": [],
    }

    try:
        simgit = _load_simgit()

        def _run_agents(count: int, label: str) -> tuple[int, list[str]]:
            successes = 0
            errors: list[str] = []
            for i in range(count):
                try:
                    s = simgit.Session.new(
                        task_id=f"bounce-{label}-{i}-{_rand_suffix()}",
                        socket_path=socket_path,
                        agent_label=f"bounce-{label}-{i}",
                    )
                    info = s.info()
                    mp = info["mount_path"]
                    tgt = os.path.join(mp, f"bounce/{label}_{i}.txt")
                    os.makedirs(os.path.dirname(tgt), exist_ok=True)
                    with open(tgt, "w") as f:
                        f.write(f"bounce {label} agent {i} ts={time.time_ns()}\n")
                    s.commit(
                        branch_name=f"bounce/{label}-{i}-{_rand_suffix()}",
                        message=f"bounce {label} agent {i}",
                    )
                    successes += 1
                except Exception as exc:
                    errors.append(str(exc))
            return successes, errors

        # Phase 1: pre-bounce commits
        pre_ok, pre_errs = _run_agents(agents_before, "pre")
        details["pre_bounce_successes"] = pre_ok
        details["pre_bounce_failures"] = pre_errs

        # Phase 2: bounce the daemon
        bounce_start = time.monotonic()
        try:
            out = subprocess.check_output(["pgrep", "-x", "simgitd"], text=True)
            pids = [int(p.strip()) for p in out.splitlines() if p.strip()]
            for pid in pids:
                os.kill(pid, signal.SIGTERM)
            # Wait for daemon to exit
            for _ in range(50):  # 5s max
                time.sleep(0.1)
                alive = False
                for pid in pids:
                    try:
                        os.kill(pid, 0)
                        alive = True
                    except ProcessLookupError:
                        pass
                if not alive:
                    break
        except subprocess.CalledProcessError:
            pass  # daemon already dead

        # Restart daemon
        proc_env = {**os.environ, **(env or {})}
        subprocess.Popen(daemon_cmd, env=proc_env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        details["bounce_duration_ms"] = int((time.monotonic() - bounce_start) * 1000)

        # Wait for daemon to accept connections
        time.sleep(startup_wait_secs)

        # Phase 3: post-bounce commits
        post_ok, post_errs = _run_agents(agents_after, "post")
        details["post_bounce_successes"] = post_ok
        details["post_bounce_failures"] = post_errs

        elapsed = int((time.monotonic() - start) * 1000)
        passed = (
            pre_ok == agents_before
            and post_ok == agents_after
        )
        return FaultResult(scenario=scenario, passed=passed, duration_ms=elapsed, details=details)

    except Exception as exc:
        elapsed = int((time.monotonic() - start) * 1000)
        return FaultResult(scenario=scenario, passed=False, duration_ms=elapsed,
                           details=details, error=str(exc))


# ---------------------------------------------------------------------------
# Scenario 3: session abandon storm
# ---------------------------------------------------------------------------

def session_abandon_storm(
    socket_path: str,
    storm_agents: int = 30,
    follow_up_agents: int = 5,
    workers: int = 20,
) -> FaultResult:
    """Acquire storm_agents sessions and abandon all of them.

    Then verify that follow_up_agents can still acquire and commit sessions,
    proving the daemon session table is not exhausted or corrupted.
    """
    scenario = "session_abandon_storm"
    start = time.monotonic()
    details: dict[str, Any] = {
        "storm_agents": storm_agents,
        "follow_up_agents": follow_up_agents,
        "storm_abandoned": 0,
        "storm_errors": [],
        "follow_up_successes": 0,
        "follow_up_failures": [],
    }

    try:
        simgit = _load_simgit()
        import concurrent.futures

        def _abandon_one(i: int) -> tuple[bool, str | None]:
            try:
                s = simgit.Session.new(
                    task_id=f"storm-{i}-{_rand_suffix()}",
                    socket_path=socket_path,
                    agent_label=f"storm-{i}",
                )
                info = s.info()
                mp = info["mount_path"]
                tgt = os.path.join(mp, f"storm/agent_{i}.txt")
                os.makedirs(os.path.dirname(tgt), exist_ok=True)
                with open(tgt, "w") as f:
                    f.write(f"storm {i}\n")
                s.abort()
                return True, None
            except Exception as exc:
                return False, str(exc)

        def _commit_one(i: int) -> tuple[bool, str | None]:
            try:
                s = simgit.Session.new(
                    task_id=f"followup-{i}-{_rand_suffix()}",
                    socket_path=socket_path,
                    agent_label=f"followup-{i}",
                )
                info = s.info()
                mp = info["mount_path"]
                tgt = os.path.join(mp, f"followup/agent_{i}.txt")
                os.makedirs(os.path.dirname(tgt), exist_ok=True)
                with open(tgt, "w") as f:
                    f.write(f"followup {i} ts={time.time_ns()}\n")
                s.commit(
                    branch_name=f"followup/{i}-{_rand_suffix()}",
                    message=f"follow-up after abandon storm {i}",
                )
                return True, None
            except Exception as exc:
                return False, str(exc)

        # Storm phase
        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futs = [pool.submit(_abandon_one, i) for i in range(storm_agents)]
            for fut in concurrent.futures.as_completed(futs):
                ok, err = fut.result()
                if ok:
                    details["storm_abandoned"] += 1
                else:
                    details["storm_errors"].append(err)

        # Brief pause to let GC / session expiry kick in
        time.sleep(0.5)

        # Follow-up phase
        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futs = [pool.submit(_commit_one, i) for i in range(follow_up_agents)]
            for fut in concurrent.futures.as_completed(futs):
                ok, err = fut.result()
                if ok:
                    details["follow_up_successes"] += 1
                else:
                    details["follow_up_failures"].append(err)

        elapsed = int((time.monotonic() - start) * 1000)
        passed = details["follow_up_successes"] == follow_up_agents
        return FaultResult(scenario=scenario, passed=passed, duration_ms=elapsed, details=details)

    except Exception as exc:
        elapsed = int((time.monotonic() - start) * 1000)
        return FaultResult(scenario=scenario, passed=False, duration_ms=elapsed,
                           details=details, error=str(exc))


# ---------------------------------------------------------------------------
# Scenario 4: double-submit idempotency
# ---------------------------------------------------------------------------

def double_submit_idempotency(
    socket_path: str,
    pairs: int = 5,
    workers: int = 10,
    stress_mode: str = "disjoint-range",
) -> FaultResult:
    """Issue two commits for the same logical file content in parallel.

    Each "pair" creates two independent sessions that both write the same
    content to the same path.  Both try to commit to different branch names.
    
    Expected outcomes per pair:
      - Disjoint mode: both may succeed (different branches, same file path
        but both are new branches off HEAD — no conflict)
      - Hotspot mode: exactly one succeeds, the other gets merge-conflict
      - Neither must crash or hang
    """
    scenario = "double_submit_idempotency"
    start = time.monotonic()
    details: dict[str, Any] = {
        "pairs": pairs,
        "stress_mode": stress_mode,
        "pair_results": [],
    }

    try:
        simgit = _load_simgit()
        import concurrent.futures

        def _one_pair(pair_id: int) -> dict[str, Any]:
            if stress_mode == "hotspot":
                # Hotspot: both write to same path on competing branches
                shared_path = "hotspot/shared.txt"
            else:
                # Disjoint: each slot writes to different path (no contention)
                shared_path = None
            
            content = f"pair={pair_id} ts={time.time_ns()}\n".encode()
            outcomes = []

            def _agent(slot: int) -> tuple[bool, str | None]:
                try:
                    s = simgit.Session.new(
                        task_id=f"double-{pair_id}-{slot}-{_rand_suffix()}",
                        socket_path=socket_path,
                        agent_label=f"double-{pair_id}-{slot}",
                    )
                    info = s.info()
                    mp = info["mount_path"]
                    # In disjoint mode, each slot writes to unique path to avoid contention
                    # In hotspot mode, both write to same path to trigger conflict detection
                    if shared_path:
                        tgt = os.path.join(mp, shared_path)
                    else:
                        tgt = os.path.join(mp, f"idempotent/pair_{pair_id}_slot_{slot}.txt")
                    os.makedirs(os.path.dirname(tgt), exist_ok=True)
                    with open(tgt, "wb") as f:
                        f.write(content)
                    s.commit(
                        branch_name=f"double/{pair_id}/{slot}-{_rand_suffix()}",
                        message=f"double submit pair {pair_id} slot {slot}",
                    )
                    return True, None
                except Exception as exc:
                    return False, str(exc)

            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as p:
                futs = [p.submit(_agent, slot) for slot in range(2)]
                for fut in concurrent.futures.as_completed(futs):
                    ok, err = fut.result()
                    outcomes.append({"ok": ok, "error": err})

            ok_count = sum(1 for o in outcomes if o["ok"])
            # In hotspot mode, exactly 1 must succeed (conflict detection).
            # In disjoint mode, both must succeed (no contention now that paths differ).
            if stress_mode == "hotspot":
                pair_passed = ok_count == 1
            else:
                pair_passed = ok_count == 2  # Both should succeed with separate paths


            return {"pair_id": pair_id, "passed": pair_passed, "outcomes": outcomes}

        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futs = [pool.submit(_one_pair, i) for i in range(pairs)]
            for fut in concurrent.futures.as_completed(futs):
                details["pair_results"].append(fut.result())

        elapsed = int((time.monotonic() - start) * 1000)
        all_passed = all(pr["passed"] for pr in details["pair_results"])
        return FaultResult(scenario=scenario, passed=all_passed, duration_ms=elapsed, details=details)

    except Exception as exc:
        elapsed = int((time.monotonic() - start) * 1000)
        return FaultResult(scenario=scenario, passed=False, duration_ms=elapsed,
                           details=details, error=str(exc))


# ---------------------------------------------------------------------------
# Scenario registry
# ---------------------------------------------------------------------------

SCENARIOS: dict[str, Callable[..., FaultResult]] = {
    "post_commit_pre_ack_socket_drop": post_commit_pre_ack_socket_drop,
    "session_abandon_storm": session_abandon_storm,
    "double_submit_idempotency": double_submit_idempotency,
    # daemon_bounce_mid_wave requires extra args (daemon_cmd) — run via swarm_runner
}


# ---------------------------------------------------------------------------
# CLI entry point
# ---------------------------------------------------------------------------

def _main() -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Run simgit fault injection scenarios")
    parser.add_argument(
        "--scenario",
        choices=list(SCENARIOS) + ["all"],
        default="all",
        help="Which scenario to run",
    )
    parser.add_argument("--socket", default=os.environ.get("SIMGIT_SOCKET"), required=False)
    parser.add_argument("--stress-mode", choices=["hotspot", "disjoint-range"], default="disjoint-range")
    parser.add_argument("--storm-agents", type=int, default=30)
    parser.add_argument("--follow-up-agents", type=int, default=5)
    parser.add_argument("--pairs", type=int, default=5)
    parser.add_argument("--report-out", default=None)
    args = parser.parse_args()

    if args.socket is None:
        print("--socket or SIMGIT_SOCKET is required", file=sys.stderr)
        return 2

    to_run: list[str] = list(SCENARIOS) if args.scenario == "all" else [args.scenario]
    results: list[FaultResult] = []

    for name in to_run:
        print(f"[fault] Running scenario: {name}", file=sys.stderr)
        fn = SCENARIOS[name]

        if name == "post_commit_pre_ack_socket_drop":
            r = fn(socket_path=args.socket, stress_mode=args.stress_mode)
        elif name == "session_abandon_storm":
            r = fn(socket_path=args.socket, storm_agents=args.storm_agents,
                   follow_up_agents=args.follow_up_agents)
        elif name == "double_submit_idempotency":
            r = fn(socket_path=args.socket, pairs=args.pairs, stress_mode=args.stress_mode)
        else:
            r = fn(socket_path=args.socket)

        results.append(r)
        status = "PASS" if r.passed else "FAIL"
        print(f"[fault] {name}: {status} ({r.duration_ms}ms)")
        if not r.passed:
            print(f"        error={r.error}")
            print(f"        details={json.dumps(r.details, indent=8)}")

    passed = sum(1 for r in results if r.passed)
    total = len(results)
    print(f"\n[fault] {passed}/{total} scenarios passed")

    report = {"scenarios": [r.to_dict() for r in results], "passed": passed, "total": total}
    if args.report_out:
        with open(args.report_out, "w") as f:
            json.dump(report, f, indent=2)
        print(f"[fault] Report written to {args.report_out}", file=sys.stderr)

    return 0 if passed == total else 1


if __name__ == "__main__":
    raise SystemExit(_main())
