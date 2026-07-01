# Contributing to simgit

## Getting started

```bash
git clone https://github.com/abendrothj/simgit.git
cd simgit
cargo build --workspace
cargo test --workspace
```

You'll need:
- Rust stable (1.75+)
- Git (any recent version)
- Linux: FUSE kernel module (standard on all distros)
- macOS: no extra dependencies (NFS built-in)
- Windows: [WinFSP runtime](https://github.com/winfsp/winfsp/releases)

## Project structure

```
simgit/
├── simgitd/          daemon — session manager, borrow checker, delta store, VFS
├── simgit-sdk/       Rust SDK — async JSON-RPC client + shared types
├── simgit-py/        Python bindings — PyO3, maturin wheel
├── sg/               CLI tool — `sg new`, `sg commit`, `sg status`…
├── tests/            Rust integration tests + Python stress harnesses
├── docs/             agent integration, chaos validation
└── deploy/           Docker dev stack (daemon + Prometheus)
```

## Finding something to work on

Issues labeled [good first issue](https://github.com/abendrothj/simgit/labels/good%20first%20issue) are designed for newcomers — no deep codebase knowledge needed. Issues labeled [help wanted](https://github.com/abendrothj/simgit/labels/help%20wanted) are higher-effort features we'd love help with.

## Architecture overview

simgit is a daemon that provides isolated copy-on-write filesystem overlays for AI coding agents. The core concepts:

| Component | What it does |
|-----------|-------------|
| **SessionManager** | Lifecycle: create, commit, abort, recovery |
| **BorrowRegistry** | Per-path write lock enforcement (Rust-style borrow semantics) |
| **DeltaStore** | Content-addressed CoW blob storage |
| **CommitScheduler** | Per-path serialization for conflict-safe commits |
| **VFS Backends** | FUSE (Linux), NFSv3 (macOS), WinFSP (Windows) — all share `SessionVfsOps` |
| **RPC Server** | JSON-RPC 2.0 over TCP loopback |

## Before submitting a PR

1. Run tests: `cargo test --workspace`
2. Lint (if available): `cargo clippy --workspace`
3. Check formatting: `cargo fmt -- --check`
4. Keep commits focused — one concept per commit
5. Update docs if your change affects public API or user-facing behavior

## Code style

- Follow existing patterns — look at neighboring files for conventions
- No comments unless explaining *why*, not *what*
- Error handling: use `anyhow` for application errors, `thiserror` for library types
- Async: tokio multi-thread runtime, `async-trait` for trait objects

## Communication

- Open an issue before starting on anything big
- Questions welcome in issue comments
- PRs should reference the issue they address

## License

MIT — see [LICENSE](LICENSE)
