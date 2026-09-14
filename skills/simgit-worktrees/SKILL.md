---
name: simgit-worktrees
description: Use when allocating or isolating autonomous-agent or benchmark work in Git worktrees, especially when copy-on-write capacity, disposable workspaces, detached historical runs, or safe automated cleanup matters.
---

# simgit worktrees

`simgit` is the canonical executable; `sg` is its equivalent alias. This skill describes safe use, but does not replace an allocator/provider hook. A capable harness must perform preflight, allocation, and cleanup, pass the cleanup token unchanged, and start the worker with its current directory set to the returned path.

## Fast path

One disposable workspace for one job is the common case, and this is the whole sequence. Each step names the section that owns its rules; read that section for anything the recipe does not cover.

```sh
# 1. Discover and verify — see "Discover and verify the executable"
SIMGIT=$(command -v simgit || command -v sg)
"$SIMGIT" doctor --json     # require top-level "identity": "simgit", or treat it as not installed

# 2. Preflight, run again from the source repository — see "Preflight"
"$SIMGIT" doctor --json     # require git_worktree_supported; read cow_supported, repository owner,
                            # populate_mode, stale_worktree_registrations, baseline_cache; choose an
                            # explicit absolute allocation root outside the source repository

# 3. Allocate an ephemeral workspace — see "Allocate with JSON"
"$SIMGIT" add --json --ephemeral --path <fresh-absolute-path-outside-source>
# read: path — the cwd to launch in; worktree — same absolute path; cleanup_token — retain unchanged;
#       branch — the generated agent/<uuid>, record it; mode — cow-clone, overlay, or git-checkout

# 4. Launch the worker with its cwd set to path; it inspects, edits, commits, and integrates with
#    ordinary git — see "Integrate and clean up"

# 5. Clean up — see "Integrate and clean up"
"$SIMGIT" remove --json "$cleanup_token"    # add --delete-branch while the token still resolves,
                                            # once that branch is merged
"$SIMGIT" gc --json --older-than 0s         # sweep leftovers; --older-than is never optional
"$SIMGIT" prune --json                      # deregister stale entries, age out baselines
```

A nonzero exit is the only failure signal: stdout is empty and stderr carries a one-line reason. Report that text verbatim; never retry a refusal by adding a destructive flag.

Leave the fast path for the cases that need judgement:

- Detached historical, branchless, or read-only work — `--detach --base <commit>`, in **Allocate with JSON**.
- A task that a `git-checkout` fallback would invalidate — `--require-cow`, in **Allocate with JSON**.
- Cleanup refused for a dirty worktree or an unmerged branch — **Integrate and clean up**. `--discard-dirty` and `--delete-unmerged` are never added automatically.
- Cleanup refused by a stranded `run` lock — `simgit unlock`, in **Integrate and clean up**.
- The source repository still carrying baselines — `simgit prune --all --json`, in **Integrate and clean up**.
- No candidate passing the identity check — **Install safely**, which is approval-gated.

## Discover and verify the executable

1. Resolve `command -v simgit`; only if it is absent, resolve `command -v sg`.
2. Run the selected executable's top-level `doctor --json` command. It needs no repository and can be run from any directory.
3. Parse the JSON and require the top-level `identity` field to be exactly `simgit`; reject a missing, invalid, or mismatched identity. (`product` also reads `simgit`, but `identity` is the field this contract is written against.) Do not trust a filename, banner, or unrelated command found earlier on `PATH`.
4. Retain the verified executable path for every subsequent invocation.
In the commands below, `simgit` means this verified absolute executable path, including when discovery selected the alias.

If no candidate passes that identity check, follow **Install safely** below rather than silently changing the user's environment. That includes the common case where `simgit` is absent and the `sg` on `PATH` is a different tool — ast-grep ships an `sg` whose `sg --version` output is deceptively close to simgit's. A command that exists but fails the identity check is not a fallback; treat it as "no simgit installed".

## Preflight

`doctor --json` answers two separate questions; check them in order.

First, identity and machine capability, which do not depend on a repository and can be verified from any directory:

- version and canonical identity; and
- the current directory's filesystem, and whether it supports CoW: `cow_supported` is `true` or `false` when simgit could write a probe there to find out, and `null` when it could not write one at all. `null` describes the directory, not the filesystem, and is not a reason to stop; re-run `doctor` from a writable directory to get a verdict.

Second, repository readiness, reported when `doctor` runs inside the source repository:

- the repository and common Git directory owner;
- the selected populate mode;
- the default worktree root and whether it is inside the source repository;
- Git worktree support;
- stale worktree registrations; and
- retained baseline state.

Outside a Git worktree those repository fields are `null` and the stale-registration list is empty, while identity, version, filesystem, and CoW support are still reported. A null repository means "not in a repository here"; it is never evidence that the executable is wrong or broken, and the harness must not reject the binary for it.

Stop on an identity mismatch, unsupported Git worktrees, or an ownership problem. Treat stale registrations and retained baselines as state to review, not permission to delete work. Never allocate inside the source repository: override an unsafe default and choose an explicit absolute allocation root outside the source repository.

## Allocate with JSON

Always request machine-readable output with the global `--json` flag and parse it; do not scrape human output. Success is one JSON object on stdout. Failures are not JSON: the command exits nonzero, writes nothing to stdout, and puts a one-line reason on stderr. Branch on the exit status, and report the stderr text verbatim rather than trying to parse it.

- Disposable agent or benchmark work: the fast path above. Omitting the branch without `--detach` creates a collision-resistant `agent/<uuid>` branch.
- Named branch work: add the desired branch argument and normally include `--ephemeral` when the workspace is disposable.
- Historical, branchless build, comparison, or read-only disposable work: run `simgit add --json --detach --base <commit> --ephemeral --path <absolute-outside-source-path>`. This creates no branch.
- Add `--require-cow` only when falling back to a full checkout would invalidate the task, such as by violating its disk or scaling budget. Otherwise allow the safe fallback.

`--path` is the worktree directory itself, not a parent to allocate inside: generate a fresh unique path per allocation. Missing parents are created, and an existing *empty* directory is accepted, so a harness that pre-creates one directory per job works; a non-empty directory is refused. Reusing one path for two concurrent allocations is never valid — exactly one of them is allowed to proceed. A path inside the source repository is not refused, only warned about on stderr, and that warning never appears in `--json` output: keeping allocations outside the source repository stays your responsibility.

`add` creates a branch and refuses one that already exists, naming `simgit run <branch> -- <command>` as the way to get a worktree for it. That is the situation after a removal that retained the branch: use `run`, not a second `add`.

Require a successful JSON response. `add --json` retains the top-level `worktree` path string and also returns top-level provider fields `path` and `cleanup_token`. Verify that `worktree`, `path`, and `cleanup_token` identify the same absolute removable worktree. Require top-level `branch` to be a string for a branch worktree and `null` for a detached worktree. After creation, inspect top-level `mode`: `cow-clone`, `overlay`, or `git-checkout`. Reject an unexpected mode, and reject `git-checkout` only when the task truly required CoW.

The harness must retain `cleanup_token`, set the agent or benchmark process's cwd to `path`, and then launch it. Do not merely tell a model to allocate its own workspace from the source checkout.

## Integrate and clean up

The allocated directory is a normal linked Git worktree. Agents inspect, edit, commit, and integrate through ordinary Git; coordinators merge or rebase branches through normal Git workflows.

On completion, give the unchanged `cleanup_token` to the provider's safe cleanup operation. Without a provider cleanup hook, run `simgit remove "$cleanup_token"`; the token is the absolute worktree path. Alternatively, run `simgit gc --older-than <age>`, which selects only ephemeral worktrees unless `--include-persistent` widens it. Always pass `--older-than` explicitly: it is an idle-age filter that defaults to 24h, so a bare `simgit gc` silently reaps nothing belonging to a job that just finished, and exits 0 while doing so. For cleanup right after a job, `--older-than 0s` is the correct value; anything a worktree is skipped for is reported in `skipped` with a reason, including `recently-active` for one that is merely younger than the filter. Prefer plain removal or GC with no destructive flag. Never append `--discard-dirty` or `--delete-unmerged` automatically. Worktree removal normally retains its branch, and cleanup must preserve unmerged branches.

Delete a merged agent branch while its worktree still exists: `simgit remove --json --delete-branch "$cleanup_token"`. That is a safe operation — it refuses an unmerged branch — but it only works while the token still resolves to a worktree. Once the worktree is gone the path cannot name a branch, so `remove <absent-path> --delete-branch` is an error; use the branch name recorded at allocation instead: `simgit remove --json --delete-branch "$branch"`. Do this for every disposable job whose work was merged, or the generated `agent/<uuid>` branches accumulate in the repository permanently.

Removal and GC do not reclaim the baseline cache, which lives inside the source repository at `.git/simgit/baselines` and holds one full tree per distinct base commit. To actually leave no trace, finish with `simgit prune --json`, which also deregisters stale Git worktree registrations and reports them in `pruned_registrations`. Plain `prune` keeps baselines used in the last seven days; `simgit prune --all --json` drops them all now. `--all` destroys no work — a dropped baseline is rematerialized on the next allocation that needs it — but it makes the next allocation pay full price, so use it when the job is finished rather than between jobs. Confirm with `doctor --json`: `baseline_cache.retained_bytes` is what the source repository is still carrying.

Never pass `--discard-dirty` for dirty worktrees or in-progress merges/rebases, and never pass `--delete-unmerged` for unmerged branches, unless the user has explicitly authorized discarding that specific work. Neither flag implies the other, and `--delete-unmerged` applies only alongside `--delete-branch` (removal) or `--delete-branches` (GC). If safe cleanup refuses, report the retained path and branch instead of weakening the safety checks. When it refuses because the worktree is dirty, `--commit -m "<message>"` is the preserving alternative: it commits the leftover changes to the workspace's own branch and then removes the worktree, losing nothing. It still writes a commit on the agent's behalf, so ask before using it — but it is the option to offer instead of abandoning the workspace.

Cleanup is safe to run more than once. `simgit remove` on a target that no longer has a worktree exits successfully and reports `already_absent: true`, so an at-least-once cleanup path may retry without distinguishing "already removed" from "failed to remove". `already_absent` is the field to read; `removed` is present either way and is not evidence that anything was removed. It is the worktree's canonical resolved path when the target resolved to one, and the target string repeated back verbatim when nothing resolved. A target that still exists but cannot be safely removed continues to fail.

If cleanup is refused because the workspace is locked by a `simgit run` whose launcher died, run `simgit unlock <path>` to clear the stranded lock, then retry removal. `unlock` succeeds on a workspace that is not locked and on a target that no longer exists at all, so it is safe to run unconditionally during recovery. It refuses while the PID recorded in the lock still answers `kill(pid, 0)`, and two situations make that PID answer when a naive `ps` finds nothing useful: a killed launcher whose parent has not reaped it yet is a zombie and still answers, and a process the launcher spawned can outlive it in the same process group. So if `ps` on the named PID is empty or shows state `Z` and `unlock` still refuses, stop every surviving process in that launcher's process group (`pkill -g <pid>`; list them first with `ps -eo pid,pgid,stat,command`), let its parent reap it, and unlock again. There is no override flag; do not look for one.

## Install safely

Installation is a separate, approval-gated action:

1. Ask before installing a user-level binary or changing shell configuration or `PATH`.
2. Select and pin an explicit simgit release; do not install an unpinned latest build.
3. On macOS, prefer an available Homebrew installation for that pinned release.
4. Otherwise download the pinned release's installer to a temporary file, inspect it and its release/checksum inputs, then execute that local file only after approval. Never automatically execute a curl-pipe-shell command.
5. After installation, repeat executable discovery and require `doctor --json` to report the canonical `simgit` identity before use.
