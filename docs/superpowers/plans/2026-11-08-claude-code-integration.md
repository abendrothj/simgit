# Claude Code Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `simgit hook claude-code` and a Claude Code plugin so simgit stores every worktree Claude Code creates, with Claude Code's own lifecycle.

**Architecture:** A new `hook` subcommand in `sg/src/commands/worktree/hook.rs` reads one Claude Code hook payload from stdin and maps each event onto existing operations (`add`, `add --attach`, `remove`). `run`'s reuse/attach/create flow is factored out so `hook` can run it while holding the branch claim. The repository root becomes a Claude Code marketplace with one plugin whose `hooks.json` calls the binary.

**Tech Stack:** Rust (clap 4, serde_json, anyhow), Git, Claude Code plugins.

**Spec:** `docs/superpowers/specs/2026-11-07-claude-code-integration-design.md` — read it before any task; the plan argues from it.

## Global Constraints

- Gates before every commit that touches code: `cargo fmt --all -- --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all --locked`, `python3 tests/cli_picker.py`.
- No new dependencies. Version comparison is a few lines over `X.Y.Z`.
- AGENTS.md invariants hold: flat surface; `--json` is global only (`hook` rejects it, like `run`); no `--force`; every `git` child gets an explicit working directory.
- `hook`'s own exit codes are 0 or 1, never 2. `SubagentStop` handling always exits 0.
- `hook` never passes `--delete-unmerged` and never deletes a commit no other ref reaches. It discards uncommitted work only with `--discard-dirty` in argv, and only on `WorktreeRemove`.
- Branch names: `simgit/<name>`; subagents `simgit/agent-<agent_id>`.
- Repository: from `worktree_path` for `WorktreeRemove`; from `$CLAUDE_PROJECT_DIR`, else the payload's `cwd`, for other events.
- Base: `refs/remotes/origin/HEAD` if it resolves, else `HEAD`; no fetch, except for `pr-<digits>` names.
- Unreached-commit test: `git rev-list --single-worktree <branch> --not --exclude=refs/heads/<branch> --all` run in the main checkout; non-empty means keep the branch.
- Version after this work: `0.5.0` in `Cargo.toml`, `plugin.json`, the `--requires` value in `hooks.json`, and the marketplace `ref` `v0.5.0`.
- Commits: local only; one commit per task. History gets shaped before any push, and nothing is pushed without approval.

## Review Focus

1. A payload whose `cwd` is a subdirectory or another repository → the hook still acts on the project's repository (Task 3 and Task 5 tests).
2. Names Claude Code may send that are not valid ref names (spaces, `..`, trailing `.lock`) → creation fails with one stderr line, no branch or directory left (Task 3 test).
3. A repository with no `origin` remote, or `origin` without `HEAD` → base falls back to `HEAD` (Task 3 test).
4. `WorktreeRemove` or `SubagentStop` on a worktree a `simgit run` is using → refused (remove: exit 1; stop: exit 0), worktree intact (Task 5 and Task 6 tests).
5. `hook` run by hand with empty or non-JSON stdin → exit 1 with one stderr line, no panic (Task 2 test).

---

### Task 0: Branch and baseline

**Files:**
- Modify: `README.md`, `docs/agent-integration.md` (drop the uncommitted draft)
- Add: `docs/superpowers/specs/2026-11-07-claude-code-integration-design.md`, this plan

- [ ] **Step 1:** `git switch -c claude-code-integration` from a clean `main` apart from the files below.
- [ ] **Step 2:** `git restore README.md docs/agent-integration.md` (the hand-installed-scripts draft the spec drops; it was written in this effort, not by the user).
- [ ] **Step 3:** Run the four gates. Expected: all pass on the unchanged code.
- [ ] **Step 4:** Commit the spec and plan: `git add docs/superpowers && git commit -m "Add the Claude Code integration design and plan"`.

### Task 1: Shared reuse/attach/create under a held claim

**Files:**
- Modify: `sg/src/commands/worktree/locks.rs` (`BranchClaim`)
- Modify: `sg/src/commands/worktree.rs` (`create_worktree`, `absolute_path` callers)
- Modify: `sg/src/commands/worktree/launch.rs:53-129`
- Test: unit tests in `sg/src/commands/worktree.rs` `#[cfg(test)]`; existing `sg/tests/cli_workflow.rs` stays green

**Interfaces:**
- Produces:
  - `BranchClaim::acquire_blocking(repo: &RepoContext, branch: &str) -> Result<BranchClaim>` — same file and digest as `acquire`, but `File::lock()` instead of `try_lock()`.
  - `create_worktree(repo: &RepoContext, cwd: &Path, args: &WorktreeAdd, claim: Option<BranchClaim>) -> Result<CreatedWorktree>` — uses the given claim; acquires one with `acquire` (non-blocking, as today) when `None`. All `std::env::current_dir()` reads on this path use `cwd`.
  - `enum Resolved { Existing(PathBuf), Created(CreatedWorktree) }`
  - `resolve_branch_worktree(repo: &RepoContext, cwd: &Path, template: WorktreeAdd, claim: BranchClaim) -> Result<Resolved>` — under `claim`: the registered or overlay worktree for `template.branch` → `Existing`; else create with `attach` set by whether `refs/heads/<branch>` exists → `Created`. Keeps `run`'s existing `--path` mismatch and creation-only option errors.
- `run_in_worktree` calls `resolve_branch_worktree` with `BranchClaim::acquire` (non-blocking) for an explicit branch; the picker path is unchanged.

- [ ] **Step 1: Write the failing unit tests**

```rust
#[test]
fn create_worktree_accepts_a_claim_its_caller_holds() {
    // fixture repo; claim = BranchClaim::acquire(&repo, "held/branch")
    // create_worktree(&repo, &repo.top_level, &add("held/branch"), Some(claim)) is Ok
}

#[test]
fn a_blocking_claim_waits_for_the_holder() {
    // thread A holds acquire("b") for 300ms; thread B's acquire_blocking("b")
    // returns Ok only after A drops (elapsed >= 250ms)
}
```

- [ ] **Step 2:** `cargo test --locked -p simgit-cli create_worktree_accepts a_blocking_claim` → FAIL (signatures don't exist).
- [ ] **Step 3:** Implement the interfaces above; move `launch.rs:75-129`'s logic into `resolve_branch_worktree`.
- [ ] **Step 4:** All four gates → PASS, including every existing `run` test.
- [ ] **Step 5:** Commit: `Share run's reuse, attach and create flow under a held branch claim`.

### Task 2: `hook` command skeleton

**Files:**
- Create: `sg/src/commands/worktree/hook.rs`
- Modify: `sg/src/commands/worktree.rs` (`mod hook; pub use hook::{hook, WorktreeHook};`), `sg/src/lib.rs` (`Commands::Hook`, dispatch, module doc command list)
- Test: `sg/tests/claude_code_hook.rs` (new; own small `git` helper and `HookFixture`)

**Interfaces:**
- Produces:
  - `#[derive(Args)] pub struct WorktreeHook { provider: HookProvider, #[arg(long)] requires: String, #[arg(long)] discard_dirty: bool }`
  - `#[derive(ValueEnum, Clone)] enum HookProvider { ClaudeCode }` (renders as `claude-code`)
  - `pub fn hook(args: WorktreeHook, json: bool) -> Result<()>` — rejects `json` like `run`; checks `--requires`; reads stdin as JSON; dispatches on `hook_event_name`; `SessionStart` and unknown events return `Ok(())`.
  - `fn version_at_least(running: &str, required: &str) -> Result<bool>` — numeric `X.Y.Z`; malformed `required` is an error.
  - `fn project_repo(payload: &serde_json::Value) -> Result<RepoContext>` — `discover_repo` on `$CLAUDE_PROJECT_DIR`, else payload `cwd`.
  - Test fixture `HookFixture { root, repo }` with `fn hook(&self, extra: &[&str], payload: serde_json::Value) -> Output` running `CARGO_BIN_EXE_simgit hook claude-code --requires 0.0.0 <extra>` from `root` (not the repo), with `CLAUDE_PROJECT_DIR=repo` and `SIMGIT_POPULATE=checkout`.
- `--requires` refusal message names both versions: `simgit hook requires simgit >= {required}, found {running}`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn unknown_event_and_session_start_are_no_ops()      // exit 0, empty stdout, repo unchanged
#[test] fn requires_refuses_an_older_binary_with_exit_1()   // --requires 99.0.0 → code 1, stderr contains format!("found {}", env!("CARGO_PKG_VERSION"))
#[test] fn json_flag_is_rejected()                          // `--json hook ...` → code 1, stdout empty
#[test] fn empty_or_non_json_stdin_fails_with_one_line()    // code 1, stderr one line, no "panicked"
#[test] fn unknown_provider_is_an_error()                   // `hook codex` → non-zero, no side effects
```

- [ ] **Step 2:** `cargo test --locked --test claude_code_hook` → FAIL (no `hook` subcommand).
- [ ] **Step 3:** Implement the interfaces.
- [ ] **Step 4:** Gates → PASS.
- [ ] **Step 5:** Commit: `Add simgit hook claude-code with version check and event dispatch`.

### Task 3: `WorktreeCreate`

**Files:**
- Modify: `sg/src/commands/worktree/hook.rs`
- Test: `sg/tests/claude_code_hook.rs`

**Interfaces:**
- Consumes: `resolve_branch_worktree`, `BranchClaim::acquire_blocking` (Task 1); `project_repo` (Task 2).
- Produces:
  - `fn branch_for_name(repo: &RepoContext, name: &str) -> Result<String>` — `simgit/{name}`, validated with `git check-ref-format --branch`.
  - `fn default_base(repo: &RepoContext) -> Result<String>` — `refs/remotes/origin/HEAD` if `rev-parse --verify --quiet` succeeds, else `HEAD`.
  - `fn on_worktree_create(payload: &serde_json::Value) -> Result<()>` — under `acquire_blocking`, `resolve_branch_worktree` with `base: Some(default_base)` (base ignored on reuse/attach); refuses an `Existing` path equal to `main_worktree(..)` or locked (`ensure_unlocked`); prints exactly the path and a newline to stdout; mode and warnings to stderr; persistent (no ephemeral marker).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn create_prints_exactly_one_path()                         // stdout == "<path>\n", path is a dir on branch simgit/s1
#[test] fn create_twice_returns_the_same_path()
#[test] fn concurrent_creates_return_one_fully_populated_path()     // two threads, same name; both exit 0, same path, README.md exists in it
#[test] fn create_attaches_an_existing_branch_without_moving_it()   // pre-create simgit/s2 at a commit; worktree HEAD == that commit
#[test] fn create_never_returns_the_main_checkout()                 // main checkout on simgit/s3 → exit 1
#[test] fn create_refuses_a_run_locked_worktree()                   // `simgit run simgit/s4 -- sleep 2` in background, then create s4 → exit 1
#[test] fn invalid_names_fail_cleanly()                             // "a b", "x..y", "z.lock" → exit 1, no simgit/* ref, no new dir
#[test] fn base_is_origin_default_branch_else_head()                // clone fixture with origin/HEAD ahead of local main → base = origin; no origin → base = HEAD
#[test] fn create_uses_project_dir_not_payload_cwd()                // payload cwd = a subdirectory of another repo; worktree belongs to the project
#[test] fn created_worktree_is_persistent()                         // `simgit list --json` shows ephemeral == false
```

- [ ] **Step 2:** Run them → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Gates → PASS.
- [ ] **Step 5:** Commit: `Allocate Claude Code worktrees on WorktreeCreate`.

### Task 4: Pull request names

**Files:**
- Modify: `sg/src/commands/worktree/hook.rs`
- Test: `sg/tests/claude_code_hook.rs`

**Interfaces:**
- Consumes: `on_worktree_create` (Task 3).
- Produces: `fn fetch_change(repo: &RepoContext, number: u64) -> Result<String>` — `git fetch --quiet origin pull/<n>/head`, else `merge-requests/<n>/head`; returns `FETCH_HEAD`'s commit; both failing is an error naming the number. `on_worktree_create` uses it as the base when `name` matches `^pr-[0-9]+$` and neither the worktree nor the branch exists yet.

- [ ] **Step 1: Write the failing tests** (fixture: a bare "origin" with `refs/pull/7/head` and `refs/merge-requests/8/head` pointing at commits not on `main`)

```rust
#[test] fn pr_name_creates_at_the_pull_request_head()         // name "pr-7" → worktree HEAD == origin's refs/pull/7/head
#[test] fn pr_name_falls_back_to_merge_requests()             // "pr-8" → refs/merge-requests/8/head
#[test] fn pr_name_fails_when_neither_ref_exists()            // "pr-9" → exit 1, no branch, no dir
#[test] fn pr_worktree_is_not_refetched_on_reuse()            // move origin's pull/7/head, create "pr-7" again → same HEAD as before
#[test] fn plain_names_never_fetch()                          // "feature-7": no FETCH_HEAD change
```

- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Gates → PASS.
- [ ] **Step 5:** Commit: `Check out the pull request head for pr-N worktrees`.

### Task 5: `WorktreeRemove` and branch dropping

**Files:**
- Modify: `sg/src/commands/worktree/hook.rs`
- Test: `sg/tests/claude_code_hook.rs`

**Interfaces:**
- Consumes: `teardown_worktree(repo, target, discard_dirty)`, `delete_local_branch(repo, branch_ref, force)`, `list_worktrees`.
- Produces:
  - `fn holds_unreached_commits(repo: &RepoContext, branch: &str) -> Result<bool>` — the Global Constraints command, run with the main checkout as its working directory.
  - `fn drop_branch_if_reached(repo: &RepoContext, branch: &str) -> Result<bool>` — `delete_local_branch(.., force: true)` only when `holds_unreached_commits` is false; returns whether it deleted.
  - `fn on_worktree_remove(payload: &serde_json::Value, discard_dirty: bool) -> Result<()>` — repo from `worktree_path`; refuses a worktree whose branch is not `simgit/*`; `teardown_worktree(.., discard_dirty)`; then `drop_branch_if_reached`. An absent `worktree_path` is success (idempotent).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn remove_refuses_a_worktree_not_on_a_simgit_branch()            // `simgit add other/x` → exit 1, still there
#[test] fn remove_with_discard_dirty_removes_a_dirty_worktree()          // untracked file; any cwd/prompt_id → exit 0, dir gone
#[test] fn remove_without_discard_dirty_refuses_a_dirty_worktree()       // exit 1, file intact
#[test] fn remove_drops_a_branch_with_nothing_unreached()                // clean, no commits → branch gone
#[test] fn remove_keeps_a_branch_with_an_unpushed_commit()               // one commit → worktree gone, branch kept with that commit
#[test] fn remove_drops_a_branch_whose_commits_origin_reaches()          // commit pushed to fixture origin → branch gone
#[test] fn remove_finds_the_repo_from_worktree_path()                    // payload cwd = /tmp, CLAUDE_PROJECT_DIR unset → works
#[test] fn remove_refuses_a_run_locked_worktree()                        // exit 1, intact
#[test] fn removing_an_absent_worktree_succeeds()                        // second identical payload → exit 0
#[test] fn unreached_check_counts_the_branch_while_it_is_checked_out()   // holds_unreached_commits is true before teardown for a branch with a commit
```

- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Gates → PASS.
- [ ] **Step 5:** Commit: `Remove Claude Code worktrees on WorktreeRemove and drop reached branches`.

### Task 6: `SubagentStop`

**Files:**
- Modify: `sg/src/commands/worktree/hook.rs`
- Test: `sg/tests/claude_code_hook.rs`

**Interfaces:**
- Consumes: `project_repo`, `holds_unreached_commits`, `drop_branch_if_reached`, `teardown_worktree`.
- Produces: `fn on_subagent_stop(payload: &serde_json::Value) -> Result<()>` — finds the worktree with branch `simgit/agent-<agent_id>`; removes it (`discard_dirty: false`) and drops the branch only when it is clean, unlocked and `holds_unreached_commits` is false; otherwise does nothing. Every error becomes one stderr line and `Ok(())`. Never reads the payload's `cwd`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn stop_removes_a_clean_subagent_worktree_and_its_branch()
#[test] fn stop_keeps_a_dirty_subagent_worktree()                 // exit 0, file intact, no commit created
#[test] fn stop_keeps_a_worktree_with_an_unpushed_commit()        // exit 0, worktree and branch kept
#[test] fn stop_keeps_a_run_locked_worktree()                     // exit 0
#[test] fn stop_does_nothing_when_the_branch_was_switched_away()  // worktree switched to another branch → branch and dir kept
#[test] fn stop_ignores_a_plain_subagent()                        // agent_id with no simgit/agent-* branch; cwd = main checkout → exit 0, nothing changes
#[test] fn parallel_stops_are_safe()                              // two at once → both exit 0, worktree gone once
#[test] fn stop_exits_zero_outside_a_repository()                 // CLAUDE_PROJECT_DIR=/tmp → exit 0
```

- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** Gates → PASS.
- [ ] **Step 5:** Commit: `Remove clean subagent worktrees on SubagentStop`.

### Task 7: Plugin, marketplace and version

**Files:**
- Create: `.claude-plugin/marketplace.json`, `.claude-plugin/plugin.json`, `integrations/claude-code/hooks.json`, `integrations/claude-code/README.md`
- Modify: `Cargo.toml` (`version = "0.5.0"`), `Cargo.lock`
- Test: `sg/tests/claude_code_hook.rs`

**Interfaces:**
- `marketplace.json`: `{"name": "simgit", "owner": {"name": "abendrothj"}, "plugins": [{"name": "simgit", "source": {"source": "github", "repo": "abendrothj/simgit", "ref": "v0.5.0"}, "description": "..."}]}`
- `plugin.json`: `{"name": "simgit", "version": "0.5.0", "description": "...", "hooks": "./integrations/claude-code/hooks.json"}`
- `hooks.json` (exactly the spec's table):
  - `WorktreeCreate`: `{"type": "command", "command": "simgit", "args": ["hook", "claude-code", "--requires", "0.5.0"]}`
  - `WorktreeRemove`: same args plus `"--discard-dirty"`
  - `SubagentStop`: `{"type": "command", "command": "simgit hook claude-code --requires 0.5.0 || exit 0"}`
  - `SessionStart`: `{"type": "command", "command": "simgit hook claude-code --requires 0.5.0 >/dev/null 2>&1 || echo '{\"systemMessage\": \"The simgit plugin needs simgit 0.5.0 or newer: brew install abendrothj/tap/simgit (or brew upgrade simgit)\"}'"}`
- `integrations/claude-code/README.md`: three lines and a link to the Claude Code section of `docs/agent-integration.md`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn plugin_version_equals_the_cargo_version()        // plugin.json "version" == env!("CARGO_PKG_VERSION")
#[test] fn hooks_require_no_newer_than_the_cargo_version()  // every "--requires" value in hooks.json <= CARGO_PKG_VERSION
#[test] fn marketplace_ref_names_the_cargo_version()        // ref == "v" + CARGO_PKG_VERSION
```

- [ ] **Step 2:** Run → FAIL. **Step 3:** Add the files, bump the version. **Step 4:** Gates → PASS; `claude plugin validate .` → passes.
- [ ] **Step 5:** Commit: `Publish the repository as a Claude Code plugin`.

### Task 8: Documentation

**Files:**
- Modify: `docs/reference.md`, `README.md`, `docs/agent-integration.md`, `AGENTS.md`, `skills/simgit-worktrees/SKILL.md`, `CONTRIBUTING.md`

Content, per AGENTS.md ownership (link rather than repeat):

- [ ] **Step 1:** `docs/reference.md`: a `hook` section (arguments, events table from the spec, exit codes, `--discard-dirty` scope, `--json` rejected) and `hook` in the command list.
- [ ] **Step 2:** `README.md`: a command-table row for `hook` and one line linking to the Claude Code section.
- [ ] **Step 3:** `docs/agent-integration.md`: the storage principle in the provider contract; the "provider allocations require ephemeral" line becomes the harness's choice (hook allocations are persistent); a Claude Code section with install, the spec's lifecycle table, and "Claude Code behaviour the integration works within".
- [ ] **Step 4:** `AGENTS.md`: `hook` in the flat surface list and the `--json` rule; `--discard-dirty` row lists `remove`, `gc`, `hook`; beside `--delete-unmerged`, the note that `hook` may delete a branch the merged check keeps only when nothing becomes unreachable.
- [ ] **Step 5:** `skills/simgit-worktrees/SKILL.md`: with the plugin active the harness allocates and removes, so the agent does not; the version it describes becomes 0.5.0.
- [ ] **Step 6:** `CONTRIBUTING.md` release steps: bump `plugin.json` and `hooks.json` with the version; move `marketplace.json`'s `ref` to the new tag only after the Homebrew formula serves it.
- [ ] **Step 7:** Gates → PASS (doc-only, but `cargo test` covers manifest tests).
- [ ] **Step 8:** Commit: `Document simgit hook and the Claude Code plugin`.

### Task 9: Live install check

No code. Results go into the spec's "Live check results" and "Not yet observed".

- [ ] **Step 1:** Build: `cargo build --release`; put `target/release/simgit` first on `PATH` for the session.
- [ ] **Step 2:** Install the plugin from this checkout: `claude plugin marketplace add ./` then `claude plugin install simgit@simgit --scope local`. Expected: `claude plugin list` shows it. (The pinned `ref` points at an unreleased tag; for this check, temporarily install via a local marketplace copy whose entry uses `"source": "./"`, and do not commit that copy.)
- [ ] **Step 3:** Exercise the spec's lifecycle table and record each row: desktop worktree session, `--worktree`, `--bg --worktree`, `EnterWorktree`, worktree subagent (clean, dirty, with commit), CLI exit remove, `ExitWorktree` remove, desktop "Archive anyway", `--worktree "#<open PR>"`, missing binary (`PATH` without simgit → `SessionStart` message in CLI and desktop).
- [ ] **Step 4:** Try the "Not yet observed" routes the user authorises (agent-view delete, a workflow agent, `/batch`); record or leave them listed.
- [ ] **Step 5:** Uninstall the plugin and marketplace; remove every test worktree and branch; confirm `git worktree list` shows only the main checkout.
- [ ] **Step 6:** Commit the spec update: `Record the Claude Code live install results`.
