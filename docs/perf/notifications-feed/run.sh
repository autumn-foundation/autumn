#!/usr/bin/env bash
# usage: run.sh <variant> [fixture] > <variant>.txt
# Prints buffers per call for each feed read, index sizes, page checksums and plans.
# Variants are in indexes.sql. `fixture` seeds the table first. Needs PG env vars and pg_stat_statements.
set -euo pipefail
variant="${1:?usage: run.sh <variant> [fixture]}"
d=$(dirname "$0")
p() { psql -X -v ON_ERROR_STOP=1 "$@"; }
[ "${2:-}" = fixture ] && p -q -f "$d/fixture.sql" >/dev/null
p -q -v variant="$variant" -f "$d/indexes.sql" >/dev/null
p -qAtc "CREATE EXTENSION IF NOT EXISTS pg_stat_statements" >/dev/null
echo "variant=$variant"
p -qAtc "SELECT version()"
p -qAtc "SELECT 'rows recipient=' || recipient_id || ' ' || count(*) FROM notifications
  WHERE tenant_id = '' AND recipient_id IN (1, 58, 6373) GROUP BY recipient_id ORDER BY recipient_id"
printf '%-8s %-22s %10s %6s\n' recipient statement buffers rows
for r in 1 58 6373; do
  while IFS='|' read -r label sql; do
    [[ "$label" = --* ]] && continue
    q=${sql//%1\$s/$r}
    p -qAtc "SELECT pg_stat_statements_reset()" >/dev/null
    for _ in $(seq 20); do echo "$q;"; done | p -qAt >/dev/null
    p -qAtF ' ' -c "SELECT round(sum(shared_blks_hit + shared_blks_read)::numeric / sum(calls), 1), round(sum(rows)::numeric / sum(calls), 1)
      FROM pg_stat_statements WHERE query LIKE 'SELECT %FROM notifications%'" |
      while read -r b n; do printf '%-8s %-22s %10s %6s\n' "$r" "$label" "$b" "$n"; done
  done < "$d/workload.sql"
done
p -qAtc "SELECT 'size ' || indexrelname || ' ' || pg_size_pretty(pg_relation_size(indexrelid)) FROM pg_stat_user_indexes WHERE relname = 'notifications' ORDER BY 1"
p -qAtc "SELECT 'size table ' || pg_size_pretty(pg_table_size('notifications'))"
for o in "id DESC" "created_at DESC, id DESC"; do
  p -qAtc "SELECT 'md5 $o r=' || r || ' ' || md5(string_agg(id::text, ',')) FROM (VALUES (1), (58), (6373)) v(r),
    LATERAL (SELECT id FROM notifications WHERE tenant_id = '' AND recipient_id = r ORDER BY $o LIMIT 100) p GROUP BY r ORDER BY r"
done
for r in 1 58; do
  echo "--- plan, recipient $r, page id desc"
  p -qAtc "EXPLAIN (ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = $r ORDER BY id DESC LIMIT 20 OFFSET 0"
done
echo '--- plan, recipient 1, total count'
p -qAtc "EXPLAIN (ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = 1"
