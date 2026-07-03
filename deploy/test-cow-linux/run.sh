#!/usr/bin/env bash
# Linux overlayfs integration test for the CoW backend.
#
# Runs inside a privileged container. /work is a tmpfs so the overlay upperdir
# is NOT on the container's own overlay2 rootfs (overlay-on-overlay is rejected
# by the kernel). Asserts the session is a real overlay mount (not the reflink
# fallback), then that write/modify/delete commit to the correct git tree.
set -euo pipefail

echo "[build] compiling simgitd + sg…"
cargo build --workspace --exclude simgit-py 2>&1 | grep -E "Finished|error" | tail -3

SIMGITD=/app/target/debug/simgitd
SG=/app/target/debug/sg
W=/work/repo
rm -rf "$W"; mkdir -p "$W"; cd "$W"

git config --global user.email t@t.co
git config --global user.name t
git init -q
printf 'baseline\n' > keep.txt
printf 'delete-me\n' > gone.txt
printf 'original\n' > mod.txt
mkdir -p sub && printf 'nested\n' > sub/deep.txt
git add -A && git commit -qm init

export SIMGIT_REPO="$W"
export SIMGIT_STATE_DIR="$W/.git/simgit"
export SIMGIT_METRICS_ENABLED=0
export SIMGIT_BACKEND=cow
RUST_LOG=info "$SIMGITD" > "$W/daemon.log" 2>&1 &
DPID=$!
trap 'kill $DPID 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do [ -f "$SIMGIT_STATE_DIR/control.port" ] && break; sleep 0.2; done
SOCK="$SIMGIT_STATE_DIR/control.port"

MNT=$("$SG" --socket "$SOCK" worktree add feat/ovl)
echo "[mount] $MNT"

fail=0

echo "[assert] session is a real overlay mount (not reflink fallback)"
if mount | grep -F "$MNT" | grep -q overlay; then
  echo "  PASS: overlay mount present"
else
  echo "  FAIL: not an overlay mount — fell back to clone"; mount | grep -F "$MNT" || true
  grep -i "falling back" "$W/daemon.log" || true
  fail=1
fi

echo "[assert] baseline files visible through the overlay"
ls "$MNT" | tr '\n' ' '; echo
[ -f "$MNT/keep.txt" ] && [ -f "$MNT/sub/deep.txt" ] || { echo "  FAIL: baseline not served"; fail=1; }

# Make changes directly on the native (overlay) filesystem.
rm "$MNT/gone.txt"
printf 'MODIFIED\n' > "$MNT/mod.txt"
printf 'brand new\n' > "$MNT/added.txt"
rm "$MNT/sub/deep.txt"

SID=$("$SG" --socket "$SOCK" worktree list --json | python3 -c 'import sys,json;print(json.load(sys.stdin)[0]["session_id"])' 2>/dev/null \
      || "$SG" --socket "$SOCK" worktree list --json | grep -o '"session_id":"[^"]*"' | head -1 | cut -d'"' -f4)

echo "[commit] $SID -> feat/ovl"
"$SG" --socket "$SOCK" commit --session "$SID" --branch feat/ovl --message "overlay changes" 2>&1 | grep -iE "branch|error" | head -2

echo "[assert] committed tree is correct (upperdir capture)"
check() { # path expected|__deleted__
  local p="$1" want="$2" got
  if [ "$want" = "__deleted__" ]; then
    if git cat-file -e "refs/heads/feat/ovl:$p" 2>/dev/null; then echo "  FAIL: $p should be deleted"; fail=1; else echo "  ok: $p deleted"; fi
  else
    got=$(git cat-file -p "refs/heads/feat/ovl:$p" 2>/dev/null || echo "__missing__")
    if [ "$got" = "$want" ]; then echo "  ok: $p = $want"; else echo "  FAIL: $p = '$got' (want '$want')"; fail=1; fi
  fi
}
check gone.txt __deleted__
check sub/deep.txt __deleted__
check mod.txt MODIFIED
check added.txt "brand new"
check keep.txt baseline

if [ "$fail" -eq 0 ]; then echo "OVERLAY_TEST: PASS"; else echo "OVERLAY_TEST: FAIL"; exit 1; fi
