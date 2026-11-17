#!/usr/bin/env bash
# Base-branch gate: every PR targets `trunk-dev`; only the release PR,
# `trunk-dev` -> `trunk`, targets `trunk`.
#
# WHY THIS EXISTS: a feature branch merged into `trunk` lands there as one
# squash commit. Git cannot match that commit to the `trunk-dev` commits it
# copies, so the merge base of the two branches stays at the last release, and
# the next release PR conflicts on every file that both the squash and later
# `trunk-dev` work touched. #3705 was merged into `trunk` this way and gave the
# 0.11.0 release PR (#3754) 116 conflicting files.
#
# WHY THIS FIRED, and how to satisfy it:
#
#   "this PR targets trunk"
#       Change the PR's base branch to `trunk-dev`. The gate re-runs when the
#       base changes. A real hotfix that must go to `trunk` directly carries
#       the `trunk-hotfix` label; port it to `trunk-dev` in the same breath.
#
# USAGE:
#   scripts/check-base-branch.sh               # gate, reads the env below
#   scripts/check-base-branch.sh --self-test   # table of cases
#
# Environment (set by .github/workflows/base-branch.yml):
#   BASE_REF    the PR's base branch
#   HEAD_REF    the PR's head branch
#   HEAD_REPO   owner/name of the head repository (empty when the head fork
#               was deleted)
#   BASE_REPO   owner/name of the base repository
#   LABELS      the PR's labels, comma-separated

set -euo pipefail

RELEASE_BASE="trunk"
RELEASE_HEAD="trunk-dev"
HOTFIX_LABEL="trunk-hotfix"

# Prints one line and returns 0 (allowed) or 1 (refused).
decide() {
  local base="$1" head="$2" head_repo="$3" base_repo="$4" labels="$5"
  if [[ "$base" != "$RELEASE_BASE" ]]; then
    echo "ok:    base is \`$base\`, not \`$RELEASE_BASE\`"
    return 0
  fi
  if [[ "$head" == "$RELEASE_HEAD" && "$head_repo" == "$base_repo" ]]; then
    echo "ok:    release PR: \`$RELEASE_HEAD\` -> \`$RELEASE_BASE\`"
    return 0
  fi
  if [[ ",$labels," == *",$HOTFIX_LABEL,"* ]]; then
    echo "ok:    \`$HOTFIX_LABEL\` label: \`$head\` -> \`$RELEASE_BASE\` allowed; port it to \`$RELEASE_HEAD\` too"
    return 0
  fi
  echo "error: this PR targets \`$RELEASE_BASE\` from \`$head\` ($head_repo)." \
    "Every PR targets \`$RELEASE_HEAD\`; only the release PR" \
    "(\`$RELEASE_HEAD\` -> \`$RELEASE_BASE\`) targets \`$RELEASE_BASE\`." \
    "Change the base branch to \`$RELEASE_HEAD\`."
  return 1
}

self_test() {
  local pass=0 total=0
  # name | base | head | head_repo | base_repo | labels | expected exit
  local cases=(
    "feature into trunk-dev|trunk-dev|claude/x|o/r|o/r||0"
    "release PR|trunk|trunk-dev|o/r|o/r||0"
    "feature into trunk|trunk|claude/x|o/r|o/r||1"
    "fork's trunk-dev into trunk|trunk|trunk-dev|fork/r|o/r||1"
    "hotfix label|trunk|fix/urgent|o/r|o/r|bug,trunk-hotfix|0"
    "label name is matched whole|trunk|fix/urgent|o/r|o/r|trunk-hotfix-later|1"
    "base other than trunk|release/0.11|claude/x|o/r|o/r||0"
    "deleted fork into trunk-dev|trunk-dev|x||o/r||0"
    "deleted fork into trunk|trunk|trunk-dev||o/r||1"
  )
  local entry name base head head_repo base_repo labels expected got
  for entry in "${cases[@]}"; do
    IFS='|' read -r name base head head_repo base_repo labels expected <<<"$entry"
    total=$((total + 1))
    got=0
    decide "$base" "$head" "$head_repo" "$base_repo" "$labels" >/dev/null || got=$?
    if [[ "$got" == "$expected" ]]; then
      pass=$((pass + 1))
      echo "self-test PASS: $name"
    else
      echo "self-test FAIL: $name (expected $expected, exit=$got)" >&2
    fi
  done
  echo "self-test: $pass/$total passed"
  [[ "$pass" == "$total" ]]
}

case "${1:-}" in
  --self-test) self_test ;;
  "")
    : "${BASE_REF:?BASE_REF is not set}"
    : "${HEAD_REF:?HEAD_REF is not set}"
    : "${BASE_REPO:?BASE_REPO is not set}"
    decide "$BASE_REF" "$HEAD_REF" "${HEAD_REPO:-}" "$BASE_REPO" "${LABELS:-}"
    ;;
  *)
    echo "usage: scripts/check-base-branch.sh [--self-test]" >&2
    exit 2
    ;;
esac
