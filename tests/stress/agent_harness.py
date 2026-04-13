#!/usr/bin/env python3
"""Concurrent simgit agent harness.

This script stress-tests control-plane session operations using the Python
bindings. It can run in two modes:

- abort: create session and abort (safe default)
- commit: create session and commit to branch (for disposable repos only)
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import random
import string
import sys
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any


def _load_simgit_module() -> Any:
    try:
        import simgit  # type: ignore
    except Exception as exc:  # pragma: no cover
        raise RuntimeError(
            "Failed to import simgit Python bindings. Build/install via maturin first."
        ) from exc
    return simgit


@dataclass
class AgentResult:
    agent_id: int
    ok: bool
    mode: str
    session_id: str | None
    duration_ms: int
    error: str | None = None


def rand_suffix(n: int = 6) -> str:
    return "".join(random.choice(string.ascii_lowercase) for _ in range(n))


def run_agent(
    agent_id: int,
    mode: str,
    socket_path: str | None,
    overlap_path: str,
    jitter_ms: int,
    barrier: threading.Barrier | None = None,
) -> AgentResult:
    start = time.time()
    simgit = _load_simgit_module()

    try:
        session = simgit.Session.new(
            task_id=f"stress-agent-{agent_id}-{rand_suffix()}",
            socket_path=socket_path,
            agent_label=f"stress-agent-{agent_id}",
        )

        session_id = session.session_id

        info = session.info()

        if mode == "commit":
            mount_path = info["mount_path"]
            target = os.path.join(mount_path, overlap_path)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "w", encoding="utf-8") as f:
                f.write(f"agent={agent_id} ts={time.time_ns()}\n")

            # If using two-phase barrier, wait for all agents to write before committing
            if barrier is not None:
                barrier.wait()

            if jitter_ms > 0:
                time.sleep(random.randint(0, jitter_ms) / 1000.0)

            branch_name = f"stress/agent-{agent_id}-{rand_suffix()}"
            session.commit(branch_name=branch_name, message="stress harness commit")
        else:
            session.abort()

        elapsed = int((time.time() - start) * 1000)
        return AgentResult(agent_id, True, mode, session_id, elapsed)
    except Exception as exc:  # pragma: no cover
        elapsed = int((time.time() - start) * 1000)
        return AgentResult(agent_id, False, mode, None, elapsed, str(exc))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run concurrent simgit agent sessions")
    parser.add_argument("--agents", type=int, default=50, help="number of agents")
    parser.add_argument(
        "--workers",
        type=int,
        default=20,
        help="thread pool size for concurrent agent operations",
    )
    parser.add_argument(
        "--mode",
        choices=["abort", "commit"],
        default="abort",
        help="session termination mode",
    )
    parser.add_argument(
        "--socket",
        default=os.environ.get("SIMGIT_SOCKET"),
        help="override control socket path",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print structured JSON output",
    )
    parser.add_argument(
        "--overlap-path",
        default="hotspot/shared.txt",
        help="relative path written by all agents in commit mode",
    )
    parser.add_argument(
        "--commit-jitter-ms",
        type=int,
        default=50,
        help="max random delay before commit in commit mode",
    )
    parser.add_argument(
        "--two-phase-barrier",
        action="store_true",
        help="use two-phase barrier: all agents write, then all commit simultaneously",
    )
    parser.add_argument(
        "--report-out",
        help="optional file path to write JSON report",
    )
    return parser.parse_args()


def percentile(values: list[int], pct: float) -> int:
    if not values:
        return 0
    s = sorted(values)
    idx = int((len(s) - 1) * pct)
    return s[idx]


def classify_error(error: str | None) -> str:
    if not error:
        return "none"
    e = error.lower()
    if "max sessions" in e:
        return "quota"
    if "borrow" in e or "conflict" in e or "-32001" in e:
        return "lock_conflict"
    if "merge" in e or "-32003" in e:
        return "merge_conflict"
    if "not found" in e or "-32002" in e:
        return "session_not_found"
    if "import simgit" in e or "maturin" in e:
        return "python_binding"
    if "socket" in e or "connection" in e:
        return "transport"
    return "other"


def main() -> int:
    args = parse_args()
    started = time.time()

    if args.agents < 1:
        print("--agents must be >= 1", file=sys.stderr)
        return 2

    # Create barrier if two-phase mode is enabled (commit mode only)
    barrier = None
    if args.two_phase_barrier and args.mode == "commit":
        barrier = threading.Barrier(args.agents)

    results: list[AgentResult] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        futs = [
            pool.submit(
                run_agent,
                i,
                args.mode,
                args.socket,
                args.overlap_path,
                args.commit_jitter_ms,
                barrier,
            )
            for i in range(args.agents)
        ]
        for fut in concurrent.futures.as_completed(futs):
            results.append(fut.result())

    results.sort(key=lambda r: r.agent_id)
    successes = sum(1 for r in results if r.ok)
    failures = len(results) - successes
    durations = [r.duration_ms for r in results]

    failure_breakdown: dict[str, int] = {}
    for r in results:
        if r.ok:
            continue
        kind = classify_error(r.error)
        failure_breakdown[kind] = failure_breakdown.get(kind, 0) + 1

    latency = {
        "min_ms": min(durations) if durations else 0,
        "max_ms": max(durations) if durations else 0,
        "p50_ms": percentile(durations, 0.50),
        "p95_ms": percentile(durations, 0.95),
        "p99_ms": percentile(durations, 0.99),
    }

    summary = {
        "started_at": datetime.now(timezone.utc).isoformat(),
        "total_duration_ms": int((time.time() - started) * 1000),
        "agents": args.agents,
        "workers": args.workers,
        "mode": args.mode,
        "two_phase_barrier": args.two_phase_barrier,
        "successes": successes,
        "failures": failures,
        "failure_breakdown": failure_breakdown,
        "latency": latency,
        "results": [r.__dict__ for r in results],
    }

    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        barrier_label = " (two-phase barrier)" if args.two_phase_barrier else ""
        print(
            f"agents={args.agents} mode={args.mode}{barrier_label} "
            f"successes={successes} failures={failures}"
        )
        for r in results:
            if not r.ok:
                print(f"  agent={r.agent_id} error={r.error}")
        print(
            "latency_ms "
            f"p50={latency['p50_ms']} p95={latency['p95_ms']} p99={latency['p99_ms']} "
            f"max={latency['max_ms']}"
        )

    if args.report_out:
        with open(args.report_out, "w", encoding="utf-8") as f:
            json.dump(summary, f, indent=2)

    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
