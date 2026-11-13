#!/usr/bin/env bash
# usage: profile.sh <out-dir> <n-posts>  — reset stats, drive workload, snapshot counters.
set -eu; O=$1; N=${2:-100000}; P="psql -h 127.0.0.1 -p 5433 -U postgres -d blog -X"
mkdir -p $O; D=$(dirname "$0")
$P -qAt -c "select pg_stat_statements_reset()" -c "select pg_stat_reset()" >/dev/null
$P -qAt -c "select pg_current_wal_lsn()" > $O/.lsn
"$D/workload.sh" http://localhost:3111 $N
Q="select regexp_replace(left(query,60),'\s+',' ','g')||'...' as q, calls, rows, shared_blks_hit+shared_blks_read as buffers, shared_blks_hit, shared_blks_read, temp_blks_read, temp_blks_written, wal_bytes, round(100.0*(shared_blks_hit+shared_blks_read)/nullif(sum(shared_blks_hit+shared_blks_read) over(),0),1) as pct_buffers, round(100.0*calls/sum(calls) over(),1) as pct_calls from pg_stat_statements where dbid=(select oid from pg_database where datname='blog') and query like '%posts%' and query not like '%pg_stat%'"
$P -c "$Q order by buffers desc" > $O/pg_stat_statements_by_buffers.txt
$P -c "$Q order by calls desc" > $O/pg_stat_statements_by_calls.txt
$P -c "select relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch from pg_stat_user_tables where relname='posts'" > $O/pg_stat_user_tables.txt
$P -c "select indexrelname, idx_scan from pg_stat_user_indexes where relname='posts'" > $O/pg_stat_user_indexes.txt
$P -c "select heap_blks_read+heap_blks_hit as heap, idx_blks_read+idx_blks_hit as idx, toast_blks_read+toast_blks_hit as toast, tidx_blks_read+tidx_blks_hit as toast_idx from pg_statio_user_tables where relname='posts'" > $O/pg_statio.txt
$P -c "select sum(shared_blks_hit+shared_blks_read) total_buffers, sum(calls) total_calls, sum(temp_blks_written) temp_written from pg_stat_statements where dbid=(select oid from pg_database where datname='blog')" > $O/totals.txt
cat $O/totals.txt $O/pg_stat_statements_by_buffers.txt $O/pg_statio.txt
