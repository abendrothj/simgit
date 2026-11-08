# Claude Code integration

## Goal

Make simgit the worktree storage for Claude Code with one install, one
version, and no hand-written hooks. The integration works within what Claude
Code offers today and asks nothing of Claude Code.

## Principle

simgit is storage. The harness owns every lifecycle decision: when a worktree
is created, kept, or removed, and whether its uncommitted work is discarded.
The adapter carries out what Claude Code does with its own worktrees, using
simgit's operations, so swapping in simgit changes how worktrees are stored,
not what happens to them.

One difference is deliberate: no commit is ever deleted that no other ref
reaches. Claude Code natively keeps a worktree with unpushed commits; the
adapter applies the same rule to branches, which Claude Code would delete with
its own worktrees.

A per-route check on removal was considered and dropped. The live checks
showed Claude Code shows nobody a remove hook's stderr: a refused CLI exit
quits silently and leaves the worktree, and `ExitWorktree` reports only "could
not remove it — kept". The CLI exit had already warned the person ("You have 1
uncommitted file. These will be lost"), and `ExitWorktree` already demands
`discard_changes` for every hook-created worktree, so the consent exists where
Claude Code asks for it.

## Decisions

### Command: `simgit hook claude-code`

```
simgit hook claude-code --requires <version> [--discard-dirty]
```

- Reads one Claude Code hook payload on stdin and dispatches on
  `hook_event_name`. `claude-code` is the only provider; others are an error.
- Lives in `sg/src/commands/worktree/hook.rs`, beside `launch.rs`, so it uses
  the existing `pub(super)` helpers without widening them. Functions it shares
  with `add`, `remove` and `run` take the repository and working directory as
  parameters instead of reading `std::env::current_dir()`.
- Rejects the global `--json`: the protocol fixes the output, as with `run`.
- `--requires <version>`: when the running binary is older, exit 1 with one
  stderr line. Newer binaries are accepted.
- `--discard-dirty`: permits discarding uncommitted work on `WorktreeRemove`.
  Without it, `hook` never discards.
  Destructiveness is therefore decidable from argv, as AGENTS.md requires: the
  plugin passes the flag on the `WorktreeRemove` entry only.
- The adapter's own exit codes are 0 or 1, never 2.

### Naming and repository

- Branches are `simgit/<name>`. Subagent worktrees are `simgit/agent-<agent_id>`
  because Claude Code names them `agent-<agent_id>` (observed, undocumented; if
  it changes, subagent cleanup finds nothing and does nothing). `claude/` is
  avoided: Claude Code on the web and the GitHub app use it for their own
  branches.
- `name` is validated with `git check-ref-format --branch`; an invalid name
  fails creation with one stderr line.
- The repository comes from `worktree_path` for `WorktreeRemove`, and from
  `$CLAUDE_PROJECT_DIR` (falling back to the payload's `cwd`) for the other
  events. The payload's `cwd` follows the agent's `cd`, so it can name a
  subdirectory, a submodule, or another repository.

### Base commit

New worktrees start from origin's default branch (`refs/remotes/origin/HEAD`)
when it exists, else the project's `HEAD`. That is Claude Code's default
`worktree.baseRef: "fresh"`, except that the adapter does not fetch for it.
`worktree.baseRef: "head"` is not honoured.

Pull requests are the one exception. `claude --worktree "#29"` sends the hook
only `name: "pr-29"`, and with a hook configured Claude Code fetches nothing
(observed: the worktree came out at `main`, not the PR head). Natively it
fetches the change's head from origin. So for a name matching `pr-<digits>`
the adapter fetches it as Claude Code would: `pull/<n>/head` from origin, then
`merge-requests/<n>/head` if that fails, and creates the worktree there. If
both fetches fail, creation fails with one stderr line; it never hands back
the base in place of the pull request. A person who names a worktree `pr-29`
by hand gets pull request 29 too. Reuse follows the native rule: a pull
request worktree reopens at its old tip and is not refetched.

### Dropping a branch

A branch is dropped only when it holds no commit that another ref does not
reach: `git rev-list --single-worktree <branch> --not --exclude=refs/heads/<branch> --all`,
run in the main checkout, is empty. `--single-worktree` matters: by default
`--all` includes every worktree's `HEAD`, and the worktree being removed has
the branch checked out, so the check would always pass. Then deleting it loses
nothing, so the adapter deletes it directly; it
never relies on simgit's merged check, which compares against whatever the
main checkout has out and would keep a clean branch forever after a branch
switch. Pushed commits count as reached, matching Claude Code's "unpushed
commits" rule.

### Events

All creation and lookup for one branch happens under simgit's existing branch
claim. `run` already implements "reuse the registered worktree, else attach
the branch, else create" (`launch.rs`); that flow is factored out to accept a
claim its caller holds, and both `run` and `hook` use it.

| Event | Action | stdout | Exit |
|---|---|---|---|
| `WorktreeCreate` | Under the claim: reuse the `simgit/<name>` worktree; else `add --attach` the branch if it exists; else `add` from the base, or from the fetched head for `pr-<n>`. Never the main checkout; a worktree locked by `simgit run` is refused. Allocations are persistent. | the path only | 1 with one stderr line on failure |
| `WorktreeRemove` | Only for a worktree on a `simgit/*` branch; others are refused. Remove it, discarding uncommitted work when `--discard-dirty` is given and refusing a dirty one otherwise. Then drop the branch if it holds no unreached commit. | nothing | 1 when refused or failed |
| `SubagentStop` | If `simgit/agent-<agent_id>` is checked out in a worktree that is clean, unlocked, and holds no unreached commit: remove it and drop the branch. Otherwise nothing. | nothing | always 0 |
| `SessionStart` | nothing (the plugin's version check, below) | nothing | 0 |
| any other | nothing | nothing | 0 |

`WorktreeCreate` holds the claim across the existence check and the
allocation, so a parallel duplicate hook (the plugin plus the same command in
a settings file) waits, then reuses the finished worktree and never returns a
half-populated path.

`WorktreeRemove` behaves the same on every route (CLI exit, `ExitWorktree`,
desktop archive and cleanup): the plugin passes `--discard-dirty`, so it
discards uncommitted work as Claude Code's own `git worktree remove --force`
does. Claude Code calls it only after its own decision, and gets consent where
it asks for it: the CLI exit warns how many uncommitted files will be lost,
the desktop asks before "Archive anyway", and `ExitWorktree` demands
`discard_changes`. The payload is not used to choose a route.

`SubagentStop` never acts on the payload's `cwd`, which for a plain subagent is
the main checkout or the parent session's worktree. It only acts when the
branch is still checked out in a worktree: if the agent switched branches
there, it does nothing rather than drop a branch and leak the worktree.

### Lifecycle

Observed with Claude Code 2.1.319 (CLI and desktop app):

| Claude Code action | Result | As natively? |
|---|---|---|
| Any worktree creation (desktop, `--worktree`, `--bg --worktree`, `EnterWorktree`, subagent) | simgit allocation, or the existing worktree for that name | yes |
| `--worktree "#<n>"` | worktree at the pull request's fetched head | yes |
| CLI exit with remove, `ExitWorktree` remove, desktop "Archive anyway", desktop cleanup | removed; uncommitted work discarded; branch dropped unless it holds unpushed commits | yes, except unpushed commits are kept |
| `claude rm` of a background session | Claude Code refuses without calling the hook ("files that weren't checked against a repository") | yes: Claude Code's own rule for hook-created worktrees |
| Subagent finishes clean, nothing unpushed | removed with its branch | yes |
| Subagent finishes dirty or with unpushed commits | kept; each later stop checks again | yes |
| Clean desktop archive, `-p` runs | nothing until Claude Code removes it | yes |
| Resume after a clean subagent removal | refused by Claude Code | no: natively it runs in the main checkout |

### One install, one version

The repository is a Claude Code marketplace holding one plugin, `simgit`:

```
.claude-plugin/marketplace.json   plugin entry pinned to a release tag
.claude-plugin/plugin.json        version = Cargo version; hooks -> integrations/claude-code/hooks.json
integrations/claude-code/         hooks.json, README
skills/simgit-worktrees/          shared by every harness, loaded by the plugin unchanged
```

`hooks.json`:

| Hook | Form | Command |
|---|---|---|
| `WorktreeCreate` | exec | `simgit hook claude-code --requires <v>` |
| `WorktreeRemove` | exec | `simgit hook claude-code --requires <v> --discard-dirty` |
| `SubagentStop` | shell | `simgit hook claude-code --requires <v> \|\| exit 0` |
| `SessionStart` | shell | `simgit hook claude-code --requires <v> >/dev/null 2>&1 \|\| echo '<systemMessage JSON>'` |

`SubagentStop` is shell form because exit 2 there stops the subagent from
finishing, and a binary without `hook` exits 2 (clap's unknown-subcommand
error). `SessionStart` turns a missing binary, a binary without `hook`, and a
too-old binary into one message naming the install or upgrade command.

`<v>` is the oldest release whose adapter behaves as the plugin expects,
bumped only when that behaviour changes.

Releases:

- Claude Code caches plugins by version, so any plugin change ships with a
  Cargo version bump. A `cargo test` asserts `plugin.json`'s version equals the
  Cargo version and `<v>` is not newer, so the release workflow's tag check
  covers the plugin.
- The marketplace entry points at a release tag, and moves to a new tag only
  after the Homebrew formula serves that release, so the plugin never gets
  ahead of brew. That step joins the release checklist.

The plugin root is the repository root because Claude Code copies plugins to a
cache, where `../` paths to the shared skill would break. `.claude-plugin/` is
the only Claude-specific path at the root.

Install:

```sh
brew install abendrothj/tap/simgit
claude plugin install simgit --marketplace abendrothj/simgit
```

### Approval-gate contract

Unchanged in substance; AGENTS.md's tables grow by one entry:

- `--discard-dirty` is accepted by `remove`, `gc`, and `hook`. A guard
  classifies `hook` exactly as the others: destructive with the flag,
  automatic without it.
- `hook` never deletes a commit that no other ref reaches, and never passes
  `--delete-unmerged`. It may delete a branch that simgit's merged check would
  keep (one whose commits are reachable only from origin, say), but only when
  that loses nothing; AGENTS.md says so beside the `--delete-unmerged` row.

The four tests AGENTS.md names as the gate guarantee are untouched.

## Changes

- `sg/src/lib.rs`: `Hook` subcommand and module doc; `sg/src/commands/worktree/hook.rs`;
  the shared reuse/attach/create flow factored out of `launch.rs`;
  repository and working directory passed as parameters where `hook` shares
  code.
- `.claude-plugin/`, `integrations/claude-code/`.
- Tests in `sg/tests`:
  - Create: prints exactly one path; same name twice gives the same path; two
    concurrent creates give the same, fully populated path; an existing branch
    without a worktree is attached; the main checkout and a `run`-locked
    worktree are never returned; an invalid name fails; the base is origin's
    default branch when present, else `HEAD`; the worktree is persistent;
    `pr-<n>` is created at `pull/<n>/head` fetched from a local "origin"
    fixture, falls back to `merge-requests/<n>/head`, fails when neither
    exists, and is not refetched on reuse.
  - Remove: refuses a worktree not on `simgit/*`; with `--discard-dirty`,
    removes a dirty worktree whatever the payload's `cwd` and `prompt_id`;
    without it, refuses a dirty one; drops a branch with nothing unreached;
    keeps a branch with an unpushed commit; repository found from
    `worktree_path` when `cwd` is elsewhere.
  - Subagent stop: removes a clean worktree and its branch; keeps a dirty one;
    keeps one with an unpushed commit, checked while the worktree still has
    the branch checked out; keeps a `run`-locked one; does nothing
    when the branch was switched away; ignores a plain subagent; two parallel
    stops are safe; always exits 0, including outside a repository and on
    malformed stdin.
  - General: unknown event is a no-op; unknown provider fails; `--requires`
    refuses an older binary with exit 1; `--json` is rejected; no event
    deletes a commit no other ref reaches; plugin version equals Cargo version
    and `<v>` is not newer.
- Docs (per AGENTS.md ownership):
  - `docs/reference.md`: the `hook` command and the command list.
  - README: command-table row and a link.
  - `docs/agent-integration.md`: the storage principle in the provider
    contract; its "provider allocations require ephemeral" line becomes the
    harness's choice, with hook allocations persistent; a Claude Code section
    (install, lifecycle table, the behaviour below).
  - `integrations/claude-code/README.md` links there.
  - AGENTS.md: command surface, `--json` rule, `hook` in the gate tables.
  - `skills/simgit-worktrees/SKILL.md`: with the plugin active, the harness
    allocates and removes, so the agent does not.
  - Release checklist: move the marketplace tag after the brew update.
- The uncommitted hand-installed-scripts draft in `README.md` and
  `docs/agent-integration.md` is dropped.

## Live check results

Run on 2026-11-08 with Claude Code 2.1.319:

1. A remove hook's stderr reaches nobody. A refused CLI exit quits with no
   message; `ExitWorktree` reports only "could not remove it — kept". Hence
   discard on every route (Principle).
2. `claude --bg --worktree <name>` sends the given `name`. `claude rm` refuses
   to delete a hook-created worktree without calling the hook. Agent-view
   deletion, workflow agents, `/batch` and remote-control were not exercised:
   see "Not yet observed".
3. `--worktree "#29"` sends `name: "pr-29"` and fetches nothing; the
   worktree came out at `main`. Hence the adapter's own fetch (Base commit).
4. A running subagent's hook-created worktree carries no git lock, and
   `SubagentStop` removal succeeded in the earlier spike.
5. Desktop hooks get the login `PATH`, including `/opt/homebrew/bin` and
   `~/.local/bin`; exec form resolved `/opt/homebrew/bin/simgit`.
6. A plugin installed at user and local scope runs once. With the plugin and
   the same hook in a settings file, both run and Claude Code uses the settings
   hook's path, even when the plugin finishes first.
7. A plugin entry pins with `{"source": "github", "repo": "abendrothj/simgit", "ref": "<tag>"}`
   (optionally `sha`); `claude plugin validate` accepts it. The marketplace
   itself stays on the default branch.

Also observed: hooks added to a settings file take effect in sessions already
running.

## Live install results

Run on 2026-11-08 with simgit 0.5.0 (this branch) and Claude Code 2.1.319,
plugin installed at local scope from this checkout:

- The plugin loads with four hooks and the skill.
- `--worktree <name>` allocates `simgit/<name>`; `--worktree "#29"` checks out
  the pull request head; `--bg --worktree` allocates.
- Worktree subagents: clean → removed with its branch; dirty → kept; with a
  commit → kept with its branch.
- `ExitWorktree` remove of a dirty worktree and a CLI exit choosing "Remove
  worktree" on a dirty one both remove it, discard the work, and drop the
  commit-free branch. The CLI warned first ("You have 1 uncommitted file").
- With simgit 0.4.0 first on `PATH`, the CLI shows the `SessionStart` message
  ("The simgit plugin needs simgit 0.5.0 or newer: …") and `--worktree` fails
  with "WorktreeCreate hook failed: … unrecognized subcommand 'hook'".
- A plugin whose hooks path resolves through a symlink outside the
  marketplace directory fails to load ("Path escapes plugin directory"); the
  committed layout (plugin root = repository root) does not.

## Not yet observed

Covered by the docs or by inference:

- Whether the desktop app shows the `SessionStart` message.
- Agent-view deletion of a background session (documented to call
  `WorktreeRemove`; auto mode would not run it unattended).
- Workflow agents and `/batch` (documented as subagents in worktrees; their
  `name` format and whether `SubagentStop` fires are unknown; if not
  `agent-<id>`, their worktrees stay until removed from `simgit list`).
- `claude remote-control` in worktree mode, and the desktop cleanup about 30
  days after archive.

## Claude Code behaviour the integration works within

- Claude Code's periodic sweep skips hook-created worktrees. A subagent
  worktree kept dirty that later becomes clean stays until that subagent stops
  again or a person removes it from `simgit list`.
- A plugin's `WorktreeCreate` hook does not run for `--worktree --tmux`;
  adding the same command to a settings file covers it. Claude Code then uses
  the settings hook's path, and the claim makes the plugin's duplicate return
  that same worktree.
- A hand-written hook that names branches differently cannot be deduplicated;
  the docs say to remove it when installing the plugin.
- `worktree.baseRef: "head"` and the 24-hour fetch of `"fresh"` are not
  reproduced.
- Branch claim files under `.git/simgit/claims/branches/` accumulate, one per
  branch ever allocated; they are empty and are not cleaned up.

## Out of scope

`doctor` reporting on the plugin; persistence settings; other providers;
changes to `simgit run -- claude` beyond sharing its flow; requests for Claude
Code changes.
