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
- Each command lives in its own module under `sg/src/commands/worktree/`:
  `add.rs`, `remove.rs`, `registry.rs` (`list` and target lookup), `launch.rs`
  (`run`), `maintenance.rs` (`gc`, `prune`, `repair`), `locks.rs` (`unlock`)
  and `doctor.rs`. `repo.rs` holds repository discovery and the `git` helpers,
  `mod.rs` the shared types, and `{cow,overlay,index}.rs` the backends.
- `update` and its notice live in `sg/src/commands/update/`: `mod.rs` (the
  command), `release.rs` (versions, checksums, signatures) and `notice.rs`.
- Unit tests sit at the bottom of the file they test; their shared fixtures
  are in `sg/src/commands/worktree/test_support.rs`.
- `simgit hook` (the Claude Code adapter) lives in
  `sg/src/commands/worktree/hook.rs`.

### Command surface invariants

- The surface is flat:
  `simgit [--json] doctor|add|list|remove|run|unlock|gc|prune|repair|hook|update`.
  There is no `worktree` namespace.
- `--json` is global only; never add a per-subcommand `--json`. `hook` rejects
  it, like `run`, because the harness fixes the output.
- Location is always `--path`.
- There is no `--force`. `--discard-dirty` permits removing a worktree with
  uncommitted changes, and `--delete-unmerged` permits deleting an unmerged
  branch and requires `--delete-branch`/`--delete-branches` alongside it.
- `doctor` exits 0 outside a Git worktree: identity, version, and filesystem are
  always reported and every repository-dependent field is `null` (with no stale
  registrations) instead of an error. `cow_supported` is `true`/`false` when a
  probe could be written in the current directory and `null` when one could not;
  never report an unprobed directory as `false`.
- `add` takes an optional branch (omitted means a generated `agent/<uuid>`),
  `--detach` for a branchless worktree, and `--attach <branch>` to check out an
  existing branch at its current commit without moving it. `--attach` refuses a
  missing branch and one already checked out anywhere, so a `cleanup_token`
  always names a worktree that allocation created. `add --json` returns
  `worktree`, `path`, `cleanup_token`, `branch`, `base`, `mode`, and
  `ephemeral`. `--path` names the worktree directory itself: an existing empty
  directory is accepted, a non-empty one is refused, and two concurrent
  allocations to one path, or for one branch, never both succeed.
- Every worktree path simgit prints or returns is symlink-resolved and lexically
  normalized, so one worktree always has exactly one string form. That includes
  `remove`'s `removed` whenever the target resolved to a real worktree. Only
  when nothing resolves do `remove`'s `removed` and `unlock`'s `unlocked` repeat
  the target verbatim; never invent a resolved form for a target Git no
  longer registers.
- Every `git` child process is spawned with an explicit working directory, never
  an inherited one: these commands routinely delete the caller's cwd.
- `run` exits with the child's exit status, `128 + signal` when a signal
  killed the child, `126` when it cannot be executed, or `127` when it cannot
  be found. The retained-workspace notice goes to stderr.
- `gc` explains itself: every worktree it considered but did not reap appears in
  `skipped` with a reason — `persistent`, `dirty`, `locked`,
  `unreached-commits` (a detached HEAD no ref reaches; no override),
  `recently-active`, `status-failed` (its state could not be read), or
  `remove-failed` (its removal failed). `--older-than` is an idle-age filter
  defaulting to 24h.
- `prune` reports the stale Git registrations it removed in
  `pruned_registrations`, and under `--json` those it kept because they may
  hold work in `kept_registrations`, never on stderr. Plain `prune` drops a
  cached baseline that has gone unused for seven days, or whose commit no ref
  and no detached worktree HEAD reaches; `--all` reclaims every cached
  baseline immediately. Neither removes a baseline an allocation is using or
  waits for one: plain `prune` then keeps the cache and reports
  `cache_busy: true`, and `--all` fails, to be retried.
- `unlock [TARGET]` clears simgit's own stranded locks, `run` and allocation
  alike: those whose content starts with `simgit:`. A linked worktree's lock is
  Git's own `.git/worktrees/<slug>/locked`; `.git/simgit-run.lock` is used only
  when `run` targets the main checkout. It succeeds on a target that is not
  locked and on one that no longer exists, and refuses while the lock's recorded
  owner PID is alive. It also refuses any lock not written by simgit, such as
  one set with `git worktree lock`, naming Git's reason and `git worktree
  unlock`; `run` reports that reason too. There is no override flag, and never
  add one.
- `remove` is idempotent: an already-absent target exits 0 with `already_absent:
  true` rather than failing. A worktree whose directory was deleted by hand is
  already absent while Git still registers it: `remove` drops that one
  registration, and `--delete-branch` deletes its branch under the merged check,
  by branch or by path. One whose registration holds staged changes, an
  operation in progress, submodule repositories or refs of its own is dirty
  and needs `--discard-dirty`. A locked
  registration, and a detached one whose HEAD no ref reaches, are refused and
  kept. With `--delete-branch` the idempotence
  holds for a branch-name target, while a path target Git no longer registers is
  an error, because the branch it once held is unknowable. A directory Git no
  longer registers is not a worktree, however much it looks like one: an empty
  one is already-absent and is cleaned up where the filesystem allows, and a
  non-empty one is refused rather than deleted.
- `remove --commit` commits before it removes, and a commit that succeeded is
  never rolled back when a later step fails. Such a failure names the commit it
  kept, so a caller can distinguish it from one that committed nothing, and
  `commit` in `remove --json` carries that hash on success. Retrying is safe
  because `--commit` commits only what the worktree still holds. On a dirty
  detached worktree `--commit` refuses before committing, naming
  `git switch -c` first, since its commit would be one no ref reaches.
- `update` replaces only a binary simgit installed (never Homebrew's or
  cargo's), and only after the release's signature against `sg/minisign.pub`,
  its checksum and its own `--version` all agree; `update --check` changes
  nothing. The update notice never appears under `--json`, `run`, `update`
  or a worktree hook, under `CI`, or when stderr is not a terminal; the
  Claude Code `SessionStart` hook returns it as a `systemMessage` instead.
- A `--json` failure emits no JSON: nonzero exit, empty stdout, one line of
  diagnostic text on stderr.

### What approval gates depend on

Tools that sit in front of simgit and decide which invocations need human
approval — HOL Guard is the first — classify an argv without running it. That
only works if the destructive surface is small, named, and stable, so treat
this as a compatibility contract rather than a description:

- Exactly two flags destroy work that Git cannot give back:
  `--discard-dirty` (uncommitted and untracked files in a worktree, on `remove`,
  `gc` and `hook`) and `--delete-unmerged` (branch deletion past the merged
  check, on `remove` and `gc`). Never rename them, and never widen what they
  cover. `hook` never takes `--delete-unmerged`; it may delete a branch the
  merged check would keep, but only when no commit on it becomes unreachable.
- Any new operation that can destroy uncommitted work or unmerged commits MUST
  be gated behind one of those two flags. A third destructive flag is a last
  resort, and adding one means updating this list in the same change.
- Without those flags no command destroys work. `remove` and `gc` refuse dirty
  worktrees and detached ones whose HEAD no ref reaches (no override), `gc`
  reaps only ephemeral worktrees idle at least `--older-than` and skips locked
  ones, branch deletion stops at the merged check, `prune`
  drops only caches that rematerialize, stale registrations that hold no
  work, and simgit's own temporary files and fetch refs once the process that
  wrote them is gone, `repair` only remounts, and `unlock`
  refuses a live owner with no override. Gates classify `doctor`, `list`,
  `unlock`, `prune`, `repair`, `update --check`, flagless
  `add`/`remove`/`gc`/`hook`, and `add --attach` (which checks out an existing
  branch without moving or deleting it) as automatic on that basis; a change
  that breaks it silently turns an approved command destructive.
- `update` without `--check` destroys no work, but it replaces the simgit
  executable with downloaded code, so gates require approval for it every
  time.
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

- Use a worktree when work must run isolated from the user's checkout:
  autonomous or parallel agents, background jobs, benchmarks, and base-vs-head
  comparisons. Interactive work the user follows in their editor stays in
  their checkout.
- The agent harness owns simgit executable discovery, identity preflight,
  worktree allocation, agent launch, and cleanup. Agents must not orchestrate
  this infrastructure when the harness provides the simgit allocator/provider.
- Use the global `--json` flag for preflight and allocation. Prefer `command -v
  simgit`, fall back to `command -v sg`, and accept the selected executable only
  after its top-level `doctor --json` output identifies it as simgit.
- Allocate every agent worktree outside the source repository. Launch the agent
  with its working directory set to the absolute `path` returned by
  `simgit add --json`.
- Allocate an existing branch, such as an open pull request's, with
  `simgit add --json --attach <branch>` after fast-forwarding it with
  `git fetch origin <branch>:<branch>`. Never use `--detach --base <branch>`
  for it: commits on a detached HEAD move no branch.
- Mark disposable automation `--ephemeral`. Ephemeral means eligible for safe
  cleanup; it is not permission to discard dirty files or unmerged commits.
- Inspect the created worktree's JSON `mode`. Use `--require-cow` only when a
  `git-checkout` fallback would invalidate the task, such as by exceeding its
  disk budget.
- Pass the returned `cleanup_token` unchanged to safe removal or cleanup. Never
  add `--discard-dirty` or `--delete-unmerged` without explicit user or
  operator authority; preserve work when safe cleanup refuses it.
- Recover a crashed agent before reuse or cleanup: confirm the child process is
  gone, clear the stranded `run` or allocation lock with `simgit unlock
  <path>`, then reuse or safely remove the workspace. `simgit unlock` succeeds
  on a target that is not locked or no longer exists, refuses a lock set with
  `git worktree lock`, and `simgit remove` on an already-absent
  target succeeds and reports `already_absent: true`, so retried cleanup is
  safe.
- Agents commit normally and integrate their branches through normal Git
  merges. simgit supplies isolated worktrees, not a replacement merge or
  coordination protocol.

`CLAUDE.md` and other agent configuration files should reference this same
block rather than maintaining a divergent copy; see
[docs/agent-integration.md](docs/agent-integration.md) for the canonical
source and the full allocator/provider contract.
