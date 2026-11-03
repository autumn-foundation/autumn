#!/usr/bin/env bash
# Check the `jwt` and `push` features of autumn-web.
#
# Both are default-on, so `cargo test --workspace` never builds a set with one
# turned off. An app can turn default features off and leave them out, to drop
# `jsonwebtoken` (which compiles `rsa` 0.9 and `crypto-bigint` 0.5) and `p256`.
#
# Phase 1: `maud,htmx` with neither feature, with each, and with both must
# compile, with no warning in the files the gates touch. `cargo tree` then
# shows that each crate is in the graph only when its feature is on.
#
# Phase 2: the specs run with both features off. The build keeps the other
# default features, so the integration test binary compiles. This runs the
# feature-off branches (the boot checks and the fail-closed tenancy arm),
# which the default `test` job never reaches.
#
# Called from the `jwt-push` lane of the `test-features` CI job. Run locally:
#
#     ./scripts/check-jwt-push-features.sh

set -euo pipefail

# Plain text: the warning check below reads the log.
export CARGO_TERM_COLOR=never

# Files that the two gates touch. A warning in one of these fails the run.
WATCHED='--> autumn/src/(app\.rs|config\.rs|tenancy\.rs|error\.rs|lib\.rs|prelude\.rs|test\.rs|auth\.rs|push/)'

LOG=$(mktemp)
TREE=$(mktemp)
trap 'rm -f "$LOG" "$TREE"' EXIT

# ── Phase 1: each combination compiles, and links only what it asks for ─────

# `name version` must be in the graph of `$combo` exactly when `expect` is
# "yes".
check() {
  local name=$1 version=$2 expect=$3
  if grep -qE "^${name} v${version}" "$TREE"; then
    if [[ "$expect" == no ]]; then
      echo "::error::--features $combo compiles $name $version"
      failures=$((failures + 1))
    fi
  elif [[ "$expect" == yes ]]; then
    echo "::error::--features $combo does not compile $name $version; the check is broken"
    failures=$((failures + 1))
  fi
}

failures=0
for combo in "maud,htmx" "maud,htmx,jwt" "maud,htmx,push" "maud,htmx,jwt,push"; do
  echo "==> cargo check -p autumn-web --lib --no-default-features --features $combo"
  if ! cargo check -p autumn-web --lib --no-default-features --features "$combo" >"$LOG" 2>&1; then
    cat "$LOG"
    echo "::error::failed to compile: --no-default-features --features $combo"
    failures=$((failures + 1))
    continue
  fi
  # The build passed, so each diagnostic in the log is a warning. No pipe:
  # under pipefail, an early `grep -q` exit can fail the grep before it.
  if grep -qE -- "$WATCHED" "$LOG"; then
    cat "$LOG"
    echo "::error::a warning in jwt or push gated code with --features $combo (see above)"
    failures=$((failures + 1))
  fi

  cargo tree -p autumn-web --no-default-features --features "$combo" \
    -e normal --prefix none --format '{p}' >"$TREE"
  jwt=no
  push=no
  [[ ",$combo," == *",jwt,"* ]] && jwt=yes
  [[ ",$combo," == *",push,"* ]] && push=yes
  check jsonwebtoken 11 "$jwt"
  check rsa 0.9 "$jwt"
  check crypto-bigint 0.5 "$jwt"
  check p256 0.14 "$push"
done

if ((failures > 0)); then
  echo "::error::$failures jwt/push feature checks failed"
  exit 1
fi
echo "Every jwt/push combination compiles and links only what it names."

# ── Phase 2: the specs pass with both features off ──────────────────────────

# The `default` list of autumn-web, without `jwt` and `push`.
defaults=$(grep -E '^default = \[' autumn/Cargo.toml | sed -E 's/^default = \[(.*)\]/\1/; s/[" ]//g')
if [[ -z "$defaults" ]]; then
  echo "::error::could not read the default feature list from autumn/Cargo.toml"
  exit 1
fi
features=$(tr ',' '\n' <<<"$defaults" | grep -vxE 'jwt|push' | paste -sd, -)
echo "==> specs with jwt and push off: --features $features"

assert_ran() {
  # A filter that matches nothing still exits 0.
  grep -qE 'test result: ok\. [1-9][0-9]* passed' "$LOG" ||
    { echo "::error::no $1 test ran"; exit 1; }
}

cargo test -p autumn-web --no-default-features --features "$features" --lib \
  -- tenancy:: static_get_route_reading_tenant_fails_closed_for_every_tenancy_source 2>&1 | tee "$LOG"
assert_ran "lib tenancy"
cargo test -p autumn-web --no-default-features --features "$features" \
  --test integration_tests -- optional_jwt_and_push compile_gate_features tenancy_unit 2>&1 | tee "$LOG"
assert_ran "integration jwt/push spec"
grep -q 'jwt_tenancy_fails_closed_without_the_feature ... ok' "$LOG" ||
  { echo "::error::the fail-closed jwt test did not run"; exit 1; }
echo "The jwt and push specs pass with both features off."
