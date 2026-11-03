#!/usr/bin/env bash
# Pre-push gate: compile the SAME targets CI compiles, COMPILE-ONLY.
#
# The gate is the `pre-push` lane of `cargo xtask` (xtask/src/lanes.rs), which
# shares its clippy feature lists with CI's `lint` lane. This file stays so the
# command in older docs and muscle memory keeps working; every argument is
# passed through, e.g. `--list`, `--dry-run`, `--from <step>`, `--only <step>`.
#
# Usage (runnable from anywhere in the tree):
#   ./scripts/pre-push-check.sh
#   cargo xtask pre-push            # the same thing, and it runs on Windows

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
exec cargo xtask pre-push "$@"
