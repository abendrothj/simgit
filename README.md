# simgit — Borrow Checker for Filesystems

> Rust-style ownership semantics at the filesystem level for safe multi-agent coding pipelines.

## Overview

**simgit** solves concurrency and isolation in multi-agent coding environments by implementing exclusive-write, shared-read semantics on file paths—like a Rust borrow checker for disk I/O.

Instead of each agent needing its own git worktree copy of the entire repository:
- **One shared read-only view** of HEAD (zero extra disk space)
- **Per-agent delta layers** (Copy-on-Write) that capture mutations
- **Borrow registry** that enforces write exclusivity at session creation time
- **Atomic flatten** to merge deltas into a new git branch

## Quick Start

### Starting the daemon

```bash
# Ensure your working repo is initialized
cd /path/to/repo
simgitd start

# Or with explicit config:
simgitd start --repo-path . --mount-dir /vdev --port 9999
```

### Creating a session

```bash
# Create a new session (returns session UUID)
sg new --task "implement-feature-x"

# Your app can then mount /vdev/<session-id> and read/write normally.
# All writes are captured in the delta layer.

# Check status
sg status <session-id>

# Commit changes to a new branch
sg commit <session-id> --branch feature-x --message "Added feature X"

# Or abort (discard all changes)
sg abort <session-id>
```

## Architecture

```
Agent → /vdev/<session-id>/ → FUSE/NFS Mount → simgitd daemon
                                               ├── VFS Router
                                               ├── Borrow Registry
                                               ├── Delta Store
                                               └── Git Integration
```

- **VFS Mount** (FUSE on Linux; NFS-loopback stub on macOS): Git tree baseline with session delta CoW overlay
- **Borrow Registry**: Tracks { path → (readers[], writer?) }, enforces write-exclusivity
- **Delta Store**: Content-addressed delta blobs per session
- **Session DB**: SQLite table { session_id, task_id, status, locks, delta_refs }
- **Git Integration**: Serves blobs from HEAD; flattens deltas to branches

## Design Decisions (ADR-001)

- **Platform**: Both Linux (FUSE) and macOS (NFS-loopback) from day one
- **Write Granularity**: Whole-file locks (not byte-range)
- **Conflict Resolution**: Auto three-way merge; block on true conflicts
- **Peer Visibility**: Optional per-session (`--peers` flag) — agents see in-flight changes from siblings
- **Git Backend**: `gitoxide` (pure Rust, no C dependencies)
- **Daemon Model**: User-scoped instances (one daemon per user per machine)
- **SDK**: Rust + Python; CLI wraps daemon via JSON-RPC 2.0 over Unix socket

## File Structure

```
simgit/
├── plan                          # Engineering plan + ADRs
├── README.md                     # This file
├── Cargo.toml                    # Workspace manifest
│
├── simgit-sdk/                   # Public SDK (types, client, errors)
│   └── src/
│       ├── lib.rs               # Module re-exports
│       ├── types.rs             # SessionInfo, LockInfo, etc.
│       ├── error.rs             # BorrowError, RpcError codes
│       └── client.rs            # Async JSON-RPC client
│
├── simgitd/                      # Main daemon
│   └── src/
│       ├── main.rs              # Entry point
│       ├── config.rs            # Config loading (TOML + env)
│       ├── daemon.rs            # Daemon loop + signal handling
│       ├── borrow/              # Borrow registry
│       │   ├── mod.rs
│       │   ├── registry.rs      # Exclusive write enforcement
│       │   └── ttl_sweeper.rs   # Release stale locks (30s timeout)
│       ├── delta/               # Delta store
│       │   ├── mod.rs
│       │   ├── store.rs         # Content-addressed blob storage
│       │   ├── flatten.rs       # Convert delta → git branch
│       │   └── manifest.rs      # Delta manifest format
│       ├── session/             # Session lifecycle
│       │   ├── mod.rs
│       │   ├── db.rs            # SQLite persistence
│       │   ├── manager.rs       # Session creation/cleanup
│       │   └── recovery.rs      # Crash recovery
│       ├── rpc/                 # JSON-RPC 2.0 server
│       │   ├── mod.rs
│       │   ├── server.rs        # Unix socket listener
│       │   └── methods.rs       # RPC method handlers
│       ├── vfs/                 # VFS abstraction layer
│       │   ├── mod.rs           # VfsManager backend selector
│       │   ├── fuse_backend.rs  # FUSE implementation (Linux)
│       │   ├── nfs_backend.rs   # NFS stub (macOS)
│       │   └── git_resolver.rs  # Git tree traversal + caches
│       └── events/              # Pub/sub event broker
│           └── mod.rs           # lock_conflict, peer_commit events
│
├── sg/                           # CLI tool
│   └── src/
│       ├── main.rs              # Entry + subcommand dispatch
│       └── commands/            # Subcommands
│           ├── new.rs           # Create session
│           ├── commit.rs        # Flatten & merge
│           ├── abort.rs         # Discard session
│           ├── status.rs        # Show session state
│           ├── diff.rs          # Diff session vs HEAD
│           ├── lock.rs          # List/wait on locks
│           ├── peer.rs          # Show peer sessions
│           ├── gc.rs            # Garbage collect
│           └── daemon.rs        # Daemon control
│
├── spike/                        # Phase 0 validation spikes
│   ├── git_reader/              # gix blob/tree reading
│   ├── overlay_test/            # CoW overlay simulation
│   └── fuse_passthrough/        # FUSE mount validation
│
└── tests/                        # Integration tests (Phase 1+)
    ├── session_lifecycle.rs
    ├── borrow_checker.rs
    ├── delta_store.rs
    └── e2e_multi_agent.rs
```

## Testing Strategy

### Unit Tests (in each module)

```bash
# Borrow registry: exclusive write enforcement
cargo test -p simgitd borrow::registry::tests

# Delta store: content integrity
cargo test -p simgitd delta::store::tests

# Session manager: CRUD + crash recovery
cargo test -p simgitd session::manager::tests

# RPC methods: error handling + edge cases
cargo test -p simgitd rpc::methods::tests
```

### Integration Tests

```bash
# Full session lifecycle: create → write → commit → verify
cargo test --test session_lifecycle

# Multi-agent concurrency: 5 agents, overlapping paths
cargo test --test e2e_multi_agent
```

### Running All Tests

```bash
cargo test --workspace
```

## Development Workflow

### Phase 0: ✅ Complete
- Workspace scaffold
- Core types + error codes
- VFS abstraction (FUSE + NFS stubs)
- Borrow registry (skeleton)
- Delta store (skeleton)
- Session DB (skeleton)
- RPC server (skeleton)
- CLI (skeleton)
- **Validation spikes**: git_reader ✓, overlay_test ✓, fuse_passthrough ✓

### Phase 1: Read-Only VFS (3 weeks)
- Git tree traversal via gix
- Directory listing (merge git tree + delta additions)
- File attribute serving
- Inode caching + LRU eviction
- Read-only blob serving from git
- Status: ✅ Completed

### Phase 2: Session Delta Store (2 weeks)
- Write capture (delta layer)
- Manifest tracking (deletes, renames, writes)
- Content-addressed blob storage
- Atomic write-then-rename
- Status: ✅ Completed (code + local validation)
- Implemented so far:
    - Existing-file `write` interception into session delta blobs
    - `create` interception for new files into session delta blobs
    - `unlink` and `rename` capture via delta manifest tombstones/renames
    - Tombstone-aware visibility in `lookup`, `readdir`, `getattr`, `open`, and `read`
    - Crash-recovery persistence tests for session metadata and delta directories
    - ACTIVE session mount re-attachment regression via NFS-loopback backend
    - Linux FUSE integration harness added for create/unlink/rename and remount flows
    - Delta-aware metadata updates for file size in `getattr`
    - Remaining external validation: Linux-runtime execution of the FUSE harness

### Phase 3: Borrow Checker (2 weeks)
- Lock acquisition at session creation
- TTL-based lock release (30s)
- Conflict detection + reporting
- Conflict queue (block until resolved)
- Status: ✅ Completed (core lock/conflict semantics)
- Implemented highlights:
    - session-aware `lock.wait` conflict context
    - structured conflict payload via `lock.acquire` RPC

### Phase 4: Flatten & Merge (2 weeks)
- Convert delta → git tree/blob objects
- Auto three-way merge
- Create commit + update branch
- Error handling (merge conflicts)
- Status: 🚧 In progress
- Implemented highlights:
    - pre-commit overlap detection across active sessions in `session.commit`
    - multi-session commit tests for overlap-block and non-overlap success
    - per-path conflict operation reporting (`ours_ops` / `peer_ops`) in overlap payloads
    - structured flatten failure taxonomy (`missing_delta_blob`, `git_conflict`, `git_operation_failed`, `filesystem_io`)

### Phase 5: CLI & SDK (1 week)
- All 9 `sg` subcommands
- Rust SDK (for embedders)
- Python SDK (for agents that don't link Rust)

### Phase 6: Peer Visibility (1 week)
- Opt-in `--peers` flag (show in-flight changes)
- Event broadcasts (lock_acquired, peer_commit)
- Eventual consistency model

### Phase 7: Performance & Polish (2 weeks)
- Persistent LRU blob cache
- Parallel multi-path reads
- Benchmark + profile
- Documentation + README

## Building & Running Tests

```bash
# Build everything
cargo build

# Run all unit+integration tests
cargo test --workspace

# Run a specific test
cargo test borrow::registry::test_exclusive_write

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Run single-threaded (for debugging)
cargo test -- --test-threads=1 --nocapture
```

## Security & Isolation

- **Borrow Locks**: Enforced on mutating file ops (`write`, `unlink`, `rename`) to prevent data races
- **Delta Isolation**: Each session's writes are private until flatten
- **Unix Socket ACL**: RPC socket is mode 0600 (user-only)
- **Session Expiry**: 30-second TTL on locks; cleanup on daemon restart
- **Path Canonicalization**: Prevent directory traversal via symlinks

## Future Work (Post-Phase 7)

- **Byte-range locks** for finer granularity (e.g., parallel line edits)
- **NFS real implementation** (full NFSv3 XDR server for macOS)
- **Network daemon** (daemon runs on central server, agents mount via TCP NFS)
- **Git-native format** (store deltas as git patches for better review UX)
- **CI integration** (trigger automated evals on flatten)

## Contributing

Tests + docs required for all PRs. Run `cargo test --workspace && cargo doc --no-deps --open` before submitting.

## License

MIT
