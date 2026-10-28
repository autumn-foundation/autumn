#!/usr/bin/env bash
# Same-commit rerun protocol: run a test filter N times, report failures/N.
# usage: scripts/rerun-protocol.sh <N> <threads> <package> <filter>...
set -u
n=$1; threads=$2; pkg=$3; shift 3
fail=0
for i in $(seq "$n"); do
  cargo test -q -p "$pkg" --lib -- --test-threads="$threads" "$@" >/tmp/rerun.$$ 2>&1 || { fail=$((fail+1)); grep -m1 -E "panicked|left:|right:" /tmp/rerun.$$; }
done
rm -f /tmp/rerun.$$
echo "failures: $fail/$n"
[ "$fail" -eq 0 ]
