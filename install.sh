#!/bin/sh
# simgit installer: fetch verified prebuilt `simgit` and `sg` binaries from
# GitHub Releases.
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
	release_url="https://github.com/${REPO}/releases/download/${VERSION}"
else
	release_url="https://github.com/${REPO}/releases/latest/download"
fi
url="${release_url}/${asset}"
sums_url="${release_url}/SHA256SUMS"

if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -qO "$2" "$1"; }
else
	die "need curl or wget to download $url"
fi

tmp="$(mktemp -d)"
stage_simgit=
stage_sg=
cleanup() {
	rm -rf "$tmp"
	[ -z "$stage_simgit" ] || rm -f "$stage_simgit"
	[ -z "$stage_sg" ] || rm -f "$stage_sg"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

printf 'simgit: downloading %s\n' "$url"
fetch "$sums_url" "$tmp/SHA256SUMS" ||
	die "could not download checksum manifest: $sums_url"
fetch "$url" "$tmp/$asset" || die "download failed: $url"

expected=
while IFS= read -r line || [ -n "$line" ]; do
	case "$line" in
	*"  $asset")
		digest=${line%% *}
		[ "$line" = "$digest  $asset" ] ||
			die "malformed checksum record for $asset"
		[ -z "$expected" ] ||
			die "duplicate checksum records for $asset"
		[ "${#digest}" -eq 64 ] ||
			die "malformed checksum for $asset"
		case "$digest" in
	*[!0123456789abcdefABCDEF]*)
		die "malformed checksum for $asset"
		;;
		esac
		expected=$digest
		;;
	*"$asset"*)
		die "malformed checksum record for $asset"
		;;
	esac
done <"$tmp/SHA256SUMS"
[ -n "$expected" ] || die "SHA256SUMS has no record for $asset"

if [ "$os" = Darwin ]; then
	if command -v shasum >/dev/null 2>&1; then
		actual_output="$(shasum -a 256 "$tmp/$asset")" ||
			die "could not checksum $asset with shasum"
	elif command -v sha256sum >/dev/null 2>&1; then
		actual_output="$(sha256sum "$tmp/$asset")" ||
			die "could not checksum $asset with sha256sum"
	else
		die "need shasum or sha256sum to verify $asset"
	fi
else
	if command -v sha256sum >/dev/null 2>&1; then
		actual_output="$(sha256sum "$tmp/$asset")" ||
			die "could not checksum $asset with sha256sum"
	elif command -v shasum >/dev/null 2>&1; then
		actual_output="$(shasum -a 256 "$tmp/$asset")" ||
			die "could not checksum $asset with shasum"
	else
		die "need sha256sum or shasum to verify $asset"
	fi
fi
actual=${actual_output%% *}
[ "${#actual}" -eq 64 ] || die "checksum tool returned malformed output"
case "$actual" in
*[!0123456789abcdefABCDEF]*) die "checksum tool returned malformed output" ;;
esac
[ "$actual" = "$expected" ] || die "checksum mismatch for $asset"

tar xzf "$tmp/$asset" -C "$tmp" || die "could not extract $asset"

archive_dir="$tmp/sg-${target}"
canonical="$archive_dir/simgit"
alias="$archive_dir/sg"
[ -f "$canonical" ] ||
	die "release archive did not contain sg-${target}/simgit"
[ -f "$alias" ] || die "release archive did not contain sg-${target}/sg"
cmp -s "$canonical" "$alias" ||
	die "release archive contained non-equivalent simgit and sg binaries"

mkdir -p "$INSTALL_DIR" || die "could not create $INSTALL_DIR"
stage_simgit="$(mktemp "$INSTALL_DIR/.simgit.XXXXXX")" ||
	die "could not stage simgit in $INSTALL_DIR"
stage_sg="$(mktemp "$INSTALL_DIR/.sg.XXXXXX")" ||
	die "could not stage sg in $INSTALL_DIR"
cp "$canonical" "$stage_simgit" || die "could not stage simgit"
cp "$canonical" "$stage_sg" || die "could not stage sg"
chmod 755 "$stage_simgit" "$stage_sg" || die "could not make binaries executable"

staged_version="$("$stage_simgit" --version 2>/dev/null)" ||
	die "downloaded simgit failed its version check"
case "$staged_version" in
simgit\ *) ;;
*) die "downloaded canonical binary reported an unexpected identity" ;;
esac

# Stage in the destination directory, then rename each executable into place.
# Existing processes retain their open executable while the aliases are updated.
mv -f "$stage_sg" "$INSTALL_DIR/sg" ||
	die "could not install sg into $INSTALL_DIR"
stage_sg=
mv -f "$stage_simgit" "$INSTALL_DIR/simgit" ||
	die "could not install simgit into $INSTALL_DIR"
stage_simgit=

installed="$("$INSTALL_DIR/simgit" --version 2>/dev/null)" ||
	die "installed simgit failed its version check"
case "$installed" in
simgit\ *) ;;
*) die "installed canonical binary reported an unexpected identity" ;;
esac
printf 'simgit: installed %s to %s (with sg alias)\n' "$installed" "$INSTALL_DIR"

case ":${PATH:-}:" in
*":$INSTALL_DIR:"*) ;;
*)
	printf 'simgit: %s is not on PATH; add it with:\n  export PATH="%s:$PATH"\n' \
		"$INSTALL_DIR" "$INSTALL_DIR"
	;;
esac

if [ "$os" = Linux ] && ! command -v fuse-overlayfs >/dev/null 2>&1; then
	printf 'simgit: fuse-overlayfs not found — on a non-reflink filesystem (ext4) simgit falls back to a plain checkout.\n         Install it (e.g. apt-get install fuse-overlayfs) to keep the copy-on-write path.\n'
fi
