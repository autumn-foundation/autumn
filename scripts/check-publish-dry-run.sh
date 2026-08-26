#!/usr/bin/env bash
# Verify that every publishable crate's manifest is valid and all referenced
# source files exist, without uploading to crates.io.
#
# Uses `cargo package --list` which enumerates the files that would be included
# in the .crate archive.  This catches: missing files referenced from Cargo.toml,
# broken include/exclude patterns, and manifest parse errors — without attempting
# registry dependency resolution.
#
# NOTE: We intentionally avoid `cargo package --no-verify` here because that
# command rewrites local path dependencies to their pinned registry versions and
# resolves them against crates.io.  Plugin crates (autumn-admin-plugin,
# autumn-storage-s3, autumn-cache-redis) depend on autumn-web features that are
# only present in the workspace but not yet in the published autumn-web on
# crates.io at the time this gate runs, which causes false failures.
# `check-crate-metadata.sh` already verifies inter-crate version pin alignment;
# the packaging step here focuses on file and manifest integrity.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-publish-dry-run.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() {
  echo "error: $*" >&2
  exit 1
}

# Publishable crates in dependency order.
CRATES=(
  autumn-macros
  autumn-schema-core
  autumn-edge
  autumn-web
  autumn-cli
  autumn-admin-plugin
  autumn-media-plugin
  autumn-storage-s3
  autumn-cache-redis
  autumn-search
)

# Assets that MUST appear in a crate's packaged file list. `--list` does not
# compile, so it cannot run the `include_dir!` / `include_str!` macros — a file
# that cargo silently drops from the tarball (e.g. a starter subtree excluded
# because it contains a nested `Cargo.toml`) would only surface as a proc-macro
# panic at real `cargo publish` verify time. Asserting the embedded-asset paths
# are present in the listing closes that gap without needing registry
# dependency resolution. Format: "crate:substring".
REQUIRED_ASSETS=(
  # include_dir!("$CARGO_MANIFEST_DIR/src/starters/saas") in starters/builtin.rs
  "autumn-cli:src/starters/saas/Cargo.toml.tmpl"
  "autumn-cli:src/starters/saas/src/main.rs"
  # include_dir!("$CARGO_MANIFEST_DIR/static") / i18n in lib.rs macros
  "autumn-web:static/"
  "autumn-web:i18n/"
)

failures=0

for crate in "${CRATES[@]}"; do
  echo ""
  echo "==> cargo package --list -p $crate"
  # --list enumerates the files that would be in the archive.
  # --allow-dirty lets this run on a working tree with uncommitted changes.
  if listing="$(cargo package -p "$crate" --list --allow-dirty 2>&1)"; then
    echo "$listing"
    echo "  PASS: $crate manifest and files are valid"
    # Assert this crate's embedded assets survived packaging.
    for spec in "${REQUIRED_ASSETS[@]}"; do
      asset_crate="${spec%%:*}"
      asset_path="${spec#*:}"
      [[ "$asset_crate" == "$crate" ]] || continue
      if ! grep -qF "$asset_path" <<<"$listing"; then
        echo "  FAIL: $crate is missing embedded asset '$asset_path' from its package" >&2
        echo "        (an include_dir!/include_str! target was dropped from the tarball —" >&2
        echo "         a nested Cargo.toml or include/exclude rule likely excluded it)" >&2
        failures=$((failures + 1))
      else
        echo "  OK: embedded asset present — $asset_path"
      fi
    done
  else
    echo "$listing"
    echo "  FAIL: $crate could not be listed for packaging" >&2
    failures=$((failures + 1))
  fi
done

echo ""
if [[ "$failures" -gt 0 ]]; then
  die "$failures crate(s) failed packaging file verification."
fi

echo "Package dry-run OK — all publishable crates have valid manifests and files."
