#!/usr/bin/env bash
# Ledger harness: the repository commit-hook worker on a production-shaped
# autumn_repository_commit_hooks (fixture.sql). T transactions, seeded: 80%
# one idle worker tick (both stale-recovery UPDATEs and a claim attempt, the
# loop body that runs every 250 ms on every replica), 20% one hook end to end
# (pending insert, mark succeeded, finalize, claim, ack). All SQL is the text
# repository_commit_hooks.rs ships (extract_sql.py). Prints the
# pg_stat_statements profile, WAL and table counters.
# Usage: [MIGRATIONS_DIR=..] [SKIP_MIGRATIONS=..] [TICK_WEIGHT=80]
#   [HOOK_WEIGHT=20] run.sh [T] [hist]. TICK_WEIGHT=0 runs hooks only. Needs PG* env pointing at a
# Postgres 16 server that preloads pg_stat_statements, autovacuum off.
set -euo pipefail
export PGOPTIONS="${PGOPTIONS:-} -c synchronize_seqscans=off"
HERE="$(cd "$(dirname "$0")" && pwd)"
T="${1:-2000}"
HIST="${2:-1000000}"
DB="${DB:-ledger_hookrecovery}"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK"

dropdb --if-exists "$DB"; createdb "$DB"
export PGDATABASE="$DB"
psql -qX -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
psql -qX -v ON_ERROR_STOP=1 -f "$WORK/schema.sql" >/dev/null
psql -qX -v ON_ERROR_STOP=1 -v hist="$HIST" -f "$HERE/fixture.sql" >/dev/null
echo "=== fixture: hist=$HIST, T=$T, migrations: ${MIGRATIONS_DIR:-autumn/migrations}, skipped: ${SKIP_MIGRATIONS:-none}"
psql -X -c "SELECT status, count(*) FROM autumn_repository_commit_hooks GROUP BY 1 ORDER BY 1" \
  -c "SELECT pg_size_pretty(pg_relation_size('autumn_repository_commit_hooks')) AS heap,
             pg_relation_size('autumn_repository_commit_hooks') / 8192 AS heap_blocks,
             pg_size_pretty(pg_indexes_size('autumn_repository_commit_hooks')) AS indexes"
psql -qX -c 'CHECKPOINT' -c 'SELECT pg_stat_statements_reset()' -c 'SELECT pg_stat_reset()' >/dev/null
LSN0="$(psql -XAt -c 'SELECT pg_current_wal_lsn()')"
echo "=== workload"
STALE="$(cat "$WORK/stale_ms")"
SCRIPTS=()
if [ "${TICK_WEIGHT:-80}" != 0 ]; then SCRIPTS+=(-f "$WORK/tick.pgbench@${TICK_WEIGHT:-80}"); fi
SCRIPTS+=(-f "$WORK/hook.pgbench@${HOOK_WEIGHT:-20}")
pgbench -n -M extended --random-seed=3742 -c 1 -t "$T" \
  -D stale="$STALE" -D worker=worker-1 -D owner=app-1 -D hname=after_update \
  -D err='stale claim recovered by worker-1' \
  -D perr='stale pending after hook recovered by worker-1' \
  -D keys='{app::PostRepository,app::CommentRepository,app::UserRepository,app::VoteRepository}' \
  -D ctx='{"request_id":"01JD0000000000000000000000","actor":"user:42","tenant":"t-1","trace":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"}' \
  -D rec='{"id":12345,"title":"A post title that is about sixty characters long, give or take","body":"lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua","score":17,"author_id":42}' \
  "${SCRIPTS[@]}" \
  | grep -E 'SQL script|weight|processed|failed'
psql -qX -c 'SELECT pg_stat_force_next_flush()' >/dev/null
psql -X -v lsn0="$LSN0" -f "$HERE/report.sql"
echo "=== end state"
psql -XAt -c "SELECT 'status counts: ' || string_agg(status || '=' || n, ', ' ORDER BY status) FROM
      (SELECT status, count(*) n FROM autumn_repository_commit_hooks GROUP BY 1) s"
rm -rf "$WORK"
