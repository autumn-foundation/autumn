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
#      token `**Breaking:**` (any case) or by sitting under a
#      `### Breaking Changes` heading. Nothing else counts.
#   2. Unmarked-break lint. An entry that talks about *breaking* something
#      without the marker fails, unless the wording is explicitly negated
#      ("non-breaking", "no breaking change", "without breaking ...") or the
#      entry carries an explicit suppression comment. Without this, an author
#      can strand users by writing prose the coverage check cannot see -- which
#      is exactly how 0.6.0's rename slipped through.
#   3. Coverage. A section with at least one breaking entry must have a guide:
#      `docs/migrations/<version>.md` for a release, `docs/migrations/next.md`
#      (the rolling draft, renamed at release time) for `## [Unreleased]`.
#      A release candidate (`## [0.7.0-rc.1]`) is gated against its release's
#      guide, `docs/migrations/0.7.0.md`.
#   4. Linkage. Every breaking entry must carry a markdown *link* to its own
#      guide -- `](docs/migrations/X.Y.Z.md)` -- so a reader lands on the fix
#      path straight from the changelog line. A bare path mention is not a link.
#
# It also checks every guide in `docs/migrations/` for the TEMPLATE.md shape
# (required headings, each with content under it, and the recorded guide-only
# upgrade walk-through, which may only be `pending` on the rolling draft) and
# for an entry in the `docs/migrations/README.md` index, so a stub that only
# exists to satisfy check 3 does not pass.
#
# Releases before $MIGRATION_GUIDE_FLOOR are out of scope (issue #1588
# explicitly excludes backfilling guides earlier than 0.4.0).
#
# The gate FAILS CLOSED on anything it cannot read: a `## ` heading it cannot
# parse and an unclosed code fence are both hard errors, because either one
# silently removes whole sections from every check above.
#
# Residual risk, stated plainly: the lint is textual. An author who describes a
# break without using the word "breaking" and without the marker still gets
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

# Headings every guide must carry, each with content under it. Keep in sync
# with docs/migrations/TEMPLATE.md and docs/migrations/README.md.
REQUIRED_SECTIONS='## At a glance|## Summary|## Before you start|## Breaking changes|## How to verify|### Guide-only upgrade walkthrough'

mode="gate"
case "${#}:${1:-}" in
  0:) ;;
  1:--list) mode="list" ;;
  *)
    echo "usage: scripts/check-migration-guides.sh [--list]" >&2
    exit 2
    ;;
esac

failures=0

# Unlike the `die` in scripts/check-release-notes.sh, this one does NOT exit:
# the gate reports every finding in one run so an author fixes them together.
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
# Emitted records (tab separated, message always last):
#   SECTION   <name>  <guide-path>  <in-scope 0|1>
#   BREAKING  <name>  <line>        <links-its-guide 0|1>  <guide-path>
#   UNMARKED  <name>  <line>        <excerpt>
#   UNPARSED  -       <line>        <the heading we could not read>
#   UNCLOSED  -       <line>        <the fence that was never closed>
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

  # Remove inline code spans, CommonMark-style: a span opens on a run of N
  # backticks and closes on the next run of exactly N. Matching only
  # single-backtick pairs left the contents of a ``double-backtick`` span
  # behind, and pairing backticks positionally let one stray backtick swallow
  # a sentence. An unclosed run is left in place — never guessed at.
  function strip_code_spans(text,   out, i, n, ch, run, j, run2, found) {
    out = ""
    i = 1
    n = length(text)
    while (i <= n) {
      ch = substr(text, i, 1)
      if (ch != "`") { out = out ch; i++; continue }
      run = 0
      while (i + run <= n && substr(text, i + run, 1) == "`") run++
      j = i + run
      found = 0
      while (j <= n) {
        if (substr(text, j, 1) != "`") { j++; continue }
        run2 = 0
        while (j + run2 <= n && substr(text, j + run2, 1) == "`") run2++
        if (run2 == run) { found = 1; break }
        j += run2
      }
      if (found) { out = out " "; i = j + run } else { out = out substr(text, i, run); i += run }
    }
    return out
  }

  function excerpt(text,   flat) {
    flat = text
    gsub(/[[:space:]]+/, " ", flat)
    return (length(flat) > 90) ? substr(flat, 1, 90) "..." : flat
  }

  function flush_entry(   lower, marked, has_link, i, prose, ticks, scratch) {
    if (entry == "") return
    if (!in_scope) { entry = ""; return }

    # Inline code spans hold *mentions* of the marker, not declarations, so an
    # entry documenting the convention must not declare itself breaking.
    prose = strip_code_spans(entry)

    lower = tolower(prose)
    marked = (lower ~ /\*\*breaking(:\*\*|\*\*:)/) || entry_breaking_heading

    # A marker that exists in the raw text but not after stripping is either a
    # deliberate mention or a real marker a stray backtick swallowed. Those are
    # textually identical and the costs are not: a missed break strands every
    # downstream app. Ask for one token rather than guess — the suppression
    # below settles it for a mention, fixing the backticks settles it for a
    # declaration.
    marker_in_span = (!marked && tolower(entry) ~ /\*\*breaking(:\*\*|\*\*:)/)

    # The suppression is for entries that talk *about* breaking changes. It
    # must never override an explicit declaration: one comment on a parent
    # bullet would otherwise silence every nested `**Breaking:**` under it.
    if (!marked && entry ~ /<!--[[:space:]]*migration-guide-gate:/) {
      entry = ""
      return
    }

    if (marker_in_span) {
      printf "AMBIGUOUS\t%s\t%d\t%s\n", section, entry_line, excerpt(entry)
      entry = ""
      return
    }

    if (marked) {
      # A markdown link, not a mention: `](<guide>)`.
      has_link = (index(entry, "](" guide_path ")") > 0)
      printf "BREAKING\t%s\t%d\t%d\t%s\n", section, entry_line, has_link, guide_path
    } else {
      # Fold to alpha-only words so "non-breaking" and "non breaking" are the
      # same token and word boundaries need no \b (mawk has none).
      gsub(/[^a-z]+/, " ", lower)
      lower = " " lower " "
      # Only the wordings that unambiguously negate. Anything looser (an
      # "avoids breaking" / "no <adjective> breaking" catch-all) starts eating
      # real breaks; entries that need more latitude use the suppression, which
      # is visible in the diff.
      for (i = 0; i < 3; i++) {
        gsub(/ (non breaking|no breaking|nothing breaking|not a breaking|without breaking|rather than a breaking) /, " ", lower)
      }
      if (lower ~ / breaking /) {
        printf "UNMARKED\t%s\t%d\t%s\n", section, entry_line, excerpt(entry)
      }
    }
    entry = ""
  }

  # Fenced code blocks hold TOML/YAML samples whose lines start with "- ".
  # They are part of the entry that opened them, never entries themselves.
  #
  # CommonMark rules that matter here, each of which a naive toggle got wrong:
  # a fence may be ``` or ~~~; a closing fence uses the same character and is
  # at least as long as its opener (so a ````markdown block may contain a ```
  # sample); and a line with an info string is an opener, never a closer.
  # Getting any of them wrong leaves the parser stuck inside a fence, which
  # swallows every section below it. Keep in sync with scan_fence in the guide
  # parser further down this script.
  {
    fence_hit = 0
    fence_body = $0
    sub(/^[[:space:]]*/, "", fence_body)
    fence_char = substr(fence_body, 1, 1)
    if (fence_char == "`" || fence_char == "~") {
      fence_run = 0
      while (substr(fence_body, fence_run + 1, 1) == fence_char) fence_run++
      if (fence_run >= 3) fence_hit = 1
    }

    if (fence_hit) {
      if (!in_fence) {
        in_fence = 1
        fence_len = fence_run
        fence_open_char = fence_char
        fence_opened_at = FNR
      } else if (fence_char == fence_open_char && fence_run >= fence_len &&
                 substr(fence_body, fence_run + 1) ~ /^[[:space:]]*$/) {
        in_fence = 0
      }
    }
    # Fence content is deliberately NOT appended to the entry. It is a code
    # sample: a `**Breaking:**` in it is a mention, a guide path in it is not a
    # link the reader can click, and a TOML comment mentioning "breaking" is
    # not a breaking change. Appending it and then trying to reason about the
    # text is how a config example turned a docs PR red.
    if (fence_hit || in_fence) next
  }

  /^## / {
    flush_entry()
    breaking_heading = 0
    if (tolower($0) ~ /^##[[:space:]]+\[?unreleased\]?[[:space:]]*$/) {
      section = "Unreleased"
      guide_path = unreleased_guide
      in_scope = 1
    } else if (match($0, /\[[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?\][[:space:]]*(-|$)/)) {
      section = substr($0, RSTART + 1, RLENGTH - 2)
      sub(/\].*$/, "", section)
      # A release candidate is gated against its release`s guide.
      release = section
      sub(/-.*$/, "", release)
      guide_path = migrations_dir "/" release ".md"
      in_scope = version_ge(release, floor)
    } else {
      # Fail closed. An unreadable heading used to emit no record at all, so
      # the section was not merely un-gated -- it was invisible, including to
      # --list, which is the operator`s only sanity check.
      printf "UNPARSED\t-\t%d\t%s\n", FNR, $0
      section = ""
      guide_path = ""
      in_scope = 0
      next
    }
    printf "SECTION\t%s\t%s\t%d\n", section, guide_path, in_scope
    next
  }

  /^### / {
    flush_entry()
    breaking_heading = (tolower($0) ~ /^###[[:space:]]+breaking/)
    next
  }

  # `-` and `*` are both legal markdown bullets.
  /^[-*] / {
    flush_entry()
    entry_line = FNR
    entry_breaking_heading = breaking_heading
    entry = $0
    next
  }

  { if (entry != "") entry = entry " " $0 }

  END {
    flush_entry()
    if (in_fence) printf "UNCLOSED\t-\t%d\tcode fence opened here is never closed\n", fence_opened_at
  }
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
# Check 0: the parser read the whole file.
# ---------------------------------------------------------------------------
while IFS=$'\t' read -r kind _ line text; do
  case "$kind" in
    UNPARSED)
      die "$CHANGELOG:$line: unreadable release heading, so this section is
       invisible to every check below:
         $text
       Use '## [X.Y.Z] - YYYY-MM-DD' (a '-rc.N' suffix is fine) or
       '## [Unreleased]'."
      ;;
    UNCLOSED)
      die "$CHANGELOG:$line: $text.
       Everything after it is read as part of one entry, which hides whole
       release sections from the gate. Close the fence."
      ;;
    AMBIGUOUS)
      die "$CHANGELOG:$line: a '**Breaking:**' marker survives only inside a code span:
       $text
       That reads two ways and the gate will not guess between them:
         * you are *mentioning* the marker — append the suppression:
             <!-- migration-guide-gate: <reason> -->
         * it is a real declaration a stray backtick swallowed — fix the
           backticks so the marker sits in the prose."
      ;;
  esac
done <<<"$findings"

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
if [[ "$unmarked" -eq 0 ]]; then
  ok "no unmarked breaking changes in $CHANGELOG"
fi

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
       Copy $MIGRATIONS_DIR/TEMPLATE.md to $guide (delete its banner) and fill
       it in — a release without an upgrade path is treated as a broken build
       (issue #1588)."
  fi
done <<<"$findings"

# ---------------------------------------------------------------------------
# Check 3: every breaking entry links its own guide, so a reader lands on the
# fix path straight from the changelog line.
# ---------------------------------------------------------------------------
while IFS=$'\t' read -r kind name line linked guide; do
  [[ "$kind" == "BREAKING" && "$linked" == "0" ]] || continue
  die "$CHANGELOG:$line: [$name] breaking entry does not link its migration guide.
       Append a markdown link: See the [migration guide]($guide)."
done <<<"$findings"

# ---------------------------------------------------------------------------
# Check 4: every guide has the TEMPLATE.md shape, records a performed
# walk-through, and is indexed.
# ---------------------------------------------------------------------------
index_file="$MIGRATIONS_DIR/README.md"
if [[ -d "$MIGRATIONS_DIR" ]]; then
  shopt -s nullglob
  for guide in "$MIGRATIONS_DIR"/*.md; do
    base="$(basename "$guide")"
    [[ "$base" == "README.md" || "$base" == "TEMPLATE.md" ]] && continue

    # `pending` is legitimate only on the rolling draft: it is renamed to
    # <version>.md at release time, and by then the walk-through is required.
    allow_pending=0
    [[ "$guide" == "$UNRELEASED_GUIDE" ]] && allow_pending=1

    guide_findings="$(
      awk -v required="$REQUIRED_SECTIONS" -v allow_pending="$allow_pending" \
          -v is_draft="$allow_pending" \
          -v walkthrough_heading="### Guide-only upgrade walkthrough" '
        function strip_code_spans(text,   out, i, n, ch, run, j, run2, found) {
          # Keep in step with the identical helper in the changelog parser.
          out = ""
          i = 1
          n = length(text)
          while (i <= n) {
            ch = substr(text, i, 1)
            if (ch != "`") { out = out ch; i++; continue }
            run = 0
            while (i + run <= n && substr(text, i + run, 1) == "`") run++
            j = i + run
            found = 0
            while (j <= n) {
              if (substr(text, j, 1) != "`") { j++; continue }
              run2 = 0
              while (j + run2 <= n && substr(text, j + run2, 1) == "`") run2++
              if (run2 == run) { found = 1; break }
              j += run2
            }
            if (found) { out = out " "; i = j + run } else { out = out substr(text, i, run); i += run }
          }
          return out
        }
        function heading_level(line,   n) {
          n = 0
          while (substr(line, n + 1, 1) == "#") n++
          return n
        }
        function flush_section() {
          if (current != "" && (current in want) && !content[current] && !is_draft) {
            printf "EMPTY\t%s\n", current
          }
        }
        BEGIN {
          split(required, list, "|")
          for (i in list) want[list[i]] = 1
        }
        # A guide illustrating what a guide looks like must not satisfy its own
        # structure from inside the sample. Fence lines and their contents count
        # as *content* for the enclosing section, but never as headings, as the
        # walk-through status, or as placeholders. Mirrors the changelog
        # parsers fence handling above.
        {
          fence_hit = 0
          fence_body = $0
          sub(/^[[:space:]]*/, "", fence_body)
          fence_char = substr(fence_body, 1, 1)
          if (fence_char == "`" || fence_char == "~") {
            fence_run = 0
            while (substr(fence_body, fence_run + 1, 1) == fence_char) fence_run++
            if (fence_run >= 3) fence_hit = 1
          }
          if (fence_hit) {
            if (!in_fence) {
              in_fence = 1
              fence_len = fence_run
              fence_open_char = fence_char
            } else if (fence_char == fence_open_char && fence_run >= fence_len &&
                       substr(fence_body, fence_run + 1) ~ /^[[:space:]]*$/) {
              in_fence = 0
            }
          }
          if (fence_hit || in_fence) {
            if (current != "") content[current] = 1
            next
          }
        }
        # Any `{...}` left over from the template, not a hand-listed few:
        # `{Area}`, `{old MSRV}` and `{Short description}` all used to pass —
        # and `{Area}` lives in a heading, so this has to run before the
        # heading rule below claims the line. Code spans are stripped first so
        # a `{:?}` in prose is not mistaken for a placeholder.
        { if (strip_code_spans($0) ~ /\{[^{}]*\}/) placeholder = 1 }

        /^#+ / {
          level = heading_level($0)
          # A deeper heading is content for the section it sits under.
          if (current != "" && level > current_level) content[current] = 1
          flush_section()
          current = $0
          sub(/[[:space:]]+$/, "", current)
          current_level = level
          seen[current] = 1
          next
        }
        # Only under the heading the release process points at — a status
        # recorded under Summary is not a recorded walk-through.
        current == walkthrough_heading && /^-[[:space:]]+\*\*Status:\*\*/ {
          status = $0
          # Validate the *whole* value, not "does it contain the word
          # performed" — "not performed 2026-08-11" says the opposite of what
          # a substring search concluded from it.
          status_value = tolower($0)
          sub(/^-[[:space:]]+\*\*status:\*\*[[:space:]]*/, "", status_value)
          sub(/[[:space:]]+$/, "", status_value)
        }
        { if (current != "" && $0 ~ /[^[:space:]]/) content[current] = 1 }
        END {
          flush_section()
          for (h in want) if (!(h in seen)) printf "MISSING\t%s\n", h
          # The rolling draft is a skeleton by design: docs/release-checklist.md
          # recreates it from TEMPLATE.md after every release so the links to it
          # keep resolving, and the template carries placeholders. Both checks
          # apply the moment it is renamed to <version>.md.
          if (is_draft) placeholder = 0
          if (status == "") {
            printf "NOSTATUS\t-\n"
          } else if (!allow_pending &&
                     status_value !~ /^performed[[:space:]]+[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]/ &&
                     status_value !~ /^backfilled([[:space:]]|$)/) {
            printf "PENDING\t%s\n", status
          }
          if (placeholder) printf "PLACEHOLDER\t-\n"
        }
      ' "$guide"
    )"

    guide_ok=true
    while IFS=$'\t' read -r kind detail; do
      case "$kind" in
        MISSING)
          guide_ok=false
          die "$guide is missing the required section '$detail'.
       Guides follow $MIGRATIONS_DIR/TEMPLATE.md: what breaks, the exact
       before/after steps, and how to verify. The heading must match exactly —
       a deeper '#### ...' does not count."
          ;;
        EMPTY)
          guide_ok=false
          die "$guide has nothing under '$detail'.
       Headings with no content are a stub; it strands the reader exactly as
       hard as no guide at all."
          ;;
        NOSTATUS)
          guide_ok=false
          die "$guide does not record the guide-only upgrade walk-through.
       Add a '- **Status:** ...' line under '### Guide-only upgrade walkthrough'
       stating whether the walk-through was performed and when (issue #1588)."
          ;;
        PENDING)
          guide_ok=false
          die "$guide does not record a performed walk-through:
         $detail
       Perform the guide-only upgrade of an 'autumn new' app from the previous
       release and record it as '- **Status:** performed YYYY-MM-DD'
       (docs/release-checklist.md, Migration Guide Gate). A guide written after
       its release shipped says 'backfilled' instead — visible in the diff, so
       claiming it for a new release is a reviewable act. 'pending' is only
       allowed on the rolling $UNRELEASED_GUIDE draft."
          ;;
        PLACEHOLDER)
          guide_ok=false
          die "$guide still contains TEMPLATE.md placeholders — replace them."
          ;;
      esac
    done <<<"$guide_findings"

    if grep -qF -- '> **Template.**' "$guide"; then
      guide_ok=false
      die "$guide still carries the TEMPLATE.md banner — delete the banner block."
    fi

    # The entry has to be a link in the Index section, not the filename
    # appearing somewhere in the process prose — `next.md` is named there
    # repeatedly, so a substring match would have let the release rename delete
    # its index bullet unnoticed.
    if [[ ! -f "$index_file" ]]; then
      guide_ok=false
      die "$index_file is missing — guides are only findable through the index."
    elif ! awk -v want="]($base)" '
            /^##[[:space:]]+Index[[:space:]]*$/ { in_index = 1; next }
            /^##[[:space:]]/                    { in_index = 0 }
            in_index && index($0, want) > 0     { found = 1 }
            END { exit found ? 0 : 1 }
          ' "$index_file"; then
      guide_ok=false
      die "$index_file does not link $base from its '## Index' section.
       Add a bullet: - [\`$base\`]($base) — <from> -> <to>"
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
