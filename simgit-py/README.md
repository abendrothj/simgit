# simgit-py

Python bindings for `simgit` via PyO3.

## Requirements

- Python 3.9+
- Rust toolchain (stable)
- `maturin` for build/publish flow

## Local Development

From the repository root:

```bash
python3 -m pip install --upgrade maturin
maturin develop -m simgit-py/Cargo.toml
```

This installs the extension into your active virtual environment.

## Build a Wheel

```bash
maturin build -m simgit-py/Cargo.toml --release
```

Wheels are emitted under `target/wheels/`.

## Publish to PyPI

```bash
maturin publish -m simgit-py/Cargo.toml
```

Use `MATURIN_PYPI_TOKEN` for non-interactive CI publishing.

## Quick Usage

```python
import simgit

session = simgit.Session.new(task_id="example-task")
print(session.session_id)
print(session.diff())
session.abort()
```

## Conflict Diagnostics

When `session.commit()` fails due to overlapping active-session changes, the Python
binding surfaces structured conflict details (conflicting paths plus blocking peer
session/task identities) instead of a path-only error.

This is useful for automated retry logic, conflict-aware orchestration, and stress
test triage.