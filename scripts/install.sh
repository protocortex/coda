#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Installs a coda release archive after checking it against the release SHA256SUMS.
# It is a script only because nothing can run before coda is installed.
set -eu

# Updated with every release.
DEFAULT_VERSION="0.1.0"

REPO="protocortex/coda"
VERSION="${CODA_VERSION:-$DEFAULT_VERSION}"
VERSION="${VERSION#v}"
INSTALL_DIR="${CODA_INSTALL_DIR:-$HOME/.local/bin}"

die() {
  printf 'coda install: %s\n' "$*" >&2
  exit 1
}

case "$VERSION" in
  '' | *[!0-9A-Za-z.+-]*) die "invalid version '$VERSION'" ;;
esac

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  *) die "unsupported platform $os $arch" ;;
esac

# CODA_RELEASE_BASE with CODA_INSTALL_TEST=1 is for the installer tests only.
base="https://github.com/$REPO/releases/download/v$VERSION"
curl_proto="=https"
if [ -n "${CODA_RELEASE_BASE:-}" ]; then
  [ "${CODA_INSTALL_TEST:-}" = "1" ] || die "CODA_RELEASE_BASE is only for tests"
  case "$CODA_RELEASE_BASE" in
    file://*) base="$CODA_RELEASE_BASE"; curl_proto="=file" ;;
    *) die "CODA_RELEASE_BASE must be a file:// URL" ;;
  esac
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  die "needs sha256sum or shasum to verify the download"
fi

name="coda-$VERSION-$target"
archive="$name.tar.gz"

work=$(mktemp -d)
staged=""
cleanup() {
  rm -rf "$work"
  [ -z "$staged" ] || rm -f "$staged"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

fetch() {
  curl --proto "$curl_proto" --tlsv1.2 -fsSL -o "$work/$1" "$base/$1" \
    || die "could not download $base/$1"
}

printf 'Downloading coda %s for %s\n' "$VERSION" "$target"
fetch "$archive"
fetch SHA256SUMS

expected=$(awk -v f="$archive" '$2 == f { print $1 }' "$work/SHA256SUMS")
[ -n "$expected" ] || die "SHA256SUMS has no entry for $archive"
actual=$(sha256 "$work/$archive")
[ "$expected" = "$actual" ] || die "checksum mismatch for $archive, nothing was installed"

tar -xzf "$work/$archive" -C "$work" || die "could not extract $archive"
[ -f "$work/$name/coda" ] || die "$archive does not contain coda"

mkdir -p "$INSTALL_DIR"
staged=$(mktemp "$INSTALL_DIR/.coda.XXXXXX")
cp "$work/$name/coda" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$INSTALL_DIR/coda"
staged=""

printf 'Installed %s\n' "$("$INSTALL_DIR/coda" --version)"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf 'Add %s to your PATH to run coda.\n' "$INSTALL_DIR" ;;
esac
if command -v gh >/dev/null 2>&1; then
  printf 'To verify the archive provenance, download %s and run:\n  gh attestation verify %s --repo %s\n' \
    "$archive" "$archive" "$REPO"
fi
