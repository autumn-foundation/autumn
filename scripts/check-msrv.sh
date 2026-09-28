#!/usr/bin/env bash
# Verify that the MSRV is declared consistently across the workspace,
# the README, and the CI matrix. Exit non-zero if any source disagrees.
#
# Sources checked:
#   - [workspace.package].rust-version in Cargo.toml  (canonical MSRV)
#   - README.md badge + "Requirements" section
#   - .github/workflows/ci.yml       (`msrv:` job pin)
#
# Called from the `msrv` job in ci.yml. Runs locally with:
#
#     ./scripts/check-msrv.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() {
  echo "error: $*" >&2
  exit 1
}

# Canonical MSRV from the workspace Cargo.toml.
canonical="$(
  awk '
    /^\[workspace\.package\]/ { in_block = 1; next }
    /^\[/ && in_block       { in_block = 0 }
    in_block && /^rust-version/ {
      match($0, /"[^"]+"/)
      print substr($0, RSTART + 1, RLENGTH - 2)
      exit
    }
  ' Cargo.toml
)"

[[ -n "$canonical" ]] || die "could not find [workspace.package].rust-version in Cargo.toml"
echo "canonical rust-version = $canonical"

# Any crate-level Cargo.toml that pins its own rust-version must match
# (we allow inheritance via `rust-version.workspace = true`).
while IFS= read -r manifest; do
  pinned="$(
    awk '
      /^\[package\]/         { in_pkg = 1; next }
      /^\[/ && in_pkg        { in_pkg = 0 }
      in_pkg && /^rust-version[[:space:]]*=[[:space:]]*"/ {
        match($0, /"[^"]+"/)
        print substr($0, RSTART + 1, RLENGTH - 2)
        exit
      }
    ' "$manifest"
  )"
  if [[ -n "$pinned" && "$pinned" != "$canonical" ]]; then
    die "$manifest pins rust-version = \"$pinned\" but workspace MSRV is \"$canonical\""
  fi
done < <(find . -type f -name Cargo.toml -not -path "./target/*")

# README badge.
if ! grep -q "rust-${canonical}" README.md; then
  die "README.md badge does not reference rust-${canonical}"
fi

# README "Requirements" section.
if ! grep -q "Rust ${canonical}" README.md; then
  die "README.md Requirements does not reference Rust ${canonical}"
fi

# CI workflow pin.
ci="$root/.github/workflows/ci.yml"
if ! grep -Eq "rust-toolchain@${canonical}\b" "$ci"; then
  die "$ci msrv job does not pin dtolnay/rust-toolchain@${canonical}"
fi
if ! grep -Eq "MSRV \(${canonical}\)" "$ci"; then
  die "$ci msrv job name does not reference MSRV (${canonical})"
fi

# windows-tier1 job pin. This job installs a toolchain for itself
# separately from the `msrv` job above — it does not inherit `msrv`'s pin
# just because both live in the same file. It must track the canonical
# MSRV directly: every scaffolded app's own rust-toolchain.toml pins the
# literal MSRV version, and if this job's pin drifts from it, the first
# `cargo` invocation inside the scaffolded app falls back to an implicit,
# un-retried rustup toolchain install mid-journey — the exact race that
# caused the "cargo.exe binary... is not applicable to the toolchain"
# failures fixed in #2994.
windows_tier1_block="$(awk '
  /^  windows-tier1:/ { flag = 1 }
  flag && /^  [A-Za-z]/ && !/^  windows-tier1:/ { flag = 0 }
  flag
' "$ci")"
if ! grep -Eq "rust-toolchain@${canonical}\b" <<<"$windows_tier1_block"; then
  die "$ci windows-tier1 job does not pin dtolnay/rust-toolchain@${canonical} (must match the canonical MSRV, not just the msrv job's own pin)"
fi

echo "MSRV alignment OK (${canonical})"
