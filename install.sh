#!/bin/sh
# simgit installer: fetch a prebuilt `sg` binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/abendrothj/simgit/main/install.sh | sh
#
# Environment:
#   SIMGIT_VERSION      tag to install (default: latest release)
#   SIMGIT_INSTALL_DIR  install directory (default: ~/.local/bin)
#
# No Rust toolchain and no jq required. Unsupported platforms are reported
# instead of silently falling back.

set -eu

REPO="abendrothj/simgit"
VERSION="${SIMGIT_VERSION:-}"
INSTALL_DIR="${SIMGIT_INSTALL_DIR:-$HOME/.local/bin}"

die() {
	printf 'simgit: %s\n' "$1" >&2
	exit 1
}

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
Darwin)
	case "$arch" in
	arm64 | aarch64) target="aarch64-apple-darwin" ;;
	x86_64) target="x86_64-apple-darwin" ;;
	*) die "unsupported macOS architecture: $arch" ;;
	esac
	;;
Linux)
	case "$arch" in
	aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
	x86_64) target="x86_64-unknown-linux-gnu" ;;
	*) die "unsupported Linux architecture: $arch" ;;
	esac
	;;
MINGW* | MSYS* | CYGWIN* | Windows_NT)
	die "Windows is not supported (NTFS lacks the reflink primitive simgit needs); run simgit under WSL on btrfs/xfs, or use plain git worktree"
	;;
*)
	die "unsupported operating system: $os"
	;;
esac

asset="sg-${target}.tar.gz"
if [ -n "$VERSION" ]; then
	url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
else
	url="https://github.com/${REPO}/releases/latest/download/${asset}"
fi

if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -qO "$2" "$1"; }
else
	die "need curl or wget to download $url"
fi

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT INT TERM

printf 'simgit: downloading %s\n' "$url"
fetch "$url" "$tmp/$asset" || die "download failed: $url"
tar xzf "$tmp/$asset" -C "$tmp" || die "could not extract $asset"

binary="$tmp/sg-${target}/sg"
[ -f "$binary" ] || die "release archive did not contain sg-${target}/sg"

mkdir -p "$INSTALL_DIR"
chmod +x "$binary"
# Replace via rename so a running `sg` is not corrupted mid-write.
mv -f "$binary" "$INSTALL_DIR/sg" || die "could not install into $INSTALL_DIR"

installed="$("$INSTALL_DIR/sg" --version 2>/dev/null || echo sg)"
printf 'simgit: installed %s to %s/sg\n' "$installed" "$INSTALL_DIR"

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
	printf 'simgit: %s is not on PATH; add it with:\n  export PATH="%s:$PATH"\n' \
		"$INSTALL_DIR" "$INSTALL_DIR"
	;;
esac

if [ "$os" = Linux ] && ! command -v fuse-overlayfs >/dev/null 2>&1; then
	printf 'simgit: fuse-overlayfs not found — on a non-reflink filesystem (ext4) sg falls back to a plain checkout.\n         Install it (e.g. apt-get install fuse-overlayfs) to keep the copy-on-write path.\n'
fi
