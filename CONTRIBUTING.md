# Contributing to simgit

## Getting started

```bash
git clone https://github.com/abendrothj/simgit.git
cd simgit
cargo build --workspace
cargo test --workspace
```

You'll need:
- Rust stable (1.75+)
- Git (any recent version)
- macOS: no extra dependencies (APFS `clonefile`)
- Linux: no extra dependencies (`overlayfs`, with a reflink-clone fallback)

## Project structure

```
simgit/
├── sg/               the CLI — `sg worktree add/list/remove/prune`
│   └── src/commands/worktree.rs   the whole implementation
├── tests/            CoW scaling benchmarks (bench_scaling.sh, bench_worktree_io.py)
└── docs/             scaling benchmark methodology
```

## Finding something to work on

Issues labeled [good first issue](https://github.com/abendrothj/simgit/labels/good%20first%20issue) are designed for newcomers — no deep codebase knowledge needed. Issues labeled [help wanted](https://github.com/abendrothj/simgit/labels/help%20wanted) are higher-effort features we'd love help with.

## Architecture overview

simgit is a small CLI that creates real Git linked worktrees populated via
filesystem copy-on-write. It has no daemon and no runtime services — each
invocation shells out to `git` and, where supported, clones the working tree
with `clonefile`/reflink from a cached baseline. The logic lives in a single
file, `sg/src/commands/worktree.rs`.

(An earlier daemon-based architecture — session manager, borrow registry, delta
store, RPC server, VFS backends — was retired; see the README "History"
section and Git history.)

## Before submitting a PR

1. Run tests: `cargo test`
2. Lint (if available): `cargo clippy`
3. Check formatting: `cargo fmt -- --check`
4. Keep commits focused — one concept per commit
5. Update docs if your change affects user-facing behavior

## Code style

- Follow existing patterns — look at neighboring files for conventions
- No comments unless explaining *why*, not *what*
- Error handling: use `anyhow`

## Communication

- Open an issue before starting on anything big
- Questions welcome in issue comments
- PRs should reference the issue they address

## License

MIT — see [LICENSE](LICENSE)
