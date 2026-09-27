#!/usr/bin/env bash
# Fold `changelog.d/` fragments into `CHANGELOG.md`.
#
# A PR writes its release note as its own file, so two PRs never edit the same
# lines (see `changelog.d/README.md`). This is the step that turns those files
# back into one section: it merges every fragment into `## [Unreleased]` under
# the kind each one declares, then deletes the fragments. Run it when you cut
# a release, before you date the section and tag.
#
# The order is stable and reviewable: fragments are read in file-name order,
# and a fragment's bullets go to the top of their kind, which is where this
# changelog puts the newest entry.
#
# USAGE:
#   scripts/update-changelog.sh            # fold the fragments in
#   scripts/update-changelog.sh --dry-run  # print the result, change nothing
#   scripts/update-changelog.sh --self-test
#
# This script does not bump the version, date the section, or tag. Those are
# release steps with their own gates — see docs/release-checklist.md.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/lib/changelog.sh
source "$root/scripts/lib/changelog.sh"

fold() {
  local dir="$1" dry_run="$2"
  local -a fragments
  mapfile -t fragments < <(cd "$dir" && changelog_fragment_paths)

  if [[ "${#fragments[@]}" -eq 0 ]]; then
    echo "ok:    no fragments to fold — CHANGELOG.md is already current"
    return 0
  fi

  python3 - "$dir" "$dry_run" "${fragments[@]}" <<'PYEOF'
import os, re, sys

root, dry_run = sys.argv[1], sys.argv[2] == "true"
fragment_paths = sys.argv[3:]

KINDS = [
    "Breaking Changes", "Added", "Changed", "Deprecated", "Removed", "Fixed",
    "Security", "Performance", "Documentation", "Testing", "Miscellaneous",
]

changelog_path = os.path.join(root, "CHANGELOG.md")
with open(changelog_path, encoding="utf-8") as handle:
    lines = handle.read().split("\n")

SECTION = re.compile(r"^##\s+\[")
UNRELEASED = re.compile(r"^##\s+\[Unreleased\]")
KIND = re.compile(r"^###\s+(.+?)\s*$")

start = next((i for i, line in enumerate(lines) if UNRELEASED.match(line)), None)
if start is None:
    sys.exit("error: CHANGELOG.md has no '## [Unreleased]' section")
end = next((i for i in range(start + 1, len(lines)) if SECTION.match(lines[i])),
           len(lines))

# Split the section into a preamble and one block per kind, keeping the order
# the file already uses. A kind the file does not carry is added in KINDS
# order, so a fresh section reads the same as an old one.
preamble, blocks, order, current = [], {}, [], None
for line in lines[start + 1:end]:
    heading = KIND.match(line)
    if heading:
        current = heading.group(1)
        if current not in blocks:
            blocks[current] = []
            order.append(current)
        continue
    (blocks[current] if current else preamble).append(line)


def strip(block):
    while block and not block[0].strip():
        block.pop(0)
    while block and not block[-1].strip():
        block.pop()
    return block


# Read each fragment into the same shape, then push its bullets to the top of
# their kind: this changelog reads newest first.
for path in fragment_paths:
    with open(os.path.join(root, path), encoding="utf-8") as handle:
        fragment = handle.read().split("\n")
    kind, body = None, {}
    for line in fragment:
        heading = KIND.match(line)
        if heading:
            kind = heading.group(1)
            body.setdefault(kind, [])
            continue
        if kind:
            body[kind].append(line)
    for kind, entries in body.items():
        entries = strip(entries)
        if not entries:
            continue
        if kind not in blocks:
            blocks[kind] = []
            order.append(kind)
        blocks[kind] = entries + ([""] if blocks[kind] else []) + blocks[kind]

order.sort(key=lambda kind: (KINDS.index(kind) if kind in KINDS else len(KINDS),
                             kind))

section = ["## [Unreleased]"]
if strip(preamble):
    section += [""] + strip(preamble)
for kind in order:
    block = strip(blocks[kind])
    if not block:
        continue
    section += ["", f"### {kind}", ""] + block
section += [""]

rewritten = lines[:start] + section + lines[end:]
output = "\n".join(rewritten)

if dry_run:
    sys.stdout.write(output)
    sys.exit(0)

with open(changelog_path, "w", encoding="utf-8") as handle:
    handle.write(output)
print(f"ok:    folded {len(fragment_paths)} fragment(s) into CHANGELOG.md")
PYEOF

  if [[ "$dry_run" == "true" ]]; then
    return 0
  fi

  local path
  for path in "${fragments[@]}"; do
    if git -C "$dir" ls-files --error-unmatch "$path" >/dev/null 2>&1; then
      git -C "$dir" rm -q "$path"
    else
      rm -f "$dir/$path"
    fi
  done
  echo "ok:    removed ${#fragments[@]} folded fragment(s)"
}

self_test() {
  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064 -- expand now: $tmp is function-local.
  trap "rm -rf '$tmp'" EXIT
  local pass=0 total=0

  check() {
    local name="$1" condition="$2"
    total=$((total + 1))
    if [[ "$condition" == "true" ]]; then
      echo "self-test PASS: $name"
      pass=$((pass + 1))
    else
      echo "self-test FAIL: $name" >&2
    fi
  }

  local repo="$tmp/repo"
  git init -q "$repo"
  git -C "$repo" config user.email test@test && git -C "$repo" config user.name test
  mkdir -p "$repo/changelog.d"
  cat > "$repo/CHANGELOG.md" <<'EOF'
# Changelog

## [Unreleased]

### Added

- an existing bullet

## [0.7.0] - 2026-08-23

### Added

- a released bullet
EOF
  printf '### Added\n\n- **money:** a typed ledger (issue #1837)\n' \
    > "$repo/changelog.d/1837-money.md"
  printf '### Fixed\n\n- **pdf:** a layout cap (issue #2810)\n' \
    > "$repo/changelog.d/2810-pdf.md"
  git -C "$repo" add -A && git -C "$repo" commit -qm base

  fold "$repo" false >/dev/null

  local folded
  folded="$(cat "$repo/CHANGELOG.md")"

  check "the new bullet is in the section" \
    "$(grep -qF 'a typed ledger' <<<"$folded" && echo true || echo false)"
  check "the existing bullet survives" \
    "$(grep -qF 'an existing bullet' <<<"$folded" && echo true || echo false)"
  check "the released section survives" \
    "$(grep -qF 'a released bullet' <<<"$folded" && echo true || echo false)"
  check "a new kind gets its own heading" \
    "$(grep -qE '^### Fixed$' <<<"$folded" && echo true || echo false)"
  check "the folded kind is not duplicated" \
    "$([[ "$(awk '/^## \[0.7.0\]/ {exit} /^### Added$/ {n++} END {print n+0}' \
        <<<"$folded")" == "1" ]] && echo true || echo false)"
  check "the newest bullet leads its kind" \
    "$([[ "$(awk '/^### Added$/ {found=1; next} found && /^- / {print; exit}' \
        <<<"$folded")" == *'a typed ledger'* ]] && echo true || echo false)"
  check "the fragments are gone" \
    "$([[ ! -f "$repo/changelog.d/1837-money.md" ]] && echo true || echo false)"
  check "the removal is staged" \
    "$(git -C "$repo" diff --cached --name-only | grep -q '1837-money' \
        && echo true || echo false)"

  # A second run has nothing to do, and must not rewrite the file.
  local before after
  before="$(cat "$repo/CHANGELOG.md")"
  fold "$repo" false >/dev/null
  after="$(cat "$repo/CHANGELOG.md")"
  check "a second run changes nothing" \
    "$([[ "$before" == "$after" ]] && echo true || echo false)"

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]]
}

case "${1-}" in
  --self-test) self_test ;;
  --dry-run)   fold "$root" true ;;
  "")          fold "$root" false ;;
  *)
    echo "usage: scripts/update-changelog.sh [--dry-run|--self-test]" >&2
    exit 2
    ;;
esac
