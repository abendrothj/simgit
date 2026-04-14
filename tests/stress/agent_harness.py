#!/usr/bin/env python3
"""Concurrent simgit agent harness with stress mode patterns.

This script stress-tests control-plane session operations using the Python
bindings. It supports two stress patterns:

- hotspot: all agents write to the same file (causes conflicts)
- disjoint-range: all agents write disjoint byte ranges in the same file
  (validates range-aware conflict detection; expect 0 conflicts)
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


@dataclass
class PreparedSession:
    agent_id: int
    session: Any
    session_id: str
    mount_path: str
    mode: str


def rand_suffix(n: int = 6) -> str:
    return "".join(random.choice(string.ascii_lowercase) for _ in range(n))


def wait_for_commit_barrier(barrier: threading.Barrier | None) -> None:
    if barrier is None:
        return
    try:
        barrier.wait(timeout=30)
    except threading.BrokenBarrierError as exc:
        raise RuntimeError("commit barrier broken") from exc


def abort_commit_barrier(barrier: threading.Barrier | None) -> None:
    if barrier is None:
        return
    try:
        barrier.abort()
    except threading.BrokenBarrierError:
        pass


def _write_hotspot_file(mount_path: str, overlap_path: str, agent_id: int) -> None:
    target = os.path.join(mount_path, overlap_path)
    os.makedirs(os.path.dirname(target), exist_ok=True)
    with open(target, "w", encoding="utf-8") as f:
        f.write(f"agent={agent_id} ts={time.time_ns()}\n")


def _write_disjoint_range_file(
    mount_path: str,
    target_file: str,
    agent_id: int,
    bytes_per_agent: int = 1024,
) -> None:
    target = os.path.join(mount_path, target_file)
    os.makedirs(os.path.dirname(target), exist_ok=True)
    offset = agent_id * bytes_per_agent
    length = bytes_per_agent
    mode = "r+b" if os.path.exists(target) else "w+b"
    with open(target, mode) as f:
        f.seek(offset)
        marker = f"AGENT_{agent_id:03d}_"
        payload = (marker.encode() + b"x" * (length - len(marker))).ljust(
            length, b"x"
        )[:length]
        f.write(payload)
        f.flush()


def prepare_agent_session(
    agent_id: int,
    socket_path: str | None,
    stress_mode: str,
    overlap_path: str,
    disjoint_file: str,
) -> tuple[PreparedSession | None, AgentResult | None]:
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
        mount_path = info["mount_path"]

        if stress_mode == "hotspot":
            _write_hotspot_file(mount_path, overlap_path, agent_id)
        else:
            _write_disjoint_range_file(mount_path, disjoint_file, agent_id)

        prepared = PreparedSession(
            agent_id=agent_id,
            session=session,
            session_id=session_id,
            mount_path=mount_path,
            mode=stress_mode,
        )
        return prepared, None
    except Exception as exc:  # pragma: no cover
        elapsed = int((time.time() - start) * 1000)
        return None, AgentResult(agent_id, False, stress_mode, None, elapsed, str(exc))


def commit_prepared_session(prepared: PreparedSession, jitter_ms: int) -> AgentResult:
    start = time.time()
    try:
        if jitter_ms > 0:
            time.sleep(random.randint(0, jitter_ms) / 1000.0)

        if prepared.mode == "hotspot":
            branch_name = f"stress/hotspot-agent-{prepared.agent_id}-{rand_suffix()}"
            message = "stress hotspot commit"
        else:
            branch_name = f"stress/disjoint-agent-{prepared.agent_id}-{rand_suffix()}"
            message = "stress disjoint-range commit"

        prepared.session.commit(branch_name=branch_name, message=message)
        elapsed = int((time.time() - start) * 1000)
        return AgentResult(prepared.agent_id, True, prepared.mode, prepared.session_id, elapsed)
    except Exception as exc:  # pragma: no cover
        try:
            prepared.session.abort()
        except Exception:
            pass
        elapsed = int((time.time() - start) * 1000)
        return AgentResult(prepared.agent_id, False, prepared.mode, prepared.session_id, elapsed, str(exc))


def run_agent_hotspot(
    agent_id: int,
    socket_path: str | None,
    overlap_path: str,
    jitter_ms: int,
    barrier: threading.Barrier | None = None,
) -> AgentResult:
    """Hotspot mode: all agents write the same file (full overlap causes conflicts)."""
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
        mount_path = info["mount_path"]
        _write_hotspot_file(mount_path, overlap_path, agent_id)

        # Wait for all agents to write
        wait_for_commit_barrier(barrier)

        if jitter_ms > 0:
            time.sleep(random.randint(0, jitter_ms) / 1000.0)

        branch_name = f"stress/hotspot-agent-{agent_id}-{rand_suffix()}"
        session.commit(branch_name=branch_name, message="stress hotspot commit")

        elapsed = int((time.time() - start) * 1000)
        return AgentResult(agent_id, True, "hotspot", session_id, elapsed)
    except Exception as exc:  # pragma: no cover
        abort_commit_barrier(barrier)
        elapsed = int((time.time() - start) * 1000)
        return AgentResult(agent_id, False, "hotspot", None, elapsed, str(exc))


def run_agent_disjoint_range(
    agent_id: int,
    socket_path: str | None,
    _num_agents: int,
    target_file: str,
    jitter_ms: int,
    barrier: threading.Barrier | None = None,
) -> AgentResult:
    """Disjoint-range mode: all agents write non-overlapping byte ranges in same file.
    
    Each agent writes to its own 1KB window: agent 0 → bytes [0, 1024),
    agent 1 → bytes [1024, 2048), etc. With range-aware conflict detection,
    these should all succeed (0 conflicts expected).
    """
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
        mount_path = info["mount_path"]
        target = os.path.join(mount_path, target_file)
        os.makedirs(os.path.dirname(target), exist_ok=True)

        _write_disjoint_range_file(mount_path, target_file, agent_id)

        # Wait for all agents to write
        wait_for_commit_barrier(barrier)

        if jitter_ms > 0:
            time.sleep(random.randint(0, jitter_ms) / 1000.0)

        branch_name = f"stress/disjoint-agent-{agent_id}-{rand_suffix()}"
        session.commit(
            branch_name=branch_name, message="stress disjoint-range commit"
        )

        elapsed = int((time.time() - start) * 1000)
        return AgentResult(agent_id, True, "disjoint-range", session_id, elapsed)
    except Exception as exc:  # pragma: no cover
        abort_commit_barrier(barrier)
        elapsed = int((time.time() - start) * 1000)
        return AgentResult(agent_id, False, "disjoint-range", None, elapsed, str(exc))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run concurrent simgit agent sessions with configurable stress patterns"
    )
    parser.add_argument("--agents", type=int, default=50, help="number of agents")
    parser.add_argument(
        "--workers",
        type=int,
        default=20,
        help="thread pool size for concurrent agent operations",
    )
    parser.add_argument(
        "--stress-mode",
        choices=["hotspot", "disjoint-range"],
        default="hotspot",
        help="stress pattern: hotspot (all agents write same file) or disjoint-range (each agent writes different byte range in same file)",
    )
    parser.add_argument(
        "--execution-mode",
        choices=["concurrent", "phased"],
        default="concurrent",
        help="execution strategy: concurrent (single worker function does create/write/commit) or phased (create+write phase then commit phase)",
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
        help="relative path for hotspot stress mode (all agents write this file)",
    )
    parser.add_argument(
        "--disjoint-file",
        default="data/disjoint.bin",
        help="target file for disjoint-range stress mode (agents write different byte offsets)",
    )
    parser.add_argument(
        "--commit-jitter-ms",
        type=int,
        default=50,
        help="max random delay before commit",
    )
    parser.add_argument(
        "--commit-workers",
        type=int,
        default=0,
        help="worker count for phased commit step (0 = same as workers)",
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
    if "socket" in e or "connection" in e or "timeout" in e:
        return "transport"
    if "barrier" in e:
        return "barrier"
    return "other"


def main() -> int:
    args = parse_args()
    started = time.time()

    if args.agents < 1:
        print("--agents must be >= 1", file=sys.stderr)
        return 2

    # Create barrier if two-phase mode is enabled. A barrier requires every
    # agent task to be able to run concurrently; otherwise the first wave of
    # workers will block forever waiting for tasks that never get scheduled.
    barrier = None
    worker_count = args.workers
    if args.two_phase_barrier:
        barrier = threading.Barrier(args.agents)
        worker_count = max(args.workers, args.agents)

    # Dispatch agents to appropriate stress mode
    results: list[AgentResult] = []
    if args.execution_mode == "phased":
        prepared: list[PreparedSession] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as pool:
            prep_futs = [
                pool.submit(
                    prepare_agent_session,
                    i,
                    args.socket,
                    args.stress_mode,
                    args.overlap_path,
                    args.disjoint_file,
                )
                for i in range(args.agents)
            ]
            for fut in concurrent.futures.as_completed(prep_futs):
                session_obj, prep_err = fut.result()
                if session_obj is not None:
                    prepared.append(session_obj)
                if prep_err is not None:
                    results.append(prep_err)

        commit_workers = args.commit_workers if args.commit_workers > 0 else worker_count
        with concurrent.futures.ThreadPoolExecutor(max_workers=commit_workers) as pool:
            commit_futs = [
                pool.submit(commit_prepared_session, s, args.commit_jitter_ms)
                for s in prepared
            ]
            for fut in concurrent.futures.as_completed(commit_futs):
                results.append(fut.result())
    else:
        with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as pool:
            if args.stress_mode == "hotspot":
                futs = [
                    pool.submit(
                        run_agent_hotspot,
                        i,
                        args.socket,
                        args.overlap_path,
                        args.commit_jitter_ms,
                        barrier,
                    )
                    for i in range(args.agents)
                ]
            else:  # disjoint-range
                futs = [
                    pool.submit(
                        run_agent_disjoint_range,
                        i,
                        args.socket,
                        args.agents,
                        args.disjoint_file,
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
        "workers": worker_count,
        "requested_workers": args.workers,
        "execution_mode": args.execution_mode,
        "stress_mode": args.stress_mode,
        "two_phase_barrier": args.two_phase_barrier,
        "successes": successes,
        "failures": failures,
        "failure_breakdown": failure_breakdown,
        "latency": latency,
    }

    # Mode-specific metadata
    if args.stress_mode == "hotspot":
        summary["conflict_expectation"] = "all (path-level whole-file write)"
        summary["overlap_path"] = args.overlap_path
    else:  # disjoint-range
        summary["conflict_expectation"] = "0 (non-overlapping byte ranges)"
        summary["target_file"] = args.disjoint_file
        summary["bytes_per_agent"] = 1024
        summary["total_file_size_bytes"] = args.agents * 1024

    # Only include full results in JSON mode to reduce output size
    if args.json:
        summary["results"] = [r.__dict__ for r in results]
        print(json.dumps(summary, indent=2))
    else:
        # Console output: summary + errors only
        barrier_label = " (two-phase barrier)" if args.two_phase_barrier else ""
        print(
            f"agents={args.agents} stress_mode={args.stress_mode}{barrier_label} "
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
        # Always write full results to disk
        summary["results"] = [r.__dict__ for r in results]
        with open(args.report_out, "w", encoding="utf-8") as f:
            json.dump(summary, f, indent=2)
        print(f"Report written to {args.report_out}")

    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
