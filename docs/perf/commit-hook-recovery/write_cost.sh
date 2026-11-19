#!/usr/bin/env bash
# Write cost of the after-hook recovery index: N hooks end to end (no idle
# ticks) on the 1M-row fixture, without and with 20261115143207. Reports WAL
# bytes (pg_stat_statements.wal_bytes per statement and the LSN delta) and the
# index sizes. Usage: write_cost.sh [N] [hist]. Needs PG* env.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
N="${1:-5000}"
HIST="${2:-1000000}"
NEW=20261115143207_add_commit_hooks_after_hook_recovery_index
for side in before after; do
  echo "##### $side"
  if [ "$side" = before ]; then export SKIP_MIGRATIONS="$NEW"; else unset SKIP_MIGRATIONS; fi
  TICK_WEIGHT=0 HOOK_WEIGHT=100 "$HERE/run.sh" "$N" "$HIST" \
    | sed -n -e '/^=== fixture/p' -e '/^--- pg_stat_statements/,/^--- pg_stat_user_tables/p' -e '/recovery /p'
done
