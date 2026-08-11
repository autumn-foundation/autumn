#!/usr/bin/env bash
# Validate that the workspace version, CHANGELOG.md, and release tag all agree
# on whether the release is patch, minor, or breaking before the gate passes.
#
# Checks performed:
#   1. The workspace version in Cargo.toml matches the release tag (if set).
#   2. CHANGELOG.md has an entry for the current workspace version.
#   3. If the release is a breaking change (MAJOR or pre-1.0 minor bump), a
#      matching migration guide stub exists under docs/migrations/.
#   4. If the release is NOT a breaking change, CHANGELOG.md must not contain
#      "Breaking" entries under the current version heading.
#
# The RELEASE_TAG env var is set automatically in CI (github.ref_name).
# When run locally without a tag, steps that require a tag are skipped.
#
# Called from the `publish-gate` workflow. Run locally with:
#
#     ./scripts/check-release-notes.sh
#     RELEASE_TAG=v0.4.0 ./scripts/check-release-notes.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() {
  echo "error: $*" >&2
  exit 1
}

ok() {
  echo "ok:    $*"
}

changelog_entry_for_version() {
  local version="$1"
  awk -v ver="$version" \
    '$0 ~ "^## \\[" ver "\\]" { p=1; next } /^## \[/ { p=0 } p' \
    CHANGELOG.md
}

changelog_has_breaking_section() {
  local version="$1"
  # Two ways to declare a break, and they must match the convention that
  # scripts/check-migration-guides.sh enforces (docs/migrations/README.md):
  # a `### Breaking Changes` heading, or the inline `**Breaking:**` token.
  # Heading-only detection is why this check never fired: no release section
  # in this changelog has ever carried one.
  #
  # Keep this in one awk process. `awk | grep -q` is flaky under `pipefail`:
  # grep exits after the first match, awk can receive SIGPIPE while writing the
  # rest of a long changelog entry, and the pipeline reports a false negative.
  awk -v ver="$version" '
    $0 ~ "^## \\[" ver "\\]" { in_version = 1; next }
    /^## \[/                 { in_version = 0 }
    in_version {
      if (tolower($0) ~ /^[[:space:]]*###[[:space:]]+breaking([[:space:]]|$)/) {
        found = 1
      }
      if ($0 ~ /\*\*Breaking(:\*\*|\*\*:)/) {
        found = 1
      }
    }
    END { exit found ? 0 : 1 }
  ' CHANGELOG.md
}

# Workspace version (canonical version for all published crates).
workspace_version="$(
  awk '
    /^\[workspace\.package\]/ { in_block = 1; next }
    /^\[/ && in_block         { in_block = 0 }
    in_block && /^[[:space:]]*version/    {
      match($0, /"[^"]+"/)
      print substr($0, RSTART + 1, RLENGTH - 2)
      exit
    }
  ' Cargo.toml
)"
[[ -n "$workspace_version" ]] || die "could not parse workspace version from Cargo.toml"
echo "workspace version = $workspace_version"

# --- Check 1: tag matches workspace version (CI only) ---
release_tag="${RELEASE_TAG:-}"
if [[ -n "$release_tag" ]]; then
  tag_version="${release_tag#v}"  # strip leading 'v'
  if [[ "$tag_version" != "$workspace_version" ]]; then
    die "release tag '$release_tag' implies version '$tag_version' but Cargo.toml declares '$workspace_version'. Update the workspace version before tagging."
  fi
  ok "release tag $release_tag matches workspace version $workspace_version"
else
  echo "skip:  no RELEASE_TAG set — skipping tag/version alignment check"
fi

# --- Check 2: CHANGELOG.md has an entry for the current version ---
if grep -q "## \[$workspace_version\]" CHANGELOG.md; then
  ok "CHANGELOG.md has entry for [$workspace_version]"
else
  die "CHANGELOG.md is missing an entry for version [$workspace_version]. Add a changelog section before tagging."
fi

# --- Check 3: breaking releases need a migration guide stub ---
# Pre-1.0: every 0.x → 0.(x+1) bump is breaking (minor component changes).
# Post-1.0: only MAJOR changes are breaking.
major="$(echo "$workspace_version" | cut -d. -f1)"
minor="$(echo "$workspace_version" | cut -d. -f2)"

is_breaking=false
if [[ "$major" -eq 0 ]]; then
  # Pre-1.0: any bump of the minor component is potentially breaking.
  # Heuristic: if CHANGELOG has "Breaking" under this version, require a guide.
  if changelog_has_breaking_section "$workspace_version"; then
    is_breaking=true
  fi
else
  # Post-1.0: MAJOR bump is breaking.
  # Compare against previous tag if available.
  prev_tag="$(git describe --tags --abbrev=0 HEAD^ 2>/dev/null || true)"
  if [[ -n "$prev_tag" ]]; then
    prev_major="$(echo "${prev_tag#v}" | cut -d. -f1)"
    if [[ "$major" -gt "$prev_major" ]]; then
      is_breaking=true
    fi
  fi
fi

if $is_breaking; then
  migration_file="docs/migrations/${workspace_version}.md"
  if [[ ! -f "$migration_file" ]]; then
    die "breaking release detected but no migration guide found at $migration_file. Create the stub before tagging."
  fi
  ok "migration guide exists: $migration_file"
else
  ok "non-breaking release — no migration guide required"
fi

# --- Check 4: non-breaking releases must not contain unacknowledged breaking notes ---
if ! $is_breaking; then
  # Look for a "Breaking" section under the current version in CHANGELOG.
  if changelog_has_breaking_section "$workspace_version"; then
    die "CHANGELOG.md has a 'Breaking' section under [$workspace_version] but no migration guide was found. Either remove the breaking changes, bump the version to signal a major release, or add a migration guide at docs/migrations/${workspace_version}.md."
  fi
  ok "no undeclared breaking changes in CHANGELOG.md"
fi

echo ""
echo "Release notes alignment OK."
