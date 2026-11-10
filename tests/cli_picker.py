#!/usr/bin/env python3
"""Exercise workspace selection through a real PTY using only the Python stdlib.

Run after cargo build: python3 tests/cli_picker.py
Set SG to test a different binary. Supports the project's macOS/Linux targets.
"""

import errno
import os
from pathlib import Path
import pty
import re
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


def number_of(output, path):
    """The picker's number for the workspace at `path`."""
    for line in output.splitlines():
        match = re.match(r"\s*(\d+)\. [^\t]*\t" + re.escape(str(path)) + r"\t", line)
        if match:
            return match.group(1)
    raise AssertionError(f"{path} not offered: {output}")


def listing(repo):
    """The picker's listing, read by cancelling it."""
    status, output = interact(repo, ["run", "--", "true"], ["q"])
    assert status != 0 and "cancelled" in output, output
    return output


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

        status, output = interact(repo, args, ["4"])
        assert status == 0 and "(detached)" in output, output
        assert (root / "detached/chosen").read_text() == "--resume"

        status, output = interact(repo, ["run", "--", "sh", "-c", "touch cancelled"], ["q"])
        assert status != 0 and "cancelled" in output, output
        assert not any(root.rglob("cancelled"))

        # A workspace deleted by hand is offered as [missing], and choosing it
        # checks its branch out again where it was.
        subprocess.run(["rm", "-rf", str(root / "beta")], check=True)
        output = listing(repo)
        beta = [line for line in output.splitlines() if str(root / "beta") in line]
        assert len(beta) == 1 and beta[0].rstrip().endswith("[missing]"), output
        alpha = [line for line in output.splitlines() if str(root / "alpha") in line]
        assert len(alpha) == 1 and "[missing]" not in alpha[0], output
        status, output = interact(repo, args, [number_of(output, root / "beta")])
        assert status == 0, output
        assert (root / "beta/chosen").read_text() == "--resume"
        assert "[missing]" not in listing(repo)

        # One whose detached HEAD no ref reaches would lose that commit, so it
        # is refused, the registration kept, and the prompt asked again.
        lost = root / "lost"
        checked(repo, "git", "worktree", "add", "--detach", str(lost), "HEAD")
        checked(lost, "git", "commit", "-qm", "only here", "--allow-empty")
        lost_sha = checked(lost, "git", "rev-parse", "HEAD").stdout.strip()
        subprocess.run(["rm", "-rf", str(lost)], check=True)
        output = listing(repo)
        status, output = interact(repo, args, [number_of(output, lost), "q"])
        assert status != 0 and "cancelled" in output, output
        assert "no branch or tag reaches" in output, output
        assert f"git -C {repo} branch <name> {lost_sha}" in output, output
        assert "then run again" in output and "simgit remove" not in output, output
        assert "Choose another workspace." in output, output
        assert str(lost) in checked(repo, "git", "worktree", "list").stdout
        assert not lost.exists()

        # One a tag reaches is created again detached at its own commit, not
        # at the caller's HEAD.
        tagged = root / "tagged"
        checked(repo, "git", "worktree", "add", "--detach", str(tagged), "HEAD")
        checked(tagged, "git", "commit", "-qm", "tagged", "--allow-empty")
        checked(tagged, "git", "tag", "kept")
        kept = checked(repo, "git", "rev-parse", "kept").stdout.strip()
        assert kept != checked(repo, "git", "rev-parse", "HEAD").stdout.strip()
        subprocess.run(["rm", "-rf", str(tagged)], check=True)
        output = listing(repo)
        status, output = interact(repo, args, [number_of(output, tagged)])
        assert status == 0, output
        assert (tagged / "chosen").read_text() == "--resume"
        assert checked(tagged, "git", "rev-parse", "HEAD").stdout.strip() == kept

        # The main checkout has no registration to mark.
        output = listing(repo)
        status, output = interact(repo, ["run", "--ephemeral", "--", "touch", "marked"],
                                  [number_of(output, repo), "q"])
        assert status != 0 and "main checkout" in output, output
        assert "Choose another workspace." in output, output
        assert not (repo / "marked").exists()
        assert not (repo / ".git/simgit-ephemeral").exists()

        # An allocation by a live simgit process holds its target out of the way
        # while it populates it: that is no deleted workspace. The same lock
        # left by a process that is gone is.
        gone = subprocess.Popen(["true"])
        gone.wait()
        for name, pid, marked in (("busy", os.getpid(), False), ("stranded", gone.pid, True)):
            held = root / name
            checked(repo, "git", "worktree", "add", "-b", name, str(held), "HEAD")
            checked(repo, "git", "worktree", "lock", "--reason",
                    f"simgit: allocating (pid {pid})", str(held))
            subprocess.run(["rm", "-rf", str(held)], check=True)
            line = [l for l in listing(repo).splitlines() if str(held) in l]
            assert len(line) == 1 and line[0].rstrip().endswith("[missing]") == marked, line

        result = subprocess.run([SG, "run", "--", "true"], cwd=repo, capture_output=True, text=True)
        assert result.returncode != 0 and "requires a terminal" in result.stderr, result
    print("OK: picker selection, retry, detached worktrees, missing workspaces, the main checkout, cancellation, terminal/argument passthrough, and noninteractive rejection")


if __name__ == "__main__":
    main()
