#!/bin/sh
# Autumn CLI installer.
#
#   curl -fsSL https://raw.githubusercontent.com/autumn-foundation/autumn/trunk-dev/scripts/install.sh | sh
#
# Downloads a prebuilt `autumn` binary from GitHub Releases, verifies its sha256
# checksum, and installs it. Linux and macOS (x86_64 and aarch64) are supported.
# Binaries correspond to tagged crate releases; "latest" is the most recent release.
#
# Environment overrides (flags --version/--dir/--target mirror them):
#   AUTUMN_VERSION      version tag to install, or "latest" (default: latest)
#   AUTUMN_INSTALL_DIR  install directory (default: $HOME/.local/bin)
#   AUTUMN_TARGET       force target triple (default: autodetected)
#   AUTUMN_BASE_URL     release base URL (default: https://github.com/autumn-foundation/autumn/releases)
set -eu

REPO_SLUG="autumn-foundation/autumn"
DEFAULT_BASE_URL="https://github.com/${REPO_SLUG}/releases"

VERSION="${AUTUMN_VERSION:-latest}"
INSTALL_DIR="${AUTUMN_INSTALL_DIR:-${HOME}/.local/bin}"
TARGET="${AUTUMN_TARGET:-}"
BASE_URL="${AUTUMN_BASE_URL:-$DEFAULT_BASE_URL}"

err() { printf 'autumn-install: error: %s\n' "$1" >&2; exit 1; }
info() { printf 'autumn-install: %s\n' "$1" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      [ $# -ge 2 ] || err "--version requires a value"
      VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --dir)
      [ $# -ge 2 ] || err "--dir requires a value"
      INSTALL_DIR="$2"; shift 2 ;;
    --dir=*) INSTALL_DIR="${1#*=}"; shift ;;
    --target)
      [ $# -ge 2 ] || err "--target requires a value"
      TARGET="$2"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    -h|--help)
      cat <<'EOF'
Autumn CLI installer.

Downloads a prebuilt autumn binary from GitHub Releases, verifies its sha256
checksum, and installs it. Linux x86_64 and aarch64 are supported.

Environment overrides (flags --version/--dir/--target mirror them):
  AUTUMN_VERSION      version tag to install, or "latest" (default: latest)
  AUTUMN_INSTALL_DIR  install directory (default: $HOME/.local/bin)
  AUTUMN_TARGET       force target triple (default: autodetected)
  AUTUMN_BASE_URL     release base URL (default: https://github.com/autumn-foundation/autumn/releases)
EOF
      exit 0 ;;
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
  case "$os" in
    Linux) os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *) err "unsupported OS: $os (prebuilt binaries cover Linux and macOS; build from source with 'cargo install --path autumn-cli')" ;;
  esac
  case "$arch" in
    x86_64|amd64) arch_part="x86_64" ;;
    aarch64|arm64) arch_part="aarch64" ;;
    *) err "unsupported architecture: $arch (build from source with 'cargo install --path autumn-cli')" ;;
  esac
  TARGET="${arch_part}-${os_part}"
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

expected=""
read -r expected _ < "$tmp/$asset.sha256" 2>/dev/null || true
[ -n "$expected" ] || err "empty checksum from ${url}.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset")"; actual="${actual%% *}"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$asset")"; actual="${actual%% *}"
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
