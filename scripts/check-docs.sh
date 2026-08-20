#!/usr/bin/env bash
# Build public documentation for every publishable crate in the feature posture
# that docs.rs will use, and fail on any broken intra-doc link.
#
# docs.rs builds each crate with the features declared in
# [package.metadata.docs.rs].features (if present), otherwise default features.
# We mirror that here so the gate catches broken links before the crate is
# published.
#
# Note: we do NOT pass --all-features because some features (e.g.
# telemetry-otlp) require system libraries (protoc) unavailable in standard CI.
# The supported docs.rs feature set is declared in [package.metadata.docs.rs]
# inside each crate's Cargo.toml.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-docs.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# Treat broken intra-doc links as errors. We do not use -D warnings here
# because that would fail on unrelated upstream crate warnings; only the
# broken-link lint is required by the AC.
export RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links"

# Cargo doc uses the dev profile while rechecking dependencies for downstream
# crates. Keep those builds bounded like the test profile; full debug info plus
# incremental caches has produced rustc access violations on Windows.
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-line-tables-only}"
export CARGO_PROFILE_DEV_INCREMENTAL="${CARGO_PROFILE_DEV_INCREMENTAL:-false}"

echo "Building workspace documentation (docs.rs feature posture, no deps)..."
echo "RUSTDOCFLAGS=$RUSTDOCFLAGS"
echo "CARGO_PROFILE_DEV_DEBUG=$CARGO_PROFILE_DEV_DEBUG"
echo "CARGO_PROFILE_DEV_INCREMENTAL=$CARGO_PROFILE_DEV_INCREMENTAL"
echo ""

# Build each publishable crate with its declared docs.rs feature set.
# autumn-web declares [package.metadata.docs.rs].features which cargo doc
# does not read directly — we pass them explicitly here.
AUTUMN_WEB_DOCS_FEATURES="maud,htmx,tailwind,db,cache-moka,ws,flash,multipart,http-client,oauth2,openapi,mcp,redis,i18n,storage,variants,mail,seed,system-info,markdown,csv,pdf,edge"

echo "==> autumn-web (explicit docs.rs features)"
cargo doc -p autumn-web --no-deps \
  --features "$AUTUMN_WEB_DOCS_FEATURES" \
  --no-default-features 2>&1

# autumn-edge's docs.rs metadata declares `all-features = true` (the `host`
# feature carries the reference host's API surface), so mirror that here.
echo ""
echo "==> autumn-edge (all features)"
cargo doc -p autumn-edge --no-deps --all-features 2>&1

# All other publishable crates: use their default features (no
# [package.metadata.docs.rs] section means docs.rs uses defaults).
for crate in autumn-macros autumn-cli autumn-admin-plugin autumn-storage-s3 autumn-cache-redis autumn-search; do
  echo ""
  echo "==> $crate (default features)"
  cargo doc -p "$crate" --no-deps 2>&1
done

echo ""
echo "Documentation build OK — no broken intra-doc links."
