#!/usr/bin/env bash
# Clean-room rerun of the README quickstart on the toolchain the README
# advertises ("Rust 1.88.0+", [workspace.package] rust-version).
#
# This is the same journey `quickstart-gate.yml` runs on its MSRV leg, made
# reproducible on a laptop: a pristine CARGO_HOME (so no cached registry or
# lockfile hides a resolution failure) and RUSTUP_TOOLCHAIN pinned to the MSRV.
# It delegates every phase to scripts/check-quickstart.sh, so the commands are
# the README's, verbatim — nothing is scripted around a problem.
#
# Usage:  scripts/clean-room-msrv.sh [phase ...]
#         (default phases: install new setup build serve)
# Output: one `phase  status  seconds` row per phase, then a summary line.
# Exit:   0 if every requested phase passed, 1 at the first failing phase.
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
msrv="$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' "$repo_root/Cargo.toml" | head -n 1)"
[[ -n "$msrv" ]] || { echo "could not read rust-version from Cargo.toml" >&2; exit 2; }

work="$(mktemp -d "${TMPDIR:-/tmp}/autumn-clean-room.XXXXXX")"
export CARGO_HOME="$work/cargo-home"
export QUICKSTART_STATE_DIR="$work/state"
export RUSTUP_TOOLCHAIN="$msrv"
rustup toolchain install "$msrv" --profile minimal >/dev/null 2>&1 || true

phases=("$@")
[[ ${#phases[@]} -gt 0 ]] || phases=(install new setup build serve)

echo "clean-room: $(rustc --version) | work dir $work"
total=0 ran=0
for phase in "${phases[@]}"; do
  start=$SECONDS
  "$repo_root/scripts/check-quickstart.sh" "$phase" >"$work/$phase.log" 2>&1
  rc=$?
  took=$((SECONDS - start)); total=$((total + took)); ran=$((ran + 1))
  if [[ $rc -eq 0 ]]; then
    printf '%-10s PASS  %4ss\n' "$phase" "$took"
  else
    printf '%-10s FAIL  %4ss  (log: %s)\n' "$phase" "$took" "$work/$phase.log"
    grep -m1 -B6 '::error::' "$work/$phase.log" | sed 's/^/    | /'
    echo "clean-room: FAILED at step ${ran}/${#phases[@]} after ${total}s"
    exit 1
  fi
done
echo "clean-room: PASSED ${ran}/${#phases[@]} steps in ${total}s"
