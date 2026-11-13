#!/usr/bin/env bash
# Check what the no-DB daemon starter does not compile (#2795).
#
# `plugin-conformance`, `runtime-config` and `axum-ws` are default-on, so
# `cargo test --workspace` never builds autumn-web with one of them off. The
# daemon starter does: `autumn new --daemon` names none of them.
#
# Phase 1: the feature list that `autumn new --daemon` writes turns none of
# the three on, and its dependency tree has no WebSocket crate. The default
# feature set has that crate, so the check cannot pass on an empty match.
#
# Phase 2: that feature list compiles.
#
# Phase 3: the specs run with the other default features and the three off.
# This is the only build that compiles the test targets with them off.
#
# Called from the `daemon-starter` lane of the `test-features` CI job. Run
# locally:
#
#     ./scripts/check-daemon-starter-features.sh

set -euo pipefail

# Plain text: the checks below read the log.
export CARGO_TERM_COLOR=never

GATES=(plugin-conformance runtime-config axum-ws)
# Crates that only axum's `ws` feature brings in.
WS_CRATES=(tungstenite tokio-tungstenite)

LOG=$(mktemp)
TREE=$(mktemp)
trap 'rm -f "$LOG" "$TREE"' EXIT

resolved_features() {
  cargo tree -p autumn-web --no-default-features --features "$1" \
    -e normal --depth 0 --prefix none --format '{f}'
}

crates() {
  cargo tree -p autumn-web --no-default-features --features "$1" \
    -e normal --prefix none --format '{p}' | cut -d' ' -f1 | sort -u
}

# ── Phase 1: the daemon starter's features and dependency tree ──────────────

# The list that `autumn new --daemon` writes into the app's manifest.
daemon=$(grep -E '^const DAEMON_NO_DB_FEATURES' autumn-cli/src/new.rs |
  sed -E 's/^.*= &\[(.*)\];.*$/\1/; s/[" ]//g')
if [[ -z "$daemon" ]]; then
  echo "::error::could not read the daemon feature list from autumn-cli/src/new.rs"
  exit 1
fi
# The scaffold turns `flash` on through its own default feature.
daemon="$daemon,flash"

# The `default` list of autumn-web.
defaults=$(grep -E '^default = \[' autumn/Cargo.toml | sed -E 's/^default = \[(.*)\]/\1/; s/[" ]//g')
if [[ -z "$defaults" ]]; then
  echo "::error::could not read the default feature list from autumn/Cargo.toml"
  exit 1
fi

resolved=$(resolved_features "$daemon")
for gate in "${GATES[@]}"; do
  if [[ ",$defaults," != *",$gate,"* ]]; then
    echo "::error::\`$gate\` is not in the default feature list"
    exit 1
  fi
  if [[ ",$resolved," == *",$gate,"* ]]; then
    echo "::error::the daemon starter's features turn \`$gate\` on: $daemon"
    exit 1
  fi
done

crates "$defaults" >"$TREE"
for crate in "${WS_CRATES[@]}"; do
  if ! grep -qxF -- "$crate" "$TREE"; then
    echo "::error::the default build has no \`$crate\`: this check proves nothing"
    exit 1
  fi
done
crates "$daemon" >"$TREE"
for crate in "${WS_CRATES[@]}"; do
  if grep -qxF -- "$crate" "$TREE"; then
    echo "::error::the daemon starter compiles \`$crate\` (--features $daemon)"
    exit 1
  fi
done
echo "The daemon starter compiles no WebSocket crate."

# ── Phase 2: the daemon starter's feature list compiles ─────────────────────

echo "==> cargo check -p autumn-web --lib --no-default-features --features $daemon"
if ! cargo check -p autumn-web --lib --no-default-features --features "$daemon" >"$LOG" 2>&1; then
  cat "$LOG"
  echo "::error::failed to compile: --no-default-features --features $daemon"
  exit 1
fi
echo "The daemon starter's feature list compiles."

# ── Phase 3: the specs pass with the three features off ─────────────────────

pattern=$(IFS='|'; echo "${GATES[*]}")
features=$(tr ',' '\n' <<<"$defaults" | grep -vxE -- "$pattern" | paste -sd, -)

# Each one must be off in the resolved set, not only absent from the list.
resolved=$(resolved_features "$features")
for gate in "${GATES[@]}"; do
  if [[ ",$resolved," == *",$gate,"* ]]; then
    echo "::error::\`$gate\` is on: a default feature turns it on"
    exit 1
  fi
done
echo "==> specs with ${GATES[*]} off: --features $features"

assert_ran() {
  # A filter that matches nothing still exits 0.
  grep -qE 'test result: ok\. [1-9][0-9]* passed' "$LOG" ||
    { echo "::error::no $1 test ran"; exit 1; }
}

assert_passed() {
  grep -qE -- "$1 \.\.\. ok" "$LOG" ||
    { echo "::error::$1 did not run"; exit 1; }
}

assert_absent() {
  if grep -qF -- "$1" "$LOG"; then
    echo "::error::$1 ran, so its feature is on"
    exit 1
  fi
}

cargo test -p autumn-web --no-default-features --features "$features" --lib \
  -- cost::tests 2>&1 | tee "$LOG"
assert_ran "lib cost"
assert_passed 'cost::tests::a_pending_signal_holds_work_until_the_first_value'
assert_absent 'signal_syncs_from_runtime_config'

cargo test -p autumn-web --no-default-features --features "$features" \
  --test integration_tests -- compile_gate_features cost_metering \
  server_http_limits plugin_contract 2>&1 | tee "$LOG"
assert_ran "integration"
assert_passed 'compile_gate_features::axum_ws_is_owned_by_its_feature'
assert_passed 'compile_gate_features::every_gated_module_has_a_cfg'
assert_absent 'signal_follows_runtime_config_without_redeploy'
assert_absent 'websocket_outlives_keep_alive_timeout'
echo "The specs pass with ${GATES[*]} off."
