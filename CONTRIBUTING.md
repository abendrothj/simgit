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
├── sg/               the CLI — `sg worktree add/list/remove/prune/gc`
│   └── src/commands/worktree.rs   the whole implementation
├── tests/            CoW scaling benchmarks + overlay_integration.sh (Linux)
├── packaging/        Homebrew formula
└── docs/             scaling benchmark methodology
```

## Finding something to work on

Issues labeled [good first issue](https://github.com/abendrothj/simgit/labels/good%20first%20issue) are designed for newcomers — no deep codebase knowledge needed. Issues labeled [help wanted](https://github.com/abendrothj/simgit/labels/help%20wanted) are higher-effort features we'd love help with.

## Architecture overview

simgit is a small CLI that creates real Git linked worktrees populated via
filesystem copy-on-write. It has no daemon and no runtime services — each
invocation shells out to `git` and, where supported, populates the working tree
via `clonefile`/reflink or a `fuse-overlayfs` mount from a cached baseline. The
logic lives in a single file, `sg/src/commands/worktree.rs`.

The overlay path only activates on Linux with `fuse-overlayfs`; it can't run on
macOS, so it's covered by `tests/overlay_integration.sh` in the `overlay-linux`
CI job. `SIMGIT_POPULATE=reflink|overlay|checkout` forces a populate mode.

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

## Releasing

Published as `simgit-cli` on crates.io (binary `sg`).

1. Bump `version` in the workspace `Cargo.toml`, `cargo build` to refresh the lockfile, commit.
2. Tag and push: `git tag -a vX.Y.Z -m "…" && git push origin main vX.Y.Z`.
3. `cargo publish -p simgit-cli`.
4. GitHub release: `gh release create vX.Y.Z --title vX.Y.Z --notes "…"`.
5. Homebrew: update `url`/`sha256` in `packaging/homebrew/simgit.rb`
   (`curl -sL <tarball> | shasum -a 256`) and copy it into the
   `abendrothj/homebrew-tap` repo's `Formula/simgit.rb`.

## Communication

- Open an issue before starting on anything big
- Questions welcome in issue comments
- PRs should reference the issue they address

## License

MIT — see [LICENSE](LICENSE)
