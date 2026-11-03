#!/usr/bin/env bash
# Check every combination of the widget family features (issue #3408).
#
# Each family (`widgets-search`, `widgets-social`, `widgets-workflow`,
# `widgets-charts`) is default-on. An app can turn default features off and
# name only the families it uses. So each of the 16 combinations must
# compile. Each run is `--no-default-features`, with `maud` and `htmx` (the
# widget core and its script bundle).
#
# Two more runs add `db,postgres`: the `#[commentable]` router needs
# `widgets-social`, so it is checked with and without that family.
#
# A warning in widget code (a dead helper or an unused import in one
# combination) also fails the run.
#
# Called from the `widgets` lane of the `test-features` CI job. Run locally:
#
#     ./scripts/check-widget-features.sh

set -euo pipefail

FAMILIES=(widgets-search widgets-social widgets-workflow widgets-charts)
# Files that the family gates touch. A warning in one of these fails the run.
WATCHED='autumn/src/(widgets/|ui/widgets_css\.rs|htmx\.rs|stories/|commentable\.rs|prelude\.rs|route_listing\.rs)'

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

LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

failures=0
for combo in "${COMBOS[@]}"; do
  echo "==> cargo check -p autumn-web --lib --no-default-features --features $combo"
  if ! cargo check -p autumn-web --lib --no-default-features --features "$combo" 2>&1 | tee "$LOG"; then
    echo "::error::widget features failed to compile: --no-default-features --features $combo"
    failures=$((failures + 1))
    continue
  fi
  if grep -A3 '^warning' "$LOG" | grep -qE -- "--> $WATCHED"; then
    echo "::error::a warning in widget code with --features $combo (see above)"
    failures=$((failures + 1))
  fi
done

if ((failures > 0)); then
  echo "::error::$failures of ${#COMBOS[@]} widget feature combinations failed"
  exit 1
fi
echo "All ${#COMBOS[@]} widget feature combinations compile."
