# Testing Guide

simgit is a single-binary CLI (`sg worktree`) with no daemon or runtime
services, so testing is correspondingly small.

## Rust

```bash
cargo build      # compile the sg binary
cargo test       # run unit tests
cargo clippy     # lint
cargo fmt -- --check
```

## Manual smoke test

```bash
sg=$(pwd)/target/debug/sg
tmp=$(mktemp -d) && cd "$tmp"
git init -q && git commit -q --allow-empty -m init

cd "$("$sg" worktree add feature-x)"   # creates + enters a CoW worktree
"$sg" worktree list                     # shows main + feature-x
"$sg" worktree list --json              # machine-readable
"$sg" worktree remove "$PWD" --force    # tears it down
```

## CoW scaling benchmarks

These measure the physical-disk and I/O properties of `sg worktree` versus
plain `git worktree`. They need a filesystem with clone support (APFS, or a
reflink-capable Linux FS). See [docs/scaling_benchmark.md](docs/scaling_benchmark.md)
for methodology and headline numbers.

```bash
tests/bench_scaling.sh          # disk scaling: N worktrees, du/df accounting
python3 tests/bench_worktree_io.py   # hot-cache stat/read/write cost
```

Run benchmarks against disposable repositories only.
