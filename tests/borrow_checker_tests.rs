//! Borrow-checker integration test placeholder.
//!
//! The behavioral unit tests for the borrow registry (exclusive-write enforcement,
//! multiple-reader coexistence, lock-release semantics, re-entrant acquisition) live
//! in the `#[cfg(test)]` section of `simgitd/src/borrow/registry.rs`.
//!
//! End-to-end integration tests (spawn daemon → create session → write via FUSE →
//! verify EBUSY → commit → re-acquire) are tracked as a Phase 7 hardening item
//! and will live here once simgitd is refactored to expose a test-harness library target.
//!
//! See: `simgitd/src/borrow/registry.rs` tests module for:
//!   - `exclusive_write_blocks_second_writer`
//!   - `writer_reentrant_succeeds`
//!   - `multiple_readers_coexist_on_same_path`
//!   - `release_session_frees_write_lock_for_next_writer`
//!   - `release_session_removes_reader_from_lock_entry`
//!   - `release_all_sessions_removes_lock_entry_entirely`
