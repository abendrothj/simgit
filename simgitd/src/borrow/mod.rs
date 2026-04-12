//! Borrow registry — enforces Rust-like ownership semantics on filesystem paths.
//!
//! # Overview
//!
//! The borrow registry implements the core locking mechanism that prevents data races
//! in multi-agent scenarios. It enforces the invariant:
//!
//! > At most one session may hold an exclusive (write) lock on any path at any time.
//! > Multiple sessions may hold shared (read) locks on the same path simultaneously.
//!
//! This mirrors Rust's borrow checker semantics `&mut T` (exclusive) and `&T` (shared).
//!
//! # Rules
//!
//! 1. **Exclusive Write**: The first session to acquire a write lock on a path holds it uniquely.
//!    If another session attempts to write the same path while the lock is held, the request fails
//!    immediately with [`simgit_sdk::BorrowError`] (no queueing or blocking).
//!
//! 2. **Shared Read**: Multiple sessions can acquire read locks on the same path simultaneously.
//!    Reads never block. A reader can proceed even if a writer holds the same path.
//!
//! 3. **Re-entrance**: A session can call `acquire_write` on a path it already holds — this succeeds.
//!    (Prevents deadlocks in re-entrant code.)
//!
//! 4. **Atomicity**: Lock acquisitions/releases are atomic (protected by a Mutex).
//!
//! 5. **TTL Expiry**: Locks can have a time-to-live. The TTL sweeper (in [`ttl_sweeper`])
//!    periodically releases stale locks to recover from daemon crashes.
//!
//! # Example
//!
//! ```ignore
//! let reg = BorrowRegistry::new(session_manager);
//!
//! // Session 1 acquires write lock on /src/main.rs
//! reg.acquire_write(session_id_1, Path::new("/src/main.rs"), Some(30)).ok();
//!
//! // Session 2 tries to write the same path → BorrowError
//! let err = reg.acquire_write(session_id_2, Path::new("/src/main.rs"), Some(30));
//! assert!(err.is_err());
//! if let Err(borrow_err) = err {
//!     eprintln!("Conflict: {}", borrow_err);
//!     eprintln!("Held by: {} (task: {})", borrow_err.holder.session_id, borrow_err.holder.task_id);
//! }
//!
//! // Multiple readers of the same path is fine
//! reg.acquire_read(session_id_3, Path::new("/README.md"));
//! reg.acquire_read(session_id_4, Path::new("/README.md"));
//! // Both succeed
//!
//! // When session 1 ends, its locks are released
//! reg.release_session(session_id_1);
//! // Now session 2 could acquire the lock on /src/main.rs
//! ```
//!
//! # Implementation Notes
//!
//! - The lock table is in-memory (HashMap), protected by a Mutex.
//! - SQLite persistence is handled by the [`crate::session::Session`] module separately.
//! - This in-memory map is the fast-path for all lock operations.
//! - No async/await in lock operations; all are synchronous (millisecond-level blocking tolerance).
//! - Path canonicalization is the caller's responsibility (VFS layer).

pub mod registry;
pub mod ttl_sweeper;

pub use registry::BorrowRegistry;
