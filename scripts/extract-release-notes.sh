#!/usr/bin/env bash
# Print the CHANGELOG.md section for one release, without its heading, so the
# GitHub Release body is the same curated text the packaged crates carry.
#
#     ./scripts/extract-release-notes.sh 0.8.0 > RELEASE_NOTES.md
#     ./scripts/extract-release-notes.sh v0.8.0     # a leading `v` is accepted
#
# Exits non-zero when the section is missing or empty, so a release cannot be
# published with a blank body.
#
# A GitHub Release body is capped (the release action truncates at 124,999
# characters, silently). A section longer than MAX_NOTES_CHARS is cut at a line
# boundary and ends with a link to the complete section in CHANGELOG.md at the
# tag, so the page says it is partial instead of stopping mid-bullet.

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

# The changelog links its guides by repository path (`docs/migrations/0.8.0.md`),
# which resolves under /releases/tag/ on the release page and 404s. Point every
# repository-relative link at the file as of the tag; anchors, absolute URLs
# and `#fragment` links are left alone.
repo_url="https://github.com/autumn-foundation/autumn/blob/v${version}"
notes="$(printf '%s\n' "$notes" | awk -v base="$repo_url" '
  {
    line = $0; out = ""
    while (match(line, /\]\([^)]*\)/)) {
      pre = substr(line, 1, RSTART - 1)
      target = substr(line, RSTART + 2, RLENGTH - 3)
      line = substr(line, RSTART + RLENGTH)
      if (target !~ /^([A-Za-z][A-Za-z0-9+.-]*:|#|\/)/) {
        sub(/^\.\//, "", target)
        target = base "/" target
      }
      out = out pre "](" target ")"
    }
    print out line
  }
')"

max="${MAX_NOTES_CHARS:-120000}"
if [ "${#notes}" -gt "$max" ]; then
  link="https://github.com/autumn-foundation/autumn/blob/v${version}/CHANGELOG.md"
  notes="$(printf '%s\n' "$notes" | awk -v max="$max" '
    # Keep reading after the cut: exiting early would SIGPIPE the printf and
    # fail the pipeline under `set -o pipefail`.
    stop { next }
    used + length($0) + 1 > max { stop = 1; next }
    { print; used += length($0) + 1 }
  ')"
  # Drop a trailing partial bullet group back to the last blank line so the cut
  # does not land inside a sentence.
  notes="$(printf '%s\n' "$notes" | awk '{ lines[NR] = $0 } END {
    last = NR; while (last > 0 && lines[last] != "") last--
    if (last == 0) last = NR
    for (i = 1; i <= last; i++) print lines[i]
  }')"
  notes="${notes}

---

_These notes are cut to fit GitHub's release length limit. The complete notes for ${version} are in [CHANGELOG.md](${link})._"
fi

printf '%s\n' "$notes"
