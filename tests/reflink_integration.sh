#!/usr/bin/env bash
# Run the CoW tests on a real Linux reflink filesystem.
# The Rust tests use TMPDIR for their repositories, so the test fixtures and
# the .git baseline cache both live on the reflink filesystem.
set -euo pipefail

mnt=/mnt/simgit-reflink
image=/tmp/simgit-reflink.img
cleanup() {
    sudo umount "$mnt" 2>/dev/null || true
    rm -f "$image"
}
trap cleanup EXIT

sudo apt-get update -qq
sudo apt-get install -y -qq btrfs-progs xfsprogs >/dev/null
sudo mkdir -p "$mnt"
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
