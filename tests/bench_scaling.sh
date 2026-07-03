#!/usr/bin/env bash
# Scaling benchmark: does simgit's "avoid N copies of the working tree" claim hold?
# Compares real host disk and time-to-ready for N `git worktree` checkouts vs
# N `sg worktree` sessions. See docs/scaling_benchmark.md for a recorded run.
#
#   NFILES=400 FSIZE_KB=128 NS="1 2 4 8 16" bash tests/bench_scaling.sh
#
# `du -sxk` and filesystem allocation are deliberately reported separately:
# - `du` excludes NFS/FUSE mounts with `-x`, but charges APFS/reflink clones
#   their full per-file block count even when their extents are shared.
# - the `df` used-block delta observes physical filesystem allocation, including
#   shared-clone behavior, but is noisy if other processes write to the volume.
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SG="${SG:-$REPO_ROOT/target/debug/sg}"
SIMGITD="${SIMGITD:-$REPO_ROOT/target/debug/simgitd}"
WORK="${WORK:-$(mktemp -d)}"
NFILES="${NFILES:-400}"        # files in the working tree
FSIZE_KB="${FSIZE_KB:-128}"    # size per file
NS="${NS:-1 2 4 8}"            # session counts to test
DISK_SETTLE_SECS="${DISK_SETTLE_SECS:-1}"

for bin in "$SG" "$SIMGITD"; do
  [ -x "$bin" ] || { echo "missing $bin — run: cargo build --workspace"; exit 1; }
done

kb() { local v; v=$(du -sxk "$1" 2>/dev/null | awk '{print $1; exit}'); echo "${v:-0}"; }
used_kb() { df -Pk "$1" | awk 'NR == 2 {print $3}'; }
now_ns() { python3 -c 'import time; print(time.monotonic_ns())'; }
elapsed_s() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f", (a-b)/1000000000}'; }
delta_mb() {
  awk -v a="$1" -v b="$2" 'BEGIN{d=(a-b)/1024; if (d < 0) d=0; printf "%.1f", d}'
}
settle_disk() { sync; sleep "$DISK_SETTLE_SECS"; }

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

stop_daemon() {
  mount 2>/dev/null | grep -i "simgit/mnt" | awk '{print $3}' | while read -r m; do
    umount -f "$m" 2>/dev/null || fusermount -u "$m" 2>/dev/null || true
  done
  pkill -f "$SIMGITD" 2>/dev/null || true; sleep 1
}

TREE_KB=""
printf '%-6s | %-34s | %-34s\n' "N" "git worktree" "simgit CoW sessions"
printf '%-6s | %-9s %-10s %-9s | %-9s %-10s %-9s\n' "" "du(MB)" "phys(MB)" "time(s)" "du(MB)" "phys(MB)" "time(s)"
echo "-------|------------------------------------|------------------------------------"

for N in $NS; do
  # ---------- git worktree ----------
  GWREPO="$WORK/bench-gw"
  make_repo "$GWREPO"
  TREE_KB=$(kb "$GWREPO/src")
  settle_disk
  gw_before=$(used_kb "$WORK")
  t0=$(now_ns)
  ( cd "$GWREPO"; for j in $(seq 1 "$N"); do git worktree add -q -b "feat/wt$j" "$WORK/bench-gw-wt-$j" HEAD >/dev/null 2>&1; done )
  t1=$(now_ns)
  settle_disk
  gw_after=$(used_kb "$WORK")
  gw_time=$(elapsed_s "$t1" "$t0")
  gw_kb=0
  for j in $(seq 1 "$N"); do gw_kb=$((gw_kb + $(kb "$WORK/bench-gw-wt-$j"))); done
  gw_mb=$(awk -v k="$gw_kb" 'BEGIN{printf "%.1f", k/1024}')
  gw_phys_mb=$(delta_mb "$gw_after" "$gw_before")
  rm -rf "$WORK"/bench-gw-wt-* "$GWREPO"
  settle_disk

  # ---------- simgit sessions ----------
  SGREPO="$WORK/bench-sg"
  make_repo "$SGREPO"
  settle_disk
  sg_before=$(used_kb "$WORK")
  t0=$(now_ns)
  ( cd "$SGREPO"; for j in $(seq 1 "$N"); do "$SG" worktree add "feat/s$j" >/dev/null 2>&1; done )
  t1=$(now_ns)
  settle_disk
  sg_after=$(used_kb "$WORK")
  sg_time=$(elapsed_s "$t1" "$t0")
  sg_kb=$(kb "$SGREPO/.git/simgit")
  sg_mb=$(awk -v k="$sg_kb" 'BEGIN{printf "%.1f", k/1024}')
  sg_phys_mb=$(delta_mb "$sg_after" "$sg_before")
  stop_daemon
  rm -rf "$SGREPO"
  settle_disk

  printf '%-6s | %-9s %-10s %-9s | %-9s %-10s %-9s\n' \
    "$N" "$gw_mb" "$gw_phys_mb" "$gw_time" "$sg_mb" "$sg_phys_mb" "$sg_time"
done

echo
echo "Working-tree size per checkout: $(awk -v k="$TREE_KB" 'BEGIN{printf "%.1f MB", k/1024}')  ($NFILES files x ${FSIZE_KB}KB)"
echo "du(MB) is per-path block accounting; phys(MB) is the volume used-block delta."
rm -rf "$WORK"
