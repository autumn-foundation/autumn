#!/bin/sh
# Autumn CLI installer.
#
#   curl -fsSL https://raw.githubusercontent.com/madmax983/autumn/trunk-dev/scripts/install.sh | sh
#
# Downloads a prebuilt `autumn` binary from GitHub Releases, verifies its sha256
# checksum, and installs it. Linux x86_64 and aarch64 are supported.
#
# Environment overrides (flags --version/--dir/--target mirror them):
#   AUTUMN_VERSION      version tag to install, or "latest" (default: latest)
#   AUTUMN_INSTALL_DIR  install directory (default: $HOME/.local/bin)
#   AUTUMN_TARGET       force target triple (default: autodetected)
#   AUTUMN_BASE_URL     release base URL (default: https://github.com/madmax983/autumn/releases)
set -eu

REPO_SLUG="madmax983/autumn"
DEFAULT_BASE_URL="https://github.com/${REPO_SLUG}/releases"

VERSION="${AUTUMN_VERSION:-latest}"
INSTALL_DIR="${AUTUMN_INSTALL_DIR:-${HOME}/.local/bin}"
TARGET="${AUTUMN_TARGET:-}"
BASE_URL="${AUTUMN_BASE_URL:-$DEFAULT_BASE_URL}"

err() { printf 'autumn-install: error: %s\n' "$1" >&2; exit 1; }
info() { printf 'autumn-install: %s\n' "$1" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version requires a value}"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --dir) INSTALL_DIR="${2:?--dir requires a value}"; shift 2 ;;
    --dir=*) INSTALL_DIR="${1#*=}"; shift ;;
    --target) TARGET="${2:?--target requires a value}"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    -h|--help) sed -n '2,15p' "$0" 2>/dev/null || true; exit 0 ;;
    *) err "unknown argument: $1 (try --help)" ;;
  esac
done

if command -v curl >/dev/null 2>&1; then
  download() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  download() { wget -qO "$2" "$1"; }
else
  err "need curl or wget to download"
fi

if [ -z "$TARGET" ]; then
  os="$(uname -s)"
  arch="$(uname -m)"
  [ "$os" = "Linux" ] || err "unsupported OS: $os (prebuilt binaries are Linux-only for now; build from source with 'cargo install --path autumn-cli')"
  case "$arch" in
    x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
    aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
    *) err "unsupported architecture: $arch (build from source with 'cargo install --path autumn-cli')" ;;
  esac
fi

asset="autumn-${TARGET}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="${BASE_URL}/latest/download/${asset}"
else
  url="${BASE_URL}/download/${VERSION}/${asset}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

info "downloading ${asset} (${VERSION})"
download "$url" "$tmp/$asset" || err "download failed: $url"
download "${url}.sha256" "$tmp/$asset.sha256" || err "checksum download failed: ${url}.sha256"

expected="$(awk '{print $1}' "$tmp/$asset.sha256")"
[ -n "$expected" ] || err "empty checksum from ${url}.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
else
  err "need sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || err "checksum mismatch: expected $expected, got $actual"
info "checksum ok ($actual)"

tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/autumn" ] || err "archive did not contain an 'autumn' binary"

mkdir -p "$INSTALL_DIR"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "$tmp/autumn" "$INSTALL_DIR/autumn"
else
  cp "$tmp/autumn" "$INSTALL_DIR/autumn"
  chmod 0755 "$INSTALL_DIR/autumn"
fi
info "installed autumn -> ${INSTALL_DIR}/autumn"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *) info "note: ${INSTALL_DIR} is not on your PATH; add it, e.g. export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac

"$INSTALL_DIR/autumn" --version 2>/dev/null || true
