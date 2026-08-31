#!/usr/bin/env bash
# Generate the workspace SBOM and prove it describes the tagged source tree.
#
# This is the executable half of issue #1615's first acceptance criterion.
# Generating an SBOM in CI and then attaching it is not, by itself, a gate: the
# job that produced the file would always agree with itself. What makes it a
# gate is that the file is:
#
#   1. Regenerated from this checkout and compared component-by-component
#      (`autumn sbom --verify`), so a hand-edited, stale, or substituted SBOM
#      fails — and the failure names the components that drifted.
#   2. Checked to describe the version actually being released: `--expect-version`
#      makes the CLI assert its root component equals [workspace.package].version,
#      and this script separately requires that version to equal RELEASE_TAG
#      when one is set (as on a tag push).
#   3. `--locked`, so a Cargo.lock that does not match the manifests is a gate
#      failure rather than a silently different dependency set.
#
# Writes the verified SBOM to $SBOM_OUT (default sbom.cdx.json) for the
# release job to attach and attest.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-sbom.sh
#     RELEASE_TAG=v0.7.0 ./scripts/check-sbom.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

SBOM_OUT="${SBOM_OUT:-sbom.cdx.json}"
RELEASE_TAG="${RELEASE_TAG:-}"

die() {
  echo "error: $*" >&2
  exit 1
}

ok() {
  echo "ok:    $*"
}

# --- 0. Build the generator from THIS checkout ------------------------------
# Deliberately not a released `autumn` binary: the SBOM must be produced by the
# code being released, so a change to the generator is itself gated.
echo "==> Building the SBOM generator from this checkout"
cargo build --locked -p autumn-cli --bin autumn
AUTUMN_BIN="$root/target/debug/autumn"
[[ -x "$AUTUMN_BIN" ]] || die "expected the autumn CLI at $AUTUMN_BIN"

# --- 1. Read the version being released -------------------------------------
workspace_version="$(
  sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml \
    | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n1
)"
[[ -n "$workspace_version" ]] || die "could not read [workspace.package].version from Cargo.toml"
ok "workspace version is $workspace_version"

if [[ -n "$RELEASE_TAG" ]]; then
  tag_version="${RELEASE_TAG#v}"
  [[ "$tag_version" == "$workspace_version" ]] \
    || die "tag $RELEASE_TAG does not match [workspace.package].version $workspace_version"
  ok "release tag $RELEASE_TAG matches the workspace version"
else
  echo "note:  RELEASE_TAG unset — skipping the tag agreement check"
fi

# --- 2. Generate ------------------------------------------------------------
# `--expect-version` makes the CLI itself assert the SBOM's root component
# describes the version being released, so this script never has to parse
# CycloneDX with sed. `--locked` makes a Cargo.lock that disagrees with the
# manifests a gate failure rather than a silently different dependency set.
echo "==> Generating $SBOM_OUT"
"$AUTUMN_BIN" sbom --locked --expect-version "$workspace_version" --output "$SBOM_OUT" \
  || die "could not generate a workspace SBOM for version $workspace_version"
[[ -s "$SBOM_OUT" ]] || die "$SBOM_OUT is missing or empty"
ok "generated $SBOM_OUT"

# --- 3. Verify it against the source tree -----------------------------------
# Regenerates and compares component-by-component. This is what turns "we
# attached an SBOM" into a gate: it catches a stale checked-in SBOM, a
# substituted one, and any nondeterminism in the generator itself.
echo "==> Verifying $SBOM_OUT against the source tree"
"$AUTUMN_BIN" sbom --locked --expect-version "$workspace_version" --verify "$SBOM_OUT" \
  || die "$SBOM_OUT does not match this source tree at version $workspace_version"
ok "$SBOM_OUT matches the source tree"

# --- 4. Shape ---------------------------------------------------------------
format="$(sed -n 's/.*"bomFormat"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$SBOM_OUT" | head -n1)"
[[ "$format" == "CycloneDX" ]] \
  || die "$SBOM_OUT is not a CycloneDX document (bomFormat=${format:-<absent>})"
ok "bomFormat is CycloneDX"

purl_count="$(grep -c '"purl"' "$SBOM_OUT" || true)"
[[ "$purl_count" -gt 1 ]] \
  || die "$SBOM_OUT lists no dependencies — the generator resolved nothing"
ok "SBOM carries $purl_count package URLs"

echo
echo "SBOM gate passed for $workspace_version."
