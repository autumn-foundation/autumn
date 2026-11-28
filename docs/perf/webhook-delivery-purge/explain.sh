#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of the hourly purge after the
# run.sh workload, at each fixture size. Usage: explain.sh [LOGS...]
# (default 100000 1000000 3000000). Same PG* env as run.sh.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
for logs in "${@:-100000 1000000 3000000}"; do
    for l in $logs; do
        echo "### $l logs"
        EXPLAIN=1 "$HERE/run.sh" "$l" 2>&1 | grep -v NOTICE
    done
done
