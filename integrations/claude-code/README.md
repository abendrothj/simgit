The simgit plugin makes Claude Code store every worktree it creates with simgit, copy-on-write and outside the repository.

PRE-RELEASE (remove at release): the plugin installs only once `v0.5.0` is tagged, and works only once Homebrew serves simgit 0.5.0.

Install simgit: `brew install abendrothj/tap/simgit`, then add the plugin: `claude plugin install simgit --marketplace abendrothj/simgit`.

Codex loads the same plugin: `codex plugin marketplace add abendrothj/simgit`, then `codex plugin add simgit@simgit`.

Details: [docs/agent-integration.md](../../docs/agent-integration.md#claude-code) and [Codex](../../docs/agent-integration.md#codex).
