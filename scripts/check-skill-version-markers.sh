#!/usr/bin/env bash
# Verify the `autumn-web` agent skill's version markers against CHANGELOG.md.
#
# The skill annotates APIs with the release they arrived in — "(0.6.0)",
# "**(0.7.0)**", "(0.7.0, #1182)", "(feature `tls`, 0.6.0, #1603)", and more.
# A marker that names too NEW a release is the damaging direction: it tells a
# reader on that line an API is out of reach when it already shipped.
#
# These markers were hand-maintained and drifted badly across the 0.6.0 -> 0.7.0
# cut, in several punctuation shapes that ad-hoc greps kept missing. This script
# matches the version token itself rather than any surrounding punctuation, so a
# new shape cannot hide from it.
#
# Method: for a marker line that also cites an issue (`#1234`), find that
# issue's OLDEST mention in CHANGELOG.md — the file is newest-first, so the
# oldest mention is the release that introduced it — and require the marker to
# name that release. Lines with no issue reference are reported as unverifiable
# and must be checked by hand.
set -euo pipefail

cd "$(dirname "$0")/.."
CHANGELOG="CHANGELOG.md"
SKILL_DIR="skills/autumn-web"

# Lines whose issue reference is NOT the marked feature's own issue, verified by
# hand. Keyed on line content, since line numbers move. Keep this list tiny and
# justify every entry.
ALLOW=(
  # Marks `from_shard`/`with_pool_untracked` (0.6.0); cites #1629 only for the
  # `autumn upgrade` tooling that applies its codemod, which is 0.7.0.
  'with_pool_untracked` is the 0.6.0 rename'
)

# Every released version, from the CHANGELOG's own section headings. Anything
# not in this set is not a version marker (sample IPs, MSRVs, ports).
mapfile -t RELEASES < <(grep -o '^## \[[0-9]\+\.[0-9]\+\.[0-9]\+\]' "$CHANGELOG" | tr -d '#[] ')
is_release() {
  local v="$1"
  for r in "${RELEASES[@]}"; do [[ "$v" == "$r" ]] && return 0; done
  return 1
}

section_for_line() {
  local n="$1"
  awk -v target="$n" '
    /^## \[[0-9]+\.[0-9]+\.[0-9]+\]/ { ver=$2; gsub(/[][]/, "", ver); line=NR }
    NR == target { print ver; found=1; exit }
    END { if (!found) print "" }
  ' "$CHANGELOG"
}

fail=0
unverifiable=0

while IFS= read -r entry; do
  file="${entry%%:*}"; rest="${entry#*:}"
  lineno="${rest%%:*}"; text="${rest#*:}"

  skip=0
  for a in "${ALLOW[@]}"; do
    [[ "$text" == *"$a"* ]] && skip=1 && break
  done
  [[ $skip -eq 1 ]] && continue

  # `|| true`: a non-matching grep exits 1, which `set -e` would treat as fatal.
  # Collect EVERY release token and EVERY issue on the line, not just the
  # first of each. A line can annotate more than one feature — e.g.
  # "autumn deploy (0.6.0; fleets 0.7.0, issues #1607/#1621)" — and stopping at
  # the first pair would leave the later annotation unchecked forever.
  markers=()
  for cand in $(grep -o '[0-9]\+\.[0-9]\+\.[0-9]\+' <<<"$text" || true); do
    is_release "$cand" && markers+=("$cand")
  done
  [[ ${#markers[@]} -eq 0 ]] && continue

  issues=()
  for i in $(grep -o '#[0-9]\{3,4\}' <<<"$text" || true); do issues+=("$i"); done
  if [[ ${#issues[@]} -eq 0 ]]; then
    unverifiable=$((unverifiable + 1))
    continue
  fi

  # Rule: every issue on the line must be accounted for by SOME release token
  # on that line. Positional pairing would be wrong (a line can cite an issue
  # for tooling rather than for the feature it marks), but an issue whose
  # introducing release appears nowhere on its line is drift.
  for issue in "${issues[@]}"; do
    oldest="$(grep -n -- "$issue" "$CHANGELOG" 2>/dev/null | tail -1 | cut -d: -f1 || true)"
    [[ -z "$oldest" ]] && continue
    truth="$(section_for_line "$oldest" || true)"
    [[ -z "$truth" ]] && continue

    matched=0
    for m in "${markers[@]}"; do [[ "$m" == "$truth" ]] && matched=1 && break; done
    if [[ $matched -eq 0 ]]; then
      echo "MISMATCH $file:$lineno" >&2
      echo "         $issue was introduced in $truth, which appears nowhere on this line" >&2
      echo "         line carries: ${markers[*]}" >&2
      echo "         ${text:0:100}" >&2
      fail=$((fail + 1))
    fi
  done
done < <(
  grep -rn '0\.[0-9]\+\.0' "$SKILL_DIR" \
    | grep -v 'migrations/0\.[0-9]\+\.0\.md\|v0\.[0-9]\+\.0\|--version 0\.[0-9]\+\.0\|"0\.[0-9]\+\.0"\|autumn-web = \|autumn-cli = \|MSRV' \
    | sed "s|^$SKILL_DIR/||"
)

# --- Second check: prose that asserts an availability boundary --------------
#
# Markers were not the only thing that went stale across the 0.6.0 -> 0.7.0 cut.
# The skill also carried sentences meaning "not shipped yet" — "trunk-dev only",
# "NOT in the published X", "do not suggest them to users on the published
# release". Every one of those became false the moment the release was cut, and
# a version-number substitution does not fix them: it leaves prose that
# contradicts its own marker, or actively tells an agent to withhold a shipped
# command. They are phrased too freely to verify mechanically, so this check
# bans the specific phrasings rather than trying to interpret them. Say when a
# feature ARRIVED ("since 0.6.0", "(0.6.0)"), never where it has not yet landed.
STALE_PHRASES=(
  'trunk-dev only'
  'trunk-dev-only'
  'not in the published'
  'do not suggest them to users on the published release'
  'On trunk-dev'
  'on trunk-dev'
)
for phrase in "${STALE_PHRASES[@]}"; do
  while IFS= read -r hit; do
    [[ -z "$hit" ]] && continue
    echo "STALE-PROSE $hit" >&2
    echo "            \"$phrase\" dates the skill to an unreleased branch." >&2
    echo "            State the release the feature ARRIVED in instead." >&2
    fail=$((fail + 1))
  done < <(grep -rn -F -- "$phrase" "$SKILL_DIR" | sed "s|^$SKILL_DIR/||" | cut -c1-120 || true)
done

if [[ $fail -gt 0 ]]; then
  echo "" >&2
  echo "$fail skill version problem(s) found." >&2
  echo "A marker names the release an API ARRIVED in. Resolve it from the" >&2
  echo "CHANGELOG section containing the issue's oldest mention." >&2
  exit 1
fi

echo "Skill version markers OK ($unverifiable marker(s) carry no issue reference and were not machine-checked)."
