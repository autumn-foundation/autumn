#!/usr/bin/env bash
# Print the CHANGELOG.md section for one release, without its heading, so the
# GitHub Release body is the same curated text the packaged crates carry.
#
#     ./scripts/extract-release-notes.sh 0.8.0 > RELEASE_NOTES.md
#     ./scripts/extract-release-notes.sh v0.8.0     # a leading `v` is accepted
#
# Exits non-zero when the section is missing or empty, so a release cannot be
# published with a blank body.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

version="${1:-}"
version="${version#v}"
if [ -z "$version" ]; then
  echo "usage: $0 <version>" >&2
  exit 2
fi

notes="$(awk -v ver="$version" '
  index($0, "## [" ver "]") == 1 { p = 1; next }
  /^## \[/ { p = 0 }
  p
' "$root/CHANGELOG.md")"

if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
  echo "error: CHANGELOG.md has no entries under ## [$version]" >&2
  exit 1
fi

printf '%s\n' "$notes"
