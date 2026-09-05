#!/usr/bin/env python3
"""Exercise workspace selection through a real PTY using only the Python stdlib.

Run after cargo build: python3 tests/cli_picker.py
Set SG to test a different binary. Supports the project's macOS/Linux targets.
"""

import errno
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import tempfile
import time

SG = str(Path(os.environ.get("SG", "target/debug/sg")).resolve())


def checked(cwd, *args):
    # Picker fixtures must not create mounts that TemporaryDirectory cannot remove.
    return subprocess.run(
        args, cwd=cwd, check=True, capture_output=True, text=True,
        env={**os.environ, "SIMGIT_POPULATE": "checkout"},
    )


def interact(repo, args, answers):
    pid, terminal = pty.fork()
    if pid == 0:
        os.chdir(repo)
        os.execve(SG, [SG, *args], {**os.environ, "SIMGIT_POPULATE": "checkout"})
    output = bytearray()
    sent = 0
    reaped = False
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            readable, _, _ = select.select([terminal], [], [], 0.1)
            if readable:
                try:
                    chunk = os.read(terminal, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b""
                output.extend(chunk)
                # Wait for each prompt before typing; do not race terminal setup.
                if output.count(b"or q to cancel: ") > sent and sent < len(answers):
                    os.write(terminal, answers[sent].encode() + b"\n")
                    sent += 1
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                reaped = True
                # Drain the final child output before asserting.
                while select.select([terminal], [], [], 0)[0]:
                    try:
                        chunk = os.read(terminal, 65536)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
                        break
                    if not chunk:
                        break
                    output.extend(chunk)
                return os.waitstatus_to_exitcode(status), output.decode(errors="replace")
        raise AssertionError(f"picker timed out: {output.decode(errors='replace')}")
    finally:
        if not reaped:
            os.killpg(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(terminal)


def main():
    with tempfile.TemporaryDirectory(prefix="simgit-picker-") as temporary:
        root = Path(temporary).resolve()
        repo = root / "repo"
        repo.mkdir()
        checked(repo, "git", "init", "-qb", "main")
        checked(repo, "git", "config", "user.email", "test@example.com")
        checked(repo, "git", "config", "user.name", "Test User")
        checked(repo, "git", "commit", "-qm", "initial", "--allow-empty")
        for name in ("alpha", "beta"):
            checked(repo, SG, "run", name, "--path", str(root / name), "--", "true")
        checked(repo, "git", "worktree", "add", "--detach", str(root / "detached"), "HEAD")
        # Git lists main first, then linked worktrees in path order.
        args = ["run", "--", "sh", "-c",
                'test -t 0 && test -t 1 && test -t 2 && printf "%s" "$1" > chosen',
                "sh", "--resume"]
        status, output = interact(repo, args, ["0", "not-a-number", "2"])
        assert status == 0, output
        assert "Enter a number" in output and "alpha" in output and "beta" in output, output
        assert (root / "alpha/chosen").read_text() == "--resume"
        assert not (root / "beta/chosen").exists()
        assert not (repo / "chosen").exists()

        # The same picker is available through the original command spelling.
        status, output = interact(repo, ["worktree", *args], ["3"])
        assert status == 0, output
        assert (root / "beta/chosen").read_text() == "--resume"

        status, output = interact(repo, args, ["4"])
        assert status == 0 and "(detached)" in output, output
        assert (root / "detached/chosen").read_text() == "--resume"

        status, output = interact(repo, ["run", "--", "sh", "-c", "touch cancelled"], ["q"])
        assert status != 0 and "cancelled" in output, output
        assert not any(root.rglob("cancelled"))

        result = subprocess.run([SG, "run", "--", "true"], cwd=repo, capture_output=True, text=True)
        assert result.returncode != 0 and "requires a terminal" in result.stderr, result
    print("OK: picker selection, retry, detached worktrees, cancellation, terminal/argument passthrough, and noninteractive rejection")


if __name__ == "__main__":
    main()
