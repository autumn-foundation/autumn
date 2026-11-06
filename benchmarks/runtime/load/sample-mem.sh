#!/usr/bin/env bash
# sample-mem.sh — Sample a compose service's memory once a second until killed.
#
# Usage:
#   ./load/sample-mem.sh <framework> <out.csv> &
#   ... run k6 ...
#   kill %1
#
# Run from benchmarks/runtime. Writes `epoch_s,mem_mib` rows; peak and mean of
# the column are the "memory under load" figures in RESULTS.md.

set -euo pipefail

fw="$1"
out="$2"
id=$(docker compose ps -q "$fw")
[[ -n "$id" ]] || { echo "service $fw is not running" >&2; exit 1; }

echo "epoch_s,mem_mib" > "$out"
while :; do
  docker stats --no-stream --format '{{.MemUsage}}' "$id" | awk -v t="$(date +%s)" '
    {
      v = $1; n = v + 0
      if (v ~ /GiB$/) n *= 1024
      else if (v ~ /KiB$/) n /= 1024
      else if (v ~ /[0-9]B$/) n /= 1048576
      printf "%s,%.1f\n", t, n
    }' >> "$out"
done
