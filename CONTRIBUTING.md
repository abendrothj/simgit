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
├── sg/               the CLI — `simgit doctor|add|list|remove|run|unlock|gc|prune|repair|hook`
│   ├── src/lib.rs                 CLI surface (`run_cli`), parser and dispatch
│   ├── src/bin/simgit.rs          canonical binary — one-line wrapper
│   ├── src/bin/sg.rs              `sg` alias — same code, one-line wrapper
│   ├── src/commands/worktree.rs   command and lifecycle orchestration
│   ├── src/commands/worktree/     launch/picker, CoW baseline, overlay, index backends,
│   │                              locks, gc/prune maintenance, `hook.rs` (Claude Code adapter)
│   └── tests/                     CLI workflow, installer, and `claude_code_hook.rs` tests
├── skills/           `simgit-worktrees` agent skill
├── .claude-plugin/   Claude Code and Codex plugin manifest and marketplace entry
├── integrations/     `claude-code/hooks.json`, the hooks the plugin registers
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
launcher's PID. `simgit unlock` is the recovery verb for a lock a killed
launcher stranded. It has no override flag by design: the correct answer to a
live owner is to stop that process. Which locks it clears and which it refuses
are in [the reference](docs/reference.md#run-locks-and-recovery).

Allocation is guarded by locks that need no recovery verb. `PathClaim` and
`BranchClaim` in `sg/src/commands/worktree/locks.rs` let one allocation at a
time hold a destination path or a branch, and `BaselineCacheLock` in
`sg/src/commands/worktree/cow.rs` is held shared by an allocation while it uses
a baseline and taken exclusively, without waiting, by `prune` to drop one. The
branch claim and the cache lock are OS file locks that die with their process;
new locks should follow that pattern or fail fast, never block indefinitely.

`simgit doctor` is the preflight command agents and harnesses read before they
start work. It runs anywhere: identity, version, filesystem, and CoW support
are always reported, and the repository-dependent diagnostics are `null`
outside a Git worktree. Inside a repository it reports the repository context
and the populate mode the machine actually supports, so a caller can branch on
capability instead of guessing.
The global `--json` flag makes every other command emit machine-readable
output; `run` and `hook` reject it, because `run` streams the child's output
and the harness fixes `hook`'s. See
[the reference](docs/reference.md#machine-readable-output).

Destructive behavior is always opt-in through an explicit flag rather than a
blanket force switch. The two flags and the rules that keep them stable are in
[AGENTS.md](AGENTS.md#what-approval-gates-depend-on). Cleanup is idempotent
where that costs nothing, so at-least-once harness cleanup needs no special
case; see [removal and unlock](docs/reference.md#removal-and-unlock-semantics).

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
6. User-facing changes must be reflected in the docs that state the changed
   fact: `docs/reference.md` for command semantics, `README.md` for the
   surface and value proposition, and `docs/agent-integration.md` plus
   `skills/simgit-worktrees/SKILL.md` for the agent-facing contract

## Code style

- Follow existing patterns — look at neighboring files for conventions
- No comments unless explaining *why*, not *what*
- Error handling: use `anyhow`

## Releasing

Published as `simgit-cli` on crates.io. The crate ships two binaries: `simgit`
(canonical) and `sg`, an equivalent alias built from the same code.

1. Bump `version` in the workspace `Cargo.toml`, `cargo build` to refresh the lockfile, commit.
   If `skills/simgit-worktrees/SKILL.md` now relies on behavior new in this
   release, raise the version its Preflight section names in the same commit:
   the skill is installed from `main`, so agents read it before users upgrade.
   The Claude Code plugin ships from the same commit. Bump `version` in
   `.claude-plugin/plugin.json` every release (Claude Code caches plugins by
   version, and a test checks it equals the crate version). When the adapter's
   behaviour changes, also raise the `--requires` values and the `systemMessage`
   version in `integrations/claude-code/hooks.json` to the new version; leave
   them alone otherwise, since they are the oldest release that behaves as the
   plugin expects.
2. Push `main` and wait for its CI to pass, then tag that commit and push the
   tag: `git tag -a vX.Y.Z -m "…" && git push origin vX.Y.Z`.
   The `v*` tag triggers `.github/workflows/release.yml`. It refuses a tag
   whose commit is not on `main` or has no successful CI run, reruns the gates
   above and `cargo package`, builds the four targets (`aarch64`/`x86_64` ×
   macOS/Linux), smoke-tests the native archives, packages both binaries into
   each `sg-<target>.tar.gz`, and writes a `SHA256SUMS` manifest covering all
   four. It stages them on a draft release, checks what GitHub serves against
   the manifest, and only then publishes. A rerun refuses a release that is
   already published. `install.sh` verifies that manifest before extracting,
   so never edit or replace release assets by hand.
3. `cargo publish -p simgit-cli`.
4. Homebrew: `packaging/homebrew/simgit.rb` installs `simgit` with an `sg`
   symlink; refresh its `url`/`sha256` for the new tag
   (`curl -sL <tarball> | shasum -a 256`) and copy it into the
   `abendrothj/homebrew-tap` repo's `Formula/simgit.rb`.
5. Move the `ref` in `.claude-plugin/marketplace.json` to the new tag, only after
   the Homebrew formula serves that release, so the plugin never gets ahead of
   brew. For 0.5.0 the `ref` was set to `v0.5.0` ahead of the tag.
6. For 0.5.0 only: once `v0.5.0` is tagged and Homebrew serves it, delete the
   `PRE-RELEASE` note in `integrations/claude-code/README.md` and the one in
   `docs/agent-integration.md` (under Claude Code), and the sentence pointing
   to it in that file's Codex section.

## Communication

- Open an issue before starting on anything big
- Questions welcome in issue comments
- PRs should reference the issue they address

## License

MIT — see [LICENSE](LICENSE)
