#!/usr/bin/env python3
"""Sustained-load soak runner for simgitd.

Runs the existing stress harness in a loop and samples daemon RSS over time.
Intended to detect memory growth or rising failure/latency under sustained load.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import tempfile
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class IterationResult:
    index: int
    started_at: str
    duration_ms: int
    success: bool
    failures: int
    p95_ms: int
    p99_ms: int


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run sustained soak loop over stress harness")
    parser.add_argument("--minutes", type=int, default=10, help="soak duration in minutes")
    parser.add_argument("--agents", type=int, default=20, help="agents per iteration")
    parser.add_argument("--workers", type=int, default=20, help="worker threads per iteration")
    parser.add_argument(
        "--stress-mode",
        choices=["hotspot", "disjoint-range"],
        default="hotspot",
        help="stress pattern",
    )
    parser.add_argument(
        "--execution-mode",
        choices=["concurrent", "phased"],
        default="phased",
        help="harness execution strategy",
    )
    parser.add_argument("--commit-workers", type=int, default=20, help="commit workers in phased mode")
    parser.add_argument("--socket", required=True, help="control socket path")
    parser.add_argument("--daemon-pid", type=int, help="optional fixed simgitd pid for RSS sampling")
    parser.add_argument("--sample-interval-secs", type=float, default=1.0, help="RSS sample period")
    parser.add_argument("--report-out", required=True, help="output JSON summary path")
    return parser.parse_args()


def _read_rss_kb(pid: int) -> int | None:
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True)
        value = out.strip()
        if not value:
            return None
        return int(value)
    except Exception:
        return None


def _discover_simgitd_pid() -> int | None:
    try:
        out = subprocess.check_output(["pgrep", "-x", "simgitd"], text=True)
        pids = [int(line.strip()) for line in out.splitlines() if line.strip()]
        if not pids:
            return None
        return pids[-1]
    except Exception:
        return None


def _percentile(values: list[int], pct: float) -> int:
    if not values:
        return 0
    arr = sorted(values)
    idx = int((len(arr) - 1) * pct)
    return arr[idx]


def run_iteration(args: argparse.Namespace, index: int) -> tuple[IterationResult, dict[str, Any]]:
    started = time.time()
    report_file = tempfile.NamedTemporaryFile(prefix=f"simgit-soak-{index}-", suffix=".json", delete=False)
    report_file.close()

    cmd = [
        "python3",
        "tests/stress/agent_harness.py",
        "--agents",
        str(args.agents),
        "--workers",
        str(args.workers),
        "--stress-mode",
        args.stress_mode,
        "--execution-mode",
        args.execution_mode,
        "--commit-workers",
        str(args.commit_workers),
        "--socket",
        args.socket,
        "--report-out",
        report_file.name,
    ]

    proc = subprocess.run(cmd, capture_output=True, text=True)

    report: dict[str, Any]
    try:
        with open(report_file.name, "r", encoding="utf-8") as f:
            report = json.load(f)
    except Exception:
        report = {
            "failures": args.agents,
            "latency": {"p95_ms": 0, "p99_ms": 0},
            "error": "missing or invalid harness report",
            "stdout": proc.stdout,
            "stderr": proc.stderr,
        }
    finally:
        try:
            os.unlink(report_file.name)
        except OSError:
            pass

    elapsed_ms = int((time.time() - started) * 1000)
    failures = int(report.get("failures", args.agents))
    latency = report.get("latency", {}) or {}

    result = IterationResult(
        index=index,
        started_at=datetime.now(timezone.utc).isoformat(),
        duration_ms=elapsed_ms,
        success=(proc.returncode == 0 and failures == 0),
        failures=failures,
        p95_ms=int(latency.get("p95_ms", 0) or 0),
        p99_ms=int(latency.get("p99_ms", 0) or 0),
    )

    return result, report


def main() -> int:
    args = parse_args()
    soak_secs = args.minutes * 60
    end_at = time.time() + soak_secs

    rss_samples: list[dict[str, Any]] = []
    iteration_results: list[IterationResult] = []
    iteration_reports: list[dict[str, Any]] = []

    stop_sampling = threading.Event()

    def sampler() -> None:
        while not stop_sampling.is_set():
            pid = args.daemon_pid or _discover_simgitd_pid()
            rss_kb = _read_rss_kb(pid) if pid is not None else None
            rss_samples.append(
                {
                    "ts": datetime.now(timezone.utc).isoformat(),
                    "pid": pid,
                    "rss_kb": rss_kb,
                }
            )
            time.sleep(max(args.sample_interval_secs, 0.1))

    t = threading.Thread(target=sampler, daemon=True)
    t.start()

    index = 1
    try:
        while time.time() < end_at:
            result, report = run_iteration(args, index)
            iteration_results.append(result)
            iteration_reports.append(report)
            index += 1
    finally:
        stop_sampling.set()
        t.join(timeout=2)

    durations = [it.duration_ms for it in iteration_results]
    p95_values = [it.p95_ms for it in iteration_results if it.p95_ms > 0]
    p99_values = [it.p99_ms for it in iteration_results if it.p99_ms > 0]
    total_failures = sum(it.failures for it in iteration_results)
    success_iters = sum(1 for it in iteration_results if it.success)

    rss_kb_values = [s["rss_kb"] for s in rss_samples if isinstance(s.get("rss_kb"), int)]
    rss_start = rss_kb_values[0] if rss_kb_values else None
    rss_end = rss_kb_values[-1] if rss_kb_values else None

    summary = {
        "started_at": datetime.now(timezone.utc).isoformat(),
        "duration_minutes": args.minutes,
        "config": {
            "agents": args.agents,
            "workers": args.workers,
            "stress_mode": args.stress_mode,
            "execution_mode": args.execution_mode,
            "commit_workers": args.commit_workers,
            "socket": args.socket,
            "daemon_pid": args.daemon_pid,
        },
        "iterations": {
            "count": len(iteration_results),
            "success": success_iters,
            "failed": len(iteration_results) - success_iters,
            "total_agent_failures": total_failures,
            "duration_ms": {
                "min": min(durations) if durations else 0,
                "max": max(durations) if durations else 0,
                "p50": _percentile(durations, 0.50),
                "p95": _percentile(durations, 0.95),
            },
            "commit_latency_ms": {
                "p95_p50": int(statistics.median(p95_values)) if p95_values else 0,
                "p95_max": max(p95_values) if p95_values else 0,
                "p99_p50": int(statistics.median(p99_values)) if p99_values else 0,
                "p99_max": max(p99_values) if p99_values else 0,
            },
        },
        "rss_kb": {
            "samples": len(rss_kb_values),
            "start": rss_start,
            "end": rss_end,
            "min": min(rss_kb_values) if rss_kb_values else None,
            "max": max(rss_kb_values) if rss_kb_values else None,
            "delta": (rss_end - rss_start) if rss_start is not None and rss_end is not None else None,
        },
        "rss_samples": rss_samples,
        "iteration_results": [it.__dict__ for it in iteration_results],
        "iteration_reports": iteration_reports,
    }

    out_path = Path(args.report_out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")

    print(json.dumps({
        "report_out": str(out_path),
        "iterations": summary["iterations"],
        "rss_kb": summary["rss_kb"],
    }, indent=2))

    return 0 if summary["iterations"]["failed"] == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
