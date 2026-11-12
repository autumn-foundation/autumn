#!/usr/bin/env bash
# Result equivalence of idx_autumn_jobs_unique_key_enqueued: the same enqueue
# SQL with and without the index.
# 1. The run.sh workload at 100,000, 1,000,000 and 3,000,000 history rows on
#    schema_before.sql and schema.sql: inserted rows and the end-state md5.
# 2. equivalence_cases.sql on an empty table, before (no index) and after
#    (with it, enable_seqscan = off so the small table uses it).
# Usage: equivalence.sh
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK"
for n in 100000 1000000 3000000; do
  for s in schema_before.sql schema.sql; do
    echo "--- $n rows, $s"
    SCHEMA="$s" DB=ledger_jobenqueue_eq "$HERE/run.sh" 2000 "$n" 2>&1 \
      | grep -E 'insert (ttl|plain|running)|^ +2201 |end-state md5|unique holds|evicted' \
      | sed -E 's/^ *([0-9]+) \| *([0-9]+) \|.*\| *([0-9]+) \| *[0-9]+ \| *[0-9]+ \| *[0-9.]+ \| *[0-9.]+ \| (.*)$/\4: calls \1, rows \3/' \
      | grep -vE '^ +2201'
  done
done
cases() {
  local schema="$1" opts="$2"
  dropdb --if-exists ledger_jobenqueue_cases; createdb ledger_jobenqueue_cases
  (cd "$HERE" && psql -qX -v ON_ERROR_STOP=1 -d ledger_jobenqueue_cases -f "$schema" >/dev/null)
  PGOPTIONS="$opts" psql -qX -v ON_ERROR_STOP=1 -d ledger_jobenqueue_cases \
    -v insert="$(cat "$WORK/insert.sql")" -f "$HERE/equivalence_cases.sql"
}
cases schema_before.sql "" > "$WORK/before.txt"
cases schema.sql "-c enable_seqscan=off" > "$WORK/after.txt"
echo "--- edge cases, before (no index)"; cat "$WORK/before.txt"
echo "--- edge cases, after (index, enable_seqscan = off)"; cat "$WORK/after.txt"
echo "--- outcome columns identical: $(diff <(cut -s -d'|' -f1,2 "$WORK/before.txt") <(cut -s -d'|' -f1,2 "$WORK/after.txt") >/dev/null && echo yes || echo NO)"
rm -rf "$WORK"
