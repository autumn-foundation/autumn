#!/usr/bin/env bash
# Reproduce: LEDGER_PG_URL=postgres://u@host:port ./run.sh <outdir>
# Needs shared_preload_libraries=pg_stat_statements and a superuser role.
set -euo pipefail
out=${1:?outdir}; mkdir -p "$out"
P() { psql "$LEDGER_PG_URL/postgres" -XAtq "$@"; }
P -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements"
log=$out/test.log; : > "$log"
(cd "$(git rev-parse --show-toplevel)/autumn" && cargo test --test ledger_capsule_profile -- --ignored --nocapture >"$log" 2>&1) &
pid=$!
snap() { # phase
  P -c "COPY (SELECT calls, shared_blks_hit+shared_blks_read AS buffers, shared_blks_hit, shared_blks_read, temp_blks_written, wal_bytes, rows, left(regexp_replace(query,'\s+',' ','g'),110) AS q FROM pg_stat_statements s JOIN pg_database d ON d.oid=s.dbid WHERE d.datname IN ('ledger_src','ledger_dst') ORDER BY 2 DESC) TO STDOUT WITH CSV HEADER" > "$out/pgss-$1.csv"
}
until grep -q "LEDGER-MARK export-start" "$log"; do sleep 0.2; done
P -c "SELECT pg_stat_statements_reset()" >/dev/null
until grep -q "LEDGER-MARK export-end" "$log"; do sleep 0.2; done
snap export
P -c "SELECT pg_stat_statements_reset()" >/dev/null
until grep -q "LEDGER-MARK import-end" "$log"; do sleep 0.2; done
snap import
wait $pid; grep LEDGER-ROWS "$log"
# Result-equivalence digest of the imported copy: every table's rows in key
# order plus the state of every sequence. Must be identical before/after a change.
D() { psql "$LEDGER_PG_URL/ledger_dst" -XAtq "$@"; }
{
  D -c "SELECT 'users', count(*), md5(string_agg(to_jsonb(t)::text, '|' ORDER BY id)) FROM users t"
  for i in $(seq 0 29); do D -c "SELECT 't$i', count(*), md5(string_agg(to_jsonb(t)::text, '|' ORDER BY id)) FROM t$i t"; done
  D -c "SELECT sequencename, last_value FROM pg_sequences ORDER BY 1"
} > "$out/digest.txt"
md5sum "$out/digest.txt"
