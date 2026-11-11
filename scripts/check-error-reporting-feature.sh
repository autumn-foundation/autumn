#!/usr/bin/env bash
# Check the `error-reporting` feature of autumn-web without `reporting`.
#
# `reporting` is default-on and turns `error-reporting` on, so
# `cargo test --workspace` never builds the panic-catch layer without failure
# capture. The no-DB daemon starter does: it names `error-reporting` only.
#
# Phase 1: the daemon starter's feature set compiles, and `reporting.rs` gives
# no warning.
#
# Phase 2: the specs run with the other default features and `reporting` off.
# A handler panic must still give a 500 and one event for each reporter.
#
# Called from the `error-reporting` lane of the `test-features` CI job. Run
# locally:
#
#     ./scripts/check-error-reporting-feature.sh

set -euo pipefail

# Plain text: the checks below read the log.
export CARGO_TERM_COLOR=never

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

# ── Phase 1: the daemon starter's feature set compiles ──────────────────────

daemon="maud,htmx,tailwind,error-reporting,flash"
echo "==> cargo check -p autumn-web --lib --no-default-features --features $daemon"
if ! cargo check -p autumn-web --lib --no-default-features --features "$daemon" >"$LOG" 2>&1; then
  cat "$LOG"
  echo "::error::failed to compile: --no-default-features --features $daemon"
  exit 1
fi
if grep -qE -- '--> autumn/src/reporting\.rs' "$LOG"; then
  cat "$LOG"
  echo "::error::reporting.rs warns without the \`reporting\` feature"
  exit 1
fi
echo "The daemon starter's feature set compiles."

# ── Phase 2: the specs pass with `reporting` off ────────────────────────────

# The `default` list of autumn-web, without `reporting`.
defaults=$(grep -E '^default = \[' autumn/Cargo.toml | sed -E 's/^default = \[(.*)\]/\1/; s/[" ]//g')
if [[ -z "$defaults" ]]; then
  echo "::error::could not read the default feature list from autumn/Cargo.toml"
  exit 1
fi
features=$(tr ',' '\n' <<<"$defaults" | grep -vxE 'reporting' | paste -sd, -)
if [[ ",$features," != *",error-reporting,"* ]]; then
  echo "::error::\`error-reporting\` is not in the default feature list"
  exit 1
fi
echo "==> specs with reporting off: --features $features"

assert_ran() {
  # A filter that matches nothing still exits 0.
  grep -qE 'test result: ok\. [1-9][0-9]* passed' "$LOG" ||
    { echo "::error::no $1 test ran"; exit 1; }
}

cargo test -p autumn-web --no-default-features --features "$features" --lib \
  -- reporting:: failure_capture 2>&1 | tee "$LOG"
assert_ran "lib reporting"
grep -q 'failure_capture_enabled_without_the_feature_fails ... ok' "$LOG" ||
  { echo "::error::the failure capture boot check did not run"; exit 1; }
cargo test -p autumn-web --no-default-features --features "$features" \
  --test integration_tests -- error_reporting compile_gate_features 2>&1 | tee "$LOG"
assert_ran "integration error reporting"
echo "The error reporting specs pass with reporting off."
