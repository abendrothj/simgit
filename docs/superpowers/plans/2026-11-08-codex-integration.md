# Codex Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Codex install simgit's existing plugin with the same one install as Claude Code.

**Architecture:** The marketplace entry switches to a `url` source both harnesses accept; Codex then loads the same plugin, skill and hooks. Docs explain the Codex install and that the skill allocates worktrees there.

**Tech Stack:** Claude Code / Codex plugins, Rust tests.

**Spec:** `docs/superpowers/specs/2026-11-08-codex-integration-design.md`

## Global Constraints

- Gates before every commit touching code or manifests: `cargo fmt --all -- --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --all --locked`, `python3 tests/cli_picker.py`.
- No binary or `hooks.json` change; version stays 0.5.0.
- Commits local, one per task; nothing pushed without approval.

## Review Focus

1. A Claude Code user who already added the marketplace with the old `github` entry → `claude plugin marketplace update` picks up the `url` entry and the plugin still installs (Task 3 live check).
2. The `url` entry's `ref` names an unreleased tag before `v0.5.0` exists → install fails clearly in both harnesses; the docs say to install after the release (Task 2 docs).
3. A Codex user who never trusts the hooks → the skill still works and nothing else is promised (Task 2 docs).

---

### Task 1: Marketplace `url` source

**Files:**
- Modify: `.claude-plugin/marketplace.json`
- Test: `sg/tests/claude_code_hook.rs` (`marketplace_ref_names_a_release_no_newer_than_the_crate`)

**Interfaces:**
- The plugin entry's `source` becomes exactly `{"source": "url", "url": "https://github.com/abendrothj/simgit.git", "ref": "v0.5.0"}`.

- [ ] **Step 1:** Update the manifest test to require `source.source == "url"` and `source.url == "https://github.com/abendrothj/simgit.git"`, keeping the `ref` check. Run it → FAIL against the `github` entry.
- [ ] **Step 2:** Change the entry.
- [ ] **Step 3:** Gates → PASS; `claude plugin validate .` → passes.
- [ ] **Step 4:** Commit: `Use a url marketplace source that Codex also reads`.

### Task 2: Documentation

**Files:**
- Modify: `docs/agent-integration.md`, `README.md`, `integrations/claude-code/README.md`

- [ ] **Step 1:** `docs/agent-integration.md`: a Codex section after the Claude Code section: install (`codex plugin marketplace add abendrothj/simgit`, `codex plugin add simgit@simgit`), available once `v0.5.0` is released; trust the hooks with `/hooks` (the skill works without them); worktrees in Codex come from the agent through the skill; Codex uses Claude Code's hook protocol so one plugin serves both; the version-check message reaches the model as context.
- [ ] **Step 2:** `README.md`: Codex beside Claude Code in the integration pointer line. `integrations/claude-code/README.md`: Codex install lines.
- [ ] **Step 3:** Gates → PASS. Commit: `Document installing simgit in Codex`.

### Task 3: Live checks

No code; results go into the spec as "Live results".

- [ ] **Step 1:** Codex: `codex plugin marketplace add` an uncommitted copy of this checkout's marketplace with `ref` set to this branch (pushed) or install from a local copy with `"source": "./"`; `codex plugin add simgit@<name>`; confirm install and that `/hooks` lists `SessionStart` and `SubagentStop`.
- [ ] **Step 2:** Claude Code: install the same entry at local scope; confirm four hooks and the skill load; confirm `claude plugin marketplace update` moves an existing `github`-entry marketplace to the `url` entry.
- [ ] **Step 3:** Remove every test plugin, marketplace and trust entry; confirm `~/.claude` and `~/.codex` config match their backups.
- [ ] **Step 4:** Commit the spec update: `Record the Codex live results`.
