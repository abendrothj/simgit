#!/usr/bin/env bash
# End-to-end test of the fuse-overlayfs populate mode.
#
# Requires Linux with fuse-overlayfs installed and /dev/fuse available. Exercises
# the mount lifecycle that cannot be tested on macOS: overlay create (baseline as
# lowerdir), clean status, in-worktree commit, baseline immutability, and unmount
# on remove/gc, reboot-style remount recovery, and stale-state cleanup.
#
#   SG=./target/release/sg bash tests/overlay_integration.sh
set -euo pipefail

SG="${SG:-$(pwd)/target/release/sg}"
if ! command -v fuse-overlayfs >/dev/null; then
    echo "SKIP: fuse-overlayfs not installed"
    exit 0
fi

fail() { echo "FAIL: $1" >&2; exit 1; }
mount_present() {
    local path="$1"
    awk -v path="$path" '$5 == path { found = 1 } END { exit(found ? 0 : 1) }' /proc/self/mountinfo
}
unmount_and_wait() {
    local path="$1"
    fusermount3 -u "$path" 2>/dev/null || fusermount -u "$path"
    for _ in $(seq 1 100); do
        if ! mount_present "$path" && ! test -e "$path/.git"; then return 0; fi
        sleep 0.1
    done
    fail "overlay remained accessible after unmount: $path"
}
crash_and_wait() {
    local path="$1"
    pkill -f "fuse-overlayfs.*${path}" || fail "could not find overlay process for $path"
    for _ in $(seq 1 100); do
        if ! mount_present "$path" && ! test -e "$path/.git"; then return 0; fi
        sleep 0.1
    done
    fail "overlay remained accessible after process crash: $path"
}

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

echo "== repair remounts an interrupted overlay without losing its upperdir =="
repair="$("$SG" worktree add repair-me --ephemeral --json | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
echo preserved >"$repair/preserved.txt"
sync "$repair/preserved.txt"
crash_and_wait "$repair"
repair_out="$("$SG" worktree repair)"
echo "$repair_out"
grep -qx preserved "$repair/preserved.txt" || fail "repair lost upperdir data"
git -C "$repair" status --porcelain >/dev/null || fail "repaired overlay is not a usable Git worktree"
"$SG" worktree remove repair-me --force --delete-branch

echo "== stale unmounted overlays can be removed without remounting =="
stale="$("$SG" worktree add stale-me --ephemeral --json | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
unmount_and_wait "$stale"
"$SG" worktree remove stale-me --force --delete-branch
if test -d "$stale"; then fail "stale overlay directory survived remove"; fi
if git show-ref --verify --quiet refs/heads/stale-me; then fail "stale branch survived remove"; fi

echo "== remove unmounts and deregisters =="
"$SG" worktree remove agent-1 --force --delete-branch
if test -d "$wt"; then fail "worktree dir still present after remove"; fi
if mount | grep -q "$wt"; then fail "overlay still mounted after remove"; fi
if "$SG" worktree list --json | grep -q agent-1; then fail "agent-1 still registered"; fi

echo "== gc reaps ephemeral overlay worktrees and unmounts them =="
a="$("$SG" worktree add gc-a --ephemeral --json | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
b="$("$SG" worktree add gc-b --ephemeral --json | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
"$SG" worktree gc --ephemeral --older-than 0s --delete-branches --force
for d in "$a" "$b"; do
    if test -d "$d"; then fail "gc left $d on disk"; fi
    if mount | grep -q "$d"; then fail "gc left $d mounted"; fi
done
if git show-ref --verify --quiet refs/heads/gc-a; then fail "gc-a branch survived gc"; fi
if git show-ref --verify --quiet refs/heads/gc-b; then fail "gc-b branch survived gc"; fi

echo "OK: overlay integration passed"
