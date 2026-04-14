# Linux FUSE Integration Tests

This directory contains Docker setup for running Phase 2's Linux FUSE integration tests on macOS.

## Quick Start

```bash
cd /Users/ja/Desktop/projects/simgit/deploy/test-fuse-linux
docker compose up --build
```

## What It Does

- Builds a Linux Debian container with Rust toolchain and FUSE3 development libraries
- Mounts `/dev/fuse` from the host (required for FUSE operations)
- Runs the two Linux-only FUSE integration tests:
  - `fuse_mount_roundtrip_create_unlink_rename` — Tests create/write/rename/unlink operations
  - `fuse_mount_can_remount_same_session_path` — Tests mount/unmount/remount idempotency

## Expected Output

On success, you'll see:

```
running 2 tests
test fuse_mount_roundtrip_create_unlink_rename ... ok
test fuse_mount_can_remount_same_session_path ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## Troubleshooting

### "Cannot access /dev/fuse"
Ensure your Docker daemon has access to the host's FUSE device. On macOS with Docker Desktop, you may need to use `--privileged` or check Docker settings.

### "modprobe: command not found"
The container already handles this gracefully (the `modprobe` call is wrapped in `|| true`).

### Container exits after tests
This is normal if all tests pass. If you want to inspect the container, modify the `CMD` in `Dockerfile` to run a shell:
```dockerfile
CMD ["/bin/bash"]
```

Then you can manually run the tests:
```bash
cargo test -p simgitd linux_integration_tests -- --ignored --nocapture --test-threads=1
```

## Logs & Debugging

To see full RUST_BACKTRACE output:

```bash
docker compose run --build linux-fuse-tests sh -c \
  "RUST_BACKTRACE=1 cargo test -p simgitd linux_integration_tests -- --ignored --nocapture --test-threads=1"
```

## CI Integration

These tests are automatically scheduled in `.github/workflows/fuse-linux-integration.yml` (runs nightly on GitHub Actions).

For local development on macOS, this Docker setup provides an equivalent environment.
