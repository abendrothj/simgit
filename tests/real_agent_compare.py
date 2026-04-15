#!/usr/bin/env python3

"""Compare two real-agent harness reports and produce a decision summary."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any, Dict


def load_report(path: str) -> Dict[str, Any]:
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def metrics(report: Dict[str, Any]) -> Dict[str, float]:
    summary = report.get("summary", {})
    weakness = report.get("weakness_summary", {})
    latency = weakness.get("commit_latency_ms", {})
    return {
        "success_rate": float(summary.get("success_rate", 0.0)),
        "conflict_rate": float(weakness.get("conflict_rate", 0.0)),
        "retry_rate_per_success": float(weakness.get("retry_rate_per_success", 0.0)),
        "p95_ms": float(latency.get("p95", 0.0)),
    }


def decide(disjoint: Dict[str, float], hotspot: Dict[str, float]) -> str:
    hotspot_conflict = hotspot["conflict_rate"]
    disjoint_conflict = disjoint["conflict_rate"]
    hotspot_success = hotspot["success_rate"]
    disjoint_success = disjoint["success_rate"]

    if hotspot_conflict >= 0.30 and disjoint_conflict <= 0.05:
        return "edit_isolation_or_ast_locking"
    if hotspot["p95_ms"] > 5000 and hotspot_conflict < 0.20:
        return "scheduler_or_backpressure"
    if hotspot_success < disjoint_success and hotspot_conflict >= 0.15:
        return "task_routing_or_overlap_policy"
    return "collect_more_data"


def main() -> None:
    parser = argparse.ArgumentParser(description="Compare disjoint vs hotspot real-agent runs")
    parser.add_argument("--disjoint", required=True, help="Path to disjoint JSON report")
    parser.add_argument("--hotspot", required=True, help="Path to hotspot JSON report")
    parser.add_argument("--out", help="Optional output path for decision JSON")
    args = parser.parse_args()

    disjoint_report = load_report(args.disjoint)
    hotspot_report = load_report(args.hotspot)

    disjoint_metrics = metrics(disjoint_report)
    hotspot_metrics = metrics(hotspot_report)
    recommendation = decide(disjoint_metrics, hotspot_metrics)

    result = {
        "inputs": {
            "disjoint": str(Path(args.disjoint)),
            "hotspot": str(Path(args.hotspot)),
        },
        "disjoint": disjoint_metrics,
        "hotspot": hotspot_metrics,
        "recommendation": recommendation,
        "rationale": [
            "Compare overlap-heavy and non-overlap controls.",
            "Prioritize isolation when conflict delta is large and disjoint remains clean.",
        ],
    }

    print("REAL_AGENT_DECISION")
    print(json.dumps(result, indent=2))

    if args.out:
        with open(args.out, "w", encoding="utf-8") as handle:
            json.dump(result, handle, indent=2)
        print(f"\nWrote decision JSON to: {args.out}")


if __name__ == "__main__":
    main()
