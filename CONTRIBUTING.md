# Contributing to simgit

## Getting started

```bash
git clone https://github.com/abendrothj/simgit.git
cd simgit
cargo build --workspace
cargo test --workspace
```

You'll need:
- Rust stable (the current stable toolchain is used in CI)
- Git (any recent version)
- macOS: no extra dependencies (APFS `clonefile`)
- Linux: native reflink where supported; install `fuse-overlayfs` for the
  overlay fallback, otherwise a normal checkout is used

## Project structure

```
simgit/
├── sg/               the CLI — `sg run` and `sg worktree add/run/list/remove/prune/gc/repair`
│   ├── src/commands/worktree.rs   command and lifecycle orchestration
│   └── src/commands/worktree/     launch/picker, CoW baseline, and overlay backends
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
command lifecycle lives in `sg/src/commands/worktree.rs`; filesystem-specific
CoW baseline and overlay recovery logic lives in the adjacent backend modules.

The `sg run` alias and `sg worktree run` share the same argument parser and
implementation. `sg/src/commands/worktree/launch.rs` handles workspace
selection and child execution; it never stores a preferred harness. Omitting
the branch opens a terminal-only picker; scripts must pass a branch and `--`
before the command. New workspaces are persistent; disposable runs opt in with
`--ephemeral`. GC requires `--include-persistent` to select persistent workspaces.

The overlay path only activates on Linux with `fuse-overlayfs`; it can't run on
macOS, so it's covered by `tests/overlay_integration.sh` in the `overlay-linux`
CI job. `SIMGIT_POPULATE=reflink|overlay|checkout` forces a populate mode.

(An earlier daemon-based architecture — session manager, borrow registry, delta
store, RPC server, VFS backends — was retired; see the README "History"
section and Git history.)

## Before submitting a PR

1. Run tests: `cargo test --workspace --locked` and `python3 tests/cli_picker.py`
   (run `cargo build --locked` first if testing the picker on its own)
2. Lint: `cargo clippy --all-targets --locked -- -D warnings`
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
