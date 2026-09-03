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
#
# `cargo deny fetch` loads the config BEFORE it touches the network, so a
# malformed `deny.toml` fails here too — deterministically, on every attempt.
# Retrying that is pointless and reporting it as "database unreachable" sends
# the reader hunting a network problem they do not have, so a failure naming a
# config file is surfaced immediately instead.
fetch_advisory_db() {
  local attempt=1 output
  while [ "$attempt" -le "$RETRIES" ]; do
    set +e
    output="$(cargo deny fetch db 2>&1)"
    local status=$?
    set -e
    printf '%s\n' "${output}" >&2
    if [ "${status}" -eq 0 ]; then
      return 0
    fi
    if printf '%s' "${output}" | grep -qE 'deny(-sqlite)?\.toml'; then
      echo "error: cargo-deny could not load the advisory config (see above) - this is a" >&2
      echo "       configuration error, not a network failure. Fix the file and re-run." >&2
      return 1
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
# The feature list is the UNION of every autumn-web feature any scaffold flavor
# can turn on — `--bundled-pg` is the one that matters, because
# `managed-pg-bundled` drags in the embedded-Postgres stack and with it an
# advisory the default flavor's tree does not have. Auditing only the default
# set is how a flavor ships waivers that do not cover its own tree; the
# `the_gate_audits_every_feature_a_scaffold_flavor_can_enable` test keeps this
# list honest as flavors change. (`--daemon` and `--api` switch defaults off and
# name subsets of this, so the union covers them.)
#
# Feature narrowing here is one-directional: cargo-deny keeps optional
# dependencies in the graph whether or not their feature is on, so naming a
# feature can only ADD crates (it pulls in feature-gated chains like
# managed-pg-bundled's build-time stack). Never treat an omitted feature as
# proof that its crates were excluded.
#
# Scope, honestly: cargo-deny audits every crate reachable from a manifest,
# optional dependencies included, and this roots at autumn-web resolved against
# the WORKSPACE lockfile with `--exclude-dev`. A generated app roots at its own
# manifest — adding `maud`, `diesel_migrations` and its dev-dependencies — and
# resolves its own lockfile from crates.io. So this covers the autumn-web half
# of a generated app's tree, generously (a superset of what the app compiles
# from autumn-web), and not the app's own direct dependencies. A pass means the
# shipped waiver set still covers what autumn-web brings; a finding is triaged
# like any other — fixed, or waived with a reason in the scaffold's `deny.toml`.
SCAFFOLD_FEATURES="flash,managed-pg-bundled,i18n,seed,embed-assets"

audit_scaffold_graph() {
  log "scaffold day-one graph (${SCAFFOLD_POLICY})"
  cargo deny --offline \
    --manifest-path autumn/Cargo.toml \
    --exclude-dev \
    --features "${SCAFFOLD_FEATURES}" \
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
# known-vulnerable dependency and requires, for EACH policy this repository
# ships:
#
#   1. the unwaived advisory to FAIL the check, naming its id, and
#   2. the same tree to PASS once — and only once — that id is waived,
#      which is the waiver mechanism the docs describe.
#
# Both policies are covered on purpose. The workspace `deny.toml` is what gates
# this repo; the scaffold's `deny.toml.tmpl` is what gates every generated app,
# and it is otherwise only ever exercised against trees that happen to be
# clean — so nothing would notice if it stopped blocking.

# Write the throwaway crate carrying the injected vulnerable dependency.
write_fixture_crate() {
  local dir="$1"
  cat > "${dir}/Cargo.toml" <<EOF
[package]
name = "advisory-gate-self-test"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
${VULNERABLE_DEP}

[workspace]
EOF
  mkdir -p "${dir}/src"
  echo 'fn main() {}' > "${dir}/src/main.rs"

  if ! ( cd "${dir}" && cargo fetch >/dev/null 2>&1 ); then
    echo "error: could not fetch the self-test fixture's dependencies." >&2
    return 1
  fi
}

# Prove one policy both blocks the injected advisory and honours its waiver.
#
#   $1  label for the log
#   $2  path to the config whose `[advisories]` section is under test
prove_policy_blocks() {
  local label="$1" policy="$2" fixture output status
  fixture="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand $fixture now, not at trap time
  trap "rm -rf '${fixture}'" RETURN

  write_fixture_crate "${fixture}"

  # Only the advisories policy travels: the rest of a config ([graph] features,
  # licenses) names crates this fixture does not have.
  advisories_section "${policy}" > "${fixture}/deny.toml"
  if ! grep -q 'unused-ignored-advisory' "${fixture}/deny.toml"; then
    # The policy's own waivers are for crates this fixture does not have; that
    # is not what is under test here.
    echo 'unused-ignored-advisory = "allow"' >> "${fixture}/deny.toml"
  fi
  if grep -q "${VULNERABLE_ADVISORY}" "${fixture}/deny.toml"; then
    echo "error: ${label} waives ${VULNERABLE_ADVISORY}, the advisory this self-test injects;" >&2
    echo "       pick a different injected dependency, the negative case proves nothing." >&2
    return 1
  fi

  log "self-test (${label}): an injected known-vulnerable dependency must FAIL the gate"
  set +e
  output="$( cd "${fixture}" && cargo deny --offline check advisories 2>&1 )"
  status=$?
  set -e
  if [ "${status}" -eq 0 ]; then
    echo "error: ${label} PASSED a tree containing ${VULNERABLE_ADVISORY}." >&2
    echo "${output}" >&2
    return 1
  fi
  if ! printf '%s' "${output}" | grep -q "${VULNERABLE_ADVISORY}"; then
    echo "error: ${label} failed, but not for ${VULNERABLE_ADVISORY}:" >&2
    echo "${output}" >&2
    return 1
  fi
  echo "blocked ${VULNERABLE_ADVISORY} as expected"

  log "self-test (${label}): the documented waiver must unblock exactly that advisory"
  if ! grep -q '^ignore = \[' "${fixture}/deny.toml"; then
    echo "error: ${label}'s [advisories] section has no 'ignore = [' list to waive into;" >&2
    echo "       the waiver half of this self-test cannot run." >&2
    return 1
  fi
  awk -v waiver="    { id = \"${VULNERABLE_ADVISORY}\", reason = \"advisory gate self-test\" }," \
    '{ print } /^ignore = \[/ { print waiver }' \
    "${fixture}/deny.toml" > "${fixture}/deny.waived.toml"
  mv "${fixture}/deny.waived.toml" "${fixture}/deny.toml"
  set +e
  output="$( cd "${fixture}" && cargo deny --offline check advisories 2>&1 )"
  status=$?
  set -e
  # The fixture's own tiny tree (libc, winapi) is audited under a policy that
  # denies unmaintained and unsound crates too, so a future advisory against one
  # of them would fail this run for a reason that has nothing to do with the
  # waiver under test. What the waiver must achieve is precise: the injected
  # advisory stops being reported.
  if printf '%s' "${output}" | grep -q "${VULNERABLE_ADVISORY}"; then
    echo "error: ${label} still reports a WAIVED advisory - the waiver mechanism is broken." >&2
    echo "${output}" >&2
    return 1
  fi
  if [ "${status}" -ne 0 ]; then
    echo "note: ${label} still fails the fixture, but no longer for ${VULNERABLE_ADVISORY};" >&2
    echo "      an unrelated advisory now affects the fixture's own tree:" >&2
    echo "${output}" >&2
  fi
}

self_test() {
  require_cargo_deny
  fetch_advisory_db

  prove_policy_blocks "the workspace policy (deny.toml)" deny.toml
  prove_policy_blocks "the scaffold policy (${SCAFFOLD_POLICY})" "${SCAFFOLD_POLICY}"

  log "advisory gate self-test OK (${VULNERABLE_ADVISORY} blocked, then waived, under both policies)"
}

case "${1:-}" in
  --self-test) self_test ;;
  "") run_gate ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
