#!/usr/bin/env bash
# Ledger capture of the autumn-web Docker sweep, then the per-query catalog.
#
#     scripts/ledger-profile.sh            add a profile for each new statement
#     scripts/ledger-profile.sh --refresh  rewrite every captured profile
#     scripts/ledger-profile.sh -- <libtest filters>   capture part of the sweep
#
# Runs the same `--ignored` sweep as CI's "Run Docker-dependent tests" step,
# with the `ledger-capture` feature and AUTUMN_LEDGER_CAPTURE_DIR set (see
# autumn/src/ledger_capture.rs). Docker must be reachable, and `pg_dump` 16+
# must be on PATH. The capture stays
# in LEDGER_CAPTURE_DIR (default: a new temp directory) for
# `scripts/perf/ledger_catalog.py report`.
#
# With filters, the script adds `__ledger_capture_installs_first`, the test
# that installs the capture before any other test connects.
#
# A failing test does not stop the profile: its statements up to the failure
# are still captured. The script exits non-zero if the sweep failed or if the
# gate still finds a statement with no profile.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

refresh=()
if [ "${1:-}" = "--refresh" ]; then
  refresh=(--refresh)
  shift
fi
if [ "${1:-}" = "--" ]; then
  shift
fi

capture="${LEDGER_CAPTURE_DIR:-$(mktemp -d -t ledger-capture.XXXXXX)}"

# Statements on a pre-16 test database are planned on a Postgres 16 schema
# mirror. Start one unless AUTUMN_LEDGER_PLAN_URL names a server already.
if [ -z "${AUTUMN_LEDGER_PLAN_URL:-}" ]; then
  docker rm -f autumn-ledger-plan >/dev/null 2>&1 || true
  docker run -d --name autumn-ledger-plan -p 55432:5432 \
    -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16-alpine >/dev/null
  trap 'docker rm -f autumn-ledger-plan >/dev/null 2>&1 || true' EXIT
  for _ in $(seq 1 60); do
    docker exec autumn-ledger-plan pg_isready -U postgres -q && break
    sleep 1
  done
  export AUTUMN_LEDGER_PLAN_URL=postgres://postgres@127.0.0.1:55432/postgres
fi
command -v pg_dump >/dev/null || echo "warning: no pg_dump on PATH; statements on pre-16 test databases get no plan" >&2
mkdir -p "$capture"
features="test-support,offline-sync,ws,mail,redis,i18n,collab,ledger-capture"

echo "== ledger capture into ${capture}"
sweep=0
AUTUMN_LEDGER_CAPTURE_DIR="$capture" cargo test -p autumn-web --features "$features" \
  --test integration_tests -- \
  --ignored \
  --test-threads=1 \
  --skip per_request_overhead_is_under_50_microseconds_p99 \
  --skip capture_overhead_on_a_successful_query_request \
  --skip outbox_claim_profile \
  ${1:+__ledger_capture_installs_first} \
  "$@" || sweep=$?

python3 scripts/perf/ledger_catalog.py update "$capture" "${refresh[@]}"
gate=0
python3 scripts/check-perf-profiles.py ledger "$capture" || gate=$?
echo "== capture kept in ${capture}"
if [ "$sweep" -ne 0 ]; then
  echo "ledger-profile: the sweep failed (exit ${sweep}); the catalog covers the statements it reached" >&2
  exit "$sweep"
fi
exit "$gate"
