#!/usr/bin/env bash
# Reproduce: LEDGER_PG_URL=postgres://u@host:port ./run.sh <outdir>
# Needs shared_preload_libraries=pg_stat_statements and a superuser role.
set -euo pipefail
out=${1:?outdir}; mkdir -p "$out"
# Absolute: the test runs from the crate directory, where a relative path
# would name a different place (or none).
out=$(cd "$out" && pwd)
P() { psql "$LEDGER_PG_URL/postgres" -XAtq "$@"; }
P -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements"
log=$out/test.log; : > "$log"
gate=$out/gate; rm -rf "$gate"; mkdir -p "$gate"
export LEDGER_GATE=$gate
# Job control puts the background job in its own process group (pgid = pid),
# so the trap below can stop cargo AND the test binary it spawned.
set -m
(cd "$(git rev-parse --show-toplevel)/autumn" && cargo test --features ledger-profile --test integration_tests ledger_capsule_profile -- --ignored --nocapture >"$log" 2>&1) &
pid=$!
# If the driver exits for any reason (a failed psql under `set -e`, Ctrl-C),
# do not leave the gated test polling for an ack that will never come: kill the
# whole group, since cargo does not forward the signal to its test binary.
trap 'kill -- -"$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true' EXIT
snap() { # phase
  P -c "COPY (SELECT calls, shared_blks_hit+shared_blks_read AS buffers, shared_blks_hit, shared_blks_read, temp_blks_written, wal_bytes, rows, left(regexp_replace(query,'\s+',' ','g'),110) AS q FROM pg_stat_statements s JOIN pg_database d ON d.oid=s.dbid WHERE d.datname IN ('ledger_src','ledger_dst') ORDER BY 2 DESC) TO STDOUT WITH CSV HEADER" > "$out/pgss-$1.csv"
}
# The test blocks at every mark until it is acknowledged, so a reset or
# snapshot always completes before the next phase issues a statement.
wait_mark() {
  until grep -q "LEDGER-MARK $1" "$log"; do
    # The test process died (compile or setup error): report it, do not spin.
    kill -0 "$pid" 2>/dev/null || { wait "$pid"; echo "profiler exited before mark $1; see $log" >&2; exit 1; }
    sleep 0.1
  done
}
ack() { touch "$gate/$1.ack"; }
wait_mark export-start; P -c "SELECT pg_stat_statements_reset()" >/dev/null; ack export-start
wait_mark export-end; snap export; P -c "SELECT pg_stat_statements_reset()" >/dev/null; ack export-end
wait_mark import-start; ack import-start
wait_mark import-end; snap import; ack import-end
wait "$pid"; trap - EXIT; grep LEDGER-ROWS "$log"
# Result-equivalence digest of the imported copy: every table's rows in key
# order plus the state of every sequence. Must be identical before/after a change.
D() { psql "$LEDGER_PG_URL/ledger_dst" -XAtq "$@"; }
{
  D -c "SELECT 'users', count(*), md5(string_agg(to_jsonb(t)::text, '|' ORDER BY id)) FROM users t"
  for i in $(seq 0 29); do D -c "SELECT 't$i', count(*), md5(string_agg(to_jsonb(t)::text, '|' ORDER BY id)) FROM t$i t"; done
  D -c "SELECT sequencename, last_value FROM pg_sequences ORDER BY 1"
} > "$out/digest.txt"
md5sum "$out/digest.txt"
