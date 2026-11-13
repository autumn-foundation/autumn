#!/usr/bin/env bash
# Check the `error-reporting` feature of autumn-web without `reporting`.
#
# `reporting` is default-on and turns `error-reporting` on, so
# `cargo test --workspace` never builds the panic-catch layer without failure
# capture. The no-DB daemon starter does: it names `error-reporting` only.
#
# Phase 1: the feature list that `autumn new --daemon` writes compiles, alone
# and with `flash` (the scaffold turns `flash` on through its own default
# feature). `reporting.rs` must give no warning.
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

# ── Phase 1: the daemon starter's feature list compiles ─────────────────────

# The list that `autumn new --daemon` writes into the app's manifest.
daemon=$(grep -E '^const DAEMON_NO_DB_FEATURES' autumn-cli/src/new.rs |
  sed -E 's/^.*= &\[(.*)\];.*$/\1/; s/[" ]//g')
if [[ ",$daemon," != *",error-reporting,"* || ",$daemon," == *",reporting,"* ]]; then
  echo "::error::could not read a daemon feature list with error-reporting and no reporting: '$daemon'"
  exit 1
fi

for combo in "$daemon" "$daemon,flash"; do
  echo "==> cargo check -p autumn-web --lib --no-default-features --features $combo"
  if ! cargo check -p autumn-web --lib --no-default-features --features "$combo" >"$LOG" 2>&1; then
    cat "$LOG"
    echo "::error::failed to compile: --no-default-features --features $combo"
    exit 1
  fi
  if grep -qE -- '--> autumn/src/reporting\.rs' "$LOG"; then
    cat "$LOG"
    echo "::error::reporting.rs warns with --features $combo"
    exit 1
  fi
done
echo "The daemon starter's feature list compiles."

# ── Phase 2: the specs pass with `reporting` off ────────────────────────────

# The `default` list of autumn-web, without `reporting`.
defaults=$(grep -E '^default = \[' autumn/Cargo.toml | sed -E 's/^default = \[(.*)\]/\1/; s/[" ]//g')
if [[ -z "$defaults" ]]; then
  echo "::error::could not read the default feature list from autumn/Cargo.toml"
  exit 1
fi
features=$(tr ',' '\n' <<<"$defaults" | grep -vxE 'reporting' | paste -sd, -)

# `reporting` must be off in the resolved set, not only absent from the list.
resolved=$(cargo tree -p autumn-web --no-default-features --features "$features" \
  -e normal --depth 0 --prefix none --format '{f}')
if [[ ",$resolved," != *",error-reporting,"* ]]; then
  echo "::error::\`error-reporting\` is not in the default feature list"
  exit 1
fi
if [[ ",$resolved," == *",reporting,"* ]]; then
  echo "::error::\`reporting\` is on: a default feature turns it on"
  exit 1
fi
echo "==> specs with reporting off: --features $features"

assert_ran() {
  # A filter that matches nothing still exits 0.
  grep -qE 'test result: ok\. [1-9][0-9]* passed' "$LOG" ||
    { echo "::error::no $1 test ran"; exit 1; }
}

assert_passed() {
  # Some filters match tests that compile in every build, so the count does
  # not show that a reporter spec ran. A `#[should_panic]` test prints
  # ` - should panic` after its name.
  grep -qE -- "$1( - should panic)? \.\.\. ok" "$LOG" ||
    { echo "::error::$1 did not run"; exit 1; }
}

cargo test -p autumn-web --no-default-features --features "$features" --lib \
  -- reporting:: failure_capture 2>&1 | tee "$LOG"
assert_ran "lib reporting"
assert_passed 'reporting::tests::panic_in_inner_call_is_caught_as_500'
assert_passed 'boot_refuses_failure_capture_in_a_build_without_reporting'
# Exact test names, not module prefixes: a `sim_` prefix would run the whole
# simulation suite in parallel.
cargo test -p autumn-web --no-default-features --features "$features" \
  --test integration_tests -- error_reporting compile_gate_features \
  handler_panic_becomes_a_500_that_still_carries_the_request_id \
  a_sampled_reporting_config_is_rejected_at_build_time \
  a_stalled_user_reporter_does_not_starve_the_fault_projection 2>&1 | tee "$LOG"
assert_ran "integration error reporting"
assert_passed 'error_reporting::handler_panic_becomes_500_problem_details'
assert_passed 'error_reporting::multiple_reporters_all_receive_the_event'
assert_passed 'handler_panic_becomes_a_500_that_still_carries_the_request_id'
assert_passed 'a_sampled_reporting_config_is_rejected_at_build_time'
assert_passed 'a_stalled_user_reporter_does_not_starve_the_fault_projection'
echo "The error reporting specs pass with reporting off."
