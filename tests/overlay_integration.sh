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
    fusermount3 -uz "$path" 2>/dev/null || fusermount -uz "$path"
    for _ in $(seq 1 100); do
        if ! mount_present "$path" && ! test -e "$path/.git"; then return 0; fi
        sleep 0.1
    done
    fail "overlay remained accessible after unmount: $path"
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
out="$("$SG" --json add agent-1 --ephemeral)"
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
repair="$("$SG" --json add repair-me --ephemeral | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
echo preserved >"$repair/preserved.txt"
sync "$repair/preserved.txt"
unmount_and_wait "$repair"
repair_out="$("$SG" repair)"
echo "$repair_out"
grep -qx preserved "$repair/preserved.txt" || fail "repair lost upperdir data"
git -C "$repair" status --porcelain >/dev/null || fail "repaired overlay is not a usable Git worktree"
"$SG" remove repair-me --discard-dirty --delete-branch

echo "== run reuses and repairs an existing overlay =="
resume="$("$SG" --json add resume-me | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
echo ongoing >"$resume/chat.txt"
sync "$resume/chat.txt"
unmount_and_wait "$resume"
"$SG" run resume-me -- sh -c 'grep -qx ongoing chat.txt'
"$SG" remove resume-me --discard-dirty --delete-branch

echo "== failed unmount preserves files and reports failure =="
blocked="$("$SG" --json add blocked-me | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
echo keep-me >"$blocked/keep.txt"
mkdir fail-unmount
for tool in fusermount3 fusermount umount; do
    printf '#!/bin/sh\nexit 1\n' >"fail-unmount/$tool"
    chmod +x "fail-unmount/$tool"
done
if PATH="$PWD/fail-unmount:$PATH" "$SG" remove blocked-me --discard-dirty; then
    fail "remove succeeded despite failed unmount"
fi
mount_present "$blocked" || fail "failed teardown detached the mount unexpectedly"
grep -qx keep-me "$blocked/keep.txt" || fail "failed teardown deleted work"
"$SG" remove blocked-me --discard-dirty --delete-branch

echo "== stale unmounted overlays can be removed without remounting =="
stale="$("$SG" --json add stale-me --ephemeral | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
unmount_and_wait "$stale"
"$SG" remove stale-me --discard-dirty --delete-branch
if test -d "$stale"; then fail "stale overlay directory survived remove"; fi
if git show-ref --verify --quiet refs/heads/stale-me; then fail "stale branch survived remove"; fi

echo "== remove unmounts and deregisters =="
"$SG" remove agent-1 --delete-branch --delete-unmerged
if test -d "$wt"; then fail "worktree dir still present after remove"; fi
if mount | grep -q "$wt"; then fail "overlay still mounted after remove"; fi
if "$SG" --json list | grep -q agent-1; then fail "agent-1 still registered"; fi

echo "== gc reaps ephemeral overlay worktrees and unmounts them =="
a="$("$SG" --json add gc-a --ephemeral | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
b="$("$SG" --json add gc-b --ephemeral | sed -n 's/.*"worktree": "\(.*\)".*/\1/p')"
"$SG" gc --older-than 0s --delete-branches --discard-dirty
for d in "$a" "$b"; do
    if test -d "$d"; then fail "gc left $d on disk"; fi
    if mount | grep -q "$d"; then fail "gc left $d mounted"; fi
done
if git show-ref --verify --quiet refs/heads/gc-a; then fail "gc-a branch survived gc"; fi
if git show-ref --verify --quiet refs/heads/gc-b; then fail "gc-b branch survived gc"; fi

echo "OK: overlay integration passed"
