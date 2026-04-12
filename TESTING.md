# Testing Guide

This guide outlines the testing strategy for simgit and explains how to run, write, and validate tests.

## Overview

simgit uses a **three-tier testing strategy**:

1. **Unit Tests** — Fast, isolated tests of individual functions/modules (within each crate)
2. **Integration Tests** — Tests of subsystem interactions (VFS + borrow registry, delta store + session manager)
3. **E2E Tests** — Full daemon + multi-agent scenarios (Phase 2+)

## Running Tests

### Run All Tests
```bash
cargo test --workspace
```

### Run Tests for a Specific Crate
```bash
# SDK tests
cargo test -p simgit-sdk

# Daemon tests
cargo test -p simgitd

# CLI tests
cargo test -p sg
```

### Run Integration Tests Only
```bash
cargo test --test '*' --lib
```

### Run a Specific Test
```bash
cargo test test_borrow_semantics_exclusive_write
```

### Run with Logging
```bash
RUST_LOG=debug cargo test -- --nocapture
```

### Run Single-Threaded (for debugging)
```bash
cargo test -- --test-threads=1 --nocapture
```

## Test Organization

### Module-Level Unit Tests

Unit tests live within each module via `#[cfg(test)]` blocks.

**Location:** `simgitd/src/borrow/registry.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acquire_write_succeeds_when_free() {
        // Inline test of registry.acquire_write()
    }
}
```

**Run:**
```bash
cargo test --lib registry
```

### Integration Tests

Integration tests live in `tests/` at the workspace root and test complete workflows.

**Location:** `tests/borrow_checker_tests.rs`

```rust
#[test]
fn test_exclusive_write_enforcement() {
    // Multi-step scenario:
    // 1. Start daemon
    // 2. Create sessions
    // 3. Verify locking behavior
}
```

**Run:**
```bash
cargo test --test borrow_checker_tests
```

## Writing Tests

### Unit Test Template

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Test should have a descriptive name: test_<component>_<scenario>_<expected_behavior>
    #[test]
    fn test_component_scenario_succeeds() {
        // Arrange
        let input = /* ... */;
        
        // Act
        let result = component.operation(input);
        
        // Assert
        assert!(result.is_ok(), "Expected success but got: {:?}", result);
        assert_eq!(result.unwrap().field, expected_value);
    }

    /// Doc comment explains what this tests.
    #[test]
    fn test_error_case_returns_expected_error() {
        let invalid_input = /* ... */;
        let result = component.operation(invalid_input);
        
        assert!(result.is_err());
        match result.unwrap_err() {
            SpecificError::Kind => { /* OK */ }
            _ => panic!("Wrong error type"),
        }
    }
}
```

### Integration Test Template

```rust
//! Integration test for <subsystem>.
//!
//! Validates behavior across multiple components.

#[test]
fn test_subsystem_workflow_end_to_end() {
    // 1. Setup: Initialize daemon, create temp repo, etc.
    let daemon = DaemonHandle::start();
    let session = daemon.create_session("task-id").unwrap();

    // 2. Execute: Run the workflow
    session.write_file("/src/main.rs", b"code").unwrap();
    let diff = session.diff_vs_head().unwrap();

    // 3. Verify: Assertions on outcome
    assert!(!diff.unified_diff.is_empty());
    session.commit("branch-name", "message").unwrap();

    // 4. Cleanup
    daemon.shutdown();
}
```

## Test Documentation Requirements

Every test **must have a doc comment** explaining:

1. **What it tests** — The component/method
2. **Scenario** — The specific situation (happy path, error case, edge case)
3. **Expected behavior** — What should happen

```rust
/// Test that the borrow registry prevents two sessions from writing the same path.
///
/// Scenario: Session A holds write lock on /src/config.rs.
///           Session B attempts to acquire write lock on the same path.
///
/// Expected: Session B receives BorrowError immediately (no blocking).
#[test]
fn test_exclusive_write_prevents_concurrent_writes() {
    // Arrange & Act & Assert
}
```

## Naming Conventions

### Test Names
- **Pattern:** `test_<component>_<scenario>`
- **Examples:**
  - `test_borrow_registry_exclusive_write_enforced`
  - `test_session_manager_creates_and_persists`
  - `test_delta_store_handles_collisions`
  - `test_rpc_server_parses_valid_json_rpc`

### Test Function Names
- Use `assert_*` for assertions (provided by `assert!`, `assert_eq!`, etc.)
- Use `should_` prefix for helpers that validate preconditions
- Example:
  ```rust
  #[test]
  fn test_borrow_registry_reader_not_blocked_by_writer() {
      // ...
      should_have_acquired_lock(&reg, session1, path);
      assert!(reg.acquire_read(session2, path).is_ok());
  }

  fn should_have_acquired_lock(reg: &BorrowRegistry, session: Uuid, path: &Path) {
      let locks = reg.list(None);
      assert!(locks.iter().any(|l| l.path == path && l.writer_session == Some(session)));
  }
  ```

## Coverage Goals

- **Borrow Registry:** 90%+ coverage (core invariant validation)
- **Delta Store:** 85%+ coverage (content integrity, atomicity)
- **Session Manager:** 80%+ coverage (CRUD, recovery)
- **RPC Methods:** 75%+ coverage (happy + error paths)
- **VFS Router:** 70%+ coverage (dispatch logic)

Check coverage with:
```bash
# Install tarpaulin (Rust coverage tool)
cargo install cargo-tarpaulin

# Run with coverage
cargo tarpaulin --workspace --out Html
```

## Common Assertions

```rust
// Basic assertions
assert!(condition);
assert_eq!(actual, expected);
assert_ne!(actual, unexpected);

// Option assertions
assert!(option.is_some());
assert!(option.is_none());

// Result assertions
assert!(result.is_ok());
assert!(result.is_err());

// Error handling
assert_eq!(error_result.unwrap_err().kind, ExpectedErrorKind::Something);

// Collection assertions
assert_eq!(vec.len(), 3);
assert!(vec.contains(&item));
```

## Debugging Tests

### Print During Test Execution
```rust
#[test]
fn test_something() {
    eprintln!("Debug info: {:?}", value);  // Use eprintln!, not println!
    assert!(condition);
}
```

### Run with Log Output
```bash
RUST_LOG=debug cargo test test_name -- --nocapture
```

### Single-Threaded Execution (prevents race conditions from masking bugs)
```bash
cargo test -- --test-threads=1
```

### gdb Integration
```bash
rust-gdb ./target/debug/deps/simgitd-<hash>
```

## Continuous Integration

Tests are run on every commit:

```yaml
# .github/workflows/test.yml (TODO: implement)
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --workspace
      - run: cargo test --doc
```

## Phases & Extended Testing

### Phase 0 (Complete)
- ✅ Module-level documentation tests
- ✅ Basic borrow registry semantics
- ⬜ Full integration tests deferred to Phase 1

### Phase 1
- Add full integration tests for read-only VFS
- Add daemon startup/shutdown test harness
- Add multi-agent concurrency tests

### Phase 2
- Add delta store integrity tests
- Add session persistence + recovery tests
- Add RPC method tests (all 7 methods)

### Phase 3+
- Add E2E multi-agent scenarios
- Add performance / stress tests
- Add chaos engineering tests (daemon crashes, network failures)

## FAQ

**Q: Should I test private functions?**
A: No. Test the public API contract. Private functions are implementation details.

**Q: How do I handle non-deterministic tests (e.g., timing)?**
A: Avoid them. Use mock clocks or inject time sources. For unavoidable timing tests, add retries:
```rust
let attempts = 3;
for attempt in 0..attempts {
    if test_passes() { return; }
    if attempt < attempts - 1 { std::thread::sleep(Duration::from_millis(100)); }
}
panic!("Test failed after {} retries", attempts);
```

**Q: How do I test async code?**
A: Use `#[tokio::test]` macro:
```rust
#[tokio::test]
async fn test_async_operation() {
    let result = async_fn().await;
    assert!(result.is_ok());
}
```

**Q: What about external dependencies?**
A: Mock them using `mockall` or similar:
```rust
#[cfg(test)]
mod tests {
    use mockall::mock;
    mock! {
        Git {}
        impl GitService for Git {
            fn read_blob(&self, oid: &str) -> Result<Vec<u8>>;
        }
    }
}
```

## References

- [Rust Testing Book](https://doc.rust-lang.org/book/ch11-00-testing.html)
- [Rust Test Driven Development](https://docs.rust-embedded.org/book/intro.html)
- [Criterion.rs](https://criterion.rs/) for benchmarking
- [Proptest](https://docs.rs/proptest/) for property-based testing

---

**Last updated:** 2026-04-12
