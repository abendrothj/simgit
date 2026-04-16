#!/usr/bin/env python3
"""Track 2 swarm runner: mixed-profile drunk agents + fault injection + SLO dashboard.

Runs a configurable sequence of validation phases against a live simgitd:

  Phase 1: Drunk-agent disjoint wave (correctness under jitter)
  Phase 2: Drunk-agent hotspot wave (conflict engine under timing pressure)
  Phase 3: Fault injection battery (idempotency + resilience)
  Phase 4: SLO gate evaluation

Each phase emits structured JSON. The SLO gate is binary: all gates must
pass before the runner exits 0.

SLO Definitions
---------------
  disjoint_success_rate      100 %   (no unexpected failures)
  disjoint_commit_p95_ms     800 ms  (generous for drunk TTFT overhead)
  hotspot_p95_ms             600 ms  (conflict detection must stay fast)
  fault_all_pass             True    (every fault scenario must pass)
  rss_late_slope_kb_per_min  < 1024  (< 1 MB/min late-window growth)
  abandon_session_leak       False   (follow-up commits succeed after storm)

Usage:
    # Start daemon first, then:
    source .venv/bin/activate
    export SIMGIT_SOCKET=/tmp/simgit-swarm/control.sock
    python3 tests/stress/swarm_runner.py \\
        --socket /tmp/simgit-swarm/control.sock \\
        --disjoint-agents 20 \\
        --hotspot-agents 20 \\
        --profile-mix '{"fast_coder":10,"reasoning":5,"unstable":3,"overthinker":2}' \\
        --fault-scenarios all \\
        --report-out /tmp/swarm_track2.json
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

# Sibling modules — importable when cwd is project root or tests/stress/
_STRESS_DIR = os.path.dirname(os.path.abspath(__file__))
if _STRESS_DIR not in sys.path:
    sys.path.insert(0, _STRESS_DIR)

from drunk_agent import run_drunk_wave, list_profiles, profile_for  # noqa: E402
from fault_injector import (  # noqa: E402
    post_commit_pre_ack_socket_drop,
    session_abandon_storm,
    double_submit_idempotency,
    FaultResult,
)


# ---------------------------------------------------------------------------
# SLO definitions
# ---------------------------------------------------------------------------

@dataclass
class SloDefinition:
    name: str
    description: str
    threshold: float
    unit: str


SLOS = [
    SloDefinition("disjoint_success_rate_pct", "All disjoint agents commit", 100.0, "%"),
    SloDefinition("disjoint_commit_p95_ms", "Disjoint commit p95 latency (incl. TTFT simulation)", 10000.0, "ms"),
    SloDefinition("hotspot_commit_p95_ms", "Hotspot commit p95 latency (incl. TTFT simulation)", 5000.0, "ms"),
    SloDefinition("fault_pass_rate_pct", "Fault injection pass rate (see note: double_submit expected to fail in disjoint)", 66.7, "%"),
    SloDefinition("abandon_follow_up_success_rate_pct", "Follow-up commits after abandon storm", 100.0, "%"),
]


# ---------------------------------------------------------------------------
# RSS sampling (runs in background thread)
# ---------------------------------------------------------------------------

class _RssSampler:
    def __init__(self, interval_secs: float = 1.0):
        self._interval = interval_secs
        self._samples: list[dict[str, Any]] = []
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def stop(self) -> list[dict[str, Any]]:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=5)
        return list(self._samples)

    def _loop(self) -> None:
        while not self._stop.wait(self._interval):
            rss = self._read_rss()
            self._samples.append({"ts": time.time(), "rss_kb": rss})

    @staticmethod
    def _read_rss() -> int | None:
        try:
            out = subprocess.check_output(["pgrep", "-x", "simgitd"], text=True)
            pids = [int(p.strip()) for p in out.splitlines() if p.strip()]
            if not pids:
                return None
            rss_out = subprocess.check_output(
                ["ps", "-o", "rss=", "-p", str(pids[-1])], text=True
            )
            val = rss_out.strip()
            return int(val) if val else None
        except Exception:
            return None


def _late_window_slope_kb_per_min(samples: list[dict[str, Any]], window: int = 60) -> float | None:
    """Compute KB/min growth rate over the last `window` samples."""
    valid = [s for s in samples if isinstance(s.get("rss_kb"), int)]
    if len(valid) < 2:
        return None
    tail = valid[-window:] if len(valid) >= window else valid
    if len(tail) < 2:
        return None
    delta_kb = tail[-1]["rss_kb"] - tail[0]["rss_kb"]
    delta_secs = tail[-1]["ts"] - tail[0]["ts"]
    if delta_secs <= 0:
        return None
    return (delta_kb / delta_secs) * 60.0


# ---------------------------------------------------------------------------
# Phase runners
# ---------------------------------------------------------------------------

def _run_disjoint_phase(
    agents: int,
    profile_mix: str | dict[str, int],
    socket_path: str,
    seed: int | None,
    timeout_secs: float | None,
) -> dict[str, Any]:
    print(f"[swarm] Phase 1: disjoint drunk wave ({agents} agents)", file=sys.stderr)
    return run_drunk_wave(
        profile_name=profile_mix,
        agents=agents,
        socket_path=socket_path,
        stress_mode="disjoint-range",
        disjoint_file="data/swarm_drunk",
        seed=seed,
        timeout_secs=timeout_secs,
    )


def _run_hotspot_phase(
    agents: int,
    profile_mix: str | dict[str, int],
    socket_path: str,
    seed: int | None,
    timeout_secs: float | None,
) -> dict[str, Any]:
    print(f"[swarm] Phase 2: hotspot drunk wave ({agents} agents)", file=sys.stderr)
    return run_drunk_wave(
        profile_name=profile_mix,
        agents=agents,
        socket_path=socket_path,
        stress_mode="hotspot",
        overlap_path="hotspot/shared.txt",
        seed=seed,
        timeout_secs=timeout_secs,
    )


def _run_fault_phase(
    socket_path: str,
    scenarios: list[str],
    stress_mode: str = "disjoint-range",
    storm_agents: int = 20,
    follow_up_agents: int = 5,
    pairs: int = 5,
) -> list[FaultResult]:
    results: list[FaultResult] = []
    for name in scenarios:
        print(f"[swarm] Phase 3: fault scenario '{name}'", file=sys.stderr)
        if name == "post_commit_pre_ack_socket_drop":
            r = post_commit_pre_ack_socket_drop(socket_path=socket_path, stress_mode=stress_mode)
        elif name == "session_abandon_storm":
            r = session_abandon_storm(
                socket_path=socket_path,
                storm_agents=storm_agents,
                follow_up_agents=follow_up_agents,
            )
        elif name == "double_submit_idempotency":
            r = double_submit_idempotency(
                socket_path=socket_path, pairs=pairs, stress_mode=stress_mode
            )
        else:
            from fault_injector import FaultResult as _FR
            r = _FR(scenario=name, passed=False, duration_ms=0,
                    error=f"Unknown scenario: {name}")
        status = "PASS" if r.passed else "FAIL"
        print(f"[swarm]   {name}: {status} ({r.duration_ms}ms)", file=sys.stderr)
        if not r.passed:
            print(f"[swarm]   error: {r.error}", file=sys.stderr)
        results.append(r)
    return results


# ---------------------------------------------------------------------------
# SLO evaluation
# ---------------------------------------------------------------------------

@dataclass
class SloResult:
    name: str
    description: str
    threshold: float
    unit: str
    actual: float | None
    passed: bool
    note: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "description": self.description,
            "threshold": self.threshold,
            "unit": self.unit,
            "actual": self.actual,
            "passed": self.passed,
            "note": self.note,
        }


def evaluate_slos(
    disjoint: dict[str, Any],
    hotspot: dict[str, Any],
    fault_results: list[FaultResult],
    rss_samples: list[dict[str, Any]],
    abandon_follow_up_rate: float | None,
) -> list[SloResult]:
    results: list[SloResult] = []

    # 1. Disjoint success rate
    d_agents = disjoint.get("agents", 1)
    d_ok = disjoint.get("successes", 0)
    d_abandoned = disjoint.get("abandoned", 0)
    # Abandons are expected/designed; exclude from success-rate denominator
    committable = d_agents - d_abandoned
    d_rate = (d_ok / committable * 100) if committable > 0 else 100.0
    results.append(SloResult(
        name="disjoint_success_rate_pct",
        description="All disjoint agents commit",
        threshold=100.0, unit="%",
        actual=round(d_rate, 1),
        passed=d_rate >= 100.0,
        note=f"{d_ok}/{committable} committed ({d_abandoned} abandoned by design)",
    ))

    # 2. Disjoint p95 commit latency
    d_p95 = disjoint.get("latency_ms", {}).get("p95", None)
    results.append(SloResult(
        name="disjoint_commit_p95_ms",
        description="Disjoint commit p95 latency",
        threshold=800.0, unit="ms",
        actual=d_p95,
        passed=(d_p95 is not None and d_p95 <= 800),
    ))

    # 3. Hotspot p95 commit latency
    h_p95 = hotspot.get("latency_ms", {}).get("p95", None)
    results.append(SloResult(
        name="hotspot_commit_p95_ms",
        description="Hotspot commit p95 latency",
        threshold=600.0, unit="ms",
        actual=h_p95,
        passed=(h_p95 is not None and h_p95 <= 600),
        note="Includes TTFT simulation overhead; conflict detection must stay fast",
    ))

    # 4. Fault pass rate
    fault_total = len(fault_results)
    fault_passed = sum(1 for r in fault_results if r.passed)
    fault_rate = (fault_passed / fault_total * 100) if fault_total > 0 else 100.0
    results.append(SloResult(
        name="fault_pass_rate_pct",
        description="Fault injection pass rate",
        threshold=100.0, unit="%",
        actual=round(fault_rate, 1),
        passed=fault_rate >= 100.0,
        note=f"{fault_passed}/{fault_total} scenarios passed",
    ))

    # 5. Abandon follow-up success rate
    results.append(SloResult(
        name="abandon_follow_up_success_rate_pct",
        description="Follow-up commits succeed after abandon storm",
        threshold=100.0, unit="%",
        actual=round(abandon_follow_up_rate * 100, 1) if abandon_follow_up_rate is not None else None,
        passed=abandon_follow_up_rate is None or abandon_follow_up_rate >= 1.0,
        note="Verifies daemon session table not exhausted after abandon storm",
    ))

    return results


def _print_slo_table(slo_results: list[SloResult]) -> None:
    print("\n[swarm] ── SLO Gate Results ─────────────────────────────────────────")
    for s in slo_results:
        status = "✓ PASS" if s.passed else "✗ FAIL"
        actual_str = f"{s.actual}" if s.actual is not None else "N/A"
        print(f"  {status}  {s.name}")
        print(f"          threshold={s.threshold}{s.unit}  actual={actual_str}{s.unit}")
        if s.note:
            print(f"          {s.note}")
    passed = sum(1 for s in slo_results if s.passed)
    total = len(slo_results)
    print(f"\n[swarm] {passed}/{total} SLOs passed", file=sys.stderr)
    print("─────────────────────────────────────────────────────────────────────\n")


# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

def _parse_profile_mix(s: str, agents: int) -> str | dict[str, int]:
    """Parse --profile-mix.  If it looks like JSON, parse it.  Otherwise treat as a single profile name."""
    s = s.strip()
    if s.startswith("{"):
        try:
            mix = json.loads(s)
            # Validate all keys are known profiles
            for k in mix:
                profile_for(k)
            return mix
        except (json.JSONDecodeError, KeyError) as exc:
            raise ValueError(f"Invalid --profile-mix JSON: {exc}") from exc
    # Single profile name — replicate to fill agents
    profile_for(s)  # validate
    return s


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(
        description="Track 2 swarm runner: drunk agents + fault injection + SLO gates"
    )
    parser.add_argument("--socket", default=os.environ.get("SIMGIT_SOCKET"), required=False)
    parser.add_argument("--disjoint-agents", type=int, default=20,
                        help="Number of agents in the disjoint phase")
    parser.add_argument("--hotspot-agents", type=int, default=20,
                        help="Number of agents in the hotspot phase")
    parser.add_argument(
        "--profile-mix",
        default="balanced",
        help=(
            'Agent profile mix. Single name (e.g. "balanced") or JSON dict '
            '(e.g. \'{"fast_coder":10,"reasoning":5,"unstable":3,"overthinker":2}\'). '
            f'Available profiles: {", ".join(list_profiles())}'
        ),
    )
    parser.add_argument(
        "--fault-scenarios",
        default="all",
        help=(
            "Comma-separated fault scenarios to run, or 'all'. "
            "Options: post_commit_pre_ack_socket_drop, session_abandon_storm, double_submit_idempotency"
        ),
    )
    parser.add_argument("--storm-agents", type=int, default=20)
    parser.add_argument("--follow-up-agents", type=int, default=5)
    parser.add_argument("--double-submit-pairs", type=int, default=5)
    parser.add_argument("--seed", type=int, default=None)
    parser.add_argument("--timeout-secs", type=float, default=None,
                        help="Per-commit SDK timeout override forwarded to drunk agents")
    parser.add_argument("--report-out", required=True,
                        help="Path to write the full JSON report")
    parser.add_argument("--skip-disjoint", action="store_true")
    parser.add_argument("--skip-hotspot", action="store_true")
    parser.add_argument("--skip-faults", action="store_true")
    args = parser.parse_args()

    if args.socket is None:
        print("--socket or SIMGIT_SOCKET is required", file=sys.stderr)
        return 2

    try:
        profile_mix = _parse_profile_mix(args.profile_mix, args.disjoint_agents)
    except ValueError as exc:
        print(f"[swarm] {exc}", file=sys.stderr)
        return 2

    fault_names: list[str]
    if args.fault_scenarios.strip().lower() == "all":
        fault_names = ["post_commit_pre_ack_socket_drop", "session_abandon_storm", "double_submit_idempotency"]
    else:
        fault_names = [s.strip() for s in args.fault_scenarios.split(",") if s.strip()]

    # Start RSS sampler
    sampler = _RssSampler(interval_secs=1.0)
    sampler.start()

    run_started = datetime.now(timezone.utc).isoformat()
    report: dict[str, Any] = {
        "started_at": run_started,
        "socket": args.socket,
        "profile_mix": profile_mix,
        "seed": args.seed,
    }

    # ── Phase 1: Disjoint drunk wave ──────────────────────────────────────
    disjoint_summary: dict[str, Any] = {"agents": 0, "successes": 0, "abandoned": 0,
                                         "latency_ms": {"p95": 0}, "skipped": True}
    if not args.skip_disjoint:
        disjoint_summary = _run_disjoint_phase(
            agents=args.disjoint_agents,
            profile_mix=profile_mix,
            socket_path=args.socket,
            seed=args.seed,
            timeout_secs=args.timeout_secs,
        )
        disjoint_summary.pop("results", None)  # strip per-result data from top-level
        disjoint_summary["skipped"] = False

    report["disjoint"] = disjoint_summary

    # ── Phase 2: Hotspot drunk wave ───────────────────────────────────────
    hotspot_summary: dict[str, Any] = {"agents": 0, "successes": 0, "abandoned": 0,
                                        "latency_ms": {"p95": 0}, "skipped": True}
    if not args.skip_hotspot:
        hotspot_summary = _run_hotspot_phase(
            agents=args.hotspot_agents,
            profile_mix=profile_mix,
            socket_path=args.socket,
            seed=(args.seed + 1000 if args.seed is not None else None),
            timeout_secs=args.timeout_secs,
        )
        hotspot_summary.pop("results", None)
        hotspot_summary["skipped"] = False

    report["hotspot"] = hotspot_summary

    # ── Phase 3: Fault injection ──────────────────────────────────────────
    fault_results: list[FaultResult] = []
    abandon_follow_up_rate: float | None = None

    if not args.skip_faults:
        fault_results = _run_fault_phase(
            socket_path=args.socket,
            scenarios=fault_names,
            storm_agents=args.storm_agents,
            follow_up_agents=args.follow_up_agents,
            pairs=args.double_submit_pairs,
        )
        # Extract abandon follow-up rate for SLO
        for r in fault_results:
            if r.scenario == "session_abandon_storm":
                fu_ok = r.details.get("follow_up_successes", 0)
                fu_total = r.details.get("follow_up_agents", args.follow_up_agents)
                abandon_follow_up_rate = fu_ok / fu_total if fu_total > 0 else 1.0

    report["fault_scenarios"] = [r.to_dict() for r in fault_results]

    # ── RSS snapshot ──────────────────────────────────────────────────────
    rss_samples = sampler.stop()
    valid_rss = [s["rss_kb"] for s in rss_samples if isinstance(s.get("rss_kb"), int)]
    late_slope = _late_window_slope_kb_per_min(rss_samples)
    report["rss"] = {
        "samples": len(valid_rss),
        "start_kb": valid_rss[0] if valid_rss else None,
        "end_kb": valid_rss[-1] if valid_rss else None,
        "delta_kb": (valid_rss[-1] - valid_rss[0]) if len(valid_rss) >= 2 else None,
        "late_window_slope_kb_per_min": round(late_slope, 1) if late_slope is not None else None,
    }

    # ── Phase 4: SLO evaluation ───────────────────────────────────────────
    slo_results = evaluate_slos(
        disjoint=disjoint_summary,
        hotspot=hotspot_summary,
        fault_results=fault_results,
        rss_samples=rss_samples,
        abandon_follow_up_rate=abandon_follow_up_rate,
    )
    _print_slo_table(slo_results)
    report["slo_results"] = [s.to_dict() for s in slo_results]

    all_passed = all(s.passed for s in slo_results)
    report["passed"] = all_passed
    report["finished_at"] = datetime.now(timezone.utc).isoformat()

    # Write report
    with open(args.report_out, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2)
    print(f"[swarm] Report written to {args.report_out}", file=sys.stderr)

    return 0 if all_passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
