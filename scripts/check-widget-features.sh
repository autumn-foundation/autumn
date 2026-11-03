#!/usr/bin/env bash
# Check the widget family features (issue #3408).
#
# Each family (`widgets-search`, `widgets-social`, `widgets-workflow`,
# `widgets-charts`) is default-on. An app can turn default features off and
# name only the families it uses.
#
# Phase 1: each of the 16 family combinations must compile. Each run is
# `--no-default-features`, with `maud` and `htmx` (the widget core and its
# script bundle). Two more runs add `db,postgres`: the `#[commentable]`
# router needs `widgets-social`, so it is checked with and without it. One
# more run has every family and no `htmx`. A
# warning in widget code (a dead helper or an unused import in one
# combination) also fails the run.
#
# Phase 2: the widget tests and specs run with every family off. The build
# keeps the other default features, so the integration test binary compiles.
# This runs the feature-off branches of the specs, which the default `test`
# job never reaches.
#
# Called from the `widgets` lane of the `test-features` CI job. Run locally:
#
#     ./scripts/check-widget-features.sh

set -euo pipefail

# Plain text: the warning check below reads the log. CI sets
# CARGO_TERM_COLOR=always, which puts escape codes inside each line.
export CARGO_TERM_COLOR=never

FAMILIES=(widgets-search widgets-social widgets-workflow widgets-charts)
# Files that the family gates touch. A warning in one of these fails the run.
WATCHED='--> autumn/src/(widgets/|ui/widgets_css\.rs|htmx\.rs|stories/|commentable\.rs|prelude\.rs|route_listing\.rs)'

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

# ── Phase 1: every family combination compiles ───────────────────────────────

COMBOS=()
count=${#FAMILIES[@]}
for ((mask = 0; mask < (1 << count); mask++)); do
  features="maud,htmx"
  for ((i = 0; i < count; i++)); do
    if ((mask & (1 << i))); then
      features+=",${FAMILIES[$i]}"
    fi
  done
  COMBOS+=("$features")
done
COMBOS+=("maud,htmx,db,postgres" "maud,htmx,db,postgres,widgets-social")
# Every family without `htmx`: the families must not need the script bundle.
COMBOS+=("maud,widgets-search,widgets-social,widgets-workflow,widgets-charts")

failures=0
for combo in "${COMBOS[@]}"; do
  echo "==> cargo check -p autumn-web --lib --no-default-features --features $combo"
  if ! cargo check -p autumn-web --lib --no-default-features --features "$combo" >"$LOG" 2>&1; then
    cat "$LOG"
    echo "::error::widget features failed to compile: --no-default-features --features $combo"
    failures=$((failures + 1))
    continue
  fi
  # The build passed, so each diagnostic in the log is a warning. Cargo
  # replays cached warnings, so a reused build still shows them. No pipe:
  # under pipefail, an early `grep -q` exit can fail the grep before it.
  if grep -qE -- "$WATCHED" "$LOG"; then
    cat "$LOG"
    echo "::error::a warning in widget code with --features $combo (see above)"
    failures=$((failures + 1))
  fi
done

if ((failures > 0)); then
  echo "::error::$failures of ${#COMBOS[@]} widget feature combinations failed"
  exit 1
fi
echo "All ${#COMBOS[@]} widget feature combinations compile."

# ── Phase 2: the specs pass with every family off ────────────────────────────

# The `default` list of autumn-web, without the widget families.
defaults=$(grep -E '^default = \[' autumn/Cargo.toml | sed -E 's/^default = \[(.*)\]/\1/; s/[" ]//g')
if [[ -z "$defaults" ]]; then
  echo "::error::could not read the default feature list from autumn/Cargo.toml"
  exit 1
fi
no_families=$(tr ',' '\n' <<<"$defaults" | grep -vE '^widgets-' | paste -sd, -)
echo "==> widget tests with every family off: --features $no_families"

assert_ran() {
  # A filter that matches nothing still exits 0.
  grep -qE 'test result: ok\. [1-9][0-9]* passed' "$LOG" ||
    { echo "::error::no $1 test ran"; exit 1; }
}

cargo test -p autumn-web --no-default-features --features "$no_families" --lib \
  -- widgets:: stories:: ui::widgets_css htmx:: route_listing:: story 2>&1 | tee "$LOG"
assert_ran "lib widget"
cargo test -p autumn-web --no-default-features --features "$no_families" \
  --test integration_tests -- widget_features widget_css_coverage stories:: 2>&1 | tee "$LOG"
assert_ran "integration widget spec"
echo "The widget specs pass with every family off."
