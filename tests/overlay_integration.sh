#!/usr/bin/env bash
# End-to-end test of the fuse-overlayfs populate mode.
#
# Requires Linux with fuse-overlayfs installed and /dev/fuse available. Exercises
# the mount lifecycle that cannot be tested on macOS: overlay create (baseline as
# lowerdir), clean status, in-worktree commit, baseline immutability, and unmount
# on remove/gc.
#
#   SG=./target/release/sg bash tests/overlay_integration.sh
set -euo pipefail

SG="${SG:-$(pwd)/target/release/sg}"
if ! command -v fuse-overlayfs >/dev/null; then
    echo "SKIP: fuse-overlayfs not installed"
    exit 0
fi

fail() { echo "FAIL: $1" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q
git config user.email t@t
git config user.name t
mkdir sub
echo hello >sub/file.txt
echo root >root.txt
git add -A
git commit -qm init

# Force overlay so the assertion holds regardless of the runner's filesystem.
export SIMGIT_POPULATE=overlay

echo "== add (overlay) =="
out="$("$SG" worktree add agent-1 --ephemeral --json)"
echo "$out"
echo "$out" | grep -q '"mode": "overlay"' || fail "expected overlay mode"
wt="$(echo "$out" | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"

echo "== mounted view is the baseline and is clean =="
test -f "$wt/sub/file.txt" || fail "baseline file missing in overlay"
test -f "$wt/root.txt" || fail "root file missing in overlay"
if ! mount | grep -q "$wt"; then fail "overlay not mounted at $wt"; fi
if [ -n "$(git -C "$wt" status --porcelain)" ]; then fail "overlay worktree dirty on create"; fi

echo "== write + commit inside the overlay =="
echo change >>"$wt/root.txt"
echo new >"$wt/added.txt"
git -C "$wt" add -A
git -C "$wt" commit -qm "agent work"
git -C "$wt" rev-parse HEAD >/dev/null

echo "== baseline (main worktree) is untouched =="
grep -qx root root.txt || fail "baseline root.txt was mutated through the overlay"

echo "== remove unmounts and deregisters =="
"$SG" worktree remove agent-1 --force
if test -d "$wt"; then fail "worktree dir still present after remove"; fi
if mount | grep -q "$wt"; then fail "overlay still mounted after remove"; fi
if "$SG" worktree list --json | grep -q agent-1; then fail "agent-1 still registered"; fi

echo "== gc reaps ephemeral overlay worktrees and unmounts them =="
a="$("$SG" worktree add gc-a --ephemeral --json | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
b="$("$SG" worktree add gc-b --ephemeral --json | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
"$SG" worktree gc --ephemeral --older-than 0s
for d in "$a" "$b"; do
    if test -d "$d"; then fail "gc left $d on disk"; fi
    if mount | grep -q "$d"; then fail "gc left $d mounted"; fi
done

echo "OK: overlay integration passed"
