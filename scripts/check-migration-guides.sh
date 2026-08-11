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
# Shared markdown state machine, injected into every awk program below.
#
# awk has no include, and this script runs three separate awk processes over
# three different files. Hand-maintaining a copy of these rules in each is what
# produced this gate's whole tail of review findings: a fix would land in one
# parser and not its sibling, or land without the fail-closed guard the sibling
# already had. One text, three programs, no drift.
#
# visible_text(line) returns what a reader actually sees on that line, and
# maintains fence and comment state across calls. Order matters and is the
# point: a fenced body is literal, so it is never scanned for HTML comment
# delimiters — an example that merely *shows* `<!--` must not comment out the
# document. Callers read these globals after each call:
#   md_fence_hit   this line is a fence delimiter
#   md_in_fence    we are inside a fenced block
#   md_in_comment  we are inside an HTML comment
#   md_fence_opened_at / md_comment_opened_at  where an unclosed one started
# ---------------------------------------------------------------------------
AWK_MARKDOWN_LIB='
function strip_code_spans(text,   out, i, n, ch, run, j, run2, found) {
  # CommonMark: a span opens on a run of N backticks and closes on the next run
  # of exactly N. An unclosed run is left in place — never guessed at, so a
  # stray backtick cannot silently swallow a marker.
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

function unescaped_index(text, needle, start,   pos, at, slashes) {
  # First occurrence of needle at or after start that is not backslash-escaped.
  # A `\<!--` renders as literal text, so it neither opens a comment nor acts
  # as a suppression.
  if (start < 1) start = 1
  while ((pos = index(substr(text, start), needle)) > 0) {
    at = start + pos - 1
    slashes = 0
    while (at - slashes - 1 >= 1 && substr(text, at - slashes - 1, 1) == "\\") slashes++
    if (slashes % 2 == 0) return at
    start = at + 1
  }
  return 0
}

function suppressed_by_comment(text,   t, at) {
  # The suppression must be a real HTML comment: not inside a code span (that
  # is a demonstration) and not escaped (that is a rendered example).
  t = strip_code_spans(text)
  at = 1
  while ((at = unescaped_index(t, "<!--", at)) > 0) {
    if (substr(t, at) ~ /^<!--[[:space:]]*migration-guide-gate:/) return 1
    at += 4
  }
  return 0
}

function expand_tabs(text,   out, i, n, ch, col) {
  # Markdown measures indentation in visual columns, and a tab advances to the
  # next multiple of four. Counting it as one column made a `-\t` item look two
  # columns narrower than it renders, which merged its siblings into it.
  # Almost every line is tab-free; rebuilding each one character by character
  # tripled the suite runtime, so leave those untouched.
  if (index(text, "\t") == 0) return text
  out = ""
  col = 0
  n = length(text)
  for (i = 1; i <= n; i++) {
    ch = substr(text, i, 1)
    if (ch == "\t") {
      do { out = out " "; col++ } while (col % 4 != 0)
    } else {
      out = out ch
      col++
    }
  }
  return out
}

function list_marker_width(text,   body, n, ch) {
  # Width of a list marker at the start of `text` (already dedented), counting
  # the marker and the whitespace after it; 0 when this is not a list item.
  # Markdown allows `-`, `*`, `+` and ordered markers such as `1.` or `1)`.
  body = expand_tabs(text)
  n = 0
  ch = substr(body, 1, 1)
  if (ch == "-" || ch == "*" || ch == "+") {
    n = 1
  } else {
    while (substr(body, n + 1, 1) ~ /^[0-9]$/) n++
    if (n == 0) return 0
    ch = substr(body, n + 1, 1)
    if (ch != "." && ch != ")") return 0
    n++
  }
  ch = substr(body, n + 1, 1)
  if (ch != " " && ch != "\t") return 0
  while (substr(body, n + 1, 1) == " " || substr(body, n + 1, 1) == "\t") n++
  return n
}

function leading_spaces(text,   n, expanded) {
  # Visual columns, not characters: see expand_tabs.
  expanded = expand_tabs(text)
  n = 0
  while (substr(expanded, n + 1, 1) == " ") n++
  return n
}

function dedent3(text,   n, expanded) {
  # Markdown permits up to three leading spaces on a block-level construct; a
  # fourth makes the line an indented code block, i.e. literal text.
  expanded = expand_tabs(text)
  n = 0
  while (n < 3 && substr(expanded, n + 1, 1) == " ") n++
  return substr(expanded, n + 1)
}

function heading_text(line,   h) {
  # The rendered text of an ATX heading: markdown allows a tab after the
  # marker, and an optional trailing run of hashes is a closing sequence, not
  # part of the heading.
  h = dedent3(line)
  sub(/[[:space:]]+#+[[:space:]]*$/, "", h)
  sub(/[[:space:]]+$/, "", h)
  return h
}

function mask_html_attributes(text,   out, rest, at) {
  # Markdown does not render a link inside an HTML attribute, so
  # `<span title="[guide](path)">` supplies no clickable path.
  #
  # The mask is deliberately narrow: only a tag that actually carries an
  # attribute (`<name ... = ... >`). Masking every `<...>` would delete
  # `Option<String>` and `Vec<Route>` from prose the marker and the lint both
  # read — this changelog is full of them — turning a fail-open hole into a
  # fail-open hole somewhere else.
  out = ""
  rest = text
  while (match(rest, /<[a-zA-Z\/][^<>]*=[^<>]*>/)) {
    out = out substr(rest, 1, RSTART - 1)
    for (at = 0; at < RLENGTH; at++) out = out " "
    rest = substr(rest, RSTART + RLENGTH)
  }
  return out rest
}

function links_to(text, path,   needle, pos, at, i, ch, start, slashes) {
  # A complete inline link `[label](path)`, not a bare `](path)`: the second
  # renders as literal text, so the reader has nothing to click.
  needle = "](" path ")"
  start = 1
  while ((pos = index(substr(text, start), needle)) > 0) {
    at = start + pos - 1
    # An escaped `]` does not close the label, so this is not a link either.
    slashes = 0
    while (at - slashes - 1 >= 1 && substr(text, at - slashes - 1, 1) == "\\") slashes++
    if (slashes % 2 == 1) { start = at + 1; continue }
    for (i = at - 1; i >= 1; i--) {
      ch = substr(text, i, 1)
      if (ch == "]") break
      # An escaped bracket renders as literal text: `\[label](path)` is not a
      # link. Odd run of backslashes before it means escaped.
      if (ch == "[") {
        slashes = 0
        while (i - slashes - 1 >= 1 && substr(text, i - slashes - 1, 1) == "\\") slashes++
        # An unescaped `!` before the bracket makes this an image, which renders
        # a picture rather than a path the reader can follow.
        if (slashes % 2 == 0 &&
            !(i > 1 && substr(text, i - 1, 1) == "!")) return 1
      }
    }
    start = at + 1
  }
  return 0
}

function mask_code_spans(text,   out, i, n, ch, run, j, run2, found, k) {
  # Same scan as strip_code_spans, but each span becomes an equal-length run of
  # spaces so offsets still line up with the original text. visible_text needs
  # that: it has to know *where* a real `<!--` sits in `line`, while ignoring
  # any that are merely displayed as code.
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
    if (found) {
      for (k = i; k < j + run; k++) out = out " "
      i = j + run
    } else {
      out = out substr(text, i, run)
      i += run
    }
  }
  return out
}

function visible_text(line,   body, ch, run, out, head, tail, masked, pos, close_at, strip) {
  md_fence_hit = 0

  # Whichever construct opened first wins, and while one is open the other is
  # inert: comment bodies and fenced bodies are both literal. Neither a fixed
  # "fences first" nor "comments first" ordering is correct on its own — an
  # example of `<!--` inside a fence must not comment out the document, and an
  # example of a fence inside a comment must not open one.
  if (md_in_comment) {
    # Code spans are not parsed inside an HTML comment, so a backtick-wrapped
    # terminator still closes it. Search the raw text, not the masked copy.
    out = line
    pos = index(out, "-->")
    if (pos == 0) return ""
    out = substr(out, pos + 3)
    md_in_comment = 0
    return scan_comments(out)
  }

  # Fences first. A closing fence uses the same character and is at least as
  # long as its opener, so a ````markdown block may contain a ``` sample.
  # Four or more leading spaces makes these backticks literal text, not a
  # fence — an over-indented example of a fence marker must not open one.
  #
  # "Leading" is measured from the enclosing list item content column, because
  # markdown strips a list item indentation before interpreting the blocks
  # inside it: a four-space-indented fence within a `- ` item is a fence.
  # md_block_indent is 0 outside a list item, leaving this as it was.
  strip = leading_spaces(line)
  if (strip > md_block_indent) strip = md_block_indent
  body = dedent3(substr(line, strip + 1))
  ch = substr(body, 1, 1)
  if (ch == "`" || ch == "~") {
    run = 0
    while (substr(body, run + 1, 1) == ch) run++
    if (run >= 3) md_fence_hit = 1
  }
  # The info string of a backtick fence may not itself contain a backtick, so
  # a line opens nothing. Treating it as a fence hid every entry until a later
  # literal run "closed" it, with the unclosed-fence guard never firing.
  if (md_fence_hit && ch == "`" && !md_in_fence && index(substr(body, run + 1), "`") > 0) {
    md_fence_hit = 0
  }
  if (md_fence_hit) {
    if (!md_in_fence) {
      md_in_fence = 1
      md_fence_len = run
      md_fence_char = ch
      md_fence_opened_at = FNR
    } else if (ch == md_fence_char && run >= md_fence_len &&
               substr(body, run + 1) ~ /^[[:space:]]*$/) {
      md_in_fence = 0
    }
    return ""
  }
  if (md_in_fence) return ""

  return scan_comments(line)
}

function scan_comments(text,   out, masked, pos, close_at, head, tail) {
  # Comment delimiters are located in a code-span-masked copy so prose that
  # *shows* `<!--` does not open one, and escaped openers are skipped. Offsets
  # are preserved by the mask, so the slices apply to the original text.
  out = text
  masked = mask_code_spans(out)
  while ((pos = unescaped_index(masked, "<!--", 1)) > 0) {
    head = substr(out, 1, pos - 1)
    tail = substr(out, pos)
    close_at = index(substr(out, pos), "-->")
    if (close_at == 0) {
      md_in_comment = 1
      md_comment_opened_at = FNR
      return head
    }
    out = head substr(tail, close_at + 3)
    masked = mask_code_spans(out)
  }
  return out
}
'

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
      -v unreleased_guide="$UNRELEASED_GUIDE" \
      "$AWK_MARKDOWN_LIB"'
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

  function flush_entry(   lower, marked, has_link, i, prose, ticks, scratch) {
    if (entry == "") return
    if (!in_scope) { entry = ""; entry_visible = ""; return }

    # Inline code spans hold *mentions* of the marker, not declarations, so an
    # entry documenting the convention must not declare itself breaking.
    prose = strip_code_spans(entry_visible)

    lower = tolower(prose)
    marked = (lower ~ /\*\*breaking(:\*\*|\*\*:)/) || entry_breaking_heading

    # A marker that exists in the raw text but not after stripping is either a
    # deliberate mention or a real marker a stray backtick swallowed. Those are
    # textually identical and the costs are not: a missed break strands every
    # downstream app. Ask for one token rather than guess — the suppression
    # below settles it for a mention, fixing the backticks settles it for a
    # declaration.
    marker_in_span = (!marked && tolower(entry_visible) ~ /\*\*breaking(:\*\*|\*\*:)/)

    # The suppression is for entries that talk *about* breaking changes. It
    # must never override an explicit declaration: one comment on a parent
    # bullet would otherwise silence every nested `**Breaking:**` under it.
    # Read the suppression from the raw entry with code spans removed: it is
    # an HTML comment, so it cannot come from `entry_visible` (comments are
    # stripped there) — but an entry that merely *displays* the token in
    # backticks is documenting the escape hatch, not using it.
    if (!marked && suppressed_by_comment(entry)) {
      entry = ""
      entry_visible = ""
      return
    }

    if (marker_in_span) {
      printf "AMBIGUOUS\t%s\t%d\t%s\n", section, entry_line, excerpt(entry)
      entry = ""
      entry_visible = ""
      return
    }

    if (marked) {
      # A markdown link, not a mention: `](<guide>)`. Read the same code-span
      # stripped text the marker was read from — a path inside backticks
      # renders as code, so it is not a link the reader can follow.
      has_link = links_to(mask_html_attributes(prose), guide_path)
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
    entry_visible = ""
  }

  # One call keeps fence and comment state; see AWK_MARKDOWN_LIB above for
  # why the ordering (fences before comments) is load-bearing. Fenced bodies
  # and commented-out text are dropped from the entry: a bullet parked inside
  # `<!-- ... -->` renders nowhere and declares nothing, and a `**Breaking:**`
  # in a code sample is a mention. The suppression token is the one comment the
  # gate reads, and flush_entry takes it from the raw entry instead.
  {
    md_block_indent = (entry == "") ? 0 : item_content_indent
    visible = visible_text($0)
    if (md_fence_hit || md_in_fence) next
  }

  heading_text(visible) ~ /^##[ \t]/ {
    flush_entry()
    breaking_heading = 0
    heading = heading_text(visible)
    if (tolower(heading) ~ /^##[[:space:]]+\[?unreleased\]?[[:space:]]*$/) {
      section = "Unreleased"
      guide_path = unreleased_guide
      in_scope = 1
    } else if (match(heading, /\[[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?\][[:space:]]*(-|$)/)) {
      section = substr(heading, RSTART + 1, RLENGTH - 2)
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
      printf "UNPARSED\t-\t%d\t%s\n", FNR, heading
      section = ""
      guide_path = ""
      in_scope = 0
      next
    }
    printf "SECTION\t%s\t%s\t%d\n", section, guide_path, in_scope
    next
  }

  heading_text(visible) ~ /^###[ \t]/ {
    flush_entry()
    # Only the documented heading declares a break. A `breaking` *prefix* match
    # turned `### Breaking down request latency` into a section where every
    # additive bullet demanded a migration guide.
    breaking_heading = (tolower(heading_text(visible)) ~ /^###[[:space:]]+breaking[[:space:]]+changes$/)
    next
  }

  # `-`, `*` and `+` are all legal markdown bullets, and markdown allows a tab
  # after the marker.
  #
  # Indentation is the subtle half, and the boundary is the open item content
  # column. A bullet starting at or past that column is nested inside the
  # entry — which is how this changelog writes multi-part entries, with the
  # marker often on a sub-bullet. A bullet left of it is a sibling (or a new
  # top-level item) and starts its own entry. Comparing against the *marker*
  # column instead left a middle band — deeper than the marker, left of the
  # content — where the bullet was neither, and the line was dropped outright.
  list_marker_width(dedent3(visible)) > 0 &&
  (entry == "" || leading_spaces(visible) < entry_content_indent) {
    flush_entry()
    entry_line = FNR
    entry_breaking_heading = breaking_heading
    entry = $0
    entry_visible = visible
    entry_indent = leading_spaces(visible)
    # Where this item content starts: past the marker and the space after it.
    entry_content_indent = entry_indent + list_marker_width(dedent3(visible))
    item_content_indent = entry_content_indent
    entry_paragraph_open = 1
    next
  }

  {
    if (entry != "") {
      # A non-blank line indented less than the item content column sits
      # outside the list item — markdown renders it as a sibling block, so it
      # is not part of this entry and must not supply its guide link.
      if (visible ~ /[^[:space:]]/ && leading_spaces(visible) < entry_content_indent &&
          entry_paragraph_open == 0) {
        flush_entry()
      } else {
        # A nested bullet opens an item of its own; blocks after it are
        # measured from *its* content column, not the outer one.
        inner_indent = leading_spaces(visible)
        inner_width = list_marker_width(substr(visible, inner_indent + 1))
        if (inner_width > 0) item_content_indent = inner_indent + inner_width
        # Four columns past the innermost item content is an indented code
        # block: literal text, so it neither adds prose nor supplies a link.
        if (visible ~ /[^[:space:]]/ && leading_spaces(visible) >= item_content_indent + 4) next
        entry = entry " " $0
        entry_visible = entry_visible " " visible
      }
    }
    # CommonMark lazy continuation: while the item paragraph is still open, an
    # unindented line continues it. A blank line closes the paragraph, after
    # which the same line would sit outside the item.
    entry_paragraph_open = (visible ~ /[^[:space:]]/) ? 1 : 0
  }

  END {
    flush_entry()
    if (md_in_fence) printf "UNCLOSED\t-\t%d\tcode fence opened here is never closed\n", md_fence_opened_at
    if (md_in_comment) printf "UNCLOSED\t-\t%d\tHTML comment opened here is never closed\n", md_comment_opened_at
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
       Everything after it is swallowed — headings, entries and all — which
       hides whole release sections from every check below. Close it."
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

# The rolling draft is permanent, not conditional on there being an unreleased
# break: docs/migrations/README.md and STABILITY.md both link it by name, and
# nothing in this repo checks markdown links, so a release that renames it away
# and forgets to recreate it leaves those links 404ing until the next breaking
# PR happens to notice.
if [[ ! -f "$UNRELEASED_GUIDE" ]]; then
  die "$UNRELEASED_GUIDE is missing.
       The rolling draft always exists between releases — recreate it from
       $MIGRATIONS_DIR/TEMPLATE.md with the banner deleted (docs/release-checklist.md,
       Migration Guide Gate). Its placeholders are fine; it is a draft."
fi

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
          -v template="$MIGRATIONS_DIR/TEMPLATE.md" \
          -v walkthrough_heading="### Guide-only upgrade walkthrough" \
          "$AWK_MARKDOWN_LIB"'
        function heading_level(line,   n) {
          n = 0
          while (substr(line, n + 1, 1) == "#") n++
          return n
        }
        BEGIN {
          split(required, list, "|")
          for (i in list) want[list[i]] = 1

          # The placeholder vocabulary is read from TEMPLATE.md rather than
          # guessed at. Listing tokens by hand missed `{Area}` and `{old MSRV}`;
          # matching any `{...}` instead flagged `{:?}`, `Route { .. }` and
          # `format!("{addr}")`, which guides are full of. The template is the
          # only authority on what a placeholder looks like, and it stays
          # correct as the template changes.
          while ((getline template_line < template) > 0) {
            rest = template_line
            while (match(rest, /\{[^{}]*\}/)) {
              tokens[substr(rest, RSTART, RLENGTH)] = 1
              rest = substr(rest, RSTART + RLENGTH)
            }
          }
          close(template)
        }

        # Runs before the fence rule below: a placeholder left in a fenced
        # snippet (`autumn-web = "{X.Y.Z}"`) or an inline span is exactly the
        # unresolved release data this catches, and both used to be skipped.
        {
          for (token in tokens) {
            if (index($0, token) > 0) { placeholder = 1; break }
          }
        }
        # A guide illustrating what a guide looks like must not satisfy its own
        # structure from inside the sample, and `<!-- TODO -->` renders as
        # nothing so it is not migration instructions. Fence delimiters are not
        # content either; the lines inside one are.
        {
          visible = visible_text($0)
          if (md_fence_hit) next
          if (md_in_fence) {
            if ($0 ~ /[^[:space:]]/) {
              for (lvl = 1; lvl <= 6; lvl++) {
                if (open_heading[lvl] != "") content[open_heading[lvl]] = 1
              }
            }
            next
          }
        }
        heading_text(visible) ~ /^#+[ \t]/ {
          level = heading_level(heading_text(visible))
          current = heading_text(visible)
          current_level = level
          seen[current] = 1
          # Remember the open heading chain. A deeper heading does NOT make its
          # parent non-empty by existing — a tree of empty headings is not
          # migration guidance — so content propagates up from real content
          # instead, below.
          for (lvl = level; lvl <= 6; lvl++) open_heading[lvl] = ""
          open_heading[level] = current
          next
        }
        # Only under the heading the release process points at — a status
        # recorded under Summary is not a recorded walk-through.
        current == walkthrough_heading && visible ~ /^-[[:space:]]+\*\*Status:\*\*/ {
          status = visible
          # Validate the *whole* value, not "does it contain the word
          # performed" — "not performed 2026-08-11" says the opposite of what
          # a substring search concluded from it.
          status_value = tolower(visible)
          sub(/^-[[:space:]]+\*\*status:\*\*[[:space:]]*/, "", status_value)
          sub(/[[:space:]]+$/, "", status_value)
        }
        {
          if (visible ~ /[^[:space:]]/) {
            for (lvl = 1; lvl <= 6; lvl++) {
              if (open_heading[lvl] != "") content[open_heading[lvl]] = 1
            }
          }
        }
        END {
          for (h in want) {
            if (!(h in seen)) {
              printf "MISSING\t%s\n", h
            } else if (!content[h] && !is_draft) {
              # Judged at END, not at the next heading: a required section may
              # take its content from a child heading further down the file.
              printf "EMPTY\t%s\n", h
            }
          }
          # The rolling draft is a skeleton by design: docs/release-checklist.md
          # recreates it from TEMPLATE.md after every release so the links to it
          # keep resolving, and the template carries placeholders. Both checks
          # apply the moment it is renamed to <version>.md.
          if (is_draft) placeholder = 0
          if (status == "") {
            printf "NOSTATUS\t-\n"
          } else if (status_value !~ /^performed[[:space:]]+[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]/ &&
                     status_value !~ /^backfilled([[:space:]]|$)/ &&
                     !(allow_pending && status_value ~ /^pending([[:space:]]|$)/)) {
            # The draft widens the accepted set by one value; it does not turn
            # the check off. `failed`, or a typo, is not a walk-through record.
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
    elif ! awk -v want_path="$base" "$AWK_MARKDOWN_LIB"'
            # Only a link the reader can click counts. A bullet inside an HTML
            # comment, a code span or a fenced sample renders as anything but a
            # link, so the guide stays undiscoverable.
            { visible = strip_code_spans(visible_text($0)) }
            heading_text(visible) ~ /^##[[:space:]]+Index$/ { in_index = 1; next }
            heading_text(visible) ~ /^##[[:space:]]/          { in_index = 0 }
            in_index && links_to(mask_html_attributes(visible), want_path) > 0 { found = 1 }
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
