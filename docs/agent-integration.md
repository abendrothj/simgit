# Agent integration

The canonical executable name is `simgit`. The installed `sg` command is an
equivalent alias, but integrations and examples in this document use the
canonical name so that command identity is unambiguous.

## Canonical agent policy

Paste the following block verbatim into `AGENTS.md`:

```markdown
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
```

`CLAUDE.md` and other common repository agent configuration files should
include or copy that exact same block and link back to this canonical document,
rather than maintaining divergent prose or a tool-specific variation.

## Installation and skill discovery

Installation is an operator action, not a preflight side effect. Obtain the
user's approval before installing a user-level binary, modifying shell startup
or `PATH` configuration, or copying a skill into the user's home directory.
On macOS, prefer Homebrew when it can supply the approved release: it uses a
versioned release asset with a pinned SHA-256 digest. Pin the
approved version instead of silently following the latest release. Never
automatically execute a curl-pipe-shell installer; present it for explicit
review and approval if no safer approved method is available.

```sh
brew install abendrothj/tap/simgit

# Optionally keep Homebrew from upgrading this reviewed version.
brew pin simgit
```

Cargo installations can also be pinned (omit one of these alternatives):

```sh
cargo install simgit-cli --version X.Y.Z --locked
cargo binstall simgit-cli --version X.Y.Z
```

For the release installer, replace `vX.Y.Z` with one exact published tag.
Download that tag's installer to a file and inspect it before running it; never
automatically execute a `curl | sh` pipeline:

```sh
SIMGIT_VERSION=vX.Y.Z
INSTALLER="/tmp/simgit-install-${SIMGIT_VERSION}.sh"
curl --proto '=https' --tlsv1.2 -fL \
  "https://raw.githubusercontent.com/abendrothj/simgit/${SIMGIT_VERSION}/install.sh" \
  -o "$INSTALLER"
less "$INSTALLER"
SIMGIT_VERSION="$SIMGIT_VERSION" sh "$INSTALLER"
rm -f "$INSTALLER"
```

The installer downloads the matching release archive and `SHA256SUMS`, then
refuses to install unless the archive's exact entry verifies and the staged
binary's `--version` equals the requested tag without its leading `v`. It
installs both `simgit` and `sg` into `~/.local/bin` by default;
`SIMGIT_INSTALL_DIR` selects another directory. It does not edit shell startup
files. Its own comment header shows the `curl … | sh` one-liner as a
convenience form; ignore it. Reading that header is exactly what the
inspection step above is for, and the download-inspect-run sequence is the
supported way to install from a release.

Verify the selected executable afterwards with `simgit doctor --json`, which
runs with or without a repository.

This repository's skill is `skills/simgit-worktrees/SKILL.md`. After receiving
approval, install it from a simgit source checkout by copying the whole skill
directory into the skill root recognized by the user's harness, without
overwriting an existing copy. A typical cross-agent installation is:

```sh
test ! -e "$HOME/.agents/skills/simgit-worktrees" ||
  { echo "skill already installed; refusing to overwrite" >&2; exit 1; }
mkdir -p "$HOME/.agents/skills"
cp -R skills/simgit-worktrees "$HOME/.agents/skills/"
```

Harnesses that use the skills CLI can install the same directory straight from
this repository, again only after approval. The CLI discovers every skill under
`skills/`, so list before installing and name the skill explicitly:

```sh
npx skills add abendrothj/simgit --list
npx skills add abendrothj/simgit --skill simgit-worktrees

# Non-interactive, user-level install for a specific agent:
npx skills add abendrothj/simgit --skill simgit-worktrees -g -a claude-code -y

# Or run it once without installing anything:
npx skills use abendrothj/simgit@simgit-worktrees
```

Installing this way sends the skill name to the skills.sh leaderboard as
anonymous telemetry; `DISABLE_TELEMETRY=1` opts out.

Installed skills never update themselves. The skills CLI installs the skill as
it stands on this repository's default branch, not at a release tag, and a
manual copy is whatever the source checkout held.
A binary upgrade therefore does not update the skill, and a skill update can
describe a newer simgit than the one installed. The skill names the version it
describes and has the agent compare it with `doctor --json`'s `version`; keep
them in step after each simgit upgrade, again with the user's approval:

```sh
# Installed with the skills CLI: reinstalls any skill whose folder changed upstream.
npx skills update simgit-worktrees -g

# Installed by copying: the snippet above refuses to overwrite, so replace it explicitly.
rm -rf "$HOME/.agents/skills/simgit-worktrees"
cp -R skills/simgit-worktrees "$HOME/.agents/skills/"
```

`npx skills use abendrothj/simgit@simgit-worktrees` installs nothing and always
reads the current default branch.

A Claude-specific installation may instead use
`$HOME/.claude/skills/simgit-worktrees/SKILL.md`. Follow the harness's documented
skill root if it differs. Do not make either user-level change without approval.

A skill teaches an agent how to use a tool; it cannot replace an allocator or
provider hook. A capable harness must run preflight, allocate the worktree,
launch the model in it, and perform cleanup itself. When that provider exists,
the model must work inside the supplied directory and must not invoke simgit to
orchestrate its own infrastructure.

## Allocator/provider contract

Every command used below writes its JSON result as a single object on stdout
and exits 0. Failures, including invalid arguments, are not JSON: the command
exits nonzero, writes nothing to stdout, and puts one line of diagnostic text
on stderr. A provider branches on the exit status, parses stdout only on
success, and reports the stderr line verbatim rather than trying to parse it;
empty stdout on a nonzero exit is the documented shape, not a malformed
response. `run` is the exception to the whole contract: it streams the child's
output, rejects `--json`, and exits with the child's own status, or one of
the codes in [its exit table](reference.md#run-locks-and-recovery).
Like `system(3)`, `simgit run` ignores SIGINT and SIGQUIT sent to its pid
alone, so stop it with SIGTERM or SIGHUP, which it forwards to the child, or
signal its process group; see
[Run locks and recovery](reference.md#run-locks-and-recovery).

A tool that classifies simgit invocations before they run — an approval gate or
checkpointer, rather than an allocator — depends on a narrower promise: which
flags can destroy work. See [For agent guard tools](#for-agent-guard-tools).

simgit is storage; the harness owns lifecycle. The harness decides when a
worktree is created, kept or removed and whether its uncommitted work is
discarded. An adapter carries out what the harness already does with its own
worktrees, using simgit's operations, so that using simgit changes how
worktrees are stored and not what happens to them. Ephemeral never gives an
adapter permission to discard dirty files or unmerged commits.

### 1. Discover and verify the executable

For each provider startup, resolve the executable without trusting a possibly
colliding command name:
1. Select `command -v simgit` when present, otherwise `command -v sg`.
   Reject a shell function or alias; some shells return a path relative to
   their current directory, so resolve it to an absolute executable path
   *before* changing directories.
2. If neither exists or the result is not an executable file, stop and report
   that simgit is unavailable; installation still requires approval.
3. Invoke the resolved path with the top-level command `doctor --json`. This
   step needs no repository; run it wherever the provider starts.
4. Parse JSON and require the top-level field `"identity": "simgit"` and a
   top-level `"version": "<semver>"` containing a valid semantic version.
   Reject malformed output, a missing or different identity, or a missing or
   invalid version. The alias must report the same simgit identity; selecting a
   binary merely named `simgit` or `sg` is not sufficient.
5. Only then, run `doctor --json` again from the source repository the
   allocation will serve, and validate the repository diagnostics below.

A shell provider can express steps 1 through 5 directly:

```sh
if SIMGIT_BIN="$(command -v simgit 2>/dev/null)"; then
  :
elif SIMGIT_BIN="$(command -v sg 2>/dev/null)"; then
  :
else
  echo "simgit is not installed" >&2
  exit 1
fi

case "$SIMGIT_BIN" in
  /*) ;;
  */*) SIMGIT_BIN="$(cd -P "${SIMGIT_BIN%/*}" && pwd -P)/${SIMGIT_BIN##*/}" ;;
  *) echo "simgit discovery did not return an executable path" >&2; exit 1 ;;
esac
[ -f "$SIMGIT_BIN" ] && [ -x "$SIMGIT_BIN" ] ||
  { echo "simgit is not an executable file: $SIMGIT_BIN" >&2; exit 1; }

doctor_json="$("$SIMGIT_BIN" doctor --json)" || exit
printf '%s\n' "$doctor_json" | jq -e '.identity == "simgit"' >/dev/null ||
  { echo "command identity mismatch: $SIMGIT_BIN" >&2; exit 1; }
```

`doctor --json` is the authoritative preflight. It emits one object whose
fields are the contract the provider validates:

| Field | Type | Contract |
|---|---|---|
| `identity` | string | Always `simgit`. This is the field to gate acceptance on. |
| `product` | string | Always `simgit`. Reported for display; `identity` remains the check. |
| `version` | string | The executable's version, a valid semantic version. |
| `filesystem` | string | Backing filesystem of the current directory, such as `apfs`, `btrfs`, or `ext4`. |
| `cow_supported` | boolean or null | Whether copy-on-write works in the current directory. `null` means simgit could not write a probe there, so the question is unanswered; it describes the directory, not the filesystem. Human output prints that case as `CoW: unknown (cannot write a probe here)`. |
| `repository` | string or null | Absolute path of the Git working tree containing the current directory; `null` outside one. |
| `repository_details` | object or null | `common_git_dir` (string), `common_git_owner` (string), `top_level` (string), `is_main_worktree` (boolean); `null` outside a repository. |
| `populate_mode` | string or null | The mode simgit would select here: `cow-clone`, `overlay`, or `git-checkout`. `null` outside a repository, and when no mode can be selected (see `populate_error`). |
| `populate_error` | string or null | Why no mode can be selected, such as a `SIMGIT_POPULATE` naming a backend this machine lacks. `null` when `populate_mode` is a string and outside a repository. `doctor` still exits 0. |
| `default_worktree_root` | string or null | Absolute root used by an allocation that passes no `--path`. |
| `default_worktree_root_inside_repository` | boolean or null | Whether that root is inside the source repository. `true` must block allocation against the default root. |
| `git_worktree_supported` | boolean or null | Whether the installed Git supports linked worktrees. |
| `stale_worktree_registrations` | array of strings | Registered worktrees whose directories are gone. An empty list outside a repository, never `null`. |
| `baseline_cache` | object or null | `root` (string), `retained` (array of base-commit oids), `retained_count` (integer), `retained_bytes` (integer). |

The first five fields are identity and machine capability. They are always
present, with or without a repository; steps 3 and 4 above validate the
identity and version among them. The remaining fields are repository
diagnostics, reported when `doctor` runs inside a Git worktree: the repository
and its common Git ownership, the populate mode, the default worktree root
and whether it is inside the source repository, linked-worktree support,
stale registrations, and retained
baseline state.

Outside a Git worktree `doctor` still succeeds: every repository field is JSON
`null` and the stale-registration list is empty. That is a repository-absent
report, not a failure of the executable, and the provider must not read it as
an identity or installation problem. It must still refuse to allocate until it
has a repository-tier report from the source repository itself.

`repository` names the worktree `doctor` ran in, which is not always the source
repository. Run inside the main checkout the two coincide, but run inside a
linked worktree — including one simgit allocated — `repository` is that
worktree and `repository_details.common_git_owner` is the source repository
that owns its Git data. A provider validating "the source repository this
allocation will serve" must compare `common_git_owner`, and can tell the two
situations apart with `repository_details.is_main_worktree`.

The provider must parse and validate these documented diagnostics rather than
scraping human output. It must reject an unsupported repository, an identity
mismatch, or a default root inside the source repository. Stale registrations
and retained baselines are surfaced to the operator; the harness must not
silently destroy them during preflight. Filesystem, CoW, and selected-mode data
determine whether the requested job can proceed and whether `--require-cow` is
needed.

### 2. Choose an outside-source destination

The allocation path must not be the source repository itself or any descendant
of it. Compare canonical, symlink-resolved paths rather than using a lexical
prefix test. Every path simgit reports is itself symlink-resolved and lexically
normalized, so a `--path` given as `../jobs/job-1` comes back as one canonical
absolute string with no `..` component left in it, and two calls describing the
same worktree always return the same string.

`--path` is the worktree directory itself, not a parent to allocate inside.
Missing parents are created. The directory may already exist if it is empty, so
a provider that pre-creates one directory per job is supported; a non-empty
directory is refused. The provider must still generate a distinct path per
allocation: two concurrent allocations to the same path never both succeed.
Allocations of one commit that has no cached baseline yet build it once: the
others wait for that build, with a notice on stderr and no time limit, so a
provider's allocation timeout must allow for one full checkout
([details](reference.md#run-locks-and-recovery)).

`add` does not refuse an inside-source destination; it warns on stderr that the
worktree will appear as untracked clutter in `git status` and can be deleted by
`git clean`. That warning is the only signal, it does not reach `--json`
stdout, and the allocation still succeeds — so enforcing the outside-source
rule remains the provider's job, before and after allocation.

The provider may use a doctor-approved default root or pass an explicit
outside-source `--path`; it must validate the path again in the allocation
result before launch.

### 3. Allocate

Request JSON and mark autonomous disposable work ephemeral. For a new branch,
omit the branch argument:

```sh
"$SIMGIT" add --json --ephemeral --path "$ALLOCATED_PATH"
```

Omitting a branch without `--detach` creates a collision-resistant
`agent/<uuid>` branch. A provider may instead supply an explicit valid branch
when its job contract requires a stable name. It may add `--base <commit>` to
choose the starting commit.

A stable name is not automatically reusable. Safe cleanup deliberately retains
branches, so a stable-named job's branch outlives its worktree and the next
`add` with that branch fails with `branch '<name>' already exists`. A provider
that relaunches stable-named jobs must choose one of four procedures:

- Attach a fresh worktree: `simgit add --json --attach <branch> --ephemeral
  --path "$ALLOCATED_PATH"` checks the existing branch out at its current commit
  and returns the usual allocation record. It refuses a branch that is already
  checked out, so the record's `cleanup_token` always names a worktree this
  allocation created. This is also how to work on a branch the provider did not
  create, such as an open pull request's: update the local branch first with
  `git fetch origin <branch>:<branch>`, which only fast-forwards. Removing the
  worktree keeps the branch; pass `--delete-branch` only for a branch the
  provider owns.
- Reuse instead of reallocating: `simgit run <branch> -- <command>` attaches to
  the branch's registered worktree, or creates one at the branch's current
  commit when only the branch survives. A registered worktree deleted by hand
  is created again at its old path, still ephemeral or sparse if it was, unless
  its registration holds work `remove` would keep (see
  [Reuse](reference.md#reuse-persistence-and-populate-backends)).
  This keeps the job's history, but `run` emits no JSON record.
- Delete the branch as part of cleanup, once its work is integrated, with
  `remove --delete-branch` (step 5). Safe deletion refuses an unmerged branch,
  so this never silently discards work.
- Generate a unique branch per run and keep the stable name in the provider's
  own job record rather than in Git.

After a failed allocation, do not delete a branch merely because it exists:
another process may have created it during allocation. Inspect the ref and
registration first; see [allocation failure semantics](reference.md#where-worktrees-live).

For disposable work against a historical commit without creating a branch:

```sh
"$SIMGIT" add --json --detach --base "$COMMIT" \
  --ephemeral --path "$ALLOCATED_PATH"
```

Add `--require-cow` only when fallback would invalidate the task. It is not a
default safety flag: ordinary jobs should permit the safe `git-checkout`
fallback.

A successful `add --json` emits one JSON object with this contract:

| Field | Type | Contract |
|---|---|---|
| `worktree` | string | Absolute path of the created, removable worktree. Retained for compatibility. |
| `path` | string | Provider launch path. It identifies the same absolute worktree as `worktree`. |
| `cleanup_token` | string | Token to retain and pass unchanged to safe cleanup. It identifies that same absolute removable worktree. |
| `branch` | string or null | Branch name for a branch worktree, including an autogenerated `agent/<uuid>`; `null` for detached worktrees. |
| `base` | string | Resolved base commit for the created worktree. |
| `mode` | string | Actual populate mode: `cow-clone`, `overlay`, or `git-checkout`. |
| `ephemeral` | boolean | Whether the created worktree is marked disposable. The harness chooses: the allocations above pass `--ephemeral`, while [Claude Code hook](#claude-code) allocations are persistent. |

The harness must reject malformed JSON, a relative or inside-source provider
path, disagreement among `worktree`, `path`, and `cleanup_token`, a detached
allocation with a non-null branch, or a branch allocation without a string
branch. It must inspect the actual returned `mode` after creation even when the
preflight predicted a mode. If CoW was required, only `cow-clone` or `overlay`
is acceptable; `git-checkout` remains valid otherwise.

A provider that shells out captures the record once and reads those fields
directly:

```sh
allocation="$("$SIMGIT" add --json --ephemeral --path "$ALLOCATED_PATH")"
path="$(printf '%s\n' "$allocation" | jq -r .path)"
cleanup_token="$(printf '%s\n' "$allocation" | jq -r .cleanup_token)"
mode="$(printf '%s\n' "$allocation" | jq -r .mode)"
branch="$(printf '%s\n' "$allocation" | jq -r .branch)"
```

### 4. Launch

Persist the full allocation record, especially `cleanup_token`, outside the
agent conversation. Launch the agent process with its operating-system working
directory set to the returned absolute `path`. The model receives an ordinary
Git linked worktree and uses normal Git commands there; it does not need to run
allocation or translate paths.

For a branch worktree, the agent commits to the returned branch and the caller
integrates it with a normal Git merge before removal. A detached historical
worktree is for disposable build, comparison, or read-only work; it intentionally
creates no branch.

### 5. Clean up safely

After the agent exits and any branch work has been integrated, pass the recorded
token back as the removal target:

```sh
# Run the merge from the intended integration checkout after reviewing the work.
git merge "$BRANCH"
"$SIMGIT" remove "$CLEANUP_TOKEN"
```

This is the normal provider cleanup path. Do not add `--discard-dirty` or
`--delete-unmerged`. Safe removal must refuse dirty work rather than discard
it, and the provider must report and preserve a refused worktree for recovery.
Branches, including unmerged branches, remain available for ordinary Git
integration. Only explicit user or operator authority for that particular
worktree may permit destructive cleanup; an `--ephemeral` marker alone is
never that authority.

A detached worktree (`add --detach`) whose commits no ref reaches is refused by
`remove` and skipped by `gc` as `unreached-commits`, and no flag overrides it.
Keep the commits with `git -C <path> branch <name>` first; see
[removal](reference.md#removal-and-unlock-semantics).

Do not use `gc --older-than 0s` as a follow-up to one job: GC selects *every*
clean, unlocked ephemeral worktree in the repository, including a different
provider's newly allocated job before its worker starts. Use the recorded
`cleanup_token` for this job; schedule repository-wide GC separately.

Cache pruning is repository maintenance too, but it is safe to run while other
jobs allocate: `prune` never removes a baseline an allocation is using and
never waits for one. When an allocation holds the cache, plain
`prune --json` keeps the baselines it would have dropped and reports
`"cache_busy": true`, so a scheduled pass simply runs again later;
`prune --all` fails instead, with a retry message on stderr. Stale
registrations it drops are listed in `pruned_registrations`, an array of
paths. Those it keeps because they may hold work are listed in
`kept_registrations`, an array of objects with `worktree` (string: the
worktree's path, or its admin dir `.git/worktrees/<name>` when the entry's
`gitdir` file cannot be read) and `reason` (`dirty`, `unreached-commits` or
`status-failed`); report them rather than deleting them.

Safe cleanup is idempotent: a target that no longer resolves to a worktree
makes `remove` exit 0 with `"already_absent": true` (`false` when the worktree
was present). The other fields are `removed` (string), `committed` and
`branch_deleted` (booleans) and `commit` (the full hash a `--commit` removal
created, else `null`). `removed` is never evidence that anything was removed.
When the target resolved to a worktree it is that worktree's canonical path,
comparable to a recorded `cleanup_token`; when nothing resolved it repeats the
target string verbatim. Key the provider's record off `already_absent` and
`branch_deleted`, never off `removed`. A retry needs no separate existence
check. Which targets are refused, how `--delete-branch` differs for a path and
a branch name, what a failed `--commit` removal keeps, and the retry after a
failed `--delete-branch` are in
[the reference](reference.md#removal-and-unlock-semantics). A provider that
deletes branches does it while the token still resolves, or afterwards passes
the `branch` from its own allocation record.

The provider should treat cleanup as idempotent at the job level: record a
successful removal, and do not reinterpret a missing or mismatched path as
permission to remove some other worktree. Never construct cleanup targets from
an agent-supplied branch name; the allocation record's `cleanup_token`, or its
recorded `branch`, are the only acceptable targets.

Allocation also leaves an empty claim file for every destination path, branch
and new baseline commit it has claimed, under `.git/simgit/claims/paths/`,
`branches/` and `baselines/`. They are kept on purpose and need no cleanup; see
[Run locks and recovery](reference.md#run-locks-and-recovery).

### 6. Recover a crashed agent

A killed `simgit run` launcher leaves its workspace locked, so removal, GC, and
a second `run` all refuse it; the lock file records the launcher's PID. The
recovery sequence is:

1. Detect that the child is gone through the harness's own process
   supervision, not through a heuristic over the worktree.
2. Clear the stranded lock:

   ```sh
   "$SIMGIT" unlock "$CLEANUP_TOKEN"
   ```

   Unlocking a workspace that is not locked, or one that no longer exists, exits
   0 with `"was_locked": false`, so recovery may run `unlock` unconditionally.
   `--json` reports `unlocked`, `was_locked` and `owner_pid`. A provider never
   needs to find the lock file itself; where locks live is in
   [Run locks and recovery](reference.md#run-locks-and-recovery).
3. If `unlock` refuses, the recorded owner is still running and the error names
   its PID. There is no override flag: stop that process, then unlock again. A
   lock set with `git worktree lock` is not simgit's: `unlock` refuses it, and a
   provider leaves it alone.
4. Then either reuse the workspace — relaunch into the same absolute `path`,
   whose files, index, and branch are exactly as the crashed agent left them —
   or clean it up with the ordinary safe removal of step 5, which retains dirty
   work and unmerged branches instead of discarding what the agent produced.

A killed `add`, or a `run` killed while it was still creating its workspace,
may leave a registered worktree it had not finished populating. That
registration stays locked with the reason `simgit: allocating (pid N)`, which
keeps `prune` and `gc` off it, so safe removal first refuses it as locked.
Clear the lock with `simgit unlock` as in step 2; it refuses while that PID is
alive. After that, the worktree shows every tracked file as deleted, so safe
removal refuses it as dirty, and `run` and the Claude Code hook refuse to reuse
it ([details](reference.md#run-locks-and-recovery)). The branch claim, the
path claim, the baseline build claim and the baseline-cache lock it held are
operating-system file locks released with the process. Since `add` never returned, no agent ever ran there
and the provider holds no allocation record for it; report the path to the operator, whose explicit
authority `remove --discard-dirty` still requires even though it loses nothing
here.

## Claude Code

The simgit plugin makes Claude Code store every worktree it creates with
simgit: copy-on-write and outside the repository. Install simgit, then the
plugin:

```sh
brew install abendrothj/tap/simgit
claude plugin install simgit --marketplace abendrothj/simgit
```

> PRE-RELEASE: the plugin installs only once `v0.5.0` is tagged, and works only
> once Homebrew serves simgit 0.5.0. Remove this note at release (CONTRIBUTING,
> "Releasing").

The plugin registers `simgit hook claude-code` for `WorktreeCreate`,
`WorktreeRemove`, `SubagentStop` and `SessionStart` (see
[the `hook` reference](reference.md#hook)). `WorktreeCreate` has a 30-minute
timeout in place of Claude Code's 600-second default, so building the first
baseline of a large repository is not killed mid-allocation. `SessionStart`
shows one message naming the install or upgrade command when simgit is missing
or too old. The `SubagentStop` hook runs in shell form with `|| exit 0`
because exit 2 there stops the subagent from finishing, and an older simgit
without `hook` exits 2.
Remove any hand-written worktree hook when you install the plugin. Claude Code
caches plugins by version, so the plugin's version is simgit's. Worktrees
Claude Code created before the plugin (branches `worktree-<name>`) are refused
by its remove hook; remove them with `git worktree remove` or `simgit remove`.

With the plugin active the harness allocates and removes worktrees, so the
agent must not run simgit for its own infrastructure. Behaviour, observed with
Claude Code 2.1.319 (CLI and desktop app):

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

`WorktreeRemove` discards uncommitted work on every route, because the plugin
passes `--discard-dirty` and Claude Code shows nobody a remove hook's stderr.
Claude Code asks for consent before it calls the hook: the CLI exit warns how
many uncommitted files will be lost, the desktop asks before "Archive anyway",
and `ExitWorktree` demands `discard_changes`. The one deliberate difference
from native is that no commit is deleted that no other ref reaches.

### Claude Code behaviour the integration works within

- Claude Code names subagent worktrees `agent-<agent_id>` (observed, not
  documented). If that changes, subagent cleanup finds nothing and does
  nothing.
- The adapter knows a name's worktree by its path, the default path of
  `simgit/<name>`, so `git switch`, `git branch -m` or a detach inside it does
  not lose it: the next create of that name returns it, and removal removes
  it but never drops the branch it is on now, which is not simgit's. Subagent
  cleanup still goes by the `simgit/agent-<agent_id>` branch, so a subagent
  worktree switched away is left for removal or a person.
- Claude Code's periodic sweep skips hook-created worktrees. A subagent
  worktree kept dirty that later becomes clean stays until that subagent stops
  again or a person removes it, as listed by `simgit list`.
- A plugin's `WorktreeCreate` hook does not run for `--worktree --tmux`. Adding
  the same command to a settings file covers it; Claude Code then uses the
  settings hook's path, and the branch claim makes the plugin's duplicate return
  that same worktree. A hand-written hook that names branches differently
  cannot be deduplicated.
- `claude --worktree "#<n>"` sends only `pr-<n>` and fetches nothing, so the
  adapter fetches the pull request head itself. Its `simgit/pr-<n>` branch is
  kept after removal, because no other local ref reaches that head.
- `worktree.baseRef: "head"` and the 24-hour fetch of `"fresh"` are not
  reproduced; new worktrees start at `origin/HEAD` or `HEAD` without a fetch.
- The payload's `cwd` follows the agent's `cd`, so it can name a subdirectory,
  a submodule or another repository. The adapter finds the repository from
  `$CLAUDE_PROJECT_DIR` (else `cwd`), and from `worktree_path` on removal.
- Branch claim files under `.git/simgit/claims/branches/` accumulate, one per
  branch ever allocated; a stop of a subagent that had no worktree creates
  none. They are empty and are not cleaned up.

## Codex

Everything in this section was observed with Codex CLI 0.166. Codex reads
Claude Code's plugin format and speaks its hook protocol, so the same plugin
serves both:

```sh
brew install abendrothj/tap/simgit
codex plugin marketplace add abendrothj/simgit
codex plugin add simgit@simgit
```

The pre-release note under [Claude Code](#claude-code) applies here too.

The plugin brings the `simgit-worktrees` skill and the `SessionStart` and
`SubagentStop` hooks; Codex ignores the Claude Code-only `WorktreeCreate` and
`WorktreeRemove`. Codex runs plugin hooks only after you trust them with
`/hooks`; the skill works without them.

Codex has no hook that replaces worktree creation, so in Codex the agent
allocates its own worktrees through the skill (`simgit add --json --ephemeral`)
and releases them with the returned `cleanup_token`; idle, clean ephemeral
leftovers can be reaped with [`simgit gc`](reference.md#gc).
`SubagentStop` removes nothing there, because Codex subagents never get
`simgit/agent-*` worktrees. Whether Codex shows the version-check message to
you or only to the model has not been checked.

## For agent guard tools

A guard that decides from argv alone which simgit invocations need approval
can write its own rule from this section. The promise behind it — that this
surface stays small and stable — is in
[AGENTS.md](../AGENTS.md#what-approval-gates-depend-on).

Two flags destroy work Git cannot give back:

| Flag | Subcommands | Destroys |
|---|---|---|
| `--discard-dirty` | `remove`, `gc`, `hook` | Uncommitted and untracked files in the removed worktrees. |
| `--delete-unmerged` | `remove`, `gc` | Branches past the merged check; it only takes effect beside `--delete-branch` (`remove`) or `--delete-branches` (`gc`). |

`hook` never takes `--delete-unmerged`. It may delete a branch simgit's merged
check would keep, but only when no commit on it becomes unreachable. Flagless
`hook` is automatic. Everything else is safe to auto-approve without them,
except `run`, whose
argv after `--` is the command a guard must judge — including a nested
`simgit` there. Nothing in configuration or the environment makes a command
destructive.

Matching is exact-word: the executable's basename is `simgit` or `sg`, the
subcommand is its first non-option word (the only global option is `--json`),
and a destructive flag is a word equal to the flag that appears before any
`--`. simgit accepts no abbreviation such as `--discard` and no `=value` form,
and a word after `--` is a positional target. A guard can exempt `--help` or
`-h` anywhere and `gc --dry-run`, which change nothing.

The pitfalls are words whose final value the guard cannot see; review them
wherever a flag could land:

- **Unquoted expansions** such as `$FLAGS`, `$(cmd)`, or `` `cmd` `` split into
  any number of words, so `simgit gc $FLAGS` or `simgit gc --prefix $PREFIX`
  can carry either flag.
- **`"$@"` and `"${array[@]}"`** are quoted yet still expand to several words.
- **A quoted single word** is still one word that can *be* a flag:
  `X=--discard-dirty; simgit remove "$X"` discards the dirty worktree
  containing the current directory. `simgit remove -- "$X"` cannot, because
  the word is after `--`; that is the form a caller can write so a guard can
  prove an expanded target is not a flag.
- **`xargs`** supplies words from stdin: `xargs simgit gc` splits input into
  flags, and `xargs -I {} simgit remove {}` substitutes each whole input line,
  which can be `--discard-dirty`. To find the nested `simgit` at all, the
  guard must know which `xargs` options take a value (`-I {}`, `-a FILE`,
  `-L 1`, `-P 4`); options are case-sensitive, and `-i` does not take a
  separate value where `-I` does, so never case-fold argv.
- **Wrappers** such as `env`, `command`, `exec`, `timeout`, `nice`, `sudo`,
  `sh -c '…'`, and `simgit run -- …` put the real invocation further along;
  `eval` and `sh -c "$CMD"` hide it entirely.

When a guard cannot resolve one of these, it should require approval rather
than assume the flag is absent.
