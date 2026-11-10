#!/usr/bin/env bash
# Run the CoW tests on a real Linux reflink filesystem.
# The Rust tests use TMPDIR for their repositories, so the test fixtures and
# the .git baseline cache both live on the reflink filesystem.
set -euo pipefail

# Unique per run, so a leftover mount or image from another run is never reused.
mnt="$(mktemp -d /tmp/simgit-reflink-mnt.XXXXXX)"
image="$(mktemp /tmp/simgit-reflink-img.XXXXXX)"
# The image goes even when the unmount fails; a lazy unmount is the fallback.
cleanup() {
    sudo umount "$mnt" 2>/dev/null || sudo umount -l "$mnt" 2>/dev/null || true
    rm -f "$image"
    rmdir "$mnt" 2>/dev/null || true
}
trap cleanup EXIT

sudo apt-get update -qq
sudo apt-get install -y -qq btrfs-progs xfsprogs >/dev/null
truncate -s 2G "$image"

if mkfs.btrfs -q "$image" 2>/dev/null && sudo mount -o loop "$image" "$mnt" 2>/dev/null; then
    echo "reflink filesystem: btrfs"
else
    sudo umount "$mnt" 2>/dev/null || true
    rm -f "$image"
    truncate -s 2G "$image"
    mkfs.xfs -q -m reflink=1 "$image"
    sudo mount -o loop "$image" "$mnt"
    echo "reflink filesystem: xfs"
fi

sudo chown "$(id -u):$(id -g)" "$mnt"
mkdir -p "$mnt/tmp"
export TMPDIR="$mnt/tmp"
export SIMGIT_REQUIRE_REFLINK=1
export SIMGIT_POPULATE=reflink
cargo test --all --locked
