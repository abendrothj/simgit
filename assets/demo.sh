#!/usr/bin/env bash
# Re-records assets/demo.gif from assets/demo.tape (macOS). The clone lives on a
# scratch APFS volume so df sees only the demo's own writes.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
volume=/Volumes/simgit-demo

for tool in vhs hdiutil git cargo; do
  command -v "$tool" >/dev/null || { echo "demo.sh: $tool is required" >&2; exit 1; }
done
if [ -e "$volume" ]; then
  echo "demo.sh: $volume is already mounted; detach it first" >&2
  exit 1
fi

cargo build --locked --quiet --manifest-path "$repo/Cargo.toml"

work=$(mktemp -d)
cleanup() {
  hdiutil detach -quiet "$volume" 2>/dev/null || hdiutil detach -quiet -force "$volume" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

hdiutil create -quiet -size 4g -type SPARSE -fs APFS -volname simgit-demo "$work/demo"
hdiutil attach -quiet -nobrowse "$work/demo.sparseimage"
# The vscode commit the README's numbers were measured on (September 2026);
# later commits check out smaller.
git init --quiet "$volume/vscode"
cd "$volume/vscode"
git fetch --quiet --depth 1 https://github.com/microsoft/vscode 3e78986a04006d1ba3383b1a8b2246ef9585c79f
git switch --quiet -c main FETCH_HEAD

PATH="$repo/target/debug:$PATH" vhs -o "$repo/assets/demo.gif" "$repo/assets/demo.tape"
