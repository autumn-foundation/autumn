#!/usr/bin/env bash
# Webhook delivery workload: N deliveries through the shipped store SQL
# (98% first-attempt success, 2% a 500 then a success) on the
# production-shaped fixture. Prints the pg_stat_statements profile, WAL and
# table counters. Usage: run.sh [N] (default 20000). Needs PG* env pointing at
# a Postgres 16 server that preloads pg_stat_statements.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
N="${1:-20000}"
CLIENTS="${CLIENTS:-1}"
DB="${DB:-ledger_webhook}"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK"

psql -X -q -d postgres -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB"
export PGDATABASE="$DB"
psql -X -q -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' \
    -f "$WORK/schema.sql" -f "$HERE/fixture.sql" >/dev/null
psql -X -q -c 'VACUUM ANALYZE' -c 'CHECKPOINT'
psql -X -q -c 'SELECT pg_stat_statements_reset()' -c 'SELECT pg_stat_reset()' >/dev/null
LSN0="$(psql -X -At -c 'SELECT pg_current_wal_lsn()')"

pgbench -n -M prepared --random-seed=3742 -c "$CLIENTS" -t $((N / CLIENTS)) \
    -f "$WORK/deliver_ok.pgbench@98" -f "$WORK/deliver_retry.pgbench@2" \
    | grep -E 'transaction type|SQL script|weight|processed|failed'
# Let the stats collector flush before reading pg_stat_user_tables.
psql -X -q -c 'SELECT pg_stat_force_next_flush()' >/dev/null
psql -X -v lsn0="$LSN0" -f "$HERE/report.sql"
