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
import time
from dataclasses import dataclass
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


def run_agent(agent_id: int, mode: str, socket_path: str | None) -> AgentResult:
    start = time.time()
    simgit = _load_simgit_module()

    try:
        session = simgit.Session.new(
            task_id=f"stress-agent-{agent_id}-{rand_suffix()}",
            socket_path=socket_path,
            agent_label=f"stress-agent-{agent_id}",
        )

        session_id = session.session_id

        # Optional noop write spot for future extension if mount-path file writes
        # are injected by a runner wrapper.
        _ = session.info()

        if mode == "commit":
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
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    if args.agents < 1:
        print("--agents must be >= 1", file=sys.stderr)
        return 2

    results: list[AgentResult] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        futs = [pool.submit(run_agent, i, args.mode, args.socket) for i in range(args.agents)]
        for fut in concurrent.futures.as_completed(futs):
            results.append(fut.result())

    results.sort(key=lambda r: r.agent_id)
    successes = sum(1 for r in results if r.ok)
    failures = len(results) - successes

    summary = {
        "agents": args.agents,
        "mode": args.mode,
        "successes": successes,
        "failures": failures,
        "results": [r.__dict__ for r in results],
    }

    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        print(
            f"agents={args.agents} mode={args.mode} "
            f"successes={successes} failures={failures}"
        )
        for r in results:
            if not r.ok:
                print(f"  agent={r.agent_id} error={r.error}")

    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
