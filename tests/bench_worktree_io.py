#!/usr/bin/env python3
"""Compare native file I/O in a Git worktree and an sg CoW worktree.

Both arguments must contain the same untouched tracked files. The benchmark
warms metadata/data caches, alternates execution order, and reports ranges.
The durable-write case runs last because it intentionally modifies each tree.
"""

from __future__ import annotations

import argparse
import os
import time
from pathlib import Path
from typing import Callable


def files_under(root: Path) -> list[Path]:
    return sorted(path for path in root.rglob("*") if path.is_file() and ".git" not in path.parts)


def timed(operation: Callable[[], int]) -> tuple[float, int]:
    started = time.perf_counter_ns()
    amount = operation()
    return (time.perf_counter_ns() - started) / 1_000_000_000, amount


def stat_all(paths: list[Path]) -> int:
    for path in paths:
        path.stat()
    return len(paths)


def read_prefixes(paths: list[Path]) -> int:
    total = 0
    for path in paths:
        with path.open("rb") as handle:
            total += len(handle.read(4096))
    return total


def read_all(paths: list[Path]) -> int:
    total = 0
    for path in paths:
        total += len(path.read_bytes())
    return total


def durable_first_writes(paths: list[Path]) -> int:
    payload = b"simgit-first-write".ljust(4096, b"\0")
    for path in paths:
        with path.open("r+b", buffering=0) as handle:
            handle.write(payload)
            os.fsync(handle.fileno())
    return len(paths)


def ranges(
    left: list[Path], right: list[Path], operation: Callable[[list[Path]], int], rounds: int
) -> tuple[list[tuple[float, int]], list[tuple[float, int]]]:
    left_results: list[tuple[float, int]] = []
    right_results: list[tuple[float, int]] = []
    for round_number in range(rounds):
        order = ((left, left_results), (right, right_results))
        if round_number % 2:
            order = tuple(reversed(order))
        for paths, results in order:
            results.append(timed(lambda paths=paths: operation(paths)))
    return left_results, right_results


def span(values: list[float]) -> str:
    return f"{min(values):.2f}–{max(values):.2f}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("git_worktree", type=Path)
    parser.add_argument("sg_worktree", type=Path)
    parser.add_argument("--rounds", type=int, default=6)
    args = parser.parse_args()

    git_files = files_under(args.git_worktree)
    sg_files = files_under(args.sg_worktree)
    if not git_files or len(git_files) != len(sg_files):
        raise SystemExit("worktrees must contain the same non-zero number of files")

    # Warm both sides before collecting hot-cache measurements.
    stat_all(git_files)
    stat_all(sg_files)
    read_all(git_files)
    read_all(sg_files)

    print(f"files: {len(git_files)}")
    print("operation                 git worktree       sg CoW worktree")

    git, sg = ranges(git_files, sg_files, stat_all, args.rounds)
    print(
        f"stat (us/file)            {span([s / n * 1e6 for s, n in git]):18} "
        f"{span([s / n * 1e6 for s, n in sg])}"
    )

    git, sg = ranges(git_files, sg_files, read_prefixes, args.rounds)
    print(
        f"read 4KiB (us/file)       {span([s / len(git_files) * 1e6 for s, _ in git]):18} "
        f"{span([s / len(sg_files) * 1e6 for s, _ in sg])}"
    )

    git, sg = ranges(git_files, sg_files, read_all, args.rounds)
    gib = 1024**3
    print(
        f"full read (GiB/s)         {span([n / gib / s for s, n in git]):18} "
        f"{span([n / gib / s for s, n in sg])}"
    )

    git_seconds, _ = timed(lambda: durable_first_writes(git_files))
    sg_seconds, _ = timed(lambda: durable_first_writes(sg_files))
    print(
        f"write 4KiB+fsync (ms/file) {git_seconds / len(git_files) * 1e3:18.3f} "
        f"{sg_seconds / len(sg_files) * 1e3:.3f}"
    )


if __name__ == "__main__":
    main()
