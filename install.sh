#!/bin/sh
set -eu

REPO="${FEV_REPO:-takemo101/fev}"
BIN="fev"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-latest}"
BASE_URL="https://github.com/$REPO/releases"

fail() {
  echo "error: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

for cmd in curl tar mktemp uname install mkdir rm; do
  need "$cmd"
done

if command -v sha256sum >/dev/null 2>&1; then
  checksum_tool="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  checksum_tool="shasum"
else
  fail "sha256sum or shasum is required to verify release downloads"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64|amd64) target="x86_64-apple-darwin" ;;
      *) fail "unsupported macOS architecture: $arch" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-musl" ;;
      arm64|aarch64) target="aarch64-unknown-linux-musl" ;;
      *) fail "unsupported Linux architecture: $arch" ;;
    esac
    ;;
  *) fail "unsupported operating system: $os" ;;
esac

asset="$BIN-$target.tar.gz"
if [ "$VERSION" = "latest" ]; then
  download_url="$BASE_URL/latest/download"
else
  download_url="$BASE_URL/download/$VERSION"
fi

tmp="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp"
}
trap cleanup 0
trap 'exit 130' 2
trap 'exit 143' 15

archive="$tmp/$asset"
echo "Downloading $download_url/$asset"
curl -fsSL "$download_url/$asset" -o "$archive" ||
  fail "could not download $asset; check the release tag and available assets"
curl -fsSL "$download_url/checksums.txt" -o "$tmp/checksums.txt" ||
  fail "could not download checksums.txt; refusing an unverified installation"

expected=""
while IFS=' ' read -r hash name extra; do
  [ "$name" = "$asset" ] || continue
  [ -z "$expected" ] || fail "duplicate checksum for $asset"
  [ -z "$extra" ] || fail "invalid checksum entry for $asset"
  expected="$hash"
done < "$tmp/checksums.txt"

[ "${#expected}" -eq 64 ] || fail "missing or invalid checksum for $asset"
case "$expected" in
  *[!0-9a-f]*) fail "invalid SHA-256 checksum for $asset" ;;
esac

if [ "$checksum_tool" = "sha256sum" ]; then
  actual="$(sha256sum "$archive")"
else
  actual="$(shasum -a 256 "$archive")"
fi
actual="${actual%% *}"
[ "$actual" = "$expected" ] || fail "checksum mismatch for $asset"
echo "Checksum verified"

# Extract only the expected member, not arbitrary archive paths.
tar -xzf "$archive" -C "$tmp" "$BIN" || fail "could not extract $BIN from $asset"
[ -f "$tmp/$BIN" ] && [ ! -L "$tmp/$BIN" ] ||
  fail "archive must contain a regular file named $BIN"

mkdir -p "$INSTALL_DIR"
[ ! -d "$INSTALL_DIR/$BIN" ] || fail "destination is a directory: $INSTALL_DIR/$BIN"
install -m 0755 "$tmp/$BIN" "$INSTALL_DIR/$BIN"
echo "Installed $BIN to $INSTALL_DIR/$BIN"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "warning: $INSTALL_DIR is not on your PATH" >&2 ;;
esac
echo "Run '$BIN --help' to get started."
