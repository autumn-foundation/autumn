#!/usr/bin/env bash
# Regenerates BOTH sides of the votes-leaderboard comparison from one seeded database.
# Usage: DATABASE_URL=... perf/compare.sh <outdir>   (app running on $BASE, default :3000)
# Writes <outdir>/before (index dropped) and <outdir>/after (index built).
set -euo pipefail
OUT=${1:?outdir}
HERE=$(cd "$(dirname "$0")" && pwd)
MIG="$HERE/../migrations/20261018055123_add_votes_post_id_value_covering_index"
DATABASE_URL=${DATABASE_URL:-postgres://autumn:autumn@127.0.0.1:5433/reddit}
export DATABASE_URL
P="psql -q -v ON_ERROR_STOP=1 $DATABASE_URL"
# Restore the index however this script exits (failed vacuum/run.sh, Ctrl-C, ...): the
# migration is recorded as applied, so a missing index would not be rebuilt by an app restart.
restore() {
  trap - EXIT
  state=$($P -Atc "select coalesce((select indisvalid::text from pg_index where indexrelid = to_regclass('idx_votes_post_id_value')), 'absent')")
  case "$state" in
    true) ;;
    false) $P -f "$MIG/down.sql"; $P -f "$MIG/up.sql" ;;
    absent) $P -f "$MIG/up.sql" ;;
  esac
}
trap restore EXIT
$P -f "$MIG/down.sql"                      # before: no covering index
$P -c "VACUUM (ANALYZE) votes"
"$HERE/run.sh" "$OUT/before"
$P -f "$MIG/up.sql"                        # after: CREATE INDEX CONCURRENTLY
$P -c "VACUUM (ANALYZE) votes"
"$HERE/run.sh" "$OUT/after"
echo "before: $OUT/before/top_buffers.txt  after: $OUT/after/top_buffers.txt"
