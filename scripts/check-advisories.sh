#!/usr/bin/env bash
# Dependency advisory gate (issue #1600).
#
# Blocks a merge — and a release — when any crate in a dependency tree Autumn
# ships carries a known RustSec advisory that no config explicitly waives.
# Three graphs are audited, because "the dependency tree" means three different
# things here:
#
#   1. deny.toml         — the workspace, default + Postgres + additive features.
#   2. deny-sqlite.toml  — the same workspace on the SQLite backend.
#   3. the scaffold      — autumn-web's tree, audited with the `deny.toml` that
#                          `autumn new` writes into a generated app. This is the
#                          gate behind "a fresh `autumn new` app's CI is green on
#                          day one": if the shipped waiver set stops covering the
#                          shipped dependency tree, it fails here rather than in
#                          a user's first CI run. See `audit_scaffold_graph` for
#                          how closely that graph matches a real app's.
#
# Usage:
#   scripts/check-advisories.sh              # audit all three graphs
#   scripts/check-advisories.sh --self-test  # prove the gate still rejects a
#                                            # known-vulnerable dependency
#
# Network behavior (mirrors what the scaffolded workflow does, deliberately):
# the advisory database fetch is retried with backoff, and if it is still
# unreachable the gate FAILS — fail closed, never "skipped, assumed clean". The
# checks themselves then run `--offline` against the fetched database, so a
# failure at that point is always a real advisory and never a network blip.
#
# Env:
#   ADVISORY_DB_FETCH_RETRIES  attempts at fetching the advisory DB (default 3)

set -euo pipefail

cd "$(dirname "$0")/.."

RETRIES="${ADVISORY_DB_FETCH_RETRIES:-3}"
SCAFFOLD_POLICY="autumn-cli/src/templates/deny.toml.tmpl"
# The known-vulnerable dependency the self-test injects. `time` 0.1.x is
# permanently affected by RUSTSEC-2020-0071 (the fix landed in 0.2.23), so this
# fixture cannot silently stop being vulnerable the way a pinned modern crate
# would once it is patched.
readonly VULNERABLE_DEP='time = "=0.1.45"'
readonly VULNERABLE_ADVISORY="RUSTSEC-2020-0071"

log() { printf '\n=== %s ===\n' "$*"; }

require_cargo_deny() {
  if ! cargo deny --version >/dev/null 2>&1; then
    echo "error: cargo-deny is not installed." >&2
    echo "       cargo install --locked cargo-deny   (CI pins it via taiki-e/install-action)" >&2
    exit 1
  fi
}

# Fetch the RustSec advisory database, retrying transient network failures.
# Returns non-zero — failing the gate — when it stays unreachable.
fetch_advisory_db() {
  local attempt=1
  while [ "$attempt" -le "$RETRIES" ]; do
    if cargo deny fetch db; then
      return 0
    fi
    echo "advisory database unreachable (attempt ${attempt}/${RETRIES})" >&2
    if [ "$attempt" -lt "$RETRIES" ]; then
      sleep "$((attempt * 10))"
    fi
    attempt=$((attempt + 1))
  done
  echo "error: RustSec advisory database unreachable after ${RETRIES} attempts - failing closed." >&2
  return 1
}

# The `[advisories]` section of a cargo-deny config, verbatim.
advisories_section() {
  awk '/^\[advisories\]/ { inside = 1; print; next } /^\[/ { inside = 0 } inside { print }' "$1"
}

# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------

audit_workspace() {
  log "workspace advisories (deny.toml)"
  cargo deny --offline check advisories

  log "workspace advisories, SQLite backend graph (deny-sqlite.toml)"
  cargo deny --offline --config deny-sqlite.toml check advisories
}

# Audit autumn-web's dependency tree against the policy `autumn new` ships, so
# a generated app's day-one CI cannot be red on its first push.
#
# Scope, honestly: cargo-deny audits every crate reachable from a manifest,
# optional dependencies included, so this is a SUPERSET of what a generated app
# actually compiles (it is narrowed to autumn-web's own tree — other workspace
# members' dependencies, e.g. the S3 storage and embedded-Postgres stacks, are
# out). A pass therefore guarantees the scaffold is advisory-clean; a finding in
# an optional dependency is triaged like any other — fixed, or waived with a
# reason in the scaffold's `deny.toml`.
audit_scaffold_graph() {
  log "scaffold day-one graph (${SCAFFOLD_POLICY})"
  cargo deny --offline \
    --manifest-path autumn/Cargo.toml \
    --exclude-dev \
    --features flash \
    --config "${SCAFFOLD_POLICY}" \
    check advisories
}

run_gate() {
  require_cargo_deny
  # `--offline` below means cargo must already have every crate in the graph.
  cargo fetch --locked
  fetch_advisory_db
  audit_workspace
  audit_scaffold_graph
  log "advisory gate OK"
}

# ---------------------------------------------------------------------------
# --self-test: prove the gate can still go red
# ---------------------------------------------------------------------------
#
# A gate nobody has ever seen fail is indistinguishable from a gate that no
# longer runs. This audits a throwaway crate carrying an injected
# known-vulnerable dependency, under this repository's *real* advisory policy,
# and requires:
#
#   1. the unwaived advisory to FAIL the check, naming its id, and
#   2. the same tree to PASS once — and only once — that id is waived,
#      which is the waiver mechanism the scaffold documents.

self_test() {
  require_cargo_deny
  fetch_advisory_db

  local fixture
  fixture="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand $fixture now, not at trap time
  trap "rm -rf '${fixture}'" EXIT

  cat > "${fixture}/Cargo.toml" <<EOF
[package]
name = "advisory-gate-self-test"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
${VULNERABLE_DEP}

[workspace]
EOF
  mkdir -p "${fixture}/src"
  echo 'fn main() {}' > "${fixture}/src/main.rs"

  # The fixture is audited under this repo's own advisory policy, so the test
  # fails if that policy is ever loosened into not catching this.
  advisories_section deny.toml > "${fixture}/deny.toml"
  if ! grep -q 'unused-ignored-advisory' "${fixture}/deny.toml"; then
    # The repo's waivers are for crates this fixture does not have; that is not
    # what is under test here.
    echo 'unused-ignored-advisory = "allow"' >> "${fixture}/deny.toml"
  fi
  if grep -q "${VULNERABLE_ADVISORY}" "${fixture}/deny.toml"; then
    echo "error: self-test fixture advisory ${VULNERABLE_ADVISORY} is waived in deny.toml;" >&2
    echo "       pick a different injected dependency, the negative case proves nothing." >&2
    exit 1
  fi

  if ! ( cd "${fixture}" && cargo fetch >/dev/null 2>&1 ); then
    echo "error: could not fetch the self-test fixture's dependencies." >&2
    exit 1
  fi

  log "self-test: an injected known-vulnerable dependency must FAIL the gate"
  local output status
  set +e
  output="$( cd "${fixture}" && cargo deny --offline check advisories 2>&1 )"
  status=$?
  set -e
  if [ "${status}" -eq 0 ]; then
    echo "error: the advisory gate PASSED a tree containing ${VULNERABLE_ADVISORY}." >&2
    echo "${output}" >&2
    exit 1
  fi
  if ! printf '%s' "${output}" | grep -q "${VULNERABLE_ADVISORY}"; then
    echo "error: the gate failed, but not for ${VULNERABLE_ADVISORY}:" >&2
    echo "${output}" >&2
    exit 1
  fi
  echo "blocked ${VULNERABLE_ADVISORY} as expected"

  log "self-test: the documented waiver must unblock exactly that advisory"
  if ! grep -q '^ignore = \[' "${fixture}/deny.toml"; then
    echo "error: deny.toml's [advisories] section has no 'ignore = [' list to waive into;" >&2
    echo "       the waiver half of this self-test cannot run." >&2
    exit 1
  fi
  awk -v waiver="    { id = \"${VULNERABLE_ADVISORY}\", reason = \"advisory gate self-test\" }," \
    '{ print } /^ignore = \[/ { print waiver }' \
    "${fixture}/deny.toml" > "${fixture}/deny.waived.toml"
  mv "${fixture}/deny.waived.toml" "${fixture}/deny.toml"
  if ! ( cd "${fixture}" && cargo deny --offline check advisories ); then
    echo "error: a waived advisory still failed the gate - the waiver mechanism is broken." >&2
    exit 1
  fi

  log "advisory gate self-test OK (${VULNERABLE_ADVISORY} blocked, then waived)"
}

case "${1:-}" in
  --self-test) self_test ;;
  "") run_gate ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
