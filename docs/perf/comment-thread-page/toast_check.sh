#!/usr/bin/env bash
# usage: toast_check.sh > toast.txt
# TOAST and heap block reads of the first page of a thread holding a 200 kB
# (TOASTed) body, for the shipped and the rewritten page read. Uses the
# equivalence_cases.sql rows, committed and then deleted again.
set -euo pipefail
d=$(dirname "$0")
p() { psql -X -v ON_ERROR_STOP=1 "$@"; }
p -q -f "$d/equivalence_cases.sql" >/dev/null
trap 'p -qc "DELETE FROM comments WHERE id >= 9000000; DELETE FROM posts WHERE id > 40000" >/dev/null' EXIT
for v in before after; do
  f="$d/statements.sql"; [ $v = after ] && f="$d/statements-after.sql"
  # Each run is its own session; the stats views are read after a short
  # sleep so the session's counters have been flushed.
  p -qAt -c "SELECT pg_stat_reset()" >/dev/null
  { cat "$f"; for _ in 1 2 3 4 5; do echo "EXECUTE page_first('Post', 40003, 50, 'Post', 40003, 40003);"; done; } | p -q >/dev/null
  sleep 1.5
  p -qAtF ' ' -c "SELECT '$v heap_blks=' || (heap_blks_hit + heap_blks_read) || ' idx_blks=' || (idx_blks_hit + idx_blks_read)
      || ' toast_blks=' || (toast_blks_hit + toast_blks_read) || ' toast_idx_blks=' || (tidx_blks_hit + tidx_blks_read)
      FROM pg_statio_user_tables WHERE relname = 'comments'"
done
