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
├── sg/               the CLI — `simgit doctor|add|list|remove|run|unlock|gc|prune|repair`
│   ├── src/lib.rs                 CLI surface (`run_cli`), parser and dispatch
│   ├── src/bin/simgit.rs          canonical binary — one-line wrapper
│   ├── src/bin/sg.rs              `sg` alias — same code, one-line wrapper
│   ├── src/commands/worktree.rs   command and lifecycle orchestration
│   └── src/commands/worktree/     launch/picker, CoW baseline, overlay, index backends
├── skills/           `simgit-worktrees` agent skill
├── tests/            CoW scaling benchmarks + reflink/overlay integration (Linux)
├── packaging/        Homebrew formula
└── docs/             agent integration guide, scaling benchmark methodology
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

`simgit run` is a single flat verb — there is no nested `worktree` namespace.
`sg/src/commands/worktree/launch.rs` handles workspace selection and child
execution; it never stores a preferred harness. Omitting the branch opens a
terminal-only picker; scripts must pass a branch and `--` before the command.
New workspaces are persistent; disposable runs opt in with `--ephemeral`. `gc`
requires `--include-persistent` to select persistent workspaces.

`run` holds a lock on its workspace for the child's lifetime, recording the
launcher's PID in the lock file. `simgit unlock [TARGET]` is the recovery verb
for a lock stranded by a killed launcher: it clears Git's worktree lock and the
main checkout's `.git/simgit-run.lock`, succeeds on an unlocked target, and
refuses while the recorded owner PID is still alive. It has no override flag by
design — the correct answer to a live owner is to stop that process.

`simgit doctor` is the preflight command agents and harnesses read before they
start work. It runs anywhere: identity, version, filesystem, and CoW support
are always reported, and the repository-dependent diagnostics are `null`
outside a Git worktree. Inside a repository it reports the repository context
and the populate mode the machine actually supports, so a caller can branch on
capability instead of guessing.
The global `--json` flag makes every command emit machine-readable output,
except `run`, which rejects it because it streams the child's output verbatim.

Destructive behavior is always opt-in through an explicit flag rather than a
blanket force switch: `--discard-dirty` permits removing a worktree with
uncommitted changes, and `--delete-unmerged` (alongside `--delete-branch`)
permits deleting a branch that is not merged.

Cleanup is idempotent where that costs nothing: `remove` on a target that no
longer resolves to a worktree exits successfully with `already_absent: true`,
so at-least-once harness cleanup does not have to special-case a workspace it
already removed.

The overlay path only activates on Linux with `fuse-overlayfs`; it can't run on
macOS, so it's covered by `tests/overlay_integration.sh` in the `overlay-linux`
CI job. `SIMGIT_POPULATE=reflink|overlay|checkout` forces a populate mode.

(An earlier daemon-based architecture — session manager, borrow registry, delta
store, RPC server, VFS backends — was retired; see the README "History"
section and Git history.)

## Before submitting a PR

1. Format: `cargo fmt --all -- --check`
2. Lint: `cargo clippy --all-targets --locked -- -D warnings`
3. Test: `cargo test --all --locked` and `python3 tests/cli_picker.py`
   (run `cargo build --locked` first if testing the picker on its own)
4. Linux only: `tests/reflink_integration.sh` and `tests/overlay_integration.sh`
5. Keep commits focused — one concept per commit
6. User-facing changes must be reflected in `README.md`,
   `skills/simgit-worktrees/SKILL.md`, and `docs/agent-integration.md`

## Code style

- Follow existing patterns — look at neighboring files for conventions
- No comments unless explaining *why*, not *what*
- Error handling: use `anyhow`

## Releasing

Published as `simgit-cli` on crates.io. The crate ships two binaries: `simgit`
(canonical) and `sg`, an equivalent alias built from the same code.

1. Bump `version` in the workspace `Cargo.toml`, `cargo build` to refresh the lockfile, commit.
2. Tag and push: `git tag -a vX.Y.Z -m "…" && git push origin main vX.Y.Z`.
   The `v*` tag triggers `.github/workflows/release.yml`, which reruns the gates
   above, builds the four targets (`aarch64`/`x86_64` × macOS/Linux), packages
   both binaries into each `sg-<target>.tar.gz`, and publishes a `SHA256SUMS`
   manifest covering all four archives. `install.sh` verifies that manifest
   before extracting, so never edit or replace release assets by hand.
3. `cargo publish -p simgit-cli`.
4. Homebrew: `packaging/homebrew/simgit.rb` installs `simgit` with an `sg`
   symlink; refresh its `url`/`sha256` for the new tag
   (`curl -sL <tarball> | shasum -a 256`) and copy it into the
   `abendrothj/homebrew-tap` repo's `Formula/simgit.rb`.

## Communication

- Open an issue before starting on anything big
- Questions welcome in issue comments
- PRs should reference the issue they address

## License

MIT — see [LICENSE](LICENSE)
