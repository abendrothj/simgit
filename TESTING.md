# Testing Guide

simgit is a single-binary CLI (canonical `simgit`, with `sg` as an equivalent
alias) with no daemon or runtime services, so testing is correspondingly small.

## Rust

```bash
cargo build      # compile the simgit and sg binaries
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

It verifies picker selection, invalid-number retry, cancellation, detached
worktree selection, terminal inheritance, and noninteractive rejection.
The macOS/Linux CI jobs run it after the Rust tests.

## Manual smoke test

Run this at an interactive terminal: the two picker lines below read a
selection from the tty and refuse to run without one (`workspace selection
requires a terminal`). Every other line is safe to paste into a script.

```bash
sg=$(pwd)/target/debug/simgit
tmp=$(mktemp -d) && cd "$tmp"
git init -q && git commit -q --allow-empty -m init

"$sg" doctor                            # identity, filesystem, CoW, worktree root
(cd / && "$sg" --json doctor)           # outside a repo: repository fields null; cow_supported null (unwritable cwd)
"$sg" run feature-x -- sh -c 'echo unfinished > scratch.txt'
"$sg" run feature-x -- cat scratch.txt  # same workspace and files
"$sg" run -- pwd                        # TTY ONLY: choose a workspace by number
"$sg" run -- sh                         # TTY ONLY: choose any workspace for another harness/shell
"$sg" list                              # branches, paths, persistence, locks
"$sg" --json list                       # machine-readable
"$sg" unlock feature-x                  # clear a stranded run lock; success when unlocked
"$sg" gc --older-than 0s --discard-dirty  # persistent workspace remains
"$sg" remove feature-x --discard-dirty --delete-branch
"$sg" --json remove feature-x           # already gone: exits 0, already_absent true
```

In a script or CI, drop the two picker lines and pass the branch explicitly.

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

These measure the physical-disk and I/O properties of `simgit add` versus
plain `git worktree`. They need a filesystem with clone support (APFS, or a
reflink-capable Linux FS). See [docs/scaling_benchmark.md](docs/scaling_benchmark.md)
for methodology and headline numbers.

Neither script is executable, so invoke them through their interpreters, the
way the overlay script above is invoked. `bench_scaling.sh` takes its inputs
from the environment; `bench_worktree_io.py` takes two positional worktrees
that hold the same untouched tracked files — one plain `git worktree`, one
`simgit add` — and it rewrites both, so create them for the run and discard
them afterwards:

```bash
simgit_repo=$(pwd)
cargo build

# disk scaling: N worktrees, du/df accounting
NFILES=400 FSIZE_KB=128 NS="1 2 4 8" bash tests/bench_scaling.sh

# hot-cache stat/read/write cost, on a disposable repository
tmp=$(mktemp -d) && cd "$tmp"
git init -q
for i in $(seq 1 200); do dd if=/dev/urandom of="f$i.bin" bs=1k count=128 2>/dev/null; done
git add . && git commit -qm init
git worktree add -q "$tmp/bench-git" HEAD
"$simgit_repo/target/debug/simgit" add bench/cow --path "$tmp/bench-cow"
python3 "$simgit_repo/tests/bench_worktree_io.py" "$tmp/bench-git" "$tmp/bench-cow"
```

Run benchmarks against disposable repositories only.
