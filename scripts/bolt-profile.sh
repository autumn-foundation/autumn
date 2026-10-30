#!/usr/bin/env bash
# Bolt profile (callgrind + dhat) of the workloads in docs/perf/bolt/workloads.toml.
# See scripts/perf/bolt_profile.py for usage. Needs valgrind and python3.11+.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
exec python3 scripts/perf/bolt_profile.py "$@"
