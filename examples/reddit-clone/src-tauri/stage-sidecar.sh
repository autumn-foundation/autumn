#!/usr/bin/env bash
# Build the autumn server sidecar (embedded assets + managed Postgres) and
# place it in src-tauri/binaries/ for Tauri to bundle.
# Wired into tauri.conf.json > build.beforeBuildCommand.
# Run manually: bash src-tauri/stage-sidecar.sh
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(dirname "$SCRIPT_DIR")"
cd "$APP_DIR"
# TAURI_ENV_TARGET_TRIPLE is set by Tauri for cross-compilation; fall back to host.
TARGET_TRIPLE="${TAURI_ENV_TARGET_TRIPLE:-$(rustc -Vv | awk '/^host/{print $2}')}";
# Resolve Cargo output dir (CARGO_TARGET_DIR or workspace target/).
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --no-deps --format-version 1 --quiet \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
mkdir -p src-tauri/binaries
autumn build --embed -p reddit-clone --bin reddit-clone --features autumn-web/managed-pg-bundled
# universal-apple-darwin: build both Darwin slices and lipo them together.
if [ "${TARGET_TRIPLE}" = "universal-apple-darwin" ]; then
    for ARCH in x86_64-apple-darwin aarch64-apple-darwin; do
        cargo build --release -p reddit-clone --target "$ARCH" --bin reddit-clone \
          --features embed-assets,autumn-web/managed-pg-bundled
    done
    lipo -create -output "src-tauri/binaries/reddit-clone-universal-apple-darwin" \
      "${TARGET_DIR}/x86_64-apple-darwin/release/reddit-clone" \
      "${TARGET_DIR}/aarch64-apple-darwin/release/reddit-clone"
    echo "Staged (universal): src-tauri/binaries/reddit-clone-universal-apple-darwin"
else
    cargo build --release -p reddit-clone --target "${TARGET_TRIPLE}" --bin reddit-clone \
      --features embed-assets,autumn-web/managed-pg-bundled
    cp "${TARGET_DIR}/${TARGET_TRIPLE}/release/reddit-clone" \
       "src-tauri/binaries/reddit-clone-${TARGET_TRIPLE}"
    echo "Staged: src-tauri/binaries/reddit-clone-${TARGET_TRIPLE}"
fi
# Stage profile config files into src-tauri/configs/ so tauri.conf.json resource
# entries are always satisfiable at bundle time.
# For alias pairs (prod/production, dev/development): AutumnConfig stops at the
# first existing file in its ordered lookup list.  Copy the available file to
# BOTH names so the profile resolves correctly regardless of AUTUMN_ENV spelling,
# avoiding an empty stub from shadowing real config in the other alias.
mkdir -p src-tauri/configs
# Ensure autumn.toml exists at the project root — tauri.conf.json always
# lists it as a bundle resource.  Projects without a config file use
# AutumnConfig defaults; an empty TOML is a valid no-op.
if [ ! -f "autumn.toml" ]; then
    : > autumn.toml
fi
# prod/production alias pair
if [ -f "autumn-prod.toml" ] && [ -f "autumn-production.toml" ]; then
    cp autumn-prod.toml src-tauri/configs/autumn-prod.toml
    cp autumn-production.toml src-tauri/configs/autumn-production.toml
elif [ -f "autumn-prod.toml" ]; then
    cp autumn-prod.toml src-tauri/configs/autumn-prod.toml
    cp autumn-prod.toml src-tauri/configs/autumn-production.toml
elif [ -f "autumn-production.toml" ]; then
    cp autumn-production.toml src-tauri/configs/autumn-prod.toml
    cp autumn-production.toml src-tauri/configs/autumn-production.toml
else
    : > src-tauri/configs/autumn-prod.toml
    : > src-tauri/configs/autumn-production.toml
fi
# dev/development alias pair (same logic)
if [ -f "autumn-dev.toml" ] && [ -f "autumn-development.toml" ]; then
    cp autumn-dev.toml src-tauri/configs/autumn-dev.toml
    cp autumn-development.toml src-tauri/configs/autumn-development.toml
elif [ -f "autumn-dev.toml" ]; then
    cp autumn-dev.toml src-tauri/configs/autumn-dev.toml
    cp autumn-dev.toml src-tauri/configs/autumn-development.toml
elif [ -f "autumn-development.toml" ]; then
    cp autumn-development.toml src-tauri/configs/autumn-dev.toml
    cp autumn-development.toml src-tauri/configs/autumn-development.toml
else
    : > src-tauri/configs/autumn-dev.toml
    : > src-tauri/configs/autumn-development.toml
fi
# Standalone profiles (no aliases)
for f in autumn-staging.toml autumn-test.toml; do
    if [ -f "$f" ]; then
        cp "$f" "src-tauri/configs/$f"
    else
        : > "src-tauri/configs/$f"
    fi
done
# Stage encrypted credentials so apps using `config.credentials()` find them at
# AUTUMN_MANIFEST_DIR/config/credentials/<profile>.toml.enc in the installed bundle.
# The staging directory is always created so the tauri.conf.json resource entry
# is satisfiable at bundle time (an empty dir is a no-op for apps with no credentials).
# Note: decryption at runtime requires the AUTUMN_MASTER_KEY env var (or the
# config/master.key file placed in the resource dir).  See the Tauri section
# of the Autumn docs for recommended key distribution strategies.
# Remove and recreate the staging directory so stale .toml.enc files from a
# previous build (deleted or rotated credentials) are not carried into the
# installer.  Autumn loads any .toml.enc it finds via AUTUMN_MANIFEST_DIR, so
# a stale file from a prior build would silently keep a revoked secret active.
rm -rf src-tauri/configs/credentials
mkdir -p src-tauri/configs/credentials
if [ -d "config/credentials" ]; then
    cp -r config/credentials/. src-tauri/configs/credentials/
fi
