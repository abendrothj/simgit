# Agent integration

The canonical executable name is `simgit`. The installed `sg` command is an
equivalent alias, but integrations and examples in this document use the
canonical name so that command identity is unambiguous.

## Canonical agent policy

Paste the following block verbatim into `AGENTS.md`:

```markdown
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
refuses to install unless the archive's exact entry verifies. It installs both
`simgit` and `sg` into `~/.local/bin` by default; `SIMGIT_INSTALL_DIR` selects
another directory. It does not edit shell startup files. Its own comment header
shows the `curl … | sh` one-liner as a convenience form; ignore it. Reading
that header is exactly what the inspection step above is for, and the
download-inspect-run sequence is the supported way to install from a release.

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
and exits 0. Failures are not JSON: the command exits nonzero, writes nothing
to stdout, and puts one line of diagnostic text on stderr. A provider branches
on the exit status, parses stdout only on success, and reports the stderr line
verbatim rather than trying to parse it; empty stdout on a nonzero exit is the
documented shape, not a malformed response. `run` is the exception to the whole
contract: it streams the child's output, rejects `--json`, and exits with the
child's own status (`128 + signal` when a signal killed the child).

### 1. Discover and verify the executable

For each provider startup, resolve the executable without trusting a possibly
colliding command name:

1. Select the absolute result of `command -v simgit` when present.
2. Otherwise select the absolute result of `command -v sg`.
3. If neither exists, stop and report that simgit is unavailable; installation
   still requires the approval described above.
4. Invoke the selected path with the top-level command `doctor --json`. This
   step needs no repository; run it wherever the provider starts.
5. Parse JSON and require the top-level field `"identity": "simgit"` and a
   top-level `"version": "<semver>"` containing a valid semantic version.
   Reject malformed output, a missing or different identity, or a missing or
   invalid version. The alias must report the same simgit identity; selecting a
   binary merely named `simgit` or `sg` is not sufficient.
6. Only then, run `doctor --json` again from the source repository the
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
| `populate_mode` | string or null | The mode simgit would select here: `cow-clone`, `overlay`, or `git-checkout`. |
| `default_worktree_root` | string or null | Absolute root used by an allocation that passes no `--path`. |
| `default_worktree_root_inside_repository` | boolean or null | Whether that root is inside the source repository. `true` must block allocation against the default root. |
| `git_worktree_supported` | boolean or null | Whether the installed Git supports linked worktrees. |
| `stale_worktree_registrations` | array of strings | Registered worktrees whose directories are gone. An empty list outside a repository, never `null`. |
| `baseline_cache` | object or null | `root` (string), `retained` (array of base-commit oids), `retained_count` (integer), `retained_bytes` (integer). |

The first five fields are identity and machine capability. They are always
present, with or without a repository; steps 4 and 5 above validate the
identity and version among them. The
remaining fields are the repository diagnostics, reported when `doctor` runs
inside a Git worktree: the repository and its common Git ownership, the
populate mode, the default worktree root and whether it is inside the source
repository, linked-worktree support, stale registrations, and retained
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
that relaunches stable-named jobs must choose one of three procedures:

- Reuse instead of reallocating: `simgit run <branch> -- <command>` attaches to
  the branch's registered worktree, or creates one at the branch's current
  commit when only the branch survives. This keeps the job's history.
- Delete the branch as part of cleanup, once its work is integrated, with
  `remove --delete-branch` (step 5). Safe deletion refuses an unmerged branch,
  so this never silently discards work.
- Generate a unique branch per run and keep the stable name in the provider's
  own job record rather than in Git.

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
| `ephemeral` | boolean | Whether the created worktree is marked disposable; provider allocations above require `true`. |

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

Safe cleanup is idempotent. When the target no longer resolves to a worktree —
an earlier attempt already removed it, or the workspace is gone for some other
reason — `remove` exits successfully and reports `"already_absent": true`
beside `"removed"`, `"committed"`, `"commit"`, and `"branch_deleted"`; when the
worktree was present the same output carries `"already_absent": false`.
`removed` is a string, `committed` and `branch_deleted` are booleans, and
`commit` is the full hash a `--commit` removal created or `null`. `removed` is
never evidence that anything was removed: it is present for a path that was
never allocated just as it is for a real removal. Its two shapes differ. When
the target resolved to a worktree, `removed` is that worktree's canonical
symlink-resolved path, even if the provider named it by branch or by a relative
path — so it is safe to compare against a recorded `cleanup_token`. When
nothing resolved, `removed` repeats the target string verbatim, because there
is no worktree left to resolve against. Key the provider's record off
`already_absent` and `branch_deleted`, never off the presence of `removed`.

An at-least-once cleanup path may therefore retry removal without a separate
existence check. `--delete-branch` is the one part that distinguishes the two
target forms once the worktree is gone: a branch-name target still deletes a
leftover branch of that name under the usual `--delete-unmerged` rule, while an
already-absent *path* target is an error, because a path that no longer
resolves cannot say which branch it once held. A provider that deletes branches
should do it while the worktree still exists, passing `cleanup_token`, or
afterwards pass the `branch` value from its own allocation record. A target
that exists but cannot be removed safely still fails.

A removal that fails partway is reported as a failure, not as a partial
success: `--json` failures emit no JSON, so a provider reads the exit status
and the stderr line. Two of those failures are worth handling explicitly.
`remove --commit` commits before it removes, and simgit never rolls that commit
back, so a failure after committing names the commit it kept — a provider that
surfaces the stderr line verbatim passes that fact on, and a retry of the same
command commits nothing a second time, because `--commit` commits only what the
worktree still holds. A failure after the worktree is gone but before
`--delete-branch` finished names the branch instead, because the path form can
no longer identify it; retry that one by branch name.

The provider should treat cleanup as idempotent at the job level: record a
successful removal, and do not reinterpret a missing or mismatched path as
permission to remove some other worktree. Never construct cleanup targets from
an agent-supplied branch name; the allocation record's `cleanup_token`, or its
recorded `branch`, are the only acceptable targets.

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

   `unlock` takes a worktree path or branch, defaulting to the worktree
   containing the current directory, and clears the lock wherever that
   workspace keeps it. A linked worktree — which every provider allocation is —
   is locked through Git's own `.git/worktrees/<slug>/locked`;
   `.git/simgit-run.lock` exists only when `run` targets the main checkout,
   which Git cannot lock. A provider never needs to find the file itself, and
   one that inspects locks directly must look under `.git/worktrees/` for its
   own allocations.

   Unlocking a workspace that is not locked is a success, and so is unlocking a
   target that no longer exists at all: both exit 0 with `"was_locked": false`.
   Recovery may therefore run `unlock` unconditionally, before it knows whether
   the workspace survived. With `--json` it reports `unlocked`, `was_locked`,
   and `owner_pid`.
3. If `unlock` refuses, the recorded owner process is still running and the
   error names its PID. There is no override flag: stop that process, then
   unlock again. An `owner_pid` of `null` is a lock file written without an
   owner and is cleared as unknown-owner.
4. Then either reuse the workspace — relaunch into the same absolute `path`,
   whose files, index, and branch are exactly as the crashed agent left them —
   or clean it up with the ordinary safe removal of step 5, which retains dirty
   work and unmerged branches instead of discarding what the agent produced.
