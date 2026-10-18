#!/usr/bin/env bash
# Drives the read workload through the public HTTP entry points, then dumps
# pg_stat_statements. Usage: perf/run.sh <outdir>   (app on :3000, PG on $PSQL)
set -euo pipefail
OUT=${1:?outdir}; mkdir -p "$OUT"
# Same database the app uses (autumn-dev.toml / docker-compose.yml). Override with
# DATABASE_URL, and use the same value for seed.sql: psql "$DATABASE_URL" -f seed.sql
DATABASE_URL=${DATABASE_URL:-postgres://autumn:autumn@127.0.0.1:5433/reddit}
PSQL=${PSQL:-"psql $DATABASE_URL"}
BASE=${BASE:-http://localhost:3000}
q() { $PSQL -Atq -c "$1"; }
# Preflight: the profile is read from pg_stat_statements, which must be preloaded by the
# server (postgres -c shared_preload_libraries=pg_stat_statements -c track_io_timing=on).
libs=$(q 'show shared_preload_libraries') || { echo "run.sh: cannot connect to the database (check DATABASE_URL)" >&2; exit 1; }
case "$libs" in
  *pg_stat_statements*) ;;
  *) echo "run.sh: server must start with -c shared_preload_libraries=pg_stat_statements" >&2; exit 1;;
esac
q "create extension if not exists pg_stat_statements" >/dev/null
# workload targets: 3 hot posts (most comments), 3 mid, 3 cold; 3 subs of varying size
HOT=$(q "select s.slug||'/posts/'||p.slug from posts p join subreddits s on s.id=p.subreddit_id order by p.comment_count desc limit 3")
MID=$(q "select s.slug||'/posts/'||p.slug from posts p join subreddits s on s.id=p.subreddit_id where p.comment_count between 20 and 40 order by p.id limit 3")
COLD=$(q "select s.slug||'/posts/'||p.slug from posts p join subreddits s on s.id=p.subreddit_id where p.comment_count=0 order by p.id limit 3")
SUBS="sub1 sub50 sub900"
q "select pg_stat_statements_reset()" >/dev/null
q "select pg_stat_reset()" >/dev/null
for i in 1 2 3 4 5; do curl -sf -o /dev/null $BASE/; done
for s in $SUBS; do for i in 1 2 3; do curl -sf -o /dev/null $BASE/r/$s; done; done
for p in $HOT $MID $COLD; do for i in 1 2 3; do curl -sf -o /dev/null $BASE/r/$p; done; done

$PSQL -c "\\x off" >/dev/null
dump() { $PSQL -P pager=off -c "$2" > "$OUT/$1.txt"; }
TOT="(select sum(shared_blks_hit+shared_blks_read) from pg_stat_statements)"
dump top_buffers "select calls, shared_blks_hit+shared_blks_read as blks, round(100.0*(shared_blks_hit+shared_blks_read)/$TOT,1) as pct, rows, temp_blks_written as tmpw, wal_bytes, left(regexp_replace(query,'\\s+',' ','g'),150) q from pg_stat_statements order by 2 desc limit 10"
dump top_calls "select calls, round(100.0*calls/(select sum(calls) from pg_stat_statements),1) as pct, shared_blks_hit+shared_blks_read as blks, left(regexp_replace(query,'\\s+',' ','g'),150) q from pg_stat_statements order by calls desc limit 10"
dump spills "select calls,temp_blks_written,left(query,150) from pg_stat_statements where temp_blks_written>0"
dump seqscans "select relname,seq_scan,seq_tup_read,idx_scan from pg_stat_user_tables order by seq_tup_read desc limit 10"
dump statio "select relname,heap_blks_read,heap_blks_hit,idx_blks_read,idx_blks_hit,toast_blks_read,toast_blks_hit from pg_statio_user_tables order by heap_blks_hit+heap_blks_read desc limit 8"
# Index evidence. Dumped BEFORE the EXPLAIN below, because EXPLAIN ANALYZE itself scans.
dump index_usage "select indexrelname, idx_scan, idx_tup_read, pg_size_pretty(pg_relation_size(indexrelid)) as size from pg_stat_user_indexes where relname='votes' order by indexrelname"
$PSQL -P pager=off -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT post_id AS agg_key, CAST(SUM(value) AS bigint) AS agg_val FROM votes WHERE post_id IS NOT NULL GROUP BY post_id ORDER BY agg_val DESC NULLS LAST, agg_key ASC LIMIT 5" > "$OUT/explain_leaderboard.txt"
