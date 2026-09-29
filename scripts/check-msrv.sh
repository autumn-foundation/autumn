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

# CI workflow pins. Each job that must track the canonical MSRV is checked
# within its OWN job block, not with a file-wide grep — a file-wide grep
# only proves *some* job pins the canonical version, which one job's own
# regression can hide behind another job's correct pin the moment there is
# more than one such job in the file (as `windows-tier1` below now is).
ci="$root/.github/workflows/ci.yml"

job_block() {
  local job="$1"
  awk -v job="  ${job}:" '
    $0 == job { flag = 1; next }
    flag && /^  [A-Za-z]/ { flag = 0 }
    flag
  ' "$ci"
}

if ! grep -Eq "rust-toolchain@${canonical}\b" <<<"$(job_block msrv)"; then
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
if ! grep -Eq "rust-toolchain@${canonical}\b" <<<"$(job_block windows-tier1)"; then
  die "$ci windows-tier1 job does not pin dtolnay/rust-toolchain@${canonical} (must match the canonical MSRV, not just the msrv job's own pin)"
fi

echo "MSRV alignment OK (${canonical})"
