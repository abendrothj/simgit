#!/usr/bin/env bash
# Scaling benchmark: does simgit's "avoid N copies of the working tree" claim hold?
# Compares real host disk and time-to-ready for N `git worktree` checkouts vs
# N `sg worktree` sessions. See docs/scaling_benchmark.md for a recorded run.
#
#   NFILES=400 FSIZE_KB=128 NS="1 2 4 8 16" bash tests/bench_scaling.sh
#
# Disk is measured with `du -sxk` (-x stays on-device), so the on-disk backing
# store is counted and the virtual NFS/FUSE mount points are excluded.
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SG="${SG:-$REPO_ROOT/target/debug/sg}"
SIMGITD="${SIMGITD:-$REPO_ROOT/target/debug/simgitd}"
WORK="${WORK:-$(mktemp -d)}"
STATE_DIR="${STATE_DIR:-$HOME/.local/state/simgit}"
NFILES="${NFILES:-400}"        # files in the working tree
FSIZE_KB="${FSIZE_KB:-128}"    # size per file
NS="${NS:-1 2 4 8}"            # session counts to test

for bin in "$SG" "$SIMGITD"; do
  [ -x "$bin" ] || { echo "missing $bin — run: cargo build --workspace"; exit 1; }
done

kb() { local v; v=$(du -sxk "$1" 2>/dev/null | awk '{print $1; exit}'); echo "${v:-0}"; }

make_repo() {
  local dir="$1"
  rm -rf "$dir"; mkdir -p "$dir/src"
  ( cd "$dir"
    git init -q; git config user.email b@b.co; git config user.name b
    for i in $(seq 1 "$NFILES"); do
      head -c $((FSIZE_KB*1024)) /dev/urandom | base64 > "src/f$i.dat"
    done
    git add -A; git commit -qm init )
}

start_daemon() {
  local repo="$1"
  pkill -f "$SIMGITD" 2>/dev/null || true; sleep 1
  rm -f "$STATE_DIR/control.port"
  ( cd "$repo" && RUST_LOG=warn nohup "$SIMGITD" > "$repo/daemon.log" 2>&1 </dev/null & )
  for _ in $(seq 1 40); do
    [ -f "$STATE_DIR/control.port" ] && return 0; sleep 0.25
  done
  echo "daemon failed to start"; cat "$repo/daemon.log"; exit 1
}

stop_daemon() {
  mount 2>/dev/null | grep -i "simgit/mnt" | awk '{print $3}' | while read -r m; do
    umount -f "$m" 2>/dev/null || fusermount -u "$m" 2>/dev/null || true
  done
  pkill -f "$SIMGITD" 2>/dev/null || true; sleep 1
}

TREE_KB=""
printf '%-6s | %-22s | %-22s\n' "N" "git worktree" "simgit sessions"
printf '%-6s | %-10s %-11s | %-10s %-11s\n' "" "disk(MB)" "time(s)" "disk(MB)" "time(s)"
echo "-------|------------------------|------------------------"

for N in $NS; do
  # ---------- git worktree ----------
  GWREPO="$WORK/bench-gw"
  make_repo "$GWREPO"
  TREE_KB=$(kb "$GWREPO/src")
  t0=$(date +%s.%N)
  ( cd "$GWREPO"; for j in $(seq 1 "$N"); do git worktree add -q -b "feat/wt$j" "$WORK/bench-gw-wt-$j" HEAD >/dev/null 2>&1; done )
  t1=$(date +%s.%N)
  gw_time=$(awk -v a="$t1" -v b="$t0" 'BEGIN{printf "%.2f", a-b}')
  gw_kb=0
  for j in $(seq 1 "$N"); do gw_kb=$((gw_kb + $(kb "$WORK/bench-gw-wt-$j"))); done
  gw_mb=$(awk -v k="$gw_kb" 'BEGIN{printf "%.1f", k/1024}')
  rm -rf "$WORK"/bench-gw-wt-* "$GWREPO"

  # ---------- simgit sessions ----------
  SGREPO="$WORK/bench-sg"
  make_repo "$SGREPO"
  start_daemon "$SGREPO"
  before=$(kb "$SGREPO/.git/simgit")
  t0=$(date +%s.%N)
  ( cd "$SGREPO"; for j in $(seq 1 "$N"); do "$SG" worktree add "feat/s$j" >/dev/null 2>&1; done )
  t1=$(date +%s.%N)
  sg_time=$(awk -v a="$t1" -v b="$t0" 'BEGIN{printf "%.2f", a-b}')
  after=$(kb "$SGREPO/.git/simgit")
  sg_mb=$(awk -v k="$((after-before))" 'BEGIN{printf "%.2f", k/1024}')
  stop_daemon
  rm -rf "$SGREPO"

  printf '%-6s | %-10s %-11s | %-10s %-11s\n' "$N" "$gw_mb" "$gw_time" "$sg_mb" "$sg_time"
done

echo
echo "Working-tree size per checkout: $(awk -v k="$TREE_KB" 'BEGIN{printf "%.1f MB", k/1024}')  ($NFILES files x ${FSIZE_KB}KB)"
rm -rf "$WORK"
