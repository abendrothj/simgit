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
    attempts: int = 1
    attempt_durations_ms: list[int] | None = None


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


def commit_prepared_session(
    prepared: PreparedSession,
    jitter_ms: int,
    retry_attempts: int = 1,
    retry_failure_mode: str = "none",
) -> AgentResult:
    start = time.time()
    attempt_durations: list[int] = []
    last_error: str | None = None

    for attempt in range(1, retry_attempts + 1):
        attempt_start = time.time()
        try:
            if jitter_ms > 0:
                time.sleep(random.randint(0, jitter_ms) / 1000.0)

            if prepared.mode == "hotspot":
                branch_name = f"stress/hotspot-agent-{prepared.agent_id}-{rand_suffix()}"
                message = "stress hotspot commit"
            else:
                branch_name = f"stress/disjoint-agent-{prepared.agent_id}-{rand_suffix()}"
                message = "stress disjoint-range commit"

            # Retry-focused benchmark mode: force failures on early attempts so
            # we can measure long-lived session commit retries.
            if retry_failure_mode == "invalid-branch" and attempt < retry_attempts:
                branch_name = f"invalid branch {prepared.agent_id} {attempt}"

            prepared.session.commit(branch_name=branch_name, message=message)
            attempt_durations.append(int((time.time() - attempt_start) * 1000))
            elapsed = int((time.time() - start) * 1000)
            return AgentResult(
                prepared.agent_id,
                True,
                prepared.mode,
                prepared.session_id,
                elapsed,
                attempts=attempt,
                attempt_durations_ms=attempt_durations,
            )
        except Exception as exc:  # pragma: no cover
            attempt_durations.append(int((time.time() - attempt_start) * 1000))
            last_error = str(exc)
            if attempt < retry_attempts:
                continue

    try:
        prepared.session.abort()
    except Exception:
        pass
    elapsed = int((time.time() - start) * 1000)
    return AgentResult(
        prepared.agent_id,
        False,
        prepared.mode,
        prepared.session_id,
        elapsed,
        last_error,
        attempts=retry_attempts,
        attempt_durations_ms=attempt_durations,
    )


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
        "--retry-attempts",
        type=int,
        default=1,
        help="number of commit attempts per prepared session in phased mode",
    )
    parser.add_argument(
        "--retry-failure-mode",
        choices=["none", "invalid-branch"],
        default="none",
        help="optional synthetic failure mode for early retry attempts (phased mode)",
    )
    parser.add_argument(
        "--report-out",
        help="optional file path to write JSON report",
    )
    parser.add_argument(
        "--drunk-profile",
        default=None,
        help=(
            "Run a drunk-agent wave instead of the standard harness. "
            "Value is a single profile name or a JSON mix dict. "
            "Available profiles: fast_coder, reasoning, unstable, overthinker, balanced."
        ),
    )
    parser.add_argument(
        "--drunk-seed",
        type=int,
        default=None,
        help="RNG seed for drunk-agent runs (enables deterministic replay)",
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

    # ── Drunk-agent fast-path ─────────────────────────────────────────────
    if args.drunk_profile is not None:
        _stress_dir = os.path.dirname(os.path.abspath(__file__))
        if _stress_dir not in sys.path:
            sys.path.insert(0, _stress_dir)
        from drunk_agent import run_drunk_wave, list_profiles, profile_for  # noqa: F401

        profile_arg: str | dict
        praw = args.drunk_profile.strip()
        if praw.startswith("{"):
            try:
                profile_arg = json.loads(praw)
                for k in profile_arg:
                    profile_for(k)  # validate
            except (json.JSONDecodeError, KeyError) as exc:
                print(f"Invalid --drunk-profile JSON: {exc}", file=sys.stderr)
                return 2
        else:
            try:
                profile_for(praw)  # validate name
            except KeyError as exc:
                print(str(exc), file=sys.stderr)
                return 2
            profile_arg = praw

        summary = run_drunk_wave(
            profile_name=profile_arg,
            agents=args.agents,
            socket_path=args.socket,
            stress_mode=args.stress_mode,
            overlap_path=args.overlap_path,
            disjoint_file=args.disjoint_file,
            workers=args.workers or None,
            seed=args.drunk_seed,
        )
        # Remove timing objects before serialisation
        for r in summary.get("results", []):
            r.pop("timing", None)

        if args.json:
            print(json.dumps(summary, indent=2))
        else:
            lat = summary.get("latency_ms", {})
            print(
                f"drunk_profile={args.drunk_profile} stress_mode={args.stress_mode} "
                f"agents={summary['agents']} "
                f"successes={summary['successes']} failures={summary['failures']} "
                f"abandoned={summary['abandoned']}"
            )
            print(
                f"latency_ms p50={lat.get('p50',0)} p95={lat.get('p95',0)} "
                f"p99={lat.get('p99',0)} max={lat.get('max',0)}"
            )

        if args.report_out:
            with open(args.report_out, "w", encoding="utf-8") as f:
                json.dump(summary, f, indent=2)
            print(f"Report written to {args.report_out}")

        return 0 if summary["failures"] == summary["abandoned"] else 1
    # ── End drunk fast-path ───────────────────────────────────────────────

    if args.agents < 1:
        print("--agents must be >= 1", file=sys.stderr)
        return 2
    if args.retry_attempts < 1:
        print("--retry-attempts must be >= 1", file=sys.stderr)
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
                pool.submit(
                    commit_prepared_session,
                    s,
                    args.commit_jitter_ms,
                    args.retry_attempts,
                    args.retry_failure_mode,
                )
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

    total_attempts = sum(r.attempts for r in results)
    attempt_latencies = [d for r in results for d in (r.attempt_durations_ms or [])]
    retry = {
        "enabled": args.retry_attempts > 1,
        "attempts_configured": args.retry_attempts,
        "failure_mode": args.retry_failure_mode,
        "attempts_observed": total_attempts,
        "avg_attempts_per_agent": round(total_attempts / len(results), 2) if results else 0,
        "attempt_latency": {
            "min_ms": min(attempt_latencies) if attempt_latencies else 0,
            "max_ms": max(attempt_latencies) if attempt_latencies else 0,
            "p50_ms": percentile(attempt_latencies, 0.50),
            "p95_ms": percentile(attempt_latencies, 0.95),
            "p99_ms": percentile(attempt_latencies, 0.99),
        },
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
        "retry": retry,
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
