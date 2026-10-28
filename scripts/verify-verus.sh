#!/usr/bin/env bash
# Run Verus on every spec in verification/ (issue #3066).
#
# Usage:
#   scripts/verify-verus.sh                          # uses `verus` on PATH
#   VERUS_BIN=/path/to/verus scripts/verify-verus.sh
#
# CI runs this script from .github/workflows/verus.yml. A new spec in
# verification/ is checked with no workflow change.
#
# The script checks all specs. It exits 1 if a spec fails.

set -euo pipefail
# An empty verification/ must not expand to the literal glob.
shopt -s nullglob

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

verus="${VERUS_BIN:-verus}"

specs=(verification/*.rs)
if (( ${#specs[@]} == 0 )); then
  echo "error: no Verus specs found in verification/" >&2
  exit 1
fi

failed=0
for spec in "${specs[@]}"; do
  echo "==> verus ${spec}"
  if ! "${verus}" "${spec}"; then
    echo "error: Verus rejected ${spec}" >&2
    failed=1
  fi
done

exit "${failed}"
