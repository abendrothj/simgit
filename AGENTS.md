# Agent working agreement

This file has two parts: rules for agents contributing to this repository, and
the canonical simgit worktree policy this project publishes for autonomous
agents working in any repository.

## Contributing to this repository

### Gates to run before yielding

Run all four locally; CI runs the same set:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked
python3 tests/cli_picker.py
```

`tests/reflink_integration.sh` and `tests/overlay_integration.sh` exercise
Linux-only filesystem behavior (reflink copies and overlay mounts). They run on
Linux and in CI; do not treat them as gates on macOS.

### Where the code lives

- `sg/src/lib.rs` defines the whole CLI surface and exposes `pub fn run_cli`.
- `sg/src/bin/simgit.rs` and `sg/src/bin/sg.rs` are one-line wrappers around it.
  `simgit` is the canonical executable name; `sg` is an equivalent alias built
  from the same code. Use `simgit` in docs and examples.
- Command lifecycle lives in `sg/src/commands/worktree.rs`; the backends live in
  `sg/src/commands/worktree/{cow,overlay,launch,index}.rs`.

### Command surface invariants

- The surface is flat:
  `simgit [--json] doctor|add|list|remove|run|unlock|gc|prune|repair`.
  There is no `worktree` namespace.
- `--json` is global only; never add a per-subcommand `--json`.
- Location is always `--path`.
- There is no `--force`. `--discard-dirty` permits removing a worktree with
  uncommitted changes, and `--delete-unmerged` permits deleting an unmerged
  branch and requires `--delete-branch`/`--delete-branches` alongside it.
- `doctor` exits 0 outside a Git worktree: identity, version, and filesystem are
  always reported and every repository-dependent field is `null` (with no stale
  registrations) instead of an error. `cow_supported` is `true`/`false` when a
  probe could be written in the current directory and `null` when one could not;
  never report an unprobed directory as `false`.
- `add` takes an optional branch (omitted means a generated `agent/<uuid>`) and
  `--detach` for a branchless worktree. `add --json` returns `worktree`, `path`,
  `cleanup_token`, `branch`, `base`, `mode`, and `ephemeral`. `--path` names the
  worktree directory itself: an existing empty directory is accepted, a
  non-empty one is refused, and two concurrent allocations to one path never
  both succeed.
- Every worktree path simgit prints or returns is symlink-resolved and lexically
  normalized, so one worktree always has exactly one string form. That includes
  `remove`'s `removed` whenever the target resolved to a real worktree. Only
  when nothing resolves do `remove`'s `removed` and `unlock`'s `unlocked` repeat
  the target verbatim; never invent a resolved form for a worktree that is gone.
- Every `git` child process is spawned with an explicit working directory, never
  an inherited one: these commands routinely delete the caller's cwd.
- `run` exits with the child's exit status, or `128 + signal` when a signal
  killed the child. The retained-workspace notice goes to stderr.
- `gc` explains itself: every worktree it considered but did not reap appears in
  `skipped` with a reason — `persistent`, `dirty`, `locked`, or
  `recently-active`. `--older-than` is an idle-age filter defaulting to 24h.
- `prune` reports the stale Git registrations it removed in
  `pruned_registrations`; `--all` reclaims every cached baseline immediately.
- `unlock [TARGET]` clears a stranded `run` lock. A linked worktree's lock is
  Git's own `.git/worktrees/<slug>/locked`; `.git/simgit-run.lock` is used only
  when `run` targets the main checkout. It succeeds on a target that is not
  locked and on one that no longer exists, and refuses while the lock's recorded
  owner PID is alive; there is no override flag, and never add one.
- `remove` is idempotent: an already-absent target exits 0 with
  `already_absent: true` rather than failing. With `--delete-branch` that holds
  for a branch-name target, while an already-absent path target is an error,
  because the branch such a path once held is unknowable. A directory Git no
  longer registers is not a worktree, however much it looks like one: an empty
  one is already-absent and is cleaned up where the filesystem allows, and a
  non-empty one is refused rather than deleted.
- `remove --commit` commits before it removes, and a commit that succeeded is
  never rolled back when a later step fails. Such a failure names the commit it
  kept, so a caller can distinguish it from one that committed nothing, and
  `commit` in `remove --json` carries that hash on success. Retrying is safe
  because `--commit` commits only what the worktree still holds.
- A `--json` failure emits no JSON: nonzero exit, empty stdout, one line of
  diagnostic text on stderr.

### What approval gates depend on

Tools that sit in front of simgit and decide which invocations need human
approval — HOL Guard is the first — classify an argv without running it. That
only works if the destructive surface is small, named, and stable, so treat
this as a compatibility contract rather than a description:

- Exactly two flags destroy work that Git cannot give back:
  `--discard-dirty` (uncommitted and untracked files in a worktree, on `remove`
  and `gc`) and `--delete-unmerged` (branch deletion past the merged check, on
  `remove` and `gc`). Never rename them, and never widen what they cover.
- Any new operation that can destroy uncommitted work or unmerged commits MUST
  be gated behind one of those two flags. A third destructive flag is a last
  resort, and adding one means updating this list in the same change.
- Without those flags no command destroys work. `remove` and `gc` refuse dirty
  worktrees, `gc` reaps only ephemeral worktrees idle at least `--older-than`
  and skips locked ones, branch deletion stops at the merged check, `prune`
  drops only caches that rematerialize, `repair` only remounts, and `unlock`
  refuses a live owner with no override. Gates classify `doctor`, `list`,
  `unlock`, `prune`, `repair` and flagless `add`/`remove`/`gc` as automatic on
  that basis; a change that breaks it silently turns an approved command
  destructive.
- `run` is the exception by design: it executes caller-supplied argv after
  `--`, so the argv, not the `run` invocation, is what a gate must judge.
- Destructiveness is decidable from argv alone. Never make it depend on
  configuration, an environment variable, or a prompt simgit answers itself.
- The tests that pin this are `gc_skips_dirty_worktrees_without_discard_dirty`,
  `gc_retains_unmerged_branches_without_delete_unmerged`,
  `running_commands_are_protected_from_gc_and_remove` and
  `unlock_clears_a_stranded_lock_and_refuses_a_live_one`. They are the
  guarantee, so never weaken one to accommodate a change.

### Documentation is part of the change

Each documentation file owns a different audience, and a behavior change must
update the ones that state the changed fact, in the same change:

- `docs/reference.md` — the detailed semantics of every command. Any change to
  flags, output, defaults, or error conditions lands here.
- `README.md` — the overview, the command table, and the disk numbers. Update
  it when the command surface or the value proposition changes, not for
  detail that `docs/reference.md` owns.
- `docs/agent-integration.md` — the allocator/provider contract for harness
  integrators, including the JSON field tables they depend on.
- `skills/simgit-worktrees/SKILL.md` — the model-facing version of that policy.

Restating one fact in several of these is how they drift. Prefer a link to a
second copy.

## simgit worktrees for autonomous agents

- The agent harness owns simgit executable discovery, identity preflight,
  worktree allocation, agent launch, and cleanup. Agents must not orchestrate
  this infrastructure when the harness provides the simgit allocator/provider.
- Use the global `--json` flag for preflight and allocation. Prefer `command -v
  simgit`, fall back to `command -v sg`, and accept the selected executable only
  after its top-level `doctor --json` output identifies it as simgit.
- Allocate every agent worktree outside the source repository. Launch the agent
  with its working directory set to the absolute `path` returned by
  `simgit add --json`.
- Mark disposable automation `--ephemeral`. Ephemeral means eligible for safe
  cleanup; it is not permission to discard dirty files or unmerged commits.
- Inspect the created worktree's JSON `mode`. Use `--require-cow` only when a
  `git-checkout` fallback would invalidate the task, such as by exceeding its
  disk budget.
- Pass the returned `cleanup_token` unchanged to safe removal or cleanup. Never
  add `--discard-dirty` or `--delete-unmerged` without explicit user or
  operator authority; preserve work when safe cleanup refuses it.
- Recover a crashed agent before reuse or cleanup: confirm the child process is
  gone, clear the stranded `run` lock with `simgit unlock <path>`, then reuse
  or safely remove the workspace. `simgit unlock` succeeds on a target that is
  not locked or no longer exists, and `simgit remove` on an already-absent
  target succeeds and reports `already_absent: true`, so retried cleanup is
  safe.
- Agents commit normally and integrate their branches through normal Git
  merges. simgit supplies isolated worktrees, not a replacement merge or
  coordination protocol.

`CLAUDE.md` and other agent configuration files should reference this same
block rather than maintaining a divergent copy; see
[docs/agent-integration.md](docs/agent-integration.md) for the canonical
source and the full allocator/provider contract.
