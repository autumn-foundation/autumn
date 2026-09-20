#!/usr/bin/env bash
# Changelog fragment gate.
#
# WHY THIS EXISTS: a release note written straight into `CHANGELOG.md` lands
# at the top of the `## [Unreleased]` section. Every open PR wrote to those
# same few lines, so every PR conflicted with every other PR, and the conflict
# was never about the code. A note now goes into its own file under
# `changelog.d/`. Two PRs never touch one file, so the conflict cannot happen.
# `scripts/update-changelog.sh` folds the files into `CHANGELOG.md` when a
# release is cut.
#
# WHY THIS FIRED, and how to satisfy it:
#
#   "PR edits CHANGELOG.md"
#       Move the note to `changelog.d/<slug>.md`. A release commit is the one
#       exception: apply the `release` label, or put the literal token
#       [changelog] in the PR body.
#
#   "fragment is malformed"
#       A fragment holds the markdown the Unreleased section holds: a
#       `### <Kind>` heading and at least one bullet under it. See
#       `changelog.d/README.md`.
#
# The shape is not decoration. The migration-guide gate and the
# plugin-freshness gate read the fragments through `changelog_view`
# (scripts/lib/changelog.sh) as if they were already in the section, so a
# fragment that does not parse is a note those gates cannot see.
#
# USAGE:
#   scripts/check-changelog-fragments.sh              # gate
#   BASE_REF=origin/trunk scripts/check-changelog-fragments.sh
#   PR_BODY="..." scripts/check-changelog-fragments.sh
#   CHANGELOG_RELEASE_LABEL=true scripts/check-changelog-fragments.sh
#   scripts/check-changelog-fragments.sh --self-test  # synthetic-repo tests
#
# NOTE: --self-test requires GNU sed; the gate itself does not.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/lib/changelog.sh
source "$root/scripts/lib/changelog.sh"

failures=0

die() {
  echo "error: $*" >&2
  failures=$((failures + 1))
}

ok() {
  echo "ok:    $*"
}

# Check 1: every fragment parses as a changelog block.
#
# The gate fails closed. A fragment it cannot read is a note that reaches no
# reader and no gate, which is the failure this directory exists to prevent.
check_fragments() {
  local dir="$1" count=0 path name first_heading

  pushd "$dir" >/dev/null

  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    count=$((count + 1))
    name="$(basename "$path")"

    if [[ ! "$name" =~ ^[a-z0-9][a-z0-9._-]*\.md$ ]]; then
      die "$path: name must be lowercase letters, digits, '.', '-' or '_' and end in .md.
Name it after the change: changelog.d/1837-money-ledger.md"
    fi

    if grep -qE '^##[[:space:]]' "$path"; then
      die "$path: a fragment must not carry a '## ' heading.
A '## ' heading starts a new release section and would cut the Unreleased
section in half. Use a '### <Kind>' heading."
      continue
    fi

    first_heading="$(grep -m1 -E '^###[[:space:]]+' "$path" || true)"
    if [[ -z "$first_heading" ]]; then
      die "$path: no '### <Kind>' heading.
Open the fragment with the kind of change it records, for example '### Added'."
      continue
    fi

    first_heading="${first_heading#\#\#\# }"
    first_heading="$(sed -e 's/[[:space:]]*$//' <<<"$first_heading")"
    if ! changelog_is_kind "$first_heading"; then
      die "$path: '### $first_heading' is not a changelog kind.
Use one of: ${CHANGELOG_KINDS[*]}"
      continue
    fi

    if ! grep -qE '^[-*][[:space:]]' "$path"; then
      die "$path: no bullet under '### $first_heading'.
A fragment records at least one entry, written as a '- ' bullet."
    fi
  done < <(changelog_fragment_paths)

  popd >/dev/null

  [[ "$failures" -eq 0 ]] && ok "$count changelog fragment(s) parse"
  return 0
}

# Check 2: the PR does not edit CHANGELOG.md.
#
# This is the check that removes the conflicts. Without it the directory is a
# suggestion, and one PR writing to the section brings back the shared lines
# for every PR open beside it.
check_changelog_untouched() {
  local dir="$1" base_ref="$2" pr_body="${3-}" release_label="${4-false}"
  local merge_base changed

  if [[ "$release_label" == "true" ]]; then
    ok "release label — CHANGELOG.md may be edited"
    return 0
  fi

  if [[ -n "$pr_body" ]] && grep -qF '[changelog]' <<<"$pr_body"; then
    ok "PR body carries the [changelog] escape hatch"
    return 0
  fi

  if ! merge_base="$(git -C "$dir" merge-base "$base_ref" HEAD 2>/dev/null)"; then
    echo "skip:  no merge base against $base_ref — skipping the CHANGELOG.md edit check"
    return 0
  fi

  changed="$(git -C "$dir" diff --name-only "$merge_base"...HEAD -- "$CHANGELOG_FILE")"
  if [[ -z "$changed" ]]; then
    ok "$CHANGELOG_FILE untouched by this change"
    return 0
  fi

  die "this change edits $CHANGELOG_FILE.
Every open PR shares the top of the '## [Unreleased]' section, so a note
written there conflicts with every other PR. Put the note in its own file:

    changelog.d/<slug>.md

with the same markdown you would have written in the section. See
changelog.d/README.md. A release commit is exempt: apply the 'release' label,
or put [changelog] in the PR body."
}

run_gate() {
  local dir="$1" base_ref="$2" pr_body="${3-}" release_label="${4-false}"
  failures=0
  check_fragments "$dir"
  check_changelog_untouched "$dir" "$base_ref" "$pr_body" "$release_label"
  [[ "$failures" -eq 0 ]]
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
    mkdir -p "$dir/changelog.d"
    printf '# Fragments\n' > "$dir/changelog.d/README.md"
    cat > "$dir/CHANGELOG.md" <<'EOF'
# Changelog

## [Unreleased]

### Added

- an existing bullet

## [0.7.0] - 2026-08-23

### Added

- a released bullet
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

  # Scenario 1: a well-formed fragment, CHANGELOG.md untouched -> passes.
  local r1="$tmp/r1"; make_repo "$r1"
  printf '### Added\n\n- **jobs:** a new thing (issue #1)\n' > "$r1/changelog.d/1-jobs.md"
  git -C "$r1" add -A && git -C "$r1" commit -qm "add fragment"
  check "well-formed fragment passes" pass run_gate "$r1" base "" false

  # Scenario 2: the note went into CHANGELOG.md -> fails.
  local r2="$tmp/r2"; make_repo "$r2"
  sed -i 's/- an existing bullet/- an existing bullet\n- a new bullet/' "$r2/CHANGELOG.md"
  git -C "$r2" commit -qam "edit changelog"
  check "CHANGELOG.md edit fails" fail run_gate "$r2" base "" false

  # Scenario 3: the release label exempts the CHANGELOG.md edit.
  local r3="$tmp/r3"; make_repo "$r3"
  sed -i 's/- an existing bullet/- an existing bullet\n- a new bullet/' "$r3/CHANGELOG.md"
  git -C "$r3" commit -qam "cut release"
  check "release label exempts the edit" pass run_gate "$r3" base "" true

  # Scenario 4: [changelog] in the PR body exempts the edit.
  local r4="$tmp/r4"; make_repo "$r4"
  sed -i 's/- an existing bullet/- an existing bullet\n- a new bullet/' "$r4/CHANGELOG.md"
  git -C "$r4" commit -qam "cut release"
  check "[changelog] PR body exempts the edit" pass run_gate "$r4" base "release prep [changelog]" false

  # Scenario 5: a fragment with no kind heading -> fails.
  local r5="$tmp/r5"; make_repo "$r5"
  printf -- '- a bullet with no heading\n' > "$r5/changelog.d/5-loose.md"
  git -C "$r5" add -A && git -C "$r5" commit -qm "loose fragment"
  check "fragment without a kind heading fails" fail run_gate "$r5" base "" false

  # Scenario 6: a fragment naming a kind that is not one -> fails.
  local r6="$tmp/r6"; make_repo "$r6"
  printf '### Sundries\n\n- a bullet\n' > "$r6/changelog.d/6-sundries.md"
  git -C "$r6" add -A && git -C "$r6" commit -qm "unknown kind"
  check "unknown kind fails" fail run_gate "$r6" base "" false

  # Scenario 7: a fragment with a '## ' heading -> fails. It would cut the
  # Unreleased section in half once spliced.
  local r7="$tmp/r7"; make_repo "$r7"
  printf '## [0.8.0]\n\n### Added\n\n- a bullet\n' > "$r7/changelog.d/7-section.md"
  git -C "$r7" add -A && git -C "$r7" commit -qm "section heading"
  check "'## ' heading in a fragment fails" fail run_gate "$r7" base "" false

  # Scenario 8: a heading with no bullet under it -> fails.
  local r8="$tmp/r8"; make_repo "$r8"
  printf '### Added\n\nprose with no bullet\n' > "$r8/changelog.d/8-prose.md"
  git -C "$r8" add -A && git -C "$r8" commit -qm "no bullet"
  check "fragment without a bullet fails" fail run_gate "$r8" base "" false

  # Scenario 9: an upper-case file name -> fails. Names sort into the release
  # section, and a case-insensitive file system would sort them differently.
  local r9="$tmp/r9"; make_repo "$r9"
  printf '### Added\n\n- a bullet\n' > "$r9/changelog.d/Nine.md"
  git -C "$r9" add -A && git -C "$r9" commit -qm "upper case name"
  check "upper-case fragment name fails" fail run_gate "$r9" base "" false

  # Scenario 10: README.md is documentation, not a fragment -> passes.
  local r10="$tmp/r10"; make_repo "$r10"
  printf '# Fragments\n\nHow to write one.\n' > "$r10/changelog.d/README.md"
  git -C "$r10" commit -qam "document the directory"
  check "README.md is not read as a fragment" pass run_gate "$r10" base "" false

  # Scenario 11: the spliced view carries the fragment inside the Unreleased
  # section, which is what the migration-guide and plugin-freshness gates read.
  local r11="$tmp/r11"; make_repo "$r11"
  printf '### Added\n\n- **money:** a spliced bullet\n' > "$r11/changelog.d/11-money.md"
  git -C "$r11" add -A && git -C "$r11" commit -qm "fragment"
  view_has_bullet_in_unreleased() {
    local dir="$1"
    ( cd "$dir" && changelog_view | awk '
        /^##[[:space:]]+\[Unreleased\]/ { in_section = 1; next }
        /^##[[:space:]]+\[/            { in_section = 0 }
        in_section && /a spliced bullet/ { found = 1 }
        END { exit(found ? 0 : 1) }
      ' )
  }
  check "fragment splices into the Unreleased section" pass view_has_bullet_in_unreleased "$r11"

  # Scenario 12: the same view at a revision, which is how a gate compares a
  # PR against its merge base.
  check "the view reads fragments from a revision" pass bash -c "
    cd '$r11' && source '$root/scripts/lib/changelog.sh' &&
    changelog_view HEAD | grep -q 'a spliced bullet'"

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]]
}

case "${1-}" in
  --self-test)
    self_test
    ;;
  *)
    run_gate "$root" "${BASE_REF:-origin/trunk-dev}" "${PR_BODY-}" \
      "${CHANGELOG_RELEASE_LABEL:-false}"
    ;;
esac
