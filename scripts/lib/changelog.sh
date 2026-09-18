#!/usr/bin/env bash
# Shared changelog-fragment reading.
#
# A release note used to go straight into `CHANGELOG.md`, at the top of the
# `## [Unreleased]` section. Every open PR wrote to those same few lines, so
# every PR conflicted with every other PR, and the conflict was never about
# the code. A note now goes into its own file under `changelog.d/`, which two
# PRs cannot both edit. `scripts/update-changelog.sh` folds the files into
# `CHANGELOG.md` at release time.
#
# The gates that read the Unreleased section — the migration-guide gate and
# the plugin-freshness gate — must see a note on the day the PR lands, not
# only at release. `changelog_view` gives them that: `CHANGELOG.md` with every
# fragment spliced into the Unreleased section. The fragments are the same
# markdown the section holds, so each gate parses the view with the parser it
# already has.
#
# This file only defines functions. Sourcing it has no side effects.
#
#   source scripts/lib/changelog.sh
#   changelog_view                # the working tree
#   changelog_view "$merge_base"  # the same view at a git revision

CHANGELOG_FILE="${CHANGELOG_FILE:-CHANGELOG.md}"
CHANGELOG_FRAGMENT_DIR="${CHANGELOG_FRAGMENT_DIR:-changelog.d}"

# Kinds a fragment may declare, in the order `update-changelog.sh` writes them
# into a release section. `Breaking Changes` is here because the
# migration-guide gate reads it; it is a section heading, not a kind an author
# picks on its own.
CHANGELOG_KINDS=(
  "Breaking Changes"
  "Added"
  "Changed"
  "Deprecated"
  "Removed"
  "Fixed"
  "Security"
  "Performance"
  "Documentation"
  "Testing"
  "Miscellaneous"
)

changelog_is_kind() {
  local candidate="$1" kind
  for kind in "${CHANGELOG_KINDS[@]}"; do
    [[ "$candidate" == "$kind" ]] && return 0
  done
  return 1
}

# Print every fragment path, sorted. `README.md` documents the directory, so
# it is not a fragment.
# Usage: changelog_fragment_paths [ref]
changelog_fragment_paths() {
  local ref="${1-}"
  if [[ -z "$ref" ]]; then
    local path
    for path in "$CHANGELOG_FRAGMENT_DIR"/*.md; do
      [[ -f "$path" ]] || continue
      [[ "$(basename "$path")" == "README.md" ]] && continue
      printf '%s\n' "$path"
    done
    return 0
  fi
  git ls-tree -r --name-only "$ref" -- "$CHANGELOG_FRAGMENT_DIR" 2>/dev/null |
    grep -E '\.md$' |
    grep -v "^$CHANGELOG_FRAGMENT_DIR/README\.md$" |
    sort || true
}

# Print one fragment, from the working tree or from a revision.
# Usage: changelog_fragment_body <path> [ref]
changelog_fragment_body() {
  local path="$1" ref="${2-}"
  if [[ -z "$ref" ]]; then
    cat "$path"
  else
    git show "$ref:$path" 2>/dev/null || true
  fi
}

# Print `CHANGELOG.md` with every fragment spliced into `## [Unreleased]`.
#
# The splice goes directly under the heading, and each fragment keeps its own
# `### <Kind>` heading. A repeated heading inside one section is valid
# markdown and every parser in this repository walks sections by `## `, so
# nothing has to merge the headings to read the view. `update-changelog.sh`
# merges them, because a published section should read as one list.
#
# A changelog with no `## [Unreleased]` heading gets one, so a fragment is
# never dropped without a word. The gates fail closed on what they cannot
# read, and an invisible note is exactly the case that rule is for.
#
# Usage: changelog_view [ref]
changelog_view() {
  local ref="${1-}" changelog="" fragments="" path body

  if [[ -z "$ref" ]]; then
    [[ -f "$CHANGELOG_FILE" ]] && changelog="$(cat "$CHANGELOG_FILE")"
  else
    changelog="$(git show "$ref:$CHANGELOG_FILE" 2>/dev/null || true)"
  fi

  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    body="$(changelog_fragment_body "$path" "$ref")"
    [[ -n "$body" ]] || continue
    fragments+="$body"$'\n\n'
  done < <(changelog_fragment_paths "$ref")

  if [[ -z "$fragments" ]]; then
    printf '%s\n' "$changelog"
    return 0
  fi

  if ! grep -qE '^##[[:space:]]+\[Unreleased\]' <<<"$changelog"; then
    printf '## [Unreleased]\n\n%s%s\n' "$fragments" "$changelog"
    return 0
  fi

  awk -v fragments="$fragments" '
    { sub(/\r$/, "") }
    /^##[[:space:]]+\[Unreleased\]/ && !spliced {
      print
      print ""
      printf "%s", fragments
      spliced = 1
      next
    }
    { print }
  ' <<<"$changelog"
}
