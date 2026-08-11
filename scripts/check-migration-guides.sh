#!/usr/bin/env bash
# Migration-guide coverage gate (issue #1588).
#
# Autumn ships every 2-4 weeks and, pre-1.0, most releases can break existing
# apps. `docs/migrations/<version>.md` is the documented upgrade path, but
# nothing forced a guide to exist: the only automated check keyed off a
# `### Breaking` CHANGELOG heading this repo has never written, so it never
# fired -- 0.6.0 shipped the `with_pool` -> `with_pool_untracked` rename with
# no mention in any guide. This script makes the guide a release *gate*.
#
# It enforces four things over CHANGELOG.md:
#
#   1. Marker convention. A breaking entry is declared either with the inline
#      token `**Breaking:**` or by sitting under a `### Breaking Changes`
#      heading. Nothing else counts.
#   2. Unmarked-break lint. An entry that talks about *breaking* something
#      without the marker fails, unless the wording is explicitly negated
#      ("non-breaking", "no breaking change", "without breaking ..."). Without
#      this, an author can strand users by writing prose the coverage check
#      cannot see -- which is exactly how 0.6.0's rename slipped through.
#   3. Coverage. A section with at least one breaking entry must have a guide:
#      `docs/migrations/<version>.md` for a release, `docs/migrations/next.md`
#      (the rolling draft, renamed at release time) for `## [Unreleased]`.
#   4. Linkage. Every breaking entry must link its own guide, so a reader
#      lands on the fix path straight from the changelog line.
#
# It also checks every guide in `docs/migrations/` for the TEMPLATE.md shape
# (what breaks, how to verify, the recorded guide-only upgrade walk-through)
# and for an entry in the `docs/migrations/README.md` index, so a stub that
# only exists to satisfy check 3 does not pass.
#
# Releases before $MIGRATION_GUIDE_FLOOR are out of scope (issue #1588
# explicitly excludes backfilling guides earlier than 0.4.0).
#
# Residual risk, stated plainly: the lint is textual. An author who describes
# a break without using the word "breaking" and without the marker still gets
# through. Reviewers remain the backstop; this gate removes the *silent*
# failure mode, it does not replace judgement.
#
# Usage:
#   scripts/check-migration-guides.sh          # gate (exit 1 on any finding)
#   scripts/check-migration-guides.sh --list   # inventory, no enforcement

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

# Earliest release this gate demands a guide for.
MIGRATION_GUIDE_FLOOR="${MIGRATION_GUIDE_FLOOR:-0.4.0}"
CHANGELOG="${CHANGELOG:-CHANGELOG.md}"
MIGRATIONS_DIR="${MIGRATIONS_DIR:-docs/migrations}"
# Rolling draft for `## [Unreleased]`; renamed to <version>.md at release time.
UNRELEASED_GUIDE="$MIGRATIONS_DIR/next.md"

mode="gate"
case "${1:-}" in
  --list) mode="list" ;;
  "") ;;
  *)
    echo "usage: scripts/check-migration-guides.sh [--list]" >&2
    exit 2
    ;;
esac

failures=0

die() {
  echo "error: $*" >&2
  failures=$((failures + 1))
}

ok() {
  echo "ok:    $*"
}

[[ -f "$CHANGELOG" ]] || {
  echo "error: $CHANGELOG not found (run from the repository root)" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Parse CHANGELOG.md into machine-readable findings.
#
# Deliberately a single awk process. `awk | grep -q` is flaky under `pipefail`:
# grep exits after the first match, awk takes SIGPIPE while still writing a
# long entry, and the pipeline reports a false negative. That bug already bit
# scripts/check-release-notes.sh once; do not reintroduce it here.
#
# Emitted records (tab separated):
#   SECTION   <name> <guide-path> <in-scope 0|1>
#   BREAKING  <name> <line> <links-its-guide 0|1> <guide-path>
#   UNMARKED  <name> <line> <excerpt>
#
# Every loop below is fed by a here-string, not a pipe: a piped `while read`
# runs in a subshell and the `failures` counter it increments is discarded.
# ---------------------------------------------------------------------------
findings="$(
  awk -v floor="$MIGRATION_GUIDE_FLOOR" \
      -v migrations_dir="$MIGRATIONS_DIR" \
      -v unreleased_guide="$UNRELEASED_GUIDE" '
  function version_ge(a, b,   pa, pb, i, na, nb) {
    na = split(a, pa, ".")
    nb = split(b, pb, ".")
    for (i = 1; i <= 3; i++) {
      if ((i <= na ? pa[i] + 0 : 0) > (i <= nb ? pb[i] + 0 : 0)) return 1
      if ((i <= na ? pa[i] + 0 : 0) < (i <= nb ? pb[i] + 0 : 0)) return 0
    }
    return 1
  }

  function excerpt(text,   flat) {
    flat = text
    gsub(/[[:space:]]+/, " ", flat)
    return (length(flat) > 90) ? substr(flat, 1, 90) "..." : flat
  }

  function flush_entry(   lower, marked, has_link, i, prose) {
    if (entry == "") return
    if (!in_scope) { entry = ""; return }

    # Explicit, greppable escape hatch for entries that talk *about* breaking
    # changes (release tooling, policy docs) rather than being one. Reviewable
    # like an #[allow]: visible in the diff, and it names its reason.
    if (entry ~ /<!--[[:space:]]*migration-guide-gate:/) { entry = ""; return }

    # A `**Breaking:**` token inside a code span is a mention, not a
    # declaration — an entry documenting the convention must not declare
    # itself breaking. Strip code spans before reading the marker.
    prose = entry
    gsub(/`[^`]*`/, " ", prose)

    # A break is declared by the `**Breaking:**` token or by a
    # `### Breaking Changes` heading above the entry. Nothing else counts.
    marked = (prose ~ /\*\*Breaking(:\*\*|\*\*:)/) || entry_breaking_heading

    if (marked) {
      has_link = (index(entry, guide_path) > 0)
      printf "BREAKING\t%s\t%d\t%d\t%s\n", section, entry_line, has_link, guide_path
    } else {
      # Fold to alpha-only words so "non-breaking" and "non breaking" are the
      # same token and word boundaries need no \b (mawk has none).
      lower = tolower(prose)
      gsub(/[^a-z]+/, " ", lower)
      lower = " " lower " "
      # Negated wordings are the overwhelming majority in this changelog.
      # Repeat: a gsub eats the shared separator between adjacent matches.
      for (i = 0; i < 3; i++) {
        gsub(/ (non breaking|no [a-z]+ breaking|no breaking|not breaking|not a breaking|without breaking|rather than a breaking|nothing breaking|never breaking|avoids? breaking|avoiding breaking|prevents? breaking|preventing breaking) /, " ", lower)
      }
      if (lower ~ / breaking /) {
        printf "UNMARKED\t%s\t%d\t%s\n", section, entry_line, excerpt(entry)
      }
    }
    entry = ""
  }

  # Fenced code blocks hold TOML/YAML samples whose lines start with "- ".
  # They are part of the entry that opened them, never entries themselves.
  /^[[:space:]]*```/ { if (entry != "") entry = entry " " $0; in_fence = !in_fence; next }
  in_fence          { if (entry != "") entry = entry " " $0; next }

  /^## / {
    flush_entry()
    breaking_heading = 0
    if ($0 ~ /^## \[Unreleased\]/) {
      section = "Unreleased"
      guide_path = unreleased_guide
      in_scope = 1
    } else if (match($0, /\[[0-9]+\.[0-9]+\.[0-9]+\]/)) {
      section = substr($0, RSTART + 1, RLENGTH - 2)
      guide_path = migrations_dir "/" section ".md"
      in_scope = version_ge(section, floor)
    } else {
      section = ""
      guide_path = ""
      in_scope = 0
    }
    if (section != "") printf "SECTION\t%s\t%s\t%d\n", section, guide_path, in_scope
    next
  }

  /^### / {
    flush_entry()
    breaking_heading = (tolower($0) ~ /^###[[:space:]]+breaking/)
    next
  }

  /^- / {
    flush_entry()
    entry_line = FNR
    entry_breaking_heading = breaking_heading
    entry = $0
    next
  }

  { if (entry != "") entry = entry " " $0 }

  END { flush_entry() }
  ' "$CHANGELOG"
)"

# How many breaking entries a given changelog section declared.
breaking_count_for() {
  awk -F'\t' -v s="$1" '$1 == "BREAKING" && $2 == s { n++ } END { print n + 0 }' \
    <<<"$findings"
}

# ---------------------------------------------------------------------------
# --list: inventory for the release operator, no enforcement.
# ---------------------------------------------------------------------------
if [[ "$mode" == "list" ]]; then
  printf '%-14s %-32s %-9s %s\n' SECTION GUIDE IN-SCOPE BREAKING-ENTRIES
  while IFS=$'\t' read -r kind name guide scope; do
    [[ "$kind" == "SECTION" ]] || continue
    if [[ "$scope" == "1" ]]; then status="yes"; else status="no"; fi
    printf '%-14s %-32s %-9s %s\n' \
      "$name" "$guide" "$status" "$(breaking_count_for "$name")"
  done <<<"$findings"
  exit 0
fi

# ---------------------------------------------------------------------------
# Check 1: no unmarked breaking prose.
# ---------------------------------------------------------------------------
unmarked=0
while IFS=$'\t' read -r kind name line text; do
  [[ "$kind" == "UNMARKED" ]] || continue
  unmarked=$((unmarked + 1))
  die "$CHANGELOG:$line: [$name] unmarked breaking change.
       $text
       Declare it so the coverage check can see it:
         - **area:** **Breaking:** <what breaks and the fix>. See the
           [migration guide](<guide for this section>).
       If the change is NOT breaking, say so explicitly (\"non-breaking\",
       \"no breaking change\") and this line passes. For an entry that talks
       *about* breaking changes without being one, append the explicit
       suppression: <!-- migration-guide-gate: <reason> -->"
done <<<"$findings"
[[ "$unmarked" -eq 0 ]] && ok "no unmarked breaking changes in $CHANGELOG"

# ---------------------------------------------------------------------------
# Check 2: a section that declares a break has a guide to point at.
# ---------------------------------------------------------------------------
while IFS=$'\t' read -r kind name guide scope; do
  [[ "$kind" == "SECTION" && "$scope" == "1" ]] || continue

  count="$(breaking_count_for "$name")"
  [[ "$count" -gt 0 ]] || continue

  if [[ -f "$guide" ]]; then
    if [[ "$count" -eq 1 ]]; then noun="entry"; else noun="entries"; fi
    ok "[$name] $count breaking $noun -> $guide"
  else
    die "[$name] declares $count breaking change(s) but there is no migration guide at $guide.
       Copy $MIGRATIONS_DIR/TEMPLATE.md to $guide and fill it in — a release
       without an upgrade path is treated as a broken build (issue #1588)."
  fi
done <<<"$findings"

# ---------------------------------------------------------------------------
# Check 3: every breaking entry links its own guide, so a reader lands on the
# fix path straight from the changelog line.
# ---------------------------------------------------------------------------
while IFS=$'\t' read -r kind name line linked guide; do
  [[ "$kind" == "BREAKING" && "$linked" == "0" ]] || continue
  die "$CHANGELOG:$line: [$name] breaking entry does not link its migration guide.
       Append: See the [migration guide]($guide)."
done <<<"$findings"

# ---------------------------------------------------------------------------
# Check 4: every guide has the TEMPLATE.md shape and is indexed.
# ---------------------------------------------------------------------------
required_sections=(
  "## At a glance"
  "## Summary"
  "## Before you start"
  "## Breaking changes"
  "## How to verify"
  "### Guide-only upgrade walkthrough"
)

index_file="$MIGRATIONS_DIR/README.md"
if [[ -d "$MIGRATIONS_DIR" ]]; then
  shopt -s nullglob
  for guide in "$MIGRATIONS_DIR"/*.md; do
    base="$(basename "$guide")"
    [[ "$base" == "README.md" || "$base" == "TEMPLATE.md" ]] && continue

    guide_ok=true
    for section in "${required_sections[@]}"; do
      if ! grep -qF -- "$section" "$guide"; then
        guide_ok=false
        die "$guide is missing the required section '$section'.
       Guides follow $MIGRATIONS_DIR/TEMPLATE.md: what breaks, the exact
       before/after steps, and how to verify. A stub strands the reader as
       hard as no guide at all."
      fi
    done

    if ! grep -qE -- '^- \*\*Status:\*\*' "$guide"; then
      guide_ok=false
      die "$guide does not record the guide-only upgrade walk-through.
       Add a '- **Status:** ...' line under '### Guide-only upgrade walkthrough'
       stating whether the walk-through was performed and when (issue #1588)."
    fi

    if grep -qF -- '> **Template.**' "$guide"; then
      guide_ok=false
      die "$guide still carries the TEMPLATE.md banner — replace the placeholders."
    fi

    if [[ ! -f "$index_file" ]]; then
      guide_ok=false
      die "$index_file is missing — guides are only findable through the index."
    elif ! grep -qF -- "$base" "$index_file"; then
      guide_ok=false
      die "$index_file does not index $base. Add it so readers can find it."
    fi

    if $guide_ok; then
      ok "$guide follows the guide template and is indexed"
    fi
  done
  shopt -u nullglob
fi

echo ""
if [[ "$failures" -gt 0 ]]; then
  echo "Migration guide gate FAILED with $failures finding(s)." >&2
  echo "See $MIGRATIONS_DIR/README.md for the process and docs/release-checklist.md" >&2
  echo "for where this sits in the release." >&2
  exit 1
fi

echo "Migration guide coverage OK."
