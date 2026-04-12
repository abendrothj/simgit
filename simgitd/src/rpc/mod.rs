//! JSON-RPC 2.0 server over Unix domain socket.
//!
//! # Overview
//!
//! The RPC server is the daemon's public interface. All CLI commands and SDK calls
//! communicate via JSON-RPC 2.0 over a Unix domain socket, enabling:
//!
//! - **Language-agnostic**: Python, Node.js, Go agents can all use the same interface
//! - **Secure**: Unix socket ACL (mode 0600) restricts to current user
//! - **Simple**: Newline-delimited JSON (no length prefixes)
//! - **Async**: All methods are non-blocking (backed by tokio)
//!
//! # Protocol
//!
//! ## Request
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "method": "session.create",
//!   "params": { "task_id": "feature-x", "base_commit": "abc123..." },
//!   "id": 1
//! }
//! ```
//!
//! ## Response (success)
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "result": { "session_id": "uuid-v7-here", "mount_path": "/vdev/..." },
//!   "id": 1
//! }
//! ```
//!
//! ## Response (error)
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "error": { "code": -32602, "message": "Invalid params", "data": null },
//!   "id": 1
//! }
//! ```
//!
//! # Methods
//!
//! All methods are in [`methods`] module:
//!
//! - **session.create** — Start a new session
//! - **session.list** — Query active sessions
//! - **session.commit** — Flatten and merge
//! - **session.abort** — Discard changes
//! - **session.diff** — Diff vs HEAD
//! - **lock.list** — Show all locks
//! - **lock.wait** — Block until path is free
//!
//! # Error Codes
//!
//! Standard JSON-RPC 2.0 codes:
//! - `-32700`: Parse error
//! - `-32600`: Invalid request
//! - `-32601`: Method not found
//! - `-32602`: Invalid params
//! - `-32603`: Internal error
//!
//! Custom codes:
//! - `-32001`: Session not found
//! - `-32002`: Borrow conflict (write lock held)
//! - `-32003`: Merge conflict
//!
//! # Example Workflow (via `sg` CLI)
//!
//! ```bash
//! # CLI creates session via RPC
//! $ sg new --task "feature-x" --peers
//! session-uuid-here
//!
//! # Mounts /vdev/session-uuid-here
//! # Agent works, VFS captures writes
//!
//! # CLI commits via RPC
//! $ sg commit session-uuid-here --branch feature-x --message "Added feature X"
//! committed: feature-x at abc123def456
//! ```

pub mod server;
pub mod methods;

pub use server::RpcServer;
