#!/usr/bin/env bash
# Build the Yew "literary boids" island into the flock example's static/islands/ dir.
#
# Pipeline (see docs/guide/wasm-islands.md):
#   1. cargo build --target wasm32-unknown-unknown --release   (raw .wasm)
#   2. wasm-bindgen --target web                                (ES-module glue)
#   3. wasm-opt -Oz                                             (optional, if present)
#
# `--target web` emits an ES module you can `import` from a <script type="module">
# — cleaner than trunk for dropping into a maud-owned page.
#
# Prereqs (install once):
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <matches the wasm-bindgen lib version>
#   (optional) wasm-opt from binaryen
set -euo pipefail

CRATE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$CRATE_DIR"
OUT_DIR="../flock/static/islands"
CRATE_NAME="autumn_island_flock"
# Honor CARGO_TARGET_DIR when set (e.g. a shared /workspace/target); otherwise
# cargo writes to ./target.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
PROFILE_DIR="$TARGET_DIR/wasm32-unknown-unknown/release"

echo "==> cargo build (wasm32, release)"
cargo build --manifest-path Cargo.toml \
  --target wasm32-unknown-unknown --release

echo "==> wasm-bindgen --target web -> $OUT_DIR"
mkdir -p "$OUT_DIR"
wasm-bindgen \
  --target web \
  --no-typescript \
  --out-dir "$OUT_DIR" \
  "$PROFILE_DIR/${CRATE_NAME}.wasm"

if command -v wasm-opt >/dev/null 2>&1; then
  echo "==> wasm-opt -Oz"
  wasm-opt -Oz \
    "$OUT_DIR/${CRATE_NAME}_bg.wasm" \
    -o "$OUT_DIR/${CRATE_NAME}_bg.wasm"
else
  echo "==> wasm-opt not found; skipping size optimization (optional)"
fi

echo "==> done. Artifacts in $OUT_DIR:"
ls -la "$OUT_DIR/${CRATE_NAME}.js" "$OUT_DIR/${CRATE_NAME}_bg.wasm"
