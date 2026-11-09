# Codex integration

## Goal

Make simgit work in OpenAI Codex with the same one install as Claude Code,
without asking anything of either harness.

Builds on `2026-11-07-claude-code-integration-design.md`; its principle and
contracts are unchanged.

## Findings that shape it

Observed on 2026-11-08 with Codex CLI 0.166.0 and Claude Code 2.1.319:

- Codex reads Claude-format marketplaces and plugins. It installed simgit's
  plugin unchanged from a local marketplace copy, and its `/hooks` view listed
  the plugin's `SessionStart` and `SubagentStop` hooks. Codex ignores the
  Claude-only `WorktreeCreate`/`WorktreeRemove` events.
- The `simgit-worktrees` skill loads in Codex sessions from the plugin.
- Codex runs plugin hooks only after a person trusts them ("Hooks can run
  outside the sandbox after you trust them"); untrusted, they are skipped and
  the skill still works.
- Codex does not accept a `"source": "github"` marketplace entry ("plugin
  `simgit` was not found in marketplace"). It does accept
  `{"source": "url", "url": "<https git url>", "ref": "<ref>"}`, and Claude
  Code installs the same entry with all four hooks and the skill.
- Codex exports `CODEX_THREAD_ID` to the agent's commands. Not used.
- No other harness offers a worktree-creation hook, and per-session cleanup is
  unreliable across them (session ids hidden from tool calls, `SessionEnd`
  absent, unawaited or limited to 1 s). Other harnesses are out of scope.

## Decisions

### One marketplace entry for both harnesses

`.claude-plugin/marketplace.json`'s plugin source becomes
`{"source": "url", "url": "https://github.com/abendrothj/simgit.git", "ref": "v<version>"}`.
The manifest test checks the `url` form and that `ref` names a version no
newer than the crate, as it does today for the `github` form.

### Codex uses `hook claude-code`

Codex speaks Claude Code's hook protocol and loads the same plugin file, so its
payloads go through `simgit hook claude-code` unchanged. No `codex` provider is
added. In Codex, `SubagentStop` is a no-op (Codex subagents never get
`simgit/agent-*` worktrees) and `SessionStart` is the version check, as in
Claude Code.

### In Codex, the skill allocates

Codex has no worktree-creation hook, so worktrees come from the agent through
the skill (`simgit add --json --ephemeral`, cleaned up with its
`cleanup_token`). The docs say so; nothing else changes.

### No automatic cleanup

Rejected: a session-start `simgit gc`. `gc` does not know who created a
worktree, so running it in every repository a session opens would reap any
idle, clean ephemeral worktree there, including a person's own automation and
benchmark worktrees, and the gitignored files inside them (build output,
saved results). Cleanup stays with the agent's `cleanup_token` and with
`simgit gc` when a person runs it.

### No version change

`hooks.json` and the binary do not change. `v0.5.0` is not tagged yet, so this
ships in 0.5.0.

## Changes

- `.claude-plugin/marketplace.json`: the `url` source.
- Test (`sg/tests/claude_code_hook.rs`): the manifest test requires the `url`
  form with `https://github.com/abendrothj/simgit.git`, keeping the `ref`
  check.
- Docs:
  - `docs/agent-integration.md`: a Codex section: install
    (`codex plugin marketplace add abendrothj/simgit`, then
    `codex plugin add simgit@simgit`), trusting the hooks with `/hooks`, that
    the skill works without them and is how worktrees are allocated in Codex,
    and that Codex uses Claude Code's hook protocol so one plugin serves both.
    Where Codex shows the version-check message is unverified (Codex's
    built-in schema has a top-level `systemMessage`, suggesting the user sees
    it), so the docs say so rather than guess.
  - `skills/simgit-worktrees/SKILL.md`: in Codex the agent allocates through
    the skill; the "harness allocates" line applies to Claude Code only
    (found by the final review).
  - README: Codex beside Claude Code in the integration pointer line.
  - `integrations/claude-code/README.md`: Codex install lines.

## Live checks during implementation

1. Codex installs the plugin from the real repository with the `url` entry (at
   `main` until `v0.5.0` exists), and `/hooks` lists `SessionStart` and
   `SubagentStop`.
2. Claude Code still installs and loads all four hooks and the skill from the
   `url` entry.
3. Optional, the user's step: after trusting the hooks in Codex, whether the
   version-check message reaches the user or only the model.

## Out of scope

Other harnesses; session-id tracking; automatic cleanup; a `codex` provider
name; changes to the Claude Code lifecycle.

## Live results

Run on 2026-11-08 with Codex CLI 0.166.0 and Claude Code 2.1.319, using the
committed entry's shape with `url` set to `file://` this checkout and `ref`
set to this branch (no push):

- Codex installed the plugin from the `url` entry with this branch's content,
  and `/hooks` reported two hooks needing review, `SessionStart` and
  `SubagentStop`.
- Claude Code: a marketplace added with the old `github` entry took the `url`
  entry on `claude plugin marketplace update`, then installed the plugin with
  all four hooks and the skill.
- Not run: the Codex version-check message after trusting the hooks (the
  user's step, optional).
