#!/usr/bin/env python3
"""Drunk-agent profile library for simgit Track 2 stress testing.

Simulates realistic LLM agent non-determinism without requiring a real model.
All timing distributions are derived from empirical LLM latency measurements:

  TTFT     (time-to-first-token → time before first write):
           lognormal μ=5.4, σ=0.9 → p50≈220ms, p95≈1.4s, p99≈3.2s

  Write    (time to finish writes after TTFT starts):
           exponential, mean varies by profile

  Payload  (bytes written per file):
           bimodal — 80% small (50–500 B), 20% large (4K–40K B)

  Stall    (mid-write "thinking" pause):
           Pareto-distributed duration, profile-specific probability

  Abandon  (agent gives up before commit):
           Bernoulli with profile-specific rate

  Double-commit (retry same logical work with same payload, new session):
           Bernoulli with profile-specific rate — exercises idempotency

Usage:
    from drunk_agent import profile_for, run_drunk_wave, DrunkAgentProfile

    results = run_drunk_wave(
        profile_name="reasoning",
        agents=20,
        socket_path="/tmp/simgit-test/control.sock",
        seed=42,
    )
"""

from __future__ import annotations

import math
import os
import random
import string
import time
from dataclasses import dataclass, field
from typing import Any


# ---------------------------------------------------------------------------
# Statistical helpers
# ---------------------------------------------------------------------------

def _lognormal_sample(rng: random.Random, mu: float, sigma: float) -> float:
    """Sample from a lognormal distribution using Box-Muller."""
    normal = rng.gauss(mu, sigma)
    return math.exp(normal)


def _pareto_sample(rng: random.Random, alpha: float, x_min: float) -> float:
    """Sample from a Pareto distribution (power-law tail)."""
    u = rng.random()
    # CDF inverse: x_min / (1 - u)^(1/alpha)
    return x_min / ((1.0 - u) ** (1.0 / alpha))


def _bimodal_payload_bytes(rng: random.Random, large_prob: float = 0.2) -> int:
    """Return payload size drawn from a bimodal distribution.

    80% of the time: small edit (50 – 500 B)
    20% of the time: large context dump (4096 – 40960 B)
    """
    if rng.random() < large_prob:
        return rng.randint(4096, 40960)
    return rng.randint(50, 500)


# ---------------------------------------------------------------------------
# Profile definitions
# ---------------------------------------------------------------------------

@dataclass
class DrunkAgentProfile:
    """Statistical parameters that characterise one LLM agent personality.

    All durations are in seconds unless noted.
    """
    name: str
    description: str

    # Time-to-first-token: lognormal(mu, sigma) where median = exp(mu)
    ttft_mu: float = -1.51     # ln(0.22) → p50 ≈ 220 ms
    ttft_sigma: float = 0.9

    # Write duration: exponential(mean_secs)
    write_mean_secs: float = 0.3

    # Probability of a mid-write stall
    stall_prob: float = 0.10
    # Stall duration: Pareto(alpha, x_min) in seconds
    stall_pareto_alpha: float = 1.5
    stall_pareto_xmin: float = 0.5

    # Probability of abandoning the session (no commit)
    abandon_prob: float = 0.02

    # Probability of issuing a double-commit attempt (same payload, new session)
    double_commit_prob: float = 0.01

    # Probability of writing a large payload (bimodal control)
    large_payload_prob: float = 0.20

    # Optional: extra jitter before commit call (uniform [0, max_secs])
    pre_commit_jitter_max_secs: float = 0.05


# Named profiles matching real-world LLM deployment archetypes
_PROFILES: dict[str, DrunkAgentProfile] = {
    "fast_coder": DrunkAgentProfile(
        name="fast_coder",
        description="Fast, cheap model (GPT-4o-mini style). Low latency, small edits, rarely stalls.",
        ttft_mu=-2.12,          # ln(0.12) → p50 ≈ 120 ms
        ttft_sigma=0.6,
        write_mean_secs=0.15,
        stall_prob=0.03,
        stall_pareto_alpha=2.0,
        stall_pareto_xmin=0.2,
        abandon_prob=0.01,
        double_commit_prob=0.005,
        large_payload_prob=0.10,
        pre_commit_jitter_max_secs=0.02,
    ),
    "reasoning": DrunkAgentProfile(
        name="reasoning",
        description="Heavy reasoning model (o3/DeepSeek style). Long TTFT, large outputs, frequent stalls.",
        ttft_mu=0.095,          # ln(1.1) → p50 ≈ 1.1 s
        ttft_sigma=1.1,
        write_mean_secs=1.5,
        stall_prob=0.30,
        stall_pareto_alpha=1.2,
        stall_pareto_xmin=1.0,
        abandon_prob=0.05,
        double_commit_prob=0.02,
        large_payload_prob=0.50,
        pre_commit_jitter_max_secs=0.2,
    ),
    "unstable": DrunkAgentProfile(
        name="unstable",
        description="Flaky agent (OOMed container, poor retry logic). High abandon rate, random double commits.",
        ttft_mu=-1.51,          # ln(0.22) → p50 ≈ 220 ms
        ttft_sigma=0.9,
        write_mean_secs=0.3,
        stall_prob=0.15,
        stall_pareto_alpha=1.3,
        stall_pareto_xmin=0.8,
        abandon_prob=0.20,      # 1-in-5 chance of abandoning
        double_commit_prob=0.15,  # aggressively retries
        large_payload_prob=0.25,
        pre_commit_jitter_max_secs=0.5,
    ),
    "overthinker": DrunkAgentProfile(
        name="overthinker",
        description="Multimodal reasoning agent. Extreme write variance, multi-second stalls, large context dumps.",
        ttft_mu=0.875,          # ln(2.4) → p50 ≈ 2.4 s
        ttft_sigma=1.4,
        write_mean_secs=3.0,
        stall_prob=0.50,
        stall_pareto_alpha=1.0,   # very heavy tail
        stall_pareto_xmin=2.0,
        abandon_prob=0.08,
        double_commit_prob=0.03,
        large_payload_prob=0.70,
        pre_commit_jitter_max_secs=0.3,
    ),
    "balanced": DrunkAgentProfile(
        name="balanced",
        description="Mix-model swarm default (GPT-4-turbo style). Moderate latency and payload variance.",
        ttft_mu=-1.51,          # ln(0.22) → p50 ≈ 220 ms
        ttft_sigma=0.9,
        write_mean_secs=0.3,
        stall_prob=0.10,
        stall_pareto_alpha=1.5,
        stall_pareto_xmin=0.5,
        abandon_prob=0.02,
        double_commit_prob=0.01,
        large_payload_prob=0.20,
        pre_commit_jitter_max_secs=0.05,
    ),
}


def profile_for(name: str) -> DrunkAgentProfile:
    """Return profile by name.  Raises KeyError for unknown names."""
    try:
        return _PROFILES[name]
    except KeyError:
        available = ", ".join(sorted(_PROFILES))
        raise KeyError(f"Unknown profile {name!r}. Available: {available}") from None


def list_profiles() -> list[str]:
    return sorted(_PROFILES)


# ---------------------------------------------------------------------------
# Agent timing simulation
# ---------------------------------------------------------------------------

@dataclass
class DrunkAgentTiming:
    """Concrete timing draw for one agent run."""
    ttft_secs: float
    write_secs: float
    stall_secs: float          # 0 if no stall
    pre_commit_jitter_secs: float
    payload_bytes: int
    will_abandon: bool
    will_double_commit: bool

    @property
    def total_pre_commit_secs(self) -> float:
        return self.ttft_secs + self.write_secs + self.stall_secs + self.pre_commit_jitter_secs


def draw_timing(profile: DrunkAgentProfile, rng: random.Random) -> DrunkAgentTiming:
    """Draw a concrete set of timing values from the profile's distributions."""
    ttft = _lognormal_sample(rng, profile.ttft_mu, profile.ttft_sigma)
    write = rng.expovariate(1.0 / profile.write_mean_secs)
    stall = (
        _pareto_sample(rng, profile.stall_pareto_alpha, profile.stall_pareto_xmin)
        if rng.random() < profile.stall_prob
        else 0.0
    )
    jitter = rng.uniform(0, profile.pre_commit_jitter_max_secs)
    payload = _bimodal_payload_bytes(rng, profile.large_payload_prob)
    abandon = rng.random() < profile.abandon_prob
    double = (not abandon) and (rng.random() < profile.double_commit_prob)

    return DrunkAgentTiming(
        ttft_secs=ttft,
        write_secs=write,
        stall_secs=stall,
        pre_commit_jitter_secs=jitter,
        payload_bytes=payload,
        will_abandon=abandon,
        will_double_commit=double,
    )


# ---------------------------------------------------------------------------
# File writing helpers
# ---------------------------------------------------------------------------

def _rand_suffix(n: int = 6) -> str:
    return "".join(random.choice(string.ascii_lowercase) for _ in range(n))


def _write_payload(path: str, content: bytes) -> None:
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "wb") as f:
        f.write(content)


def _generate_content(agent_id: int, payload_bytes: int, rng: random.Random) -> bytes:
    """Generate realistic-looking source-code-sized content."""
    header = f"# agent={agent_id} ts={time.time_ns()}\n".encode()
    body_len = max(0, payload_bytes - len(header))
    # Mix printable ASCII to simulate real text edits
    chars = string.ascii_letters + string.digits + " \n    "
    body = "".join(rng.choice(chars) for _ in range(body_len)).encode()
    return header + body


# ---------------------------------------------------------------------------
# DrunkSession: wraps a real simgit Session with timing simulation
# ---------------------------------------------------------------------------

@dataclass
class DrunkAgentResult:
    agent_id: int
    profile: str
    ok: bool
    abandoned: bool
    double_committed: bool
    session_id: str | None
    duration_ms: int
    ttft_ms: int
    write_ms: int
    stall_ms: int
    payload_bytes: int
    error: str | None = None
    double_commit_ok: bool = False
    double_commit_error: str | None = None
    timing: DrunkAgentTiming | None = field(default=None, repr=False)


def run_drunk_agent(
    agent_id: int,
    profile: DrunkAgentProfile,
    socket_path: str | None,
    stress_mode: str,
    overlap_path: str,
    disjoint_file: str,
    seed: int | None = None,
    timeout_secs: float | None = None,
) -> DrunkAgentResult:
    """Run one agent with drunk timing.  All delays are real wall-clock sleeps."""
    rng = random.Random(seed if seed is not None else random.getrandbits(64))
    timing = draw_timing(profile, rng)
    start = time.monotonic()

    def elapsed_ms() -> int:
        return int((time.monotonic() - start) * 1000)

    try:
        # 1. TTFT: simulate model "thinking" before first write
        time.sleep(timing.ttft_secs)
        ttft_done = time.monotonic()

        # 2. Acquire a simgit session
        try:
            import simgit  # type: ignore
        except ImportError as exc:
            raise RuntimeError(
                "Failed to import simgit Python bindings. Run: maturin develop -m simgit-py/Cargo.toml"
            ) from exc

        session = simgit.Session.new(
            task_id=f"drunk-agent-{agent_id}-{_rand_suffix()}",
            socket_path=socket_path,
            agent_label=f"drunk-{profile.name}-{agent_id}",
        )
        session_id = session.session_id
        info = session.info()
        mount_path = info["mount_path"]

        # 3. Write phase: simulate streaming token output to the VFS
        if stress_mode == "hotspot":
            target_path = os.path.join(mount_path, overlap_path)
        else:
            # Each agent writes to its own sub-path to avoid disjoint conflicts
            target_path = os.path.join(mount_path, disjoint_file, f"agent_{agent_id:04d}.txt")

        content = _generate_content(agent_id, timing.payload_bytes, rng)

        # Simulate write taking write_secs (model streaming tokens to disk)
        time.sleep(timing.write_secs)

        # Optional mid-write stall
        if timing.stall_secs > 0:
            time.sleep(timing.stall_secs)

        _write_payload(target_path, content)
        write_done = time.monotonic()

        # 4. Abandon path: abort without committing
        if timing.will_abandon:
            try:
                session.abort()
            except Exception:
                pass
            return DrunkAgentResult(
                agent_id=agent_id,
                profile=profile.name,
                ok=False,
                abandoned=True,
                double_committed=False,
                session_id=session_id,
                duration_ms=elapsed_ms(),
                ttft_ms=int((ttft_done - start) * 1000),
                write_ms=int((write_done - ttft_done) * 1000),
                stall_ms=int(timing.stall_secs * 1000),
                payload_bytes=timing.payload_bytes,
                error="abandoned",
                timing=timing,
            )

        # 5. Pre-commit jitter
        time.sleep(timing.pre_commit_jitter_secs)

        # 6. Commit
        branch_name = f"drunk/{profile.name}/agent-{agent_id}-{_rand_suffix()}"
        message = f"drunk agent {agent_id} ({profile.name}) payload={timing.payload_bytes}B"
        kwargs: dict[str, Any] = {"branch_name": branch_name, "message": message}
        if timeout_secs is not None:
            kwargs["timeout_secs"] = timeout_secs

        session.commit(**kwargs)

        first_ok = True
        first_error = None
    except Exception as exc:
        first_ok = False
        first_error = str(exc)
        session_id = None
        ttft_done = start
        write_done = start

    # 7. Double-commit: re-issue same logical work with a new session
    double_ok = False
    double_error = None
    if first_ok and timing.will_double_commit:
        try:
            session2 = simgit.Session.new(  # type: ignore  # noqa: F821
                task_id=f"drunk-agent-{agent_id}-retry-{_rand_suffix()}",
                socket_path=socket_path,
                agent_label=f"drunk-{profile.name}-{agent_id}-retry",
            )
            info2 = session2.info()
            mount_path2 = info2["mount_path"]
            if stress_mode == "hotspot":
                target_path2 = os.path.join(mount_path2, overlap_path)
            else:
                target_path2 = os.path.join(mount_path2, disjoint_file, f"agent_{agent_id:04d}.txt")
            _write_payload(target_path2, content)  # same content
            branch2 = f"drunk/{profile.name}/agent-{agent_id}-retry-{_rand_suffix()}"
            session2.commit(branch_name=branch2, message=message)
            double_ok = True
        except Exception as exc2:
            double_error = str(exc2)

    total_ms = elapsed_ms()
    return DrunkAgentResult(
        agent_id=agent_id,
        profile=profile.name,
        ok=first_ok,
        abandoned=False,
        double_committed=timing.will_double_commit if first_ok else False,
        session_id=session_id if first_ok else None,
        duration_ms=total_ms,
        ttft_ms=int((ttft_done - start) * 1000),
        write_ms=int((write_done - ttft_done) * 1000),
        stall_ms=int(timing.stall_secs * 1000),
        payload_bytes=timing.payload_bytes,
        error=first_error,
        double_commit_ok=double_ok,
        double_commit_error=double_error,
        timing=timing,
    )


# ---------------------------------------------------------------------------
# Wave runner
# ---------------------------------------------------------------------------

import concurrent.futures
import statistics as _stats


def run_drunk_wave(
    profile_name: str | dict[str, int],
    agents: int,
    socket_path: str | None,
    stress_mode: str = "disjoint-range",
    overlap_path: str = "hotspot/shared.txt",
    disjoint_file: str = "data/drunk",
    workers: int | None = None,
    seed: int | None = None,
    timeout_secs: float | None = None,
) -> dict[str, Any]:
    """Run a wave of drunk agents and return a structured summary.

    profile_name may be a single profile name (str) or a mix dict:
      {"fast_coder": 10, "reasoning": 5, "unstable": 5}
    """
    if isinstance(profile_name, str):
        assignments: list[tuple[int, DrunkAgentProfile]] = [
            (i, profile_for(profile_name)) for i in range(agents)
        ]
    else:
        assignments = []
        idx = 0
        for pname, count in profile_name.items():
            prof = profile_for(pname)
            for _ in range(count):
                assignments.append((idx, prof))
                idx += 1
        agents = len(assignments)

    pool_size = workers or min(agents, 64)
    base_seed = seed if seed is not None else random.getrandbits(32)

    started = time.monotonic()
    results: list[DrunkAgentResult] = []

    with concurrent.futures.ThreadPoolExecutor(max_workers=pool_size) as pool:
        futs = {
            pool.submit(
                run_drunk_agent,
                agent_id,
                prof,
                socket_path,
                stress_mode,
                overlap_path,
                disjoint_file,
                base_seed + agent_id,
                timeout_secs,
            ): agent_id
            for agent_id, prof in assignments
        }
        for fut in concurrent.futures.as_completed(futs):
            try:
                results.append(fut.result())
            except Exception as exc:
                aid = futs[fut]
                results.append(DrunkAgentResult(
                    agent_id=aid, profile="unknown", ok=False, abandoned=False,
                    double_committed=False, session_id=None,
                    duration_ms=0, ttft_ms=0, write_ms=0, stall_ms=0,
                    payload_bytes=0, error=str(exc),
                ))

    elapsed_ms = int((time.monotonic() - started) * 1000)
    results.sort(key=lambda r: r.agent_id)

    ok_results = [r for r in results if r.ok]
    fail_results = [r for r in results if not r.ok]
    abandoned = [r for r in results if r.abandoned]
    double_commits = [r for r in results if r.double_committed]

    durations = [r.duration_ms for r in results]
    ttfts = [r.ttft_ms for r in ok_results] or [0]

    def _pct(vals: list[int], p: float) -> int:
        if not vals:
            return 0
        s = sorted(vals)
        return s[int((len(s) - 1) * p)]

    return {
        "elapsed_ms": elapsed_ms,
        "agents": agents,
        "profile": profile_name if isinstance(profile_name, str) else profile_name,
        "stress_mode": stress_mode,
        "seed": base_seed,
        "successes": len(ok_results),
        "failures": len(fail_results),
        "abandoned": len(abandoned),
        "double_commits": len(double_commits),
        "double_commit_ok": sum(1 for r in double_commits if r.double_commit_ok),
        "latency_ms": {
            "p50": _pct(durations, 0.50),
            "p95": _pct(durations, 0.95),
            "p99": _pct(durations, 0.99),
            "max": max(durations) if durations else 0,
        },
        "ttft_ms": {
            "p50": _pct(ttfts, 0.50),
            "p95": _pct(ttfts, 0.95),
        },
        "results": [r.__dict__ for r in results],
    }


# ---------------------------------------------------------------------------
# CLI entry point (standalone wave smoke-test)
# ---------------------------------------------------------------------------

def _main() -> int:
    import argparse
    import json
    import sys

    parser = argparse.ArgumentParser(description="Run a drunk-agent wave")
    parser.add_argument("--agents", type=int, default=10)
    parser.add_argument("--profile", default="balanced",
                        help=f"Profile name. One of: {', '.join(list_profiles())}")
    parser.add_argument("--socket", default=os.environ.get("SIMGIT_SOCKET"))
    parser.add_argument("--stress-mode", choices=["hotspot", "disjoint-range"],
                        default="disjoint-range")
    parser.add_argument("--workers", type=int, default=0)
    parser.add_argument("--seed", type=int, default=None)
    parser.add_argument("--timeout-secs", type=float, default=None)
    parser.add_argument("--report-out", default=None)
    args = parser.parse_args()

    summary = run_drunk_wave(
        profile_name=args.profile,
        agents=args.agents,
        socket_path=args.socket,
        stress_mode=args.stress_mode,
        workers=args.workers or None,
        seed=args.seed,
        timeout_secs=args.timeout_secs,
    )

    # Strip per-result timing objects (not JSON-serialisable by default)
    for r in summary.get("results", []):
        r.pop("timing", None)

    print(json.dumps({k: v for k, v in summary.items() if k != "results"}, indent=2))
    if args.report_out:
        with open(args.report_out, "w") as f:
            json.dump(summary, f, indent=2)
        print(f"Report written to {args.report_out}", file=sys.stderr)

    return 0 if summary["failures"] == summary["abandoned"] else 1


if __name__ == "__main__":
    raise SystemExit(_main())
