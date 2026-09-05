# Testing Guide

simgit is a single-binary CLI (`sg run` / `sg worktree`) with no daemon or runtime
services, so testing is correspondingly small.

## Rust

```bash
cargo build      # compile the sg binary
cargo test       # run unit and CLI integration tests
cargo clippy     # lint
cargo fmt -- --check
```

The CLI checks cover create/reuse/attach, persistence and GC selection, active
workspace locks, failed launches, option validation, and argument passthrough.
For the numbered picker, run the stdlib-only PTY test on macOS or Linux:

```bash
cargo build --locked
python3 tests/cli_picker.py
```

It verifies both command spellings, invalid-number retry, cancellation,
detached worktree selection, terminal inheritance, and noninteractive rejection.
The macOS/Linux CI jobs run it after the Rust tests.

## Manual smoke test

```bash
sg=$(pwd)/target/debug/sg
tmp=$(mktemp -d) && cd "$tmp"
git init -q && git commit -q --allow-empty -m init

"$sg" run feature-x -- sh -c 'echo unfinished > scratch.txt'
"$sg" run feature-x -- cat scratch.txt  # same workspace and files
"$sg" run -- pwd                       # choose a workspace by number
"$sg" run -- sh                        # choose any workspace for another harness/shell
"$sg" worktree list                    # branches, paths, persistence, locks
"$sg" worktree list --json             # machine-readable
"$sg" worktree gc --older-than 0s --force  # persistent workspace remains
"$sg" worktree remove feature-x --force --delete-branch
```

## Overlay mode (Linux)

The `fuse-overlayfs` populate mode can't run on macOS, so it's exercised by an
integration script (also run by the `overlay-linux` CI job):

```bash
sudo apt-get install -y fuse-overlayfs
cargo build --release
SG="$PWD/target/release/sg" bash tests/overlay_integration.sh
```

`SIMGIT_POPULATE=reflink|overlay|checkout` forces a populate mode. The script
also unmounts live overlays to verify `repair`, stale-state cleanup, upperdir
preservation, reuse/remount through `run`, failed-unmount protection, branch
cleanup, and normal remove/GC teardown.

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
