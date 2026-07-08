#!/usr/bin/env bash
# Plugin freshness gate: keep the Claude plugin (skills/, agents/,
# .claude-plugin/) from drifting behind the framework.
#
# WHY THIS FIRED: your PR adds user-facing entries to CHANGELOG.md's
# `## [Unreleased]` `### Added`/`### Changed` sections but does not touch
# the Claude plugin. New framework surface that agents should reach for
# belongs in the plugin too (usually a row/bullet in
# skills/autumn-web/SKILL.md or references/api-reference.md).
#
# HOW TO SATISFY IT:
#   - Update the relevant plugin file in the same PR (preferred), or
#   - Exempt the change when it genuinely has no agent-facing surface:
#       * include the literal token [no-plugin] in the changelog bullet
#         (exempts that bullet only — other new bullets still need plugin
#         coverage), or
#       * include [no-plugin] in the PR body (deliberately PR-wide), or
#       * apply the `plugin-exempt` label to the PR (workflow-level check).
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Drift gate: diff against the merge base of $BASE_REF; if bullets were
#      added inside CHANGELOG.md's Unreleased Added/Changed sections and no
#      file under skills/, agents/, or .claude-plugin/ changed, fail —
#      unless an escape hatch (above) applies.
#   2. Static sanity: .claude-plugin/plugin.json parses as JSON, and every
#      docs/guide/*.md path referenced from skills/ and agents/ exists.
#
# USAGE:
#   scripts/check-plugin-freshness.sh              # gate against $BASE_REF
#                                                  #   (default origin/trunk-dev)
#   BASE_REF=origin/trunk scripts/check-plugin-freshness.sh
#   PR_BODY="..." scripts/check-plugin-freshness.sh   # PR body escape hatch
#   scripts/check-plugin-freshness.sh --static-only   # only check 2
#   scripts/check-plugin-freshness.sh --self-test     # synthetic-repo tests
#
# NOTE: --self-test requires GNU sed (uses `sed -i`); the gate itself does not.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Extract the bullets that land inside the `## [Unreleased]` section's
# `### Added` / `### Changed` subsections, given file content on stdin.
# Each multi-line bullet is joined into one whitespace-normalized logical
# line. The caller takes the set difference between base and HEAD, so pure
# moves AND pure rewraps of existing bullets don't count as additions —
# only genuinely new (or reworded) bullets do. Heading matches are tolerant
# of trailing whitespace and CRLF line endings so stray whitespace can't
# silently disable the gate.
unreleased_added_changed() {
  awk '
    function flush() {
      if (bullet != "") {
        gsub(/[[:space:]]+/, " ", bullet)
        sub(/[[:space:]]+$/, "", bullet)
        print bullet
      }
      bullet = ""
    }
    { sub(/\r$/, "") }
    /^##[[:space:]]+\[Unreleased\]/ { flush(); in_unreleased = 1; next }
    /^##[[:space:]]+\[/             { flush(); in_unreleased = 0 }
    in_unreleased && /^###[[:space:]]/ {
      flush()
      heading = $0
      sub(/^###[[:space:]]+/, "", heading)
      sub(/[[:space:]]+$/, "", heading)
      in_wanted = (heading == "Added" || heading == "Changed")
      next
    }
    in_unreleased && in_wanted {
      if ($0 ~ /^[-*][[:space:]]/) { flush(); bullet = $0 }
      else if (NF && bullet != "") { bullet = bullet " " $0 }
      else if (NF) { print }
    }
    END { flush() }
  '
}

new_changelog_lines() {
  # Compare CHANGELOG.md between two committed refs (working-tree state is
  # deliberately ignored — CI checks out the PR head commit).
  local dir="$1" base="$2" head="$3"
  local base_section head_section
  base_section="$(git -C "$dir" show "$base:CHANGELOG.md" 2>/dev/null | unreleased_added_changed || true)"
  head_section="$(git -C "$dir" show "$head:CHANGELOG.md" 2>/dev/null | unreleased_added_changed || true)"
  # Bullets in head but not in base.
  comm -13 <(printf '%s\n' "$base_section" | sort) <(printf '%s\n' "$head_section" | sort) | sed '/^$/d'
}

run_gate() {
  local dir="$1" base_ref="$2" pr_body="${3-}"

  local merge_base
  merge_base="$(git -C "$dir" merge-base "$base_ref" HEAD)" ||
    fail "cannot compute merge base against $base_ref (fetch the base branch first)"

  local changed_files new_bullets non_exempt
  changed_files="$(git -C "$dir" diff --name-only "$merge_base"...HEAD)"
  new_bullets="$(new_changelog_lines "$dir" "$merge_base" HEAD)"

  if [[ -z "$new_bullets" ]]; then
    echo "OK: no new Unreleased Added/Changed changelog entries — gate not applicable."
    return 0
  fi

  # Per-bullet escape hatch: a [no-plugin] token exempts only the bullet
  # that carries it; every other new bullet still needs plugin coverage.
  non_exempt="$(printf '%s\n' "$new_bullets" | grep -vF '[no-plugin]' || true)"
  if [[ -z "$non_exempt" ]]; then
    echo "OK: every new changelog bullet carries the [no-plugin] escape hatch."
    return 0
  fi

  if echo "$changed_files" | grep -qE '^(skills/|agents/|\.claude-plugin/)'; then
    echo "OK: changelog entries added and plugin files touched."
    return 0
  fi

  # PR-body hatch is deliberately PR-wide: one token exempts the whole PR.
  if [[ -n "$pr_body" ]] && grep -qF '[no-plugin]' <<<"$pr_body"; then
    echo "OK: PR body carries the [no-plugin] escape hatch."
    return 0
  fi

  echo "New Unreleased changelog entries without a plugin update:" >&2
  printf '%s\n' "$non_exempt" | head -20 | sed 's/^/  + /' >&2
  fail "PR adds user-facing changelog entries but touches none of skills/, agents/, .claude-plugin/.
Update the Claude plugin (see header of scripts/check-plugin-freshness.sh), or
exempt with [no-plugin] in the bullet/PR body or the plugin-exempt label."
}

run_static_checks() {
  local dir="$1"
  echo "Static check: .claude-plugin/plugin.json parses as JSON..."
  python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$dir/.claude-plugin/plugin.json" ||
    fail ".claude-plugin/plugin.json is not valid JSON"

  echo "Static check: docs/guide/*.md references from skills/ and agents/ exist..."
  local missing=0 ref
  while IFS= read -r ref; do
    if [[ ! -f "$dir/$ref" ]]; then
      echo "  missing: $ref" >&2
      missing=1
    fi
  done < <(grep -rhoE 'docs/guide/[A-Za-z0-9_.-]+\.md' "$dir/skills" "$dir/agents" | sort -u)
  [[ "$missing" -eq 0 ]] || fail "skills/ or agents/ reference docs/guide pages that do not exist"
  echo "OK: static checks passed."
}

self_test() {
  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064 -- expand now: $tmp is function-local.
  trap "rm -rf '$tmp'" EXIT
  local pass=0 total=0

  make_repo() {
    local dir="$1"
    git init -q "$dir"
    git -C "$dir" config user.email test@test && git -C "$dir" config user.name test
    mkdir -p "$dir/skills/autumn-web" "$dir/.claude-plugin" "$dir/agents" "$dir/docs/guide"
    printf '{"name": "autumn", "version": "0.5.0"}\n' > "$dir/.claude-plugin/plugin.json"
    printf '# skill\nSee docs/guide/jobs.md.\n' > "$dir/skills/autumn-web/SKILL.md"
    printf '# reviewer\n' > "$dir/agents/autumn-reviewer.md"
    printf '# jobs\n' > "$dir/docs/guide/jobs.md"
    cat > "$dir/CHANGELOG.md" <<'EOF'
# Changelog

## [Unreleased]

### Added

- **old:** an existing bullet

## [0.5.0] - 2026-06-16

### Added

- old release bullet
EOF
    git -C "$dir" add -A && git -C "$dir" commit -qm base
    git -C "$dir" branch base
  }

  check() {
    local name="$1" expected="$2"; shift 2
    total=$((total + 1))
    local got=0
    ("$@") >/dev/null 2>&1 || got=$?
    if [[ ("$expected" == pass && "$got" -eq 0) || ("$expected" == fail && "$got" -ne 0) ]]; then
      echo "self-test PASS: $name"
      pass=$((pass + 1))
    else
      echo "self-test FAIL: $name (expected $expected, exit=$got)" >&2
    fi
  }

  # Scenario 1: changelog-only Added entry, no plugin change -> gate fails.
  local r1="$tmp/r1"; make_repo "$r1"
  sed -i 's/- \*\*old:\*\* an existing bullet/- **old:** an existing bullet\n- **new:** shiny feature agents should know about/' "$r1/CHANGELOG.md"
  git -C "$r1" commit -qam "feat: changelog only"
  check "changelog-only change fails" fail run_gate "$r1" base ""

  # Scenario 2: changelog + skills change -> gate passes.
  local r2="$tmp/r2"; make_repo "$r2"
  sed -i 's/- \*\*old:\*\* an existing bullet/- **old:** an existing bullet\n- **new:** shiny feature/' "$r2/CHANGELOG.md"
  printf 'Documents the shiny feature.\n' >> "$r2/skills/autumn-web/SKILL.md"
  git -C "$r2" commit -qam "feat: changelog + plugin"
  check "changelog+skills change passes" pass run_gate "$r2" base ""

  # Scenario 3: [no-plugin] in the bullet -> gate passes.
  local r3="$tmp/r3"; make_repo "$r3"
  sed -i 's/- \*\*old:\*\* an existing bullet/- **old:** an existing bullet\n- **internal:** refactor with no agent surface [no-plugin]/' "$r3/CHANGELOG.md"
  git -C "$r3" commit -qam "feat: exempt"
  check "[no-plugin] bullet passes" pass run_gate "$r3" base ""

  # Scenario 4: [no-plugin] in the PR body -> gate passes.
  local r4="$tmp/r4"; make_repo "$r4"
  sed -i 's/- \*\*old:\*\* an existing bullet/- **old:** an existing bullet\n- **new:** something/' "$r4/CHANGELOG.md"
  git -C "$r4" commit -qam "feat: body exempt"
  check "[no-plugin] PR body passes" pass run_gate "$r4" base "internal cleanup [no-plugin]"

  # Scenario 5: no changelog change at all -> gate passes.
  local r5="$tmp/r5"; make_repo "$r5"
  printf '// code\n' > "$r5/lib.rs"
  git -C "$r5" add -A && git -C "$r5" commit -qm "chore: code only"
  check "no changelog change passes" pass run_gate "$r5" base ""

  # Scenario 6: Fixed-section-only changelog entry -> gate passes (Added/Changed only).
  local r6="$tmp/r6"; make_repo "$r6"
  awk '/^## \[0.5.0\]/ && !done { print "### Fixed\n\n- a bug fix\n"; done=1 } { print }' "$r6/CHANGELOG.md" > "$r6/CHANGELOG.md.new"
  mv "$r6/CHANGELOG.md.new" "$r6/CHANGELOG.md"
  git -C "$r6" commit -qam "fix: fixed-section only"
  check "Fixed-section-only entry passes" pass run_gate "$r6" base ""

  # Scenario 7: static checks pass on a valid repo.
  local r7="$tmp/r7"; make_repo "$r7"
  check "static checks pass on valid repo" pass run_static_checks "$r7"

  # Scenario 8: static checks fail on a broken docs/guide reference.
  local r8="$tmp/r8"; make_repo "$r8"
  printf 'See docs/guide/does-not-exist.md.\n' >> "$r8/skills/autumn-web/SKILL.md"
  check "static checks fail on missing guide ref" fail run_static_checks "$r8"

  # Scenario 9: static checks fail on invalid plugin.json.
  local r9="$tmp/r9"; make_repo "$r9"
  printf '{ not json' > "$r9/.claude-plugin/plugin.json"
  check "static checks fail on bad plugin.json" fail run_static_checks "$r9"

  # Scenario 10: [no-plugin] on one bullet does NOT exempt a second,
  # unmarked bullet -> gate fails (hatch is per-bullet, not PR-wide).
  local r10="$tmp/r10"; make_repo "$r10"
  sed -i 's/- \*\*old:\*\* an existing bullet/- **old:** an existing bullet\n- **internal:** no agent surface [no-plugin]\n- **new:** shiny feature agents should know about/' "$r10/CHANGELOG.md"
  git -C "$r10" commit -qam "feat: partial exempt"
  check "[no-plugin] bullet does not exempt other bullets" fail run_gate "$r10" base ""

  # Scenario 11: rewrapping an existing multi-line bullet (no new content)
  # -> gate passes (bullets are joined and whitespace-normalized).
  local r11="$tmp/r11"; make_repo "$r11"
  sed -i 's/- \*\*old:\*\* an existing bullet/- **old:** an existing bullet that is\n  long enough to wrap across two lines/' "$r11/CHANGELOG.md"
  git -C "$r11" commit -qam "docs: establish wrapped bullet"
  git -C "$r11" branch -f base
  sed -i -e 's/- \*\*old:\*\* an existing bullet that is/- **old:** an existing bullet\n  that is long enough to wrap/' -e 's/^  long enough to wrap across two lines/  across two lines/' "$r11/CHANGELOG.md"
  git -C "$r11" commit -qam "docs: rewrap bullet"
  check "pure rewrap of existing bullet passes" pass run_gate "$r11" base ""

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]]
}

case "${1-}" in
  --self-test)
    self_test
    ;;
  --static-only)
    run_static_checks "$root"
    ;;
  *)
    run_static_checks "$root"
    run_gate "$root" "${BASE_REF:-origin/trunk-dev}" "${PR_BODY-}"
    ;;
esac
