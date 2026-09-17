#!/usr/bin/env bash
# Guide-index completeness gate: every page in `docs/guide/` must be listed in
# `docs/guide/index.md`, and that index must be linked from `README.md`.
#
# WHY THIS EXISTS: the corpus gates every page a reader ALREADY REACHED — its
# links (`check-docs-links.sh`), its commands (`check-docs-cli.sh`), its
# `AUTUMN_*` variables (`check-docs-config.sh`), its `autumn.toml` keys
# (`check-docs-toml.sh`), its `autumn_web::…` paths (`check-docs-symbols.sh`),
# its `/actuator/…` URLs (`check-docs-routes.sh`), its macro arguments, its
# feature gates and its version pins. `check-docs-orphans.sh` goes one step
# further and proves the page can be reached AT ALL.
#
# Reachable is not findable, and the gap between them is where this corpus was
# losing readers. `check-docs-orphans.sh` asks "does any inbound link exist",
# and it passes when the only inbound link is a mid-page crosslink from another
# guide page or a mention inside an agent `SKILL.md` — a surface written for a
# language model, not for a person with a question. Its own header says where
# discovery actually happens:
#
#   "docs/guide/ has no index page of its own. The guide pages are discovered
#    through the hand-maintained `## Documentation` list in README.md and the
#    skill indexes — surfaces someone has to remember to update. Nothing
#    noticed when they forgot."
#
# Nothing noticed, and they forgot 92 times. The baseline run of this gate, on
# the commit that introduced it, found 92 of the 147 `docs/guide/` pages listed
# in NO reader-facing index — 63% of the guide. Among them were the pages a
# reader needs in their first week:
#
#   docs/guide/middleware.md      docs/guide/testing.md
#   docs/guide/migrations.md      docs/guide/repositories.md
#   docs/guide/jobs.md            docs/guide/authorization.md
#   docs/guide/pagination.md      docs/guide/websockets.md
#   docs/guide/rate-limiting.md   docs/guide/i18n.md
#   docs/guide/events.md          docs/guide/oauth.md
#
# A cold retrieval test over the README's `## Documentation` list — the repo's
# landing page, and the one surface an arriving reader is guaranteed to see —
# found the answering page absent for 11 of 15 ordinary question classes. The
# answers all existed and every drift gate was green over them. The reader
# simply had no way to land on them, which is indistinguishable, from where
# they sit, from the feature not existing.
#
# WHY AN INDEX RATHER THAN 92 MORE README BULLETS. `README.md` is the project's
# landing page, not its table of contents; its `## Documentation` list is a
# curated set of highlights with paragraph-length entries, and pasting the
# whole guide into it would bury the highlights without making the rest
# navigable. The index is a separate page whose entire job is navigation, which
# is also why it carries no answers of its own: an index that explains things
# is a page that drifts against the pages it indexes.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Every tracked page under `docs/guide/` is listed in the index that owns
#      it — exactly once, counted ACROSS ALL INDEXES. Twice is a defect: a
#      reader who meets the same page under two headings cannot tell whether
#      they are the same page, and the second entry is the one that rots. A
#      per-file tally cannot see the worst version of this — the top-level
#      index listing a tutorial chapter that `tutorial/index.md` also lists
#      shows one hit in each file — so entries are gathered from every index
#      before any of them is judged.
#   2. Every guide page an index links exists, and is a guide page. A link to
#      a page that moved is caught by `check-docs-links.sh` as a 404; this
#      catches an index pointing somewhere outside the corpus it indexes.
#   3. Every entry sits under a `## ` section heading, so a page appended to
#      the end of the file lands somewhere a reader is actually scanning.
#   4. `README.md` carries a markdown LINK whose target resolves to
#      `docs/guide/index.md`. An index nobody can reach from the landing page
#      is the very defect this gate exists to prevent, and it would otherwise
#      be the one page the gate could not see. Checking for the literal path as
#      a substring is not enough: a plain-text mention satisfies it while
#      getting the reader nowhere, so the README's link destinations are
#      parsed and resolved.
#
# AN ENTRY HAS A SHAPE, and stating it is what retired this gate's markdown
# parser. An entry is a COLUMN-ZERO list item whose content begins with a link:
#
#   - [Forms, Validation and Normalization](forms.md) — re-rendering a …
#   1. [Project Setup](01-project-setup.md) — scaffold a project, run …
#
# The first several revisions asked the opposite question — "is this link in a
# context where a reader could click it?" — and answered it by subtracting
# contexts one at a time. Review found nine of them: fenced code, HTML
# comments, inline code, four-space indented blocks, blocks indented relative
# to an enclosing list item, tab indentation, images, escaped brackets, raw
# HTML. Each finding was correct, the list had no end (the real task was
# "implement CommonMark"), and two of the patches introduced fresh bugs in the
# opposite direction — once blanking nested lists that were real rows, once
# refusing to see code nested inside a list.
#
# Matching the shape instead rejects all of those without a rule for any of
# them, and it fails SAFE: because this gate separately requires every page to
# have an entry, a row written in one of those ways is reported as a page
# listed nowhere. The index is told it is malformed, loudly, rather than
# half-checked quietly.
#
# `readable()` now handles only what is left — the multi-line regions that can
# still put a row-shaped line at column zero: fenced code, HTML comments and
# raw HTML blocks.
#
# The shape also separates an index's rows from its PROSE, which matters in the
# other direction. `tutorial/index.md` carries four ordinary cross-references
# in paragraphs and blockquotes — "see the [i18n guide]", "if you have already
# read the [Getting Started guide]" — which claim to index nothing. Counting
# them made the gate report 165 links for 161 required pages, and under the
# cross-index rule above it would have flagged every one as a duplicate listing
# of a page another index owns. Writing about a page is not indexing it.
#
# The cost is that a row must be top level. An index that nests rows under
# sub-bullets is told so by name; that is a constraint on 161 lines this gate
# also owns, and a cheap one for retiring an open-ended parser.
#
# DELEGATION TO A SUB-INDEX. A subdirectory of `docs/guide/` that carries its
# own `index.md` — `tutorial/` does — is represented in the TOP-LEVEL index by
# that `index.md` alone, and owns its own pages. The tutorial is 12 ordered
# chapters; listing them individually in a task-shaped index would spray twelve
# near-identical entries across it and tell a reader nothing about which one to
# open first. The sub-index is listed, and it owns its own ordering.
#
# Delegation is not an escape. Each page is checked against the NEAREST index
# above it, so an unlisted tutorial chapter fails against `tutorial/index.md`
# rather than vanishing from the required set — and the rule nests to any
# depth. Getting that wrong is how the first revision of this gate let a new
# tutorial chapter listed nowhere at all pass with zero defects.
#
# TRUTH SET: `git ls-files docs/guide`, read at run time. There is no snapshot
# to regenerate — a page added in a commit is required by this gate in the same
# commit, which is the only moment anyone has the context to write its line.
#
# Usage:
#   scripts/check-docs-guide-index.sh              # gate the real corpus
#   scripts/check-docs-guide-index.sh --self-test  # synthetic-corpus tests

set -euo pipefail

# The parsing is line-oriented markdown with enough link and heading regex work
# that bash would render it unreadable; python3 is already a hard dependency of
# every sibling docs gate in this directory.
run_check() {
  local dir="$1"
  python3 - "$dir" <<'PYEOF'
import re
import subprocess
import sys
import urllib.parse

root = sys.argv[1]

GUIDE = "docs/guide/"
INDEX = GUIDE + "index.md"
README = "README.md"

# A markdown link whose target is a guide page, written relative to the index
# (`middleware.md`, `tutorial/index.md`) or from the repo root
# (`docs/guide/middleware.md`). Both spellings resolve to the same page, and
# both are accepted so the gate never argues with `check-docs-links.sh` about
# relative depth — that is its sibling's job, not this one's.
#
# `(?<![!\\])` rejects two things that share every other character with a link
# and navigate nowhere: an image (`![alt](page.md)` renders a picture) and an
# escaped bracket (`\[Guide](page.md)` renders literal text).
# A link destination may contain BALANCED parentheses — `other(foo).md` is an
# ordinary path — so it is not "everything up to the first `)`". Reading it
# that way ended the span early and left the rest of the link, title included,
# for the reference scan to misread as navigation.
#
# `_FRAG` is deliberately gated behind a literal `#`. Its only job is to eat a
# fragment, and writing it as a second open-ended run would let it and `_DEST`
# match the same characters — an ambiguity the engine explores by backtracking,
# which is a quiet way to turn a 1000-line corpus into a hang.
_DEST = r"(?:[^()\s#]|\([^()\s]*\))"
_FRAG = r"(?:#(?:[^()\s]|\([^()\s]*\))*)?"
# `[A](<alpha.md#top>)` is a valid destination, and the bare form above cannot
# read it: `_DEST` stops at the `#`, capturing `<alpha.md` — an unbalanced
# fragment of a path that resolves to nothing, so a real row written that way
# was reported as listed nowhere. The angled form is therefore its own
# alternative, matched first and handed to `normalise` whole. Found by probing
# this round's fragment handling rather than by a reader hitting it, but it is
# the same false-failure class: ordinary markdown the gate rejected.
_ANGLE = r"<[^<>\n]*>"

# An ATX heading: one to six `#`, then whitespace or end of line. The trailing
# requirement is the whole point — `#not-a-heading` is a paragraph.
ATX = re.compile(r"^ {0,3}#{1,6}(?:[ \t]|$)")
# A LEVEL-ONE ATX heading specifically: exactly one `#`, then whitespace or
# end of line. `#`, `# Appendix` and `#\tAppendix` are all level-one headings;
# matching the literal prefix `"# "` saw only the middle one.
ATX_H1 = re.compile(r"^ {0,3}#(?:[ \t].*)?$")
# A level-one SETEXT underline. It only forms a heading when a paragraph line
# sits directly above it, which is why the caller checks that rather than
# treating a bare `===` — which is just a paragraph — as a heading.
SETEXT_H1 = re.compile(r"^ {0,3}=+[ \t]*$")
# Whitespace inside a link, optional and required. Neither may cross a BLANK
# line: a blank line ends the paragraph, so `[Guide](target\n\n)` is not a
# link at all and its text renders as literal characters. Plain `\s*` crossed
# one and accepted it as navigation.
_WS = r"[ \t]*(?:\n[ \t]*)?"
_WS1 = r"(?:[ \t]+|[ \t]*\n[ \t]*)"
# A title body, which may span a line ending but not a blank one, for the
# same reason.
# A title's own delimiter may be backslash-escaped and is then CONTENT, so
# each body consumes `\x` as one unit before considering the closer. Without
# that, `"title \" ) [Guide](x.md)"` ended at the escaped quote and handed the
# rest of the title back as markdown, exposing a link that renders nowhere.
#
# The parenthesised form additionally rejects an unescaped `(`: CommonMark
# allows parens in a `(...)` title only when escaped or balanced, and treating
# the first `)` as the end was the same premature-close bug in another suit.
_TITLE = (r'''"(?:\\[^\n]|[^"\\\n]|\n(?!\s*\n))*"'''
          r"""|'(?:\\[^\n]|[^'\\\n]|\n(?!\s*\n))*'"""
          r"""|\((?:\\[^\n]|[^()\\\n]|\n(?!\s*\n))*\)""")
# An optional title, then the close. Titles are `"..."`, `'...'` or `(...)`,
# and the required whitespace before one is what keeps a parenthesised title
# from being read as more balanced destination.
_CLOSE = r"(?:" + _WS1 + r"(?:" + _TITLE + r"))?" + _WS + r"\)"

# An index ENTRY, and the reason this gate no longer tries to parse markdown.
#
# An entry is a list item AT COLUMN ZERO whose content begins immediately with
# a link: `- [Title](page.md) — what it answers`, or `1. [Chapter](01-x.md)` in
# an ordered sub-index. Nothing else is an entry.
#
# Recognising entries by "a link, minus every context where a link is not
# clickable" was the source of nine of this PR's review findings. Each was
# correct and each was a different markdown construct — fenced code, HTML
# comments, inline code, four-space indented blocks, blocks indented relative
# to an enclosing list item, tab indentation, images, escaped brackets — and
# the list does not end, because the real task was "implement CommonMark",
# which no amount of regex reaches. Two of the patches introduced fresh bugs in
# the opposite direction.
#
# Stating the shape instead inverts the problem. Every one of those constructs
# fails to match this pattern, so none of them is an entry, and none of them
# needs its own rule. It also fails in the SAFE direction: this gate separately
# requires every page to have an entry, so a row written in any of those ways
# is reported as a page listed nowhere. The index is malformed loudly rather
# than accepted quietly, and the message says what the shape is.
#
# The cost is that an entry must be a top-level row. An index that nests rows
# under sub-bullets is told so by name rather than silently half-checked; that
# is a constraint on 161 lines this gate also owns, and a cheap one for
# retiring an open-ended parser.
# Brackets nest to ANY depth, so link text is scanned rather than matched.
#
# A regex can express one level (`[A [x]]`), and a bounded expansion can
# express a fixed few, but `- [A [one [two]]](alpha.md)` is an ordinary row
# and every fixed bound is a false failure one level further down. The rest
# of this file's grammar stays in regex; only the counting part moved out,
# because counting is the part a regex cannot do.
# The pieces of a link's tail that regex still handles: the opening paren
# with its whitespace, the angle-bracketed destination form, and the close
# (optional title, whitespace, `)`). The BARE destination is scanned instead,
# for the same reason link text is — its parens nest to any depth, and every
# fixed level is a false failure one level down.
_OPEN = re.compile(r"\(" + _WS)
_ANGLE_DEST = re.compile(_ANGLE)
_CLOSE_RE = re.compile(_CLOSE)


def dest_at(text, pos):
    """`(end, destination)` for a BARE destination at `pos`.

    Stops at whitespace or at a `)` that is not inside a balanced pair, so
    `a((b)).md` is one destination and `x.md)` ends at the close. A fragment
    is split off here, as `_FRAG` used to do.
    """
    depth, j, n = 0, pos, len(text)
    while j < n:
        ch = text[j]
        if ch == "\\" and j + 1 < n:
            # Same rule as `bracket_pairs`: never past a line ending.
            if text[j + 1] == "\n":
                break
            j += 2
            continue
        if ch in " \t\n":
            break
        if ch == "(":
            depth += 1
        elif ch == ")":
            if depth == 0:
                break
            depth -= 1
        j += 1
    if depth:
        return None
    return j, text[pos:j]
# A reference LABEL, which — unlike text — may not contain unescaped
# brackets, so it stays a flat run. It may not cross a blank line
# either, for the same reason the text above it cannot.
_LABEL = re.compile(r"\[((?:\\.|[^\[\]\n]|\n(?!\s*\n))*)\]")


def bracket_pairs(text):
    """Every `[` index mapped to its matching `]`, in ONE pass.

    Scanning outward from each `[` instead was quadratic: a run of unclosed
    brackets made every one of them scan to end of text, which took 3.1s on
    5000 characters and is exactly the shape of input a gate should not hang
    on. One stack pass makes the lookups free.
    """
    pairs, stack, j, n = {}, [], 0, len(text)
    while j < n:
        ch = text[j]
        if ch == "\\":
            # A backslash never swallows the LINE ENDING. Skipping two
            # characters unconditionally ate the newline before a blank
            # line, so the paragraph boundary went unseen and the bracket
            # stack survived it — `[Guide\\`, blank line, `x](y.md)` was
            # accepted as a link CommonMark does not render.
            j += 1 if j + 1 < n and text[j + 1] == "\n" else 2
            continue
        if ch == "\n":
            # A BLANK line ends the paragraph, and link text cannot span
            # one. Brackets either side of it are not a pair, so the stack
            # does not survive the gap — the same rule the destination, the
            # title and the definition already follow, applied to the one
            # construct that had been left out of it.
            k = j + 1
            while k < n and text[k] in " \t":
                k += 1
            if k >= n or text[k] == "\n":
                stack.clear()
        if ch == "[":
            stack.append(j)
        elif ch == "]" and stack:
            pairs[stack.pop()] = j
        j += 1
    return pairs


def bracket_span(text, pos, pairs=None):
    """Index just past the balanced `[...]` beginning at `pos`, or None."""
    if pairs is None:
        pairs = bracket_pairs(text)
    close = pairs.get(pos)
    return None if close is None else close + 1


def _text_renders(text, pos, close, pairs=None, resolved=frozenset()):
    """True when a link's TEXT makes it a link a reader can use.

    Two rules, and both belong to every link spelling rather than to the
    inline one that happened to get them first:

    - it must RENDER SOMETHING. `[](alpha.md)` and `[][a]` give a reader
      nothing to see or click, which is the guarantee this gate exists for.
      An image counts, which is what `IMAGE_MARK` is for.
    - A LINK MAY NOT CONTAIN A LINK. When the text holds one, CommonMark
      deactivates the OUTER opener and the inner link renders, so
      `[outer [B](beta.md)](alpha.md)` routes the reader to beta.md and
      nowhere near alpha.md. An IMAGE does not deactivate it — an image is
      not a link — and `readable()` has already blanked images by here.

    `link_at` had both and `ref_at` had neither, so the reference spellings
    of the same two bugs survived the rounds that fixed the inline ones.
    """
    if pairs is None:
        pairs = bracket_pairs(text)
    if not text[pos + 1:close - 1].strip():
        return False
    for q in range(pos + 1, close - 1):
        if text[q] != "[" or (q and text[q - 1] == "!"):
            continue
        inner_close = pairs.get(q)
        if inner_close is None or inner_close >= close - 1:
            continue
        if _tail_at(text, inner_close + 1) is not None:
            return False
        # Only an inner INLINE link deactivates without knowing the defined
        # labels; where they ARE known, a resolved reference does too.
        if resolved:
            ref = ref_at(text, q, pairs)
            if ref is not None and ref[0] <= close - 1:
                key = label_key(ref[1])
                if key is not None and key in resolved:
                    return False
    return True


def link_at(text, pos, pairs=None, resolved=frozenset()):
    """`(end, destination)` for an INLINE link starting at `pos`, or None."""
    if pos and text[pos - 1] == "!":
        return None
    close = bracket_span(text, pos, pairs)
    if close is None:
        return None
    if pairs is None:
        pairs = bracket_pairs(text)
    tail = _tail_at(text, close)
    if tail is None:
        return None
    if not _text_renders(text, pos, close, pairs, resolved):
        return None
    return tail


def _tail_at(text, pos):
    """`(end, destination)` for the `(dest "title")` after a link's text."""
    op = _OPEN.match(text, pos)
    if op is None:
        return None
    ang = _ANGLE_DEST.match(text, op.end())
    if ang is not None:
        after, dest = ang.end(), ang.group(0)
    else:
        hit = dest_at(text, op.end())
        if hit is None:
            return None
        after, dest = hit
    end = _CLOSE_RE.match(text, after)
    if end is None:
        return None
    return end.end(), dest


def ref_at(text, pos, pairs=None, _resolved=frozenset()):
    """`(end, label)` for a REFERENCE link starting at `pos`, or None.

    Covers all three spellings: full `[text][label]`, collapsed `[text][]`
    and shortcut `[label]`, which uses its own text as the label.
    """
    if pos and text[pos - 1] == "!":
        return None
    close = bracket_span(text, pos, pairs)
    if close is None:
        return None
    if not _text_renders(text, pos, close, pairs, _resolved):
        return None
    inner = text[pos + 1:close - 1]
    m = _LABEL.match(text, close)
    if m is not None:
        if text[m.end():m.end() + 1] in ("(", ":"):
            return None
        return m.end(), (m.group(1) or inner)
    if text[close:close + 1] in ("[", "("):
        return None
    # A colon after a SHORTCUT reference only disqualifies it when what
    # follows really makes a definition. `- [Alpha]: alpha.md` is one — it
    # renders an empty list item and lists no page — but `- [Alpha]: the
    # page` is not, because `the page` is no destination, so both renderers
    # show `<a href="alpha.md">Alpha</a>: the page` and the row IS an entry.
    # Rejecting on the colon alone failed that perfectly good index.
    #
    # `blank_defns` does not cover this: its `DEFN` is anchored to the line
    # start, so a definition nested in a list item never reaches it, which is
    # why the test has to happen here and has to parse rather than peek.
    if text[close:close + 1] == ":" and _DEFN_AT.match(text, pos):
        return None
    return close, inner


def _scan(text, at, resolved=frozenset()):
    """Every non-overlapping `at()` hit in `text`, left to right."""
    pairs = bracket_pairs(text)
    i, n = 0, len(text)
    while i < n:
        if text[i] == "[":
            hit = at(text, i, pairs, resolved)
            if hit is not None:
                yield (i,) + hit
                i = hit[0]
                continue
        i += 1


def link_spans(text, resolved=frozenset()):
    """`(start, end, destination)` for every inline link in `text`."""
    return _scan(text, link_at, resolved)


def ref_labels(text):
    """Every reference LABEL used in `text`."""
    return (label for _, _, label in _scan(text, ref_at))


# An index ROW is a top-level list item. Every marker CommonMark allows is
# one, and this accepted a third of them: `- ` only, with at most three
# digits. So `* [A](alpha.md)`, `+ [A](alpha.md)`, `1000. [A](alpha.md)` and
# `-  [A](alpha.md)` are all ordinary rows that render as list items in
# cmark-gfm, and all four were reported as "listed in no section" — a false
# failure telling a contributor to fix an index that is already correct.
#
# The limits are CommonMark's own: bullets are `-`, `*` or `+`; an ordered
# marker is up to NINE digits; and the content sits one to four spaces after
# the marker, because a fifth space starts an indented code block inside the
# item instead. `LIST_ITEM` below already carried the digit and bullet rules,
# which is where they should have been read from in the first place.
#
# A TAB is padding too, and this accepted only literal spaces — so `-\t[A](a.md)`
# was reported unlisted while both renderers show a linked item. A tab advances
# to the next multiple of four, so ONE of them is always valid padding after a
# marker, and up to three spaces may precede it; TWO tabs reach column eight,
# which is indented code inside the item and not a row. The rest of this file
# handles tabs by `expandtabs(4)`, which cannot be used here because the match
# end is an offset into the ORIGINAL line that `link_at` reads from.
_ROW = r"^(?:[-*+]|\d{1,9}[.)])(?: {0,3}\t| {1,4})"
ROW = re.compile(_ROW)

# The same row, written as a REFERENCE link: `- [A][alpha]`, `- [A][]` or the
# shortcut `- [A]`, with `[alpha]: alpha.md` defined elsewhere in the index.
# These are ordinary markdown rows, and recognising only the inline spelling
# was wrong in both directions at once: a reference-style row reported its
# page as listed nowhere, and — worse — an inline row plus a reference-style
# row for the SAME page counted once, so the "listed exactly once" guarantee
# silently did not hold. The duplicate is the entry that rots.
# Reference rows are read by `ref_at` at the row's content start.

# A link reference DEFINITION, `[label]: target`.
# The whitespace before the destination may include AT MOST one line ending.
# `\s*` crossed blank lines, so `[a]:`, a blank line, then `alpha.md` resolved
# — but CommonMark does not allow a definition to span a blank line, so that
# row renders as plain text and the page it claimed to list was unfindable.
#
# Nothing but an optional title may follow the destination. `[a]: alpha.md
# trailing garbage` is not a definition at all, so a row referencing it
# renders as plain text — and the page it claimed to list was unfindable.
# A title on the FOLLOWING line is still fine: this stops at the end of the
# destination's line, which leaves the definition valid and the title line to
# be read as the prose it resembles.
#
# The title may begin on the LINE AFTER the destination, and the span has to
# cover it. Ending at the destination left the title to be scanned as ordinary
# markdown, so a row-shaped line inside a multi-line title counted as an index
# entry — listing a page with definition metadata that renders nowhere. `_WS1`
# allows exactly one line ending, so a BLANK line still ends the definition.
#
# The title bodies are `_TITLE`, the same ones a link uses. They were spelled
# differently here, which meant a definition title could straddle a blank line
# when a link's could not; two grammars for one construct is how most of this
# file's findings started.
# The ANGLE form is tried FIRST, because it is the one destination that may
# contain spaces. `\S+` alone took only `<my` out of `[a]: <my file.md>` and
# the rest of the line then stopped the pattern matching at all, so a page
# whose name contains a space was reported as listed nowhere — a false
# failure on a definition both renderers resolve to `my%20file.md`. Case 199
# pinned this for the INLINE spelling last round; the definition spelling
# needed the same grammar, which `_ANGLE` already had.
DEFN = re.compile(
    r"""^ {0,3}\[((?:\\.|[^\[\]\n])+)\]:[ \t]*(?:\n[ \t]*)?"""
    r"""(""" + _ANGLE + r"""|\S+)"""
    r"""(?:""" + _WS1 + r"""(?:""" + _TITLE + r"""))?[ \t]*$""",
    re.MULTILINE)

# The same grammar with NO line-start anchor, for asking "is a definition
# HERE?" at an arbitrary offset. A definition may sit inside a list item,
# where `DEFN`'s `^ {0,3}` cannot reach it: `- [Alpha]: alpha.md` renders an
# EMPTY list item and lists no page, while `- [Alpha]: the page` is a
# paragraph whose `[Alpha]` is a live shortcut reference. Telling those two
# apart needs the destination grammar, not the mere presence of a colon.
_DEFN_AT = re.compile(
    r"""\[((?:\\.|[^\[\]\n])+)\]:[ \t]*(?:\n[ \t]*)?"""
    r"""(""" + _ANGLE + r"""|\S+)"""
    r"""(?:""" + _WS1 + r"""(?:""" + _TITLE + r"""))?[ \t]*$""",
    re.MULTILINE)


# CommonMark caps a reference label at 999 characters.
# Stands in for an image inside `readable()`'s output: not whitespace, so
# a link wrapping an image still has visible content, and not a character
# any markdown construct matches.
# Markdown drops a backslash only before ASCII PUNCTUATION. `\\index.md`
# keeps its backslash and names no tracked file, so unescaping every
# character turned a broken destination into a working one. Same class and
# same spelling as `check-docs-links.sh`'s `ESCAPED_PUNCT`, which had this
# right — the fourth finding traceable to differing from a sibling.
# A URI scheme: a letter, then letters, digits, `+`, `-` or `.`, then `:`.
URI_SCHEME = re.compile(r"[A-Za-z][A-Za-z0-9+.-]*:")

ESCAPED_PUNCT = re.compile(r"\\([!-/:-@\[-`{-~])")

IMAGE_MARK = "\ufffc"
# The same idea for a CODE SPAN, which also renders something a reader can
# see and click. Distinct from `IMAGE_MARK` only so the two are legible
# apart in a dump; nothing tests which one it is, both simply mean "visible
# content stood here".
CODE_MARK = "\ufffc"

LABEL_LIMIT = 999


def label_key(raw):
    """Markdown reference labels fold case and collapse whitespace, so
    `[guide   catalog]` and `[Guide Catalog]` are the same label.

    Shared by the index-entry scan and the README reachability scan. They read
    different files, but a label is a label in both, and the two callers want
    exactly the same folding — the reason this is shared rather than copied.

    `casefold`, not `lower`: CommonMark matches labels after a FULL Unicode
    case fold, so `[Guide][\u1e9e]` resolves against `[ss]:` where `lower()`
    gives `\u00df` and matches nothing — rejecting an index the reader can
    reach. Overlong labels are not labels at all: the spec caps them at 999
    characters, and past that the syntax renders as plain text.

    Both rules are `check-docs-orphans.sh`'s, which had them already. Writing
    this helper from scratch instead of reading the one next door is what
    made these two findings, and it returns None the same way that one does.
    """
    if len(raw) > LABEL_LIMIT:
        return None
    key = " ".join(raw.split()).casefold()
    # A label needs at least one NON-WHITESPACE character. `[ ]` folds to the
    # empty string, and an empty key matched another empty key happily — so
    # `- [A][ ]` with `[ ]: alpha.md` counted as an entry, where cmark-gfm and
    # markdown-it both render `[A][ ]` and `[ ]: alpha.md` as literal text and
    # the page is listed nowhere.
    #
    # `ref_at` already refused the EMPTY spelling `[]`; this is the same rule
    # for the spelling that merely looks empty after folding, which is where
    # that earlier fix stopped.
    return key or None


def blank_links(text):
    """Blank every complete inline-link span, space for space.

    The inline pass has already accounted for those spans, so a later pass
    must not read back inside one: a link's TITLE is ordinary text to
    CommonMark, not navigation, and not markup either.

    Newlines survive. A link span can straddle lines, and turning its newline
    into a space would join the next line to it — dropping the `^` that the
    MULTILINE definition scan anchors on, so a good definition below a
    multi-line link would stop counting.
    """
    out, last = [], 0
    for start, end, _ in link_spans(text):
        out.append(text[last:start])
        out.append(_blank(text[start:end]))
        last = end
    out.append(text[last:])
    return "".join(out)


def _defn_entries(body, origin=None):
    """Every REAL reference definition in `body`, as `(match, label, dest)`.

    `origin` is the text BEFORE links were masked, and block starts are read
    from it. Both callers that mask first — `definitions` via `blank_links`
    and `candidate_labels` via `readable` — turn a paragraph whose whole line
    is one inline link into a run of spaces. `_starts_block` then read that
    as a blank line and invented a paragraph boundary, so

        [ordinary](other.md)
        [x]: docs/guide/index.md

    resolved `x` although CommonMark keeps that second line INSIDE the
    paragraph and renders a later `[Guide][x]` as plain text. Masking is
    space-for-space, so the two strings share offsets and the original can
    answer the block question while the masked copy answers the link one.

    One scanner for all THREE callers. The prepass that computes candidate
    labels, the scan that resolves them, and `blank_defns` had grown apart:
    block-start reached all three, label and destination validation reached
    only one. So a definition-SHAPED line that defines nothing was blanked
    anyway by `blank_defns` — and a real, clickable link inside what is
    actually ordinary paragraph text went with it.

    Every rule a definition must satisfy lives here, so there is no fourth
    place to forget one:

    - it must START A BLOCK, or it is a line of the paragraph above it
    - its label must be a label — foldable, and within the 999-character cap
    - its destination must parse, angled by that grammar and bare by
      `dest_at`'s balancing

    KNOWN RENDERER SPLIT, on that last rule. The premise behind it — that
    `[x]: dest#(unterminated` "creates no definition" — is true of
    markdown-it-py and of the spec's "parentheses only if balanced" wording,
    but NOT of cmark 0.31 or cmark-gfm, which is what GitHub serves this
    README with. Both of those define the label and resolve the reference:

        [x]: bad(unbalanced        ->  <p><a href="bad(unbalanced">Guide</a></p>

        [Guide][x]

    So this rule costs a false failure against GitHub on that one spelling.
    It is kept anyway, deliberately: `check-docs-orphans.sh` applies the same
    rule to the same construct, and the repo pins it in
    `migration_guide_gate_rejects_an_unbalanced_paren_in_a_definition`. Three
    gates disagreeing about what a definition IS would be a worse defect than
    all three being stricter than GitHub on a spelling no corpus page uses.
    Changing it is a cross-gate decision, raised on the PR rather than taken
    here. INLINE destinations are not affected — there the implementations
    agree, because `)` has to close something.
    """
    blocks = body if origin is None else origin
    for m in DEFN.finditer(body):
        if not _starts_block(blocks, m.start()):
            continue
        key = label_key(m.group(1))
        if key is None:
            continue
        dest = m.group(2)
        if dest.startswith("<"):
            if not _ANGLE_DEST.fullmatch(dest):
                continue
        else:
            span = dest_at(dest, 0)
            if span is None or span[0] != len(dest):
                continue
        yield m, key, dest


def candidate_labels(text):
    """The reference labels this document appears to define.

    Read from the text as given, before links and images are blanked,
    because the accurate set is computed from `readable()`'s output and this
    is needed to PRODUCE that output. The circularity is real; the way out
    is the one `check-docs-orphans.sh` takes — over-accept slightly and say
    so. What it over-accepts is a definition sitting inside a link or image
    span, which is rare, and each use below states which direction that
    pushes it.
    """
    # Read from a FIRST PASS of `readable()` with nothing resolved, not from
    # the raw text. Scanning raw meant a definition inside a fenced code
    # block counted — `_starts_block` sees a fence OPENER above it and says
    # yes, which is right for a definition after a fence and wrong for one
    # inside it. Marking that label resolved then masked a real reference
    # image and deleted the live link in its brackets.
    #
    # The first pass terminates because it resolves no labels: with an empty
    # set every reference image is left alone, which is the conservative
    # direction, and fences and comments do not depend on labels at all.
    # NO `origin` here, deliberately. `readable` blanks BLOCKS and leaves
    # inline links alone, so its output is the better answer to the block
    # question, not a worse one: a closed `<pre></pre>` is blank in it and a
    # definition below one is real. Round 44 handed `origin` to both callers
    # when only `definitions` needed it — that one masks links, this one does
    # not — and the extra argument blinded this scan to every HTML block.
    return {key for _, key, _ in _defn_entries(readable(text, frozenset()))}


def opens_fence(line):
    """True when `line` opens a fenced code block.

    A backtick fence's info string may not contain a backtick, so ```` ```md`x ````
    is paragraph text. `readable()` had this test inline and `_starts_block`
    used the raw `FENCE` match, so the two disagreed about whether such a
    line ends a paragraph. One predicate, both callers.
    """
    m = FENCE.match(line)
    return bool(m) and not (m.group(1)[0] == "`" and "`" in m.group(2))


def _setext_context(line_above):
    """True when a SETEXT underline under `line_above` forms a heading.

    An underline needs a PARAGRAPH line above it. Under a blank line, a
    heading, a thematic break or a list marker it underlines nothing and is
    itself ordinary text. Both places that care about Setext now ask this,
    rather than one asking and the other assuming.
    """
    return bool(line_above.strip()
                and not ATX.match(line_above)
                and not THEMATIC.match(line_above)
                and not LIST_ITEM.match(line_above))


def _starts_block(text, pos):
    """True when `pos` begins a block rather than continuing a paragraph.

    Only the line above matters: a definition may follow a blank line, a
    heading, a fence, a thematic break or the start of the file, but not a
    line of ordinary prose, which would swallow it into that paragraph.
    """
    # A run of definitions is definitions only if the FIRST one starts a
    # block: `Some prose`, `[a]: x.md`, `[b]: y.md` defines neither, because
    # `a` is paragraph text and `b` is that paragraph's third line. So the
    # run is walked back to its head.
    #
    # ITERATIVE, not recursive. The obvious recursive spelling crashed with
    # RecursionError on a 2000-line definition chain — a crash rather than a
    # wrong answer, found by probing the bound rather than assuming it.
    while True:
        if pos == 0:
            return True
        start = text.rfind("\n", 0, pos)
        if start < 0:
            return False
        prev_start = text.rfind("\n", 0, start) + 1
        prev = text[prev_start:start]
        if not prev.strip():
            return True
        if ATX.match(prev) or opens_fence(prev) or THEMATIC.match(prev):
            return True
        # A SETEXT underline is a heading — and so a block boundary — only
        # when a paragraph line sits directly above it. Bare `===` at the
        # top of a file is ordinary text, so a definition beneath it is that
        # paragraph's second line and defines nothing; cmark-gfm and
        # markdown-it both leave the matching `[Guide][x]` literal. Treating
        # every `===` as a boundary resolved the label anyway and passed a
        # README whose only route to the index does not render.
        #
        # This file already had the rule — the section-heading scan below
        # spells it out and applies it. It just never reached here, which is
        # the same one-of-two-sites miss as the five findings before it, so
        # the context test is now `_setext_context` and lives in one place.
        if SETEXT.match(prev):
            if prev_start == 0:
                return False
            above_start = text.rfind("\n", 0, prev_start - 1) + 1
            return _setext_context(text[above_start:prev_start - 1])
        if not DEFN.match(prev):
            return False
        pos = prev_start


def blank_defns(text):
    """Blank every reference-definition span, space for space.

    Definitions vanish from the rendered page, so nothing inside one is
    visible and nothing inside one is clickable. Newlines survive, for the
    same reason they do in `blank_links`.
    """
    out, last = [], 0
    # Only a REAL definition is blanked, and "real" is `_defn_entries`'s
    # answer rather than a second opinion assembled here. This scan had the
    # block-start check and neither of the other two rules, so a line like
    # `[]: alpha.md "t [Guide](docs/guide/index.md)"` — an empty label, so
    # not a definition, so ordinary paragraph text whose Guide link is live
    # in cmark-gfm AND markdown-it alike — was blanked whole, and the index
    # it linked to was reported unreachable. A false failure, from the third
    # copy of one scan.
    for m, _, _ in _defn_entries(text):
        out.append(text[last:m.start()])
        out.append(_blank(m.group(0)))
        last = m.end()
    out.append(text[last:])
    return "".join(out)


def definitions(text):
    """Every `[label]: target` definition in `text`, as {folded label: target}.

    Shared deliberately. Three findings on this gate have come from two
    callers doing almost-the-same thing slightly differently, and the last
    one was this exact scan: the README side blanked inline-link spans first
    and the entry side, written one round later, did not. A helper both sides
    call cannot drift like that; a convention that they should each remember
    to has now failed three times.

    Each rule below was wrong when this was a dict comprehension per caller:

    - **Inline-link spans are blanked first.** A multi-line title can contain
      a line that looks exactly like a definition, and reading it as one let
      an undefined reference resolve — leaving a page unfindable while the
      gate passed.
    - **The FIRST definition of a label wins.** CommonMark resolves against
      the first; keeping the last let a row or a README link resolve to a
      target the reader never actually reaches.
    - **Fragments are stripped** by `normalise`, so `[a]: alpha.md#section`
      names the page `alpha.md` rather than a file that does not exist.
    """
    out = {}
    for _, key, dest in _defn_entries(blank_links(text), text):
        out.setdefault(key, dest)
    return out

# One left-to-right scan replaces what used to be six sequential passes.
#
# WHY: the passes corrupted each other's input, and no ordering fixes it.
# Comments before code spans meant a literal `` `<!--` `` in prose opened an
# unterminated comment that blanked the rest of the file. Code spans before
# comments means a lone backtick INSIDE a comment pairs with one after it and
# blanks across the gap. Both delete real rows, so the gate fails on a
# perfectly good index — the over-blanking direction, and the one that makes
# this gate wrong rather than merely lenient. Review found the first of those;
# the second is its mirror and would have arrived next.
#
# Scanning once fixes the class: whichever construct OPENS FIRST consumes its
# own extent, which is the precedence CommonMark actually gives them. Nothing
# downstream can reinterpret what an earlier construct already swallowed.
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
# A BLOCK-QUOTE marker, carrying up to three spaces of indentation. Same
# pattern, same bound and same reason as `check-docs-links.sh`, which had
# this right: allowing four would let `    > ``` ` — an INDENTED CODE line
# that merely begins with a quote marker — lose both its marker and its four
# spaces and become a column-zero fence.
#
# `> ```rust ` is still a fence, and this corpus writes them: ten of them
# live in `docs/guide/` today, in openapi.md, mcp.md and fleet-deploys.md.
# Fence detection ran on the raw line, so a quoted fence opened nothing and
# an index row or README link inside a quoted EXAMPLE counted as navigation.
BLOCKQUOTE = re.compile(r"^ {0,3}(?:>[ \t]?)+")


def quote_depth(line):
    """How many block-quote markers open this line (0 if it is not quoted)."""
    m = BLOCKQUOTE.match(line)
    return m.group(0).count(">") if m else 0
# A raw HTML block opener at line start. `<pre>`/`<script>`/`<style>`/
# `<textarea>` run to their closing tag; any other tag ends at a blank line.
#
# The lookahead is not decoration. CommonMark ends a block tag's NAME at
# whitespace, `>`, `/>` or end of line; without that check the pattern read
# `<div.class` as the tag `div` and blanked raw HTML through end of file,
# losing every real link after a line that is in fact an ordinary paragraph.
HTML_OPEN = re.compile(r"^ {0,3}<(/?)([a-zA-Z][a-zA-Z0-9-]*)(?=[ \t>]|/>|$)")
HTML_LITERAL = ("pre", "script", "style", "textarea")
# CommonMark's type-6 block tags: these open a raw block even with text after
# them. Any OTHER tag (type 7) opens one only when it is alone on its line —
# `<span>Docs:</span> [Guide](x.md)` is a paragraph containing a real link,
# and treating it as a block opener blanked that link and failed the gate.
BLOCK_TAGS = frozenset("""
address article aside base basefont blockquote body caption center col
colgroup dd details dialog dir div dl dt fieldset figcaption figure footer
form frame frameset h1 h2 h3 h4 h5 h6 head header hr html iframe legend li
link main menu menuitem nav noframes ol optgroup option p param search
section summary table tbody td tfoot th thead title tr track ul
""".split())
# Declaration-style blocks, each with its own terminator.
# An HTML comment that begins a line is a raw BLOCK; one that appears mid-line
# (`see <!-- x --> and`) is inline, and owns only itself.
COMMENT_BLOCK = re.compile(r"^ {0,3}<!--")

# The characters a live backslash escape neutralises, for this scanner's
# purposes: exactly the ones the inline loop BRANCHES on. An escape stops the
# character opening its construct, so `\<https://x>` is not an autolink and
# `\[a]` is not a link.
#
# Listing `[` and `!` only — which is what this was — meant adding the autolink
# blanking last round instantly created a false failure on `\<`. The set is
# the branch points rather than CommonMark's full ASCII-punctuation list on
# purpose: an escaped paren inside a link destination is the business of the
# destination grammar, and blanking it here would break a link this scanner is
# supposed to find.
ESCAPABLE = "[]!<`"
DECL = ((re.compile(r"^ {0,3}<\?"), "?>"),
        (re.compile(r"^ {0,3}<!\[CDATA\["), "]]>"),
        (re.compile(r"^ {0,3}<![A-Z]"), ">"))
# A URI or email autolink. `<https://example.com>` renders as a LINK, not as
# raw HTML, so treating it as a block opener blanked every row up to the next
# blank line. It is skipped rather than blanked: it is visible to the reader,
# and it can never be an index row or a link to a `.md` page anyway.
AUTOLINK = re.compile(r"<[A-Za-z][A-Za-z0-9+.-]*:[^<>\s]*>"
                      r"|<[^<>\s@]+@[^<>\s@]+\.[^<>\s@]+>")
# A thematic break and a Setext heading underline. Neither is
# paragraph text, so indented code may open straight after one.
THEMATIC = re.compile(r"^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$")
SETEXT = re.compile(r"^ {0,3}(?:=+|-+)[ \t]*$")
# A list item marker, used only to tell a nested list from an indented
# code block.
LIST_ITEM = re.compile(r"^(\s*)(?:[-*+]|\d{1,9}[.)])\s+")
# A well-formed inline HTML tag, whose ATTRIBUTES are not links. A tag may
# wrap across lines — `<span\ntitle="…">` is one tag — but never across a
# BLANK line, which bounds the damage a stray `<` can do: with no `>` before
# the next blank line there is no match at all, and nothing is blanked.
#
# Quoted attribute values are matched as units, so a `>` inside one does not
# end the tag: `<span title="a > b">` is one tag, and the text after that `>`
# is still attribute text rather than prose.
# Whitespace inside a tag, which may span a line ending but not a blank one.
_TWS = r"""(?:[ \t]|\n(?!\s*\n))"""
# One attribute: a name, optionally `= value` with the value bare, single- or
# double-quoted. Quoted values are units, so a `>` inside one does not end the
# tag.
_ATTR = (r"(?:" + _TWS + r"+[a-zA-Z_:][a-zA-Z0-9_.:-]*"
         r"(?:" + _TWS + r"*=" + _TWS + r"*"
         r"""(?:[^ \t\n"'=<>`]+|'[^']*'|"[^"]*"))?)""")
# An inline tag, matched against CommonMark's ACTUAL grammar rather than
# "angle brackets with something between them". The loose version blanked
# `<span = [catalog]>` — which is literal text, because `=` cannot begin an
# attribute name — and took a real reference link with it.
INLINE_TAG = re.compile(
    r"<(?:[a-zA-Z][a-zA-Z0-9-]*" + _ATTR + r"*" + _TWS + r"*/?>"
    r"|/[a-zA-Z][a-zA-Z0-9-]*" + _TWS + r"*>)")


def _blank(s):
    """`s` with every character replaced by a space, newlines preserved."""
    return "".join("\n" if c == "\n" else " " for c in s)


def _escaped(text, pos):
    """True when `text[pos]` is preceded by an odd number of backslashes.

    A backslash-escaped backtick is literal text, so it neither opens nor
    closes a code span. Pairing one with a real delimiter left the span's
    contents visible.
    """
    n = 0
    while pos - n - 1 >= 0 and text[pos - n - 1] == "\\":
        n += 1
    return n % 2 == 1


def readable(text, resolved=None):
    """The part of a markdown document a reader can actually see and click.

    Everything blanked is blanked SPACE FOR SPACE, so the line numbers in
    reported defects stay accurate.

    With `ENTRY` stating the shape of a row, this only has to remove the
    places a row-shaped line or a link can appear without being one:

      - fenced code, ``` and ~~~ — an example of what an entry looks like is
        documentation about the index, not a row of it
      - HTML comments — parking a row behind `<!-- -->` must not keep an
        unlisted page green
      - raw HTML blocks and declarations — `<pre>`, `<![CDATA[` and friends
        render their contents literally
      - inline code spans, including ones wrapping across lines
      - images, label and all: `![alt [x](a.md)](p.png)` is one image and its
        alt text is plain
      - inline HTML tags, whose attributes are attribute text

    ONE SCAN, not a pass per construct. Whichever construct opens first
    consumes its own extent, so nothing downstream can reinterpret what an
    earlier one swallowed. Everything unterminated is left LITERAL rather than
    run to end of file, because the alternative deletes real rows and makes
    this gate fail on a good index.
    """
    if resolved is None:
        resolved = candidate_labels(text)
    out = list(text)
    n = len(text)
    i = 0
    # Indented-code state. A block opens at four spaces after a blank line and
    # only OUTSIDE a list, where four spaces is a continuation or a nested
    # list instead. `ENTRY` is column-anchored so this never affects rows; it
    # exists for the README link scan, which matches links anywhere. The
    # single-scan rewrite dropped it on the same reasoning that dropped the
    # code-span pass once before — true for entries, false for README.
    # CommonMark: indented code cannot interrupt a PARAGRAPH, but it may open
    # after anything else — a blank line, a heading, a closed fence. Keying on
    # "previous line was blank" missed the heading case; keying on "previous
    # line was paragraph text" is the actual rule, and still lets a wrapped,
    # indented continuation line stay part of its paragraph.
    in_paragraph = False
    in_list = False

    def blank_to(start, stop):
        for k in range(start, min(stop, n)):
            if out[k] != "\n":
                out[k] = " "

    def line_end(pos):
        nl = text.find("\n", pos)
        return n if nl < 0 else nl

    def quote_limit(after, depth):
        """Where a block opened inside a quote of `depth` has to stop.

        A quote is a container: a fence, a raw HTML block, a comment, a
        declaration or an indented-code run opened in one cannot outlive it.
        Unbounded, an UNCLOSED one blanks every link in the rest of the file
        — the run-to-end-of-file false failure this scan has produced four
        times now. The bound lives here, once, because the previous fix gave
        it to the raw-tag branch and left the comment and declaration
        branches beside it unbounded, which is the very mistake the comment
        above those branches warns about.
        """
        if not depth:
            return n
        k = after
        while k <= n:
            e = line_end(k)
            if quote_depth(text[k:e]) < depth:
                return k
            if e >= n:
                break
            k = e + 1
        return n

    while i < n:
        at_line_start = i == 0 or text[i - 1] == "\n"
        eol = line_end(i)
        line = text[i:eol]

        if at_line_start:
            # BLOCK-QUOTE markers come off before anything measures or
            # matches this line. A quote is a container: what is inside it
            # is ordinary markdown, indented from the marker, not from
            # column zero. Measuring the raw line read `>     [Guide](x)` as
            # indent 0 — prose with a live link — where CommonMark reads
            # indent 4 inside the quote and renders indented CODE.
            #
            # Round 42 introduced this strip for FENCE detection only and
            # left the neighbouring tests reading the raw line, which is the
            # same one-of-several-sites divergence this gate keeps
            # producing — this time authored by me, one commit earlier. The
            # strip now happens once, here, and everything below sees the
            # quote's content.
            depth = quote_depth(line)
            content = BLOCKQUOTE.sub("", line) if depth else line
            # A tab indents to the next multiple of four, so measuring
            # spaces alone read a tab-indented code line as column zero.
            expanded = content.expandtabs(4)
            stripped = expanded.lstrip(" ")
            indent = len(expanded) - len(stripped)
            if not stripped:
                in_paragraph = False
                i = eol + 1 if eol < n else n
                continue
            if indent == 0 and not LIST_ITEM.match(content):
                in_list = False
            if indent >= 4 and not in_paragraph and not in_list:
                # Runs while the indent holds; a line back under four spaces
                # ends it. Inside a quote the run also ends when the quote
                # does, so a dedent out of the quote cannot be mistaken for
                # more code and swallow the prose after it.
                j = i
                while j < n:
                    stop = line_end(j)
                    raw = text[j:stop]
                    if depth and quote_depth(raw) < depth:
                        break
                    seg = (BLOCKQUOTE.sub("", raw) if depth
                           else raw).expandtabs(4)
                    body = seg.lstrip(" ")
                    if body and len(seg) - len(body) < 4:
                        break
                    blank_to(j, stop)
                    j = stop + 1
                i = min(j, n)
                in_paragraph = False
                continue
            if LIST_ITEM.match(content):
                in_list = True
            # Whether the PREVIOUS line was paragraph text, captured before
            # this line overwrites it. A type-7 HTML opener cannot interrupt
            # a paragraph, and that is the only way to know it is doing so.
            was_paragraph = in_paragraph
            # Whether this line actually OPENS a raw HTML block is decided
            # HERE, before anything reads it. Testing `HTML_OPEN` directly in
            # the paragraph rule below was not the same question: it marked
            # every tag-shaped line as non-paragraph, including a type-7 tag
            # that the rule further down then declined to open a block for.
            # The paragraph ended anyway, so a four-space-indented line under
            # it became code and its link vanished — the guard added for that
            # exact case, defeated by the line above it.
            #
            # An autolink is a link, not a block opener, and is checked first
            # because `HTML_OPEN`'s tag-name pattern happily matches `https`.
            auto = AUTOLINK.match(content)
            hm = HTML_OPEN.match(content)
            if hm and not auto:
                tag = hm.group(2).lower()
                alone = bool(INLINE_TAG.fullmatch(content.strip()))
                type7 = tag not in HTML_LITERAL and tag not in BLOCK_TAGS
                # A type-7 tag opens a block only when it is alone on its
                # line AND is not interrupting a paragraph. CommonMark lets
                # the type-6 list interrupt one but not type 7, so after
                # `Some prose` a lone `<span>` is inline HTML and the lines
                # under it are still paragraph text.
                if type7 and (not alone or was_paragraph):
                    hm = None
            else:
                hm = None

            # A heading or a fence line is not paragraph text, so an indented
            # line after one opens code.
            #
            # `ATX` rather than a `#` prefix test: CommonMark requires
            # whitespace (or end of line) after the opening run, so
            # `#not-a-heading` is an ordinary paragraph. Treating it as a
            # heading let the next indented line open code and swallowed a
            # link that a reader can click.
            in_paragraph = not (ATX.match(content)
                                or opens_fence(content)
                                or hm
                                # A thematic break (`---`, `***`, `___`) and a
                                # Setext underline (`===`, `---`) both end the
                                # paragraph, so an indented line after one is
                                # code.
                                or THEMATIC.match(content)
                                or SETEXT.match(content))

            # Fence detection runs on the QUOTE-STRIPPED content computed
            # above, so `> ```md` opens a fence like ```` ```md ```` does.
            # Only the detection is stripped; the text itself is untouched
            # and still blanked space for space, so reported line numbers
            # stay accurate.
            m = FENCE.match(content)
            if opens_fence(content):
                char, length = m.group(1)[0], len(m.group(1))
                j = eol + 1
                while j <= n:
                    stop = line_end(j)
                    seg = text[j:stop]
                    # A fence opened INSIDE a quote ends when the quote does.
                    # Without this an unclosed `> ```md` would blank every
                    # link in the rest of the file — the same
                    # run-to-end-of-file false failure the unmatched `<!--`
                    # had, and the reason that one is now bounded too. A
                    # blank line ends the quote, so it ends the fence.
                    if depth and quote_depth(seg) < depth:
                        blank_to(i, j)
                        i = j
                        break
                    c = FENCE.match(BLOCKQUOTE.sub("", seg) if depth else seg)
                    if (c and c.group(1)[0] == char
                            and len(c.group(1)) >= length
                            and not c.group(2).strip()):
                        blank_to(i, stop)
                        i = stop
                        break
                    if stop >= n:
                        blank_to(i, n)
                        i = n
                        break
                    j = stop + 1
                else:
                    blank_to(i, n)
                    i = n
                continue

            # A raw HTML block that STARTS a line owns that line to its end,
            # including whatever follows its terminator. `<!-- x --> [Guide]
            # (y.md)` renders the link as literal text; resuming at the `-->`
            # handed the rest of the line back to the scanner as markdown.
            #
            # All three block kinds get this, not just the one that was
            # reported: comments, declarations, and the literal blocks below.
            # Fixing one sibling and leaving the others is how the last four
            # of these findings happened.
            if COMMENT_BLOCK.match(content):
                close = text.find("-->", i + 4)
                stop = n if close < 0 else line_end(close + 3)
                stop = min(stop, quote_limit(eol + 1, depth))
                blank_to(i, stop)
                i = stop
                continue

            decl = next((end for pat, end in DECL if pat.match(content)), None)
            if decl is not None:
                close = text.find(decl, i + 2)
                stop = n if close < 0 else line_end(close + len(decl))
                stop = min(stop, quote_limit(eol + 1, depth))
                blank_to(i, stop)
                i = stop
                continue

            # `hm` was decided above, and is already None for an autolink or
            # a type-7 tag that does not open a block.
            if hm:
                quote_stop = quote_limit(eol + 1, depth)
                tag = hm.group(2).lower()
                if tag in HTML_LITERAL and not hm.group(1):
                    closer = f"</{tag}>"
                    idx = text.lower().find(closer, i)
                    # The whole CLOSING LINE belongs to the block, not just
                    # the tag. `<pre></pre> [Guide](x.md)` renders that link
                    # as literal text, but stopping at the `>` handed the
                    # rest of the line back to the scanner as markdown.
                    stop = n if idx < 0 else line_end(idx + len(closer))
                else:
                    # CommonMark ends the block at a blank line, and a line of
                    # spaces or tabs IS blank. Searching for a literal "\n\n"
                    # missed those and ran the block to end of file, blanking
                    # every link after it.
                    m_blank = re.compile(r"\n[ \t]*\n").search(text, i)
                    stop = n if not m_blank else m_blank.start()
                stop = min(stop, quote_stop)
                blank_to(i, stop)
                i = stop
                continue

        if text.startswith("<!--", i):
            # An INLINE comment needs its closer, and needs it inside the
            # same paragraph. Inline raw HTML cannot contain a blank line,
            # so `-->` on the far side of one closes nothing.
            #
            # Running to EOF when no closer existed was a false failure with
            # the widest blast radius in this file: one unmatched `<!--`
            # anywhere in README.md blanked EVERY link after it, and the gate
            # reported the index unfindable from a README where it is plainly
            # a link. cmark-gfm and markdown-it agree on all four shapes —
            # closer on the same line or the next one forms a comment; no
            # closer, or a closer past a blank line, leaves `<!--` as the
            # literal text it renders as.
            #
            # A line-initial `<!--` is a different construct — HTML block
            # type 2, which DOES run to its closer across blank lines — and
            # is handled by `COMMENT_BLOCK` above, not here.
            para = re.compile(r"\n[ \t]*\n").search(text, i)
            limit = n if para is None else para.start()
            close = text.find("-->", i + 4, limit)
            if close < 0:
                i += 1
                continue
            stop = close + 3
            blank_to(i, stop)
            i = stop
            continue

        if text[i] == "\\":
            # A backslash RUN, resolved by parity. Pairs are literal
            # backslashes; only an odd run leaves a live escape for the
            # character after it, and `\[` or `\!` then stops that character
            # opening a link or an image.
            #
            # This lives here rather than in a lookbehind on `LINK` because
            # Python's lookbehind is fixed-width and cannot count a run, so
            # `(?<!\\)` rejected `\\[Guide](…)` — an escaped BACKSLASH
            # followed by a perfectly live link — and reported the index
            # unreachable. Blanking the escaped opener instead lets the
            # pattern drop that lookbehind entirely.
            j = i
            while j < n and text[j] == "\\":
                j += 1
            if (j - i) % 2 == 1 and j < n and text[j] in ESCAPABLE:
                blank_to(j, j + 1)
                j += 1
            i = j
            continue

        if text[i] == "`":
            if _escaped(text, i):
                i += 1
                continue
            start = i
            while i < n and text[i] == "`":
                i += 1
            run = i - start
            j = i
            closed = False
            while j < n:
                # A code span cannot cross a BLANK line: the paragraph ends
                # there and the backticks are literal on both sides of it.
                # Searching past one paired an opener with a backtick in a
                # later paragraph and blanked every link between them.
                if text[j] == "\n":
                    k = j + 1
                    while k < n and text[k] in " \t":
                        k += 1
                    if k >= n or text[k] == "\n":
                        break
                # CommonMark does not process backslash escapes INSIDE a
                # code span, so a closer preceded by `\` still closes it.
                # Only the OPENER can be escaped away.
                if text[j] != "`":
                    j += 1
                    continue
                cstart = j
                while j < n and text[j] == "`":
                    j += 1
                if j - cstart == run:
                    # A code span RENDERS. `[`Guide`](docs/guide/index.md)`
                    # is `<a href="…"><code>Guide</code></a>` — visible,
                    # clickable text — but blanking every character of the
                    # label left `_text_renders` with nothing and the link
                    # was rejected as empty. That is a false failure on
                    # ordinary documentation: an index row naming a module
                    # or a command in code font is a normal way to write
                    # one. So a span with visible content leaves the same
                    # kind of sentinel an image does; a span whose content
                    # is blank renders an empty element and leaves none.
                    blank_to(start, j)
                    if text[start + run:cstart].strip():
                        out[start] = CODE_MARK
                    i = j
                    closed = True
                    break
            # Unmatched: the backticks are literal, and `i` already sits past
            # them, so scanning simply continues.
            if not closed:
                continue
            continue

        # `_escaped` counts the backslash RUN, not just the character before.
        # `\\![alt …](x.png)` is an escaped backslash followed by a live `!`,
        # so the image still opens and the link inside its alt text is only
        # alt text. Looking at one character read that as escaped, left the
        # image unblanked, and let that nested link count as navigation.
        if (text[i] == "!" and i + 1 < n and text[i + 1] == "["
                and not _escaped(text, i)):
            def balanced(pos, opener, closer):
                depth, k = 1, pos + 1
                while k < n and depth:
                    if text[k] == "\\":
                        # Same rule again: an escape never spans a newline.
                        k += 1 if k + 1 < n and text[k + 1] == "\n" else 2
                        continue
                    if text[k] == "\n":
                        # A BLANK line ends the paragraph, and an image's
                        # label cannot span one — the same rule
                        # `bracket_pairs` applies to link text, which this
                        # scan had been left out of. Without it `![alt`, a
                        # blank line, then `[Guide](index.md)](image.png)`
                        # balanced across the gap and masked the whole span,
                        # deleting a link CommonMark renders live and
                        # failing a README that does reach the index.
                        t = k + 1
                        while t < n and text[t] in " \t":
                            t += 1
                        if t >= n or text[t] == "\n":
                            return None
                    if text[k] == opener:
                        depth += 1
                    elif text[k] == closer:
                        depth -= 1
                    k += 1
                return None if depth else k

            label = balanced(i + 1, "[", "]")
            # An image is `![alt](target)` or the reference forms
            # `![alt][ref]` / `![alt][]`. All three render a picture, so a
            # link written inside the label is alt text either way.
            #
            # The bare SHORTCUT form `![alt]` is deliberately not treated as
            # an image: it is only one if a matching reference definition
            # exists, and without one CommonMark renders `![alt [x](a.md)]`
            # as literal text around a REAL link. Blanking it unconditionally
            # would delete that link — the over-blanking direction — so a
            # shortcut image is left alone.
            if label is not None and label < n and text[label] in "([":
                if text[label] == "(":
                    # The INLINE form must be a valid image tail, not merely
                    # balanced parentheses. `![alt …](not a valid dest)`
                    # forms no image, so its brackets are literal and a link
                    # inside them is real — blanking on balance alone
                    # deleted that link. Same `_tail_at` a link uses, so the
                    # two cannot drift apart.
                    hit = _tail_at(text, label)
                    end = None if hit is None else hit[0]
                else:
                    end = balanced(label, "[", "]")
                # A reference image is an image only if its label RESOLVES —
                # the same rule the shortcut form above already follows, and
                # this form was simply left out of it. With no definition,
                # `![alt [x](a.md)][missing]` renders literal brackets around
                # a REAL link, and blanking it deleted that link.
                #
                # An over-accepted label here keeps a real image masked,
                # which is the safe direction for this use.
                if end is not None and text[label] == "[":
                    ref = text[label + 1:end - 1]
                    key = label_key(ref) if ref.strip() else label_key(
                        text[i + 2:label - 1])
                    if key is None or key not in resolved:
                        end = None
                if end is not None:
                    blank_to(i, end)
                    # An image renders something a reader can SEE, so a link
                    # wrapping one has content even though its text is now
                    # blank. One marker character records that, which is the
                    # difference between `[](a.md)` — nothing to click —
                    # and a badge row. It occupies a position the span
                    # already owned, so line numbers are untouched, and it
                    # matches no markdown construct.
                    if out[i] != "\n":
                        out[i] = IMAGE_MARK
                    i = end
                    continue
            i += 1
            continue

        if text[i] == "<":
            # A `<…>` opening a LINK DESTINATION is not a tag:
            # `[A](<alpha.md>)` is a valid link, and blanking the angle form
            # as inline HTML made the destination unresolvable, so a clickable
            # link and a real index row written that way were reported missing.
            #
            # A reference DEFINITION's target is the same thing after `]:`
            # rather than `](` — `[a]: <alpha.md>`. That spelling only became
            # reachable once rows could be reference links, and it was blanked
            # the same way, so the row above it resolved to nothing.
            back = i - 1
            while back >= 0 and text[back] in " \t":
                back -= 1
            if back >= 1 and text[back] in "(:" and text[back - 1] == "]":
                close = text.find(">", i)
                if 0 <= close < line_end(i):
                    i = close + 1
                    continue
            auto = AUTOLINK.match(text, i)
            if auto:
                # BLANKED, not merely skipped. An autolink is a single link
                # whose body is a URI, so `[catalog]` inside one is part of
                # that URI and not a reference use — but leaving the text in
                # place let the label scan find it and call a definition
                # referenced. Nothing inside an autolink can ever be a guide
                # link either: an autolink needs a scheme, and `<alpha.md>`
                # after `](` or `]:` is a destination, handled above this.
                blank_to(i, auto.end())
                i = auto.end()
                continue
            tag = INLINE_TAG.match(text, i)
            if tag:
                blank_to(i, tag.end())
                i = tag.end()
                continue

        i += 1

    return "".join(out)


def tracked(root):
    out = subprocess.run(
        ["git", "ls-files", "-z", GUIDE],
        cwd=root, capture_output=True, text=True, check=True,
    ).stdout
    return sorted(p for p in out.split("\0") if p.endswith(".md"))


def index_plan(pages):
    """Map every index page under `docs/guide/` to the pages it must list.

    A page is owned by the NEAREST index above it: `tutorial/01-*.md` belongs
    to `tutorial/index.md`, and `tutorial/index.md` itself belongs to the
    top-level index, which is owned by nobody. Delegation is therefore not a
    hole — an unlisted tutorial chapter fails against its own sub-index — and
    it nests to any depth without this function knowing how deep the tree is.

    An earlier revision of this gate excluded a delegated subdirectory's pages
    from the required set and stopped there, never checking the sub-index that
    was supposed to have taken responsibility for them. A new tutorial chapter
    listed nowhere at all passed with zero defects, which is exactly the
    guarantee this gate exists to make. Caught in review on the PR that added
    it; the synthetic corpora below now pin both halves.
    """
    indexes = sorted(p for p in pages
                     if p == INDEX or p.endswith("/index.md"))
    plan = {i: set() for i in indexes}
    for p in pages:
        if p == INDEX:
            continue
        owner, owner_dir = None, None
        for idx in indexes:
            if idx == p:
                continue
            d = idx.rsplit("/", 1)[0] + "/"
            if p.startswith(d) and (owner_dir is None or len(d) > len(owner_dir)):
                owner, owner_dir = idx, d
        if owner is not None:
            plan[owner].add(p)
    return plan


def normalise(target, base):
    """Resolve a link target to a repo-relative guide path, or None.

    `base` is the directory of the file the link was written in ("" for a
    repo-root file such as README.md), so a sub-index's `01-project-setup.md`
    resolves against its own directory rather than against `docs/guide/`.

    A target that resolves outside `docs/guide/` returns None and is simply not
    an entry: an index is allowed to link docs.rs, and pointing that out is not
    this gate's job. A target INSIDE the guide is returned whether or not the
    page exists, so a link to a page that was deleted is reported as a defect
    rather than quietly ignored.
    """
    target = target.strip()
    # `[A](<alpha.md>)` is a valid destination form. Capturing the brackets
    # made the path unresolvable, so a clickable link — and a real index row
    # written that way — was reported as missing.
    # The SAME grammar every other angle test uses, not a startswith /
    # endswith glance. Every caller validates before reaching here, so this
    # is defence rather than a fix — but a glance is how the definition scan
    # accepted `<index.md?>>`, and leaving a third spelling of the rule in
    # the file is how that happens again.
    if _ANGLE_DEST.fullmatch(target):
        target = target[1:-1].strip()
    # `alpha.md#section` names the page `alpha.md`. Inline destinations are
    # already split by `_FRAG`, but a REFERENCE definition arrives whole, and
    # comparing the fragment as part of the filename rejected valid rows and
    # valid README links. Stripping here rather than at each call site is the
    # point: a caller cannot forget it.
    target = target.split("#", 1)[0].rstrip()
    # A rendered link is a URL, and `check-docs-links.sh` already resolves
    # one this way. Disagreeing with the sibling gate about what a
    # destination means is worse than either convention on its own: the two
    # would accept different indexes. So the same three transformations, in
    # the same order.
    #
    # `alpha.md?plain=1` addresses the file, not a file with a query in its
    # name; `alpha%2Emd` is percent-encoded; `alpha\.md` carries markdown
    # escapes that are not part of the path.
    target = target.split("?", 1)[0]
    target = urllib.parse.unquote(target)
    target = ESCAPED_PUNCT.sub(r"\1", target)
    target = target.rstrip("/")
    if not target:
        return None
    # ANY URI scheme leaves the guide, not the three that happened to occur
    # to me. A hard-coded list turned `ftp://example.com/file` into the
    # relative path `docs/guide/ftp:/example.com/file` and reported a page
    # that does not exist — and missed case variants like `HTTPS:` besides.
    # The grammar is scheme-agnostic and case-insensitive, as URIs are.
    if URI_SCHEME.match(target):
        return None
    # A ROOT-RELATIVE destination leaves the repository. On GitHub and every
    # other README renderer `/docs/guide/index.md` addresses the host root,
    # not this checkout, so a reader following it does not arrive. Dropping
    # the empty leading segment silently turned it into a repo path and
    # accepted a link that reaches nothing — and disagreed with
    # `check-docs-links.sh`, which rejects the same target.
    if target.startswith("/"):
        return None
    if target.startswith("./"):
        target = target[2:]
    # Written from the repo root, or relative to `base`.
    path = target if target.startswith(GUIDE) else base + target
    parts = []
    for seg in path.split("/"):
        if seg == "..":
            if not parts:
                return None
            parts.pop()
        elif seg not in ("", "."):
            parts.append(seg)
    path = "/".join(parts)
    return path if path.startswith(GUIDE) else None


def entries(text, base):
    """Every index ENTRY in a file, with the `##` section it sits under.

    An entry is a row matching `ENTRY` — a column-zero list item whose content
    begins with a link. That shape is what separates the index's rows from its
    prose, and the distinction is load-bearing in both directions:
    `tutorial/index.md` carries four cross-references in paragraphs and
    blockquotes — "see the [i18n guide]", "if you have already read the
    [Getting Started guide]" — which are ordinary writing, not claims to index
    those pages. Counting them made the gate report 165 links for 161 required
    pages, and the cross-index ownership rule below would have flagged every
    one of them.

    `readable()` removes the multi-line regions that can still put a
    row-shaped line at column zero, and blanking rather than skipping is what
    keeps the reported line numbers honest. A `## ` heading inside a fence does
    not open a section, for the same reason.
    """
    out = []
    section = None
    body = readable(text)
    # Definitions are collected from the whole file first: a reference-style
    # row may sit above the `[label]: target` line that resolves it, which is
    # the usual way people write them.
    defs = definitions(body)
    # Rows are read from a copy with definition spans blanked, while `defs`
    # above needs them intact. A definition's title may span lines, so a
    # row-shaped line inside one counted as an entry — listing a page with
    # text that renders nowhere. Found by probing the README-side fix for
    # the same thing rather than waiting to be told; blanking preserves
    # newlines, so reported line numbers stay honest.
    rows = blank_defns(body)
    prev = ""
    for lineno, line in enumerate(rows.split("\n"), 1):
        # The previous line, captured before any `continue` can skip the
        # bookkeeping. Only the Setext test needs it, and getting this wrong
        # would make that test read whichever line last fell through.
        prev, line_above = line, prev
        if line.startswith("## "):
            section = line[3:].strip()
            continue
        # A new LEVEL-ONE heading ends the section. Rows appended after one
        # are under no `## ` at all, and carrying the previous section name
        # across let them satisfy the section-placement rule from a heading
        # a reader scanning that section would never reach. A `### ` is a
        # subheading INSIDE the current section, so it does not reset.
        #
        # All three level-one spellings count, not just `# Title`: a bare
        # `#`, a tab after the `#`, and the Setext form underlined with
        # `===`. A Setext underline is only a heading when a paragraph line
        # sits directly above it — under a blank line, or under another
        # heading, `===` is just text and must NOT reset the section, or an
        # index would be told its rows are unplaced when they are not.
        if ATX_H1.match(line):
            section = None
            continue
        if SETEXT_H1.match(line) and _setext_context(line_above):
            section = None
            continue
        target = None
        row = ROW.match(line)
        if row is None:
            continue
        # The link must begin the row's CONTENT — that column-zero anchoring
        # is what separates an index's rows from its prose, and it is why
        # both forms are read at exactly `row.end()` rather than searched for.
        hit = link_at(line, row.end(), None, set(defs))
        if hit is not None and hit[1]:
            target = hit[1]
        else:
            ref = ref_at(line, row.end())
            if ref is not None:
                # `[text][label]` uses `label`; `[label][]` and the shortcut
                # `[label]` use the text itself. An undefined label is not a
                # link, so the row is not an entry and the page it meant to
                # list is reported as listed nowhere — the safe direction,
                # and the same one an unparseable inline row already takes.
                key = label_key(ref[1])
                target = None if key is None else defs.get(key)
        if target is None:
            continue
        path = normalise(target, base)
        if path is not None:
            out.append((path, lineno, section))
    return out


pages = tracked(root)

# A truth set that came back empty means `docs/guide/` was moved or renamed out
# from under this gate. Passing everything silently is the one outcome a drift
# gate must never have, so fail loudly instead.
if not pages:
    sys.exit(
        "FAIL: `git ls-files docs/guide` matched no markdown. The guide "
        "directory was moved or renamed; this gate has no truth set to read "
        "and would pass everything. Fix GUIDE in "
        "scripts/check-docs-guide-index.sh."
    )

page_set = set(pages)
plan = index_plan(pages)

if INDEX not in plan:
    print(f"corpus: {len(pages)} pages under {GUIDE}")
    print(f"defects: {len(pages)}")
    sys.exit(
        f"FAIL: {INDEX} does not exist, so none of the {len(pages)} guide "
        "pages is listed in a reader-facing index."
    )

defects = []
required_total = sum(len(n) for n in plan.values())

# Entries from EVERY index, gathered before anything is judged. Counting per
# index in isolation is what let a page be listed twice — once in the
# top-level index and once in the sub-index that owns it — while each file's
# own tally showed one. The duplicate is invisible from inside either file, so
# the check cannot live inside the per-file loop.
owner_of = {p: idx for idx, need in plan.items() for p in need}
seen = {}
for index_path in sorted(plan):
    base = index_path.rsplit("/", 1)[0] + "/"
    with open(f"{root}/{index_path}", encoding="utf-8") as fh:
        for path, lineno, section in entries(fh.read(), base):
            seen.setdefault(path, []).append((index_path, lineno, section))

linked_total = sum(len(h) for h in seen.values())

# 1. Every page is listed exactly once, across all indexes, by the index that
#    owns it.
for path, index_path in sorted(owner_of.items()):
    if path not in seen:
        defects.append((path, f"listed in no section of {index_path}"))
for path, hits in sorted(seen.items()):
    if len(hits) > 1:
        where = ", ".join(f"{i} line {n}" for i, n, _ in hits)
        defects.append((path, f"listed {len(hits)} times ({where})"))
    owner = owner_of.get(path)
    for index_path, lineno, _ in hits:
        if owner is not None and index_path != owner:
            defects.append(
                (path,
                 f"{index_path} line {lineno}: listed here, but {owner} owns "
                 "this page — an entry belongs to exactly one index")
            )

# 2. Every link resolves to a page that exists.
for path, hits in sorted(seen.items()):
    if path not in page_set:
        index_path, lineno, _ = hits[0]
        defects.append((path, f"{index_path} line {lineno}: no such guide page"))

# 3. Every entry sits under a `## ` heading.
for path, hits in sorted(seen.items()):
    for index_path, lineno, section in hits:
        if section is None:
            defects.append(
                (path,
                 f"{index_path} line {lineno}: not under any `## ` section "
                 "heading")
            )

# 4. The index is reachable from the landing page — by a LINK, not a mention.
#    Checking for the literal path as a substring passed on a plain-text or
#    inline-code mention, which is not clickable and does not get the reader
#    anywhere. Caught in review on the PR that added this gate.
with open(f"{root}/{README}", encoding="utf-8") as fh:
    readme = readable(fh.read())


def reaches_index(text):
    """True when `text` carries a clickable markdown link to the index.

    Inline links (`[Guide](docs/guide/index.md)`) and REFERENCE links
    (`[Guide][catalog]` with `[catalog]: docs/guide/index.md` below) both
    count. Only inline destinations were resolved before, so a README that
    reached the index perfectly well through a reference link was reported as
    having no link at all — a false failure on ordinary markdown, which is
    worse than the exotic near-misses this check has mostly been about.
    """
    # A reference DEFINITION is removed from the rendered output entirely, so
    # a link inside one's title is not navigation — it is not even text. The
    # inline pass therefore reads a copy with definition spans blanked, while
    # the reference pass below still needs them intact to resolve labels.
    if any(normalise(dest, "") == INDEX
           for _, _, dest in link_spans(blank_defns(text),
                                        candidate_labels(text))):
        return True
    # Definitions, then the labels actually referenced by a full
    # (`[text][label]`), collapsed (`[label][]`) or shortcut (`[label]`)
    # reference. A definition nothing references is not a link.
    #
    # `definitions()` blanks inline-link spans itself; the USES below are
    # scanned over the same blanked text, since a `[Guide][catalog]` sitting
    # inside a link's title is title text and reaches nothing.
    defs = definitions(text)
    # The USES are read from a copy with links AND definitions blanked. Only
    # `defs` above needs the definitions intact. Blanking them for the inline
    # pass but not this one left a label inside a definition's own title
    # counting as a use of itself — so a definition nothing references looked
    # referenced, and the index looked reachable from text that renders
    # nowhere.
    text = blank_links(blank_defns(text))
    if not defs:
        return False
    # No `\\` in these lookbehinds. `readable()` already resolved escape
    # PARITY and blanked any `[` a live escape applies to, so re-testing one
    # character here rejected `\\[Guide][]` — an escaped backslash followed
    # by a real reference link. `LINK` dropped this two rounds ago and these
    # two kept it, which is the same one-of-two-sites miss as four findings
    # before it; they are now the last of that shape in the file.
    used = {key for key in (label_key(l) for l in ref_labels(text))
            if key is not None}
    return any(normalise(defs[label], "") == INDEX
               for label in used if label in defs)


if not reaches_index(readme):
    defects.append(
        (README,
         f"has no markdown link whose target resolves to {INDEX}; a mention "
         "the reader cannot click — plain text, inline code, a fenced "
         "example, or a commented-out link — leaves the index unfindable")
    )

print(f"corpus: {len(pages)} pages under {GUIDE}")
print(f"indexes: {len(plan)} ({', '.join(sorted(plan))})")
print(f"entries: {required_total} required, {linked_total} linked")
print(f"defects: {len(defects)}")

if defects:
    print()
    for path, why in defects:
        print(f"  {path}: {why}")
    sys.exit(1)
PYEOF
}

# Synthetic corpora, each a throwaway git repo, so every rule above is proved
# to FAIL when violated rather than assumed to. A gate nobody has watched fail
# is a gate that might be passing for the wrong reason.
self_test() {
  local pass=0 total=0 tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  _case() {
    local name="$1" want="$2" dir="$tmp/$3"
    total=$((total + 1))
    local got=0
    run_check "$dir" >/dev/null 2>&1 || got=$?
    if [ "$got" -eq "$want" ]; then
      pass=$((pass + 1))
      echo "  ok   $name"
    else
      echo "  FAIL $name (wanted exit $want, got $got)"
    fi
  }

  _scaffold() {
    local dir="$tmp/$1"
    mkdir -p "$dir/docs/guide/tutorial"
    git -C "$dir" init -q
    git -C "$dir" config user.email t@t
    git -C "$dir" config user.name t
  }

  _commit() { git -C "$tmp/$1" add -A >/dev/null 2>&1; }

  # 1. A complete index passes.
  _scaffold ok
  printf '# A\n' > "$tmp/ok/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/ok/docs/guide/beta.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n' \
    > "$tmp/ok/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/ok/docs/guide/tutorial/01-x.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [B](beta.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/ok/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ok/README.md"
  _commit ok
  _case "complete index passes" 0 ok

  # 2. A page missing from the index fails.
  _scaffold missing
  printf '# A\n' > "$tmp/missing/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/missing/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' > "$tmp/missing/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/missing/README.md"
  _commit missing
  _case "unlisted page fails" 1 missing

  # 3. A page listed twice fails.
  _scaffold dup
  printf '# A\n' > "$tmp/dup/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n## T\n\n- [A again](alpha.md)\n' \
    > "$tmp/dup/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/dup/README.md"
  _commit dup
  _case "duplicate entry fails" 1 dup

  # 4. An entry above every `## ` heading fails.
  _scaffold nosection
  printf '# A\n' > "$tmp/nosection/docs/guide/alpha.md"
  printf '# Guide\n\n- [A](alpha.md)\n' > "$tmp/nosection/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/nosection/README.md"
  _commit nosection
  _case "entry outside a section fails" 1 nosection

  # 5. A link to a guide page that does not exist fails.
  _scaffold ghost
  printf '# A\n' > "$tmp/ghost/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [G](ghost.md)\n' \
    > "$tmp/ghost/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ghost/README.md"
  _commit ghost
  _case "link to a missing guide page fails" 1 ghost

  # 6. A README that does not link the index fails — the index's own
  #    findability is the one thing the index cannot assert about itself.
  _scaffold unlinked
  printf '# A\n' > "$tmp/unlinked/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' > "$tmp/unlinked/docs/guide/index.md"
  printf 'no pointer here\n' > "$tmp/unlinked/README.md"
  _commit unlinked
  _case "README without the index link fails" 1 unlinked

  # 7. A missing index file fails rather than passing vacuously.
  _scaffold noindex
  printf '# A\n' > "$tmp/noindex/docs/guide/alpha.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/noindex/README.md"
  _commit noindex
  _case "absent index fails" 1 noindex

  # 8. A fenced example of an entry is not counted as one.
  _scaffold fence
  printf '# A\n' > "$tmp/fence/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/fence/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [B](beta.md)\n\n```\n- [A](alpha.md)\n```\n' \
    > "$tmp/fence/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/fence/README.md"
  _commit fence
  _case "fenced entry is not double-counted" 0 fence

  # 9. A page in a delegated subdirectory, listed in NEITHER index, fails.
  #    The first revision of this gate passed this corpus: it dropped the
  #    subdirectory's pages from the required set and never checked the
  #    sub-index that was meant to own them.
  _scaffold delegated
  printf '# A\n' > "$tmp/delegated/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n' \
    > "$tmp/delegated/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/delegated/docs/guide/tutorial/01-x.md"
  printf '# T2\n' > "$tmp/delegated/docs/guide/tutorial/02-unlisted.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/delegated/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/delegated/README.md"
  _commit delegated
  _case "unlisted page in a delegated subdir fails" 1 delegated

  # 10. The same corpus passes once the sub-index lists it — a sub-index link
  #     resolves against its OWN directory, not against docs/guide/.
  _scaffold delegated_ok
  printf '# A\n' > "$tmp/delegated_ok/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n2. [Two](02-listed.md)\n' \
    > "$tmp/delegated_ok/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/delegated_ok/docs/guide/tutorial/01-x.md"
  printf '# T2\n' > "$tmp/delegated_ok/docs/guide/tutorial/02-listed.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/delegated_ok/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/delegated_ok/README.md"
  _commit delegated_ok
  _case "delegated page listed in its sub-index passes" 0 delegated_ok

  # 11. A README that MENTIONS the index without linking it fails. The
  #     substring check this replaced passed on exactly this corpus.
  _scaffold mention
  printf '# A\n' > "$tmp/mention/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' > "$tmp/mention/docs/guide/index.md"
  printf 'The guide index lives at `docs/guide/index.md` somewhere.\n' \
    > "$tmp/mention/README.md"
  _commit mention
  _case "README mention without a link fails" 1 mention

  # 12. An entry commented out in HTML is not an entry — readers cannot see or
  #     follow it. Parking one this way used to keep an unlisted page green.
  _scaffold commented
  printf '# A\n' > "$tmp/commented/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/commented/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n<!-- - [B](beta.md) -->\n' \
    > "$tmp/commented/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/commented/README.md"
  _commit commented
  _case "commented-out entry does not count" 1 commented

  # 13. A multi-line comment blanks without shifting the line numbers the
  #     remaining defects are reported at.
  _scaffold multiline
  printf '# A\n' > "$tmp/multiline/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/multiline/docs/guide/beta.md"
  printf '# Guide\n\n<!--\nparked:\n- [B](beta.md)\n-->\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/multiline/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/multiline/README.md"
  _commit multiline
  _case "multi-line commented entry does not count" 1 multiline

  # 14. A tilde fence hides its links too. Fence state used to toggle on
  #     backticks only, so a valid `~~~markdown` example counted as entries.
  _scaffold tilde
  printf '# A\n' > "$tmp/tilde/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/tilde/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n~~~markdown\n- [B](beta.md)\n~~~\n' \
    > "$tmp/tilde/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/tilde/README.md"
  _commit tilde
  _case "tilde-fenced entry does not count" 1 tilde

  # 15. A backtick fence nested inside a tilde fence must not close it early.
  _scaffold nested_fence
  printf '# A\n' > "$tmp/nested_fence/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/nested_fence/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n~~~markdown\n```\n- [B](beta.md)\n```\n~~~\n' \
    > "$tmp/nested_fence/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/nested_fence/README.md"
  _commit nested_fence
  _case "backtick fence inside a tilde fence stays hidden" 1 nested_fence

  # 16. The README link check reads the same reduction: a fenced example is
  #     not a clickable link to the index.
  _scaffold readme_fence
  printf '# A\n' > "$tmp/readme_fence/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/readme_fence/docs/guide/index.md"
  printf 'Docs\n\n```markdown\n[Guide](docs/guide/index.md)\n```\n' \
    > "$tmp/readme_fence/README.md"
  _commit readme_fence
  _case "README link only inside a fence fails" 1 readme_fence

  # 17. An inline code span is not a link either.
  _scaffold inline
  printf '# A\n' > "$tmp/inline/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/inline/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- write it as `[B](beta.md)`\n' \
    > "$tmp/inline/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/inline/README.md"
  _commit inline
  _case "inline-code entry does not count" 1 inline

  # 18. A four-space indented code block hides its links too.
  _scaffold indented
  printf '# A\n' > "$tmp/indented/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/indented/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\nFor example:\n\n    - [B](beta.md)\n' \
    > "$tmp/indented/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/indented/README.md"
  _commit indented
  _case "indented-code entry does not count" 1 indented

  # 19. A nested row is NOT an entry: `ENTRY` requires column zero. The page
  #     is then reported as listed nowhere, which is the loud direction — the
  #     index is told its row is malformed rather than half-checked.
  _scaffold nested_list
  printf '# A\n' > "$tmp/nested_list/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/nested_list/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n    - [B](beta.md)\n' \
    > "$tmp/nested_list/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/nested_list/README.md"
  _commit nested_list
  _case "nested row is not an entry" 1 nested_list

  # 20. A page listed in BOTH the top-level index and the sub-index that owns
  #     it. Each file's own tally shows one hit, so this is invisible from
  #     inside either of them.
  _scaffold cross_dup
  printf '# A\n' > "$tmp/cross_dup/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n' \
    > "$tmp/cross_dup/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/cross_dup/docs/guide/tutorial/01-x.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n- [One](tutorial/01-x.md)\n' \
    > "$tmp/cross_dup/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/cross_dup/README.md"
  _commit cross_dup
  _case "page listed in two indexes fails" 1 cross_dup

  # 21. A PROSE cross-reference is not an entry and must not trip rule 1. The
  #     real `tutorial/index.md` carries four of these; counting them would
  #     flag ordinary writing as a duplicate listing.
  _scaffold prose_xref
  printf '# A\n' > "$tmp/prose_xref/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n\nSee the [A guide](../alpha.md) when you finish.\n' \
    > "$tmp/prose_xref/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/prose_xref/docs/guide/tutorial/01-x.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/prose_xref/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/prose_xref/README.md"
  _commit prose_xref
  _case "prose cross-reference is not an entry" 0 prose_xref

  # 22. A code block NESTED IN A LIST. Under `- Example:` (content indent 2) a
  #     block starts at six spaces. Treating "inside a list" as "no code blocks
  #     here" let this count as an entry.
  _scaffold list_code
  printf '# A\n' > "$tmp/list_code/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/list_code/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- Example:\n\n      - [B](beta.md)\n' \
    > "$tmp/list_code/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/list_code/README.md"
  _commit list_code
  _case "code block nested in a list does not count" 1 list_code

  # 23. Same at a deeper indent, and after a blank line: still not column
  #     zero, so still not an entry, and still reported rather than ignored.
  _scaffold list_nested_deep
  printf '# A\n' > "$tmp/list_nested_deep/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/list_nested_deep/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n    - [B](beta.md)\n' \
    > "$tmp/list_nested_deep/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/list_nested_deep/README.md"
  _commit list_nested_deep
  _case "nested row after a blank line is not an entry" 1 list_nested_deep

  # 24. An image is not a navigable link, in an index...
  _scaffold image_entry
  printf '# A\n' > "$tmp/image_entry/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/image_entry/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- ![preview](beta.md)\n' \
    > "$tmp/image_entry/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/image_entry/README.md"
  _commit image_entry
  _case "image entry does not count" 1 image_entry

  # 25. ...nor in README.md.
  _scaffold image_readme
  printf '# A\n' > "$tmp/image_readme/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/image_readme/docs/guide/index.md"
  printf '![Guide](docs/guide/index.md)\n' > "$tmp/image_readme/README.md"
  _commit image_readme
  _case "image in README is not an index link" 1 image_readme

  # 26. A tab-indented row is not at column zero, so not an entry.
  _scaffold tab_indent
  printf '# A\n' > "$tmp/tab_indent/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/tab_indent/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n\t- [B](beta.md)\n' \
    > "$tmp/tab_indent/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/tab_indent/README.md"
  _commit tab_indent
  _case "tab-indented row is not an entry" 1 tab_indent

  # 27. `\[B](b.md)` renders literal text, so it indexes nothing...
  _scaffold escaped
  printf '# A\n' > "$tmp/escaped/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/escaped/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- \\[B](beta.md)\n' \
    > "$tmp/escaped/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/escaped/README.md"
  _commit escaped
  _case "escaped bracket is not an entry" 1 escaped

  # 28. ...and does not reach the index from README.md either.
  _scaffold escaped_readme
  printf '# A\n' > "$tmp/escaped_readme/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/escaped_readme/docs/guide/index.md"
  printf '\\[Guide](docs/guide/index.md)\n' > "$tmp/escaped_readme/README.md"
  _commit escaped_readme
  _case "escaped link in README is not an index link" 1 escaped_readme

  # 29. A raw HTML block renders its contents literally, so a row-shaped line
  #     inside one is not an entry even at column zero.
  _scaffold raw_html
  printf '# A\n' > "$tmp/raw_html/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/raw_html/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n<pre>\n- [B](beta.md)\n</pre>\n' \
    > "$tmp/raw_html/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/raw_html/README.md"
  _commit raw_html
  _case "row inside a raw HTML block is not an entry" 1 raw_html

  # 30. ...but a `<details>` wrapper closed by a blank line must not swallow
  #     the entries that follow it. Over-blanking deletes real rows, which is
  #     the direction that makes this gate quieter rather than louder.
  _scaffold html_then_entries
  printf '# A\n' > "$tmp/html_then_entries/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/html_then_entries/docs/guide/beta.md"
  printf '# Guide\n\n<details><summary>note</summary>\n\n## S\n\n- [A](alpha.md)\n- [B](beta.md)\n' \
    > "$tmp/html_then_entries/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/html_then_entries/README.md"
  _commit html_then_entries
  _case "HTML block ends at a blank line" 0 html_then_entries

  # 31. A raw HTML opener may be indented up to three spaces — the same
  #     allowance `FENCE` carries — and still opens a block.
  _scaffold html_indented
  printf '# A\n' > "$tmp/html_indented/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/html_indented/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n <pre>\n- [B](beta.md)\n</pre>\n' \
    > "$tmp/html_indented/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/html_indented/README.md"
  _commit html_indented
  _case "indented raw HTML opener still opens a block" 1 html_indented

  # 32. Declaration-style blocks — `<![CDATA[`, `<?`, `<!DOCTYPE` — are not
  #     tags, so the tag-name pattern never saw them as blocks.
  _scaffold cdata
  printf '# A\n' > "$tmp/cdata/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/cdata/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n<![CDATA[\n- [B](beta.md)\n]]>\n' \
    > "$tmp/cdata/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/cdata/README.md"
  _commit cdata
  _case "row inside a CDATA block is not an entry" 1 cdata

  # 33. A one-line `<pre>…</pre>` closes on its own line and must not swallow
  #     the rows after it — the over-blanking direction again.
  _scaffold pre_oneline
  printf '# A\n' > "$tmp/pre_oneline/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/pre_oneline/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n<pre>sample</pre>\n\n- [A](alpha.md)\n- [B](beta.md)\n' \
    > "$tmp/pre_oneline/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/pre_oneline/README.md"
  _commit pre_oneline
  _case "one-line <pre> does not swallow later rows" 0 pre_oneline

  # 34. The README scan is the one caller that still matches LINKS rather than
  #     the entry shape, so it needs inline code blanked. The redesign dropped
  #     that pass and regressed exactly this.
  _scaffold inline_readme
  printf '# A\n' > "$tmp/inline_readme/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/inline_readme/docs/guide/index.md"
  printf 'Write it as `[Guide](docs/guide/index.md)` in your docs.\n' \
    > "$tmp/inline_readme/README.md"
  _commit inline_readme
  _case "inline-code link in README is not an index link" 1 inline_readme

  # 35. A code span may wrap across lines, and its contents are still literal.
  _scaffold span_wrap
  printf '# A\n' > "$tmp/span_wrap/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/span_wrap/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\nwrite `like this:\n- [B](beta.md)` in docs\n' \
    > "$tmp/span_wrap/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/span_wrap/README.md"
  _commit span_wrap
  _case "multi-line code span hides its rows" 1 span_wrap

  # 36. An UNMATCHED backtick is literal text in CommonMark and must blank
  #     nothing. A greedy backtick-to-backtick pattern would instead swallow
  #     the rows after it — over-blanking, the direction that makes this gate
  #     quieter rather than louder.
  _scaffold stray_backtick
  printf '# A\n' > "$tmp/stray_backtick/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/stray_backtick/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n100%% of the `budget is spent\n\n- [A](alpha.md)\n- [B](beta.md)\n' \
    > "$tmp/stray_backtick/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/stray_backtick/README.md"
  _commit stray_backtick
  _case "unmatched backtick blanks nothing" 0 stray_backtick

  # 37. A link nested in image ALT TEXT is plain text, not a link. The `!`
  #     lookbehind could not see it, because the inner bracket does not follow
  #     a `!`.
  _scaffold image_alt
  printf '# A\n' > "$tmp/image_alt/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/image_alt/docs/guide/index.md"
  printf '![alt [Guide](docs/guide/index.md)](preview.png)\n' \
    > "$tmp/image_alt/README.md"
  _commit image_alt
  _case "link inside image alt text is not an index link" 1 image_alt

  # 38. ...but a badge — a link WRAPPING an image — still counts. README.md
  #     carries five of those, so blanking the image must leave the enclosing
  #     link alone.
  _scaffold badge_link
  printf '# A\n' > "$tmp/badge_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/badge_link/docs/guide/index.md"
  printf '[![CI](badge.svg)](https://ci.example)\n\n[![docs](d.svg)](docs/guide/index.md)\n' \
    > "$tmp/badge_link/README.md"
  _commit badge_link
  _case "badge link wrapping an image still counts" 0 badge_link

  # 39. An inline HTML attribute is attribute text, not a link.
  _scaffold inline_tag
  printf '# A\n' > "$tmp/inline_tag/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/inline_tag/docs/guide/index.md"
  printf 'Docs <span title="[Guide](docs/guide/index.md)">here</span>.\n' \
    > "$tmp/inline_tag/README.md"
  _commit inline_tag
  _case "link in an inline HTML attribute is not an index link" 1 inline_tag

  # 40. ...but a real link sitting NEXT TO inline HTML still counts. Blanking
  #     a tag must not take the sentence around it.
  _scaffold inline_tag_ok
  printf '# A\n' > "$tmp/inline_tag_ok/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/inline_tag_ok/docs/guide/index.md"
  printf 'See <b>the</b> [Guide index](docs/guide/index.md) for everything.\n' \
    > "$tmp/inline_tag_ok/README.md"
  _commit inline_tag_ok
  _case "link beside inline HTML still counts" 0 inline_tag_ok

  # 41. A literal comment opener inside a code span is code, not a comment.
  #     Blanking comments before code spans let it swallow the rows after it
  #     and fail a perfectly good index.
  _scaffold comment_in_span
  printf '# A\n' > "$tmp/comment_in_span/docs/guide/alpha.md"
  printf '# Guide\n\nWrite `<!--` literally.\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/comment_in_span/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/comment_in_span/README.md"
  _commit comment_in_span
  _case "comment opener inside a code span is not a comment" 0 comment_in_span

  # 42. ...and the mirror, which nobody reported: a lone backtick INSIDE a
  #     comment must not pair with one after it. Blanking code spans first
  #     would have broken this exactly as badly.
  _scaffold backtick_in_comment
  printf '# A\n' > "$tmp/backtick_in_comment/docs/guide/alpha.md"
  printf '# Guide\n\n<!-- note: ` -->\n\n## S\n\n- [A](alpha.md)\n\nsee `x` here\n' \
    > "$tmp/backtick_in_comment/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/backtick_in_comment/README.md"
  _commit backtick_in_comment
  _case "backtick inside a comment does not open a span" 0 backtick_in_comment

  # 43. A URI autolink renders as a LINK, not a raw HTML block. Treating it as
  #     a block opener blanked every row to the next blank line.
  _scaffold autolink
  printf '# A\n' > "$tmp/autolink/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n<https://example.com>\n- [A](alpha.md)\n' \
    > "$tmp/autolink/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/autolink/README.md"
  _commit autolink
  _case "autolink is not an HTML block opener" 0 autolink

  # 44. ...but a real `<div>` at line start still opens one.
  _scaffold div_block
  printf '# A\n' > "$tmp/div_block/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/div_block/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n<div>\n- [B](beta.md)\n</div>\n' \
    > "$tmp/div_block/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/div_block/README.md"
  _commit div_block
  _case "div still opens an HTML block" 1 div_block

  # 45. A reference-style image is still an image, so a link in its label is
  #     alt text: `![alt [Guide](x)][ref]`.
  _scaffold ref_image
  printf '# A\n' > "$tmp/ref_image/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_image/docs/guide/index.md"
  printf '![alt [Guide](docs/guide/index.md)][preview]\n\n[preview]: p.png\n' \
    > "$tmp/ref_image/README.md"
  _commit ref_image
  _case "link inside a reference image label is not a link" 1 ref_image

  # 46. ...but a bare SHORTCUT `![alt [x](a.md)]` is only an image when a
  #     reference definition exists. Without one it renders as literal text
  #     around a REAL link, so blanking it would delete that link.
  _scaffold shortcut_image
  printf '# A\n' > "$tmp/shortcut_image/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/shortcut_image/docs/guide/index.md"
  printf '![see [Guide](docs/guide/index.md)]\n' \
    > "$tmp/shortcut_image/README.md"
  _commit shortcut_image
  _case "shortcut image label keeps its real link" 0 shortcut_image

  # 47. An inline tag may wrap across lines; its attributes are still not
  #     links.
  _scaffold multiline_tag
  printf '# A\n' > "$tmp/multiline_tag/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/multiline_tag/docs/guide/index.md"
  printf 'Docs <span\ntitle="[Guide](docs/guide/index.md)">here</span>\n' \
    > "$tmp/multiline_tag/README.md"
  _commit multiline_tag
  _case "multi-line inline tag hides its attributes" 1 multiline_tag

  # 48. ...and a stray `<` with no `>` before the next blank line blanks
  #     nothing, so the link after it survives.
  _scaffold stray_lt
  printf '# A\n' > "$tmp/stray_lt/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/stray_lt/docs/guide/index.md"
  printf 'A <b is unfinished\n\nSee [Guide](docs/guide/index.md).\n' \
    > "$tmp/stray_lt/README.md"
  _commit stray_lt
  _case "unterminated tag blanks nothing" 0 stray_lt

  # 49. Indented code in README. `ENTRY` is column-anchored so this never
  #     mattered for rows, but the README scan matches links anywhere — and
  #     the single-scan rewrite dropped the pass on exactly that reasoning.
  _scaffold readme_indent
  printf '# A\n' > "$tmp/readme_indent/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/readme_indent/docs/guide/index.md"
  printf 'Docs:\n\n    [Guide](docs/guide/index.md)\n' \
    > "$tmp/readme_indent/README.md"
  _commit readme_indent
  _case "indented-code link in README is not an index link" 1 readme_indent

  # 50. ...but four spaces under a LIST ITEM is a continuation, not code, so
  #     a link there still counts. Over-blanking it would fail a good README.
  _scaffold readme_list_cont
  printf '# A\n' > "$tmp/readme_list_cont/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/readme_list_cont/docs/guide/index.md"
  printf 'Docs\n\n- a bullet\n    continued [Guide](docs/guide/index.md)\n' \
    > "$tmp/readme_list_cont/README.md"
  _commit readme_list_cont
  _case "list continuation is not indented code" 0 readme_list_cont

  # 51. A backslash-escaped backtick is literal, so it neither opens nor
  #     closes a span; pairing it with a real delimiter left the span visible.
  _scaffold escaped_backtick
  printf '# A\n' > "$tmp/escaped_backtick/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/escaped_backtick/docs/guide/index.md"
  printf 'Escaped \\` then `[Guide](docs/guide/index.md)`\n' \
    > "$tmp/escaped_backtick/README.md"
  _commit escaped_backtick
  _case "escaped backtick does not open a span" 1 escaped_backtick

  # 52. A quoted `>` inside an attribute does not end the tag.
  _scaffold quoted_gt
  printf '# A\n' > "$tmp/quoted_gt/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_gt/docs/guide/index.md"
  printf 'Docs <span title="not a link > [Guide](docs/guide/index.md)">t</span>\n' \
    > "$tmp/quoted_gt/README.md"
  _commit quoted_gt
  _case "quoted > does not end an inline tag" 1 quoted_gt

  # 53. A REFERENCE link reaches the index perfectly well. Resolving only
  #     inline destinations failed a README that was not broken.
  _scaffold readme_ref_link
  printf '# A\n' > "$tmp/readme_ref_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/readme_ref_link/docs/guide/index.md"
  printf 'See [Guide][catalog].\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/readme_ref_link/README.md"
  _commit readme_ref_link
  _case "reference link in README reaches the index" 0 readme_ref_link

  # 54. ...but a definition nothing references is not a link.
  _scaffold readme_unused_def
  printf '# A\n' > "$tmp/readme_unused_def/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/readme_unused_def/docs/guide/index.md"
  printf 'Nothing links it.\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/readme_unused_def/README.md"
  _commit readme_unused_def
  _case "unreferenced definition is not a link" 1 readme_unused_def

  # 55. A generic tag with text after it is a PARAGRAPH, not a raw block. The
  #     single-scan rewrite blanked the whole line and failed a good README.
  _scaffold inline_span_line
  printf '# A\n' > "$tmp/inline_span_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/inline_span_line/docs/guide/index.md"
  printf '<span>Docs:</span> [Guide](docs/guide/index.md)\n' \
    > "$tmp/inline_span_line/README.md"
  _commit inline_span_line
  _case "tag with text after it is not a block opener" 0 inline_span_line

  # 56. A tab indents to four columns, so a tab-indented line is code.
  _scaffold tab_code
  printf '# A\n' > "$tmp/tab_code/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/tab_code/docs/guide/index.md"
  printf 'Docs:\n\n\t[Guide](docs/guide/index.md)\n' > "$tmp/tab_code/README.md"
  _commit tab_code
  _case "tab-indented README line is code" 1 tab_code

  # 57. Backslash escapes do NOT apply inside a code span, so a closer
  #     preceded by `\` still closes it. Only the opener can be escaped away.
  _scaffold span_close_escape
  printf '# A\n' > "$tmp/span_close_escape/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/span_close_escape/docs/guide/index.md"
  printf 'Code `[Guide](docs/guide/index.md)\\`\n' \
    > "$tmp/span_close_escape/README.md"
  _commit span_close_escape
  _case "backslash before a span closer still closes it" 1 span_close_escape

  # 58. `[A](<alpha.md>)` is a valid destination form. The angle form was
  #     blanked as inline HTML, so a clickable link — and a real index ROW
  #     written that way — was reported missing.
  _scaffold angle_dest_row
  printf '# A\n' > "$tmp/angle_dest_row/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](<alpha.md>)\n' \
    > "$tmp/angle_dest_row/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/angle_dest_row/README.md"
  _commit angle_dest_row
  _case "angle-bracketed destination resolves in a row" 0 angle_dest_row

  # 59. ...and in README.
  _scaffold angle_dest_readme
  printf '# A\n' > "$tmp/angle_dest_readme/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/angle_dest_readme/docs/guide/index.md"
  printf 'See [Guide](<docs/guide/index.md>)\n' \
    > "$tmp/angle_dest_readme/README.md"
  _commit angle_dest_readme
  _case "angle-bracketed destination resolves in README" 0 angle_dest_readme

  # 60. A reference label folds case AND collapses internal whitespace.
  _scaffold ref_label_ws
  printf '# A\n' > "$tmp/ref_label_ws/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_label_ws/docs/guide/index.md"
  printf 'See [Guide][guide   catalog].\n\n[guide catalog]: docs/guide/index.md\n' \
    > "$tmp/ref_label_ws/README.md"
  _commit ref_label_ws
  _case "reference label collapses whitespace" 0 ref_label_ws

  # 61. An HTML block ends at ANY blank line, including one of spaces. Looking
  #     for a literal "\n\n" ran the block to EOF and blanked the link after it.
  _scaffold html_ws_blank
  printf '# A\n' > "$tmp/html_ws_blank/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/html_ws_blank/docs/guide/index.md"
  printf '<div>\nsome html\n   \n[Guide](docs/guide/index.md)\n' \
    > "$tmp/html_ws_blank/README.md"
  _commit html_ws_blank
  _case "HTML block ends at a whitespace-only line" 0 html_ws_blank

  # 62. Indented code may open after ANY non-paragraph line, not only a blank
  #     one — here, straight after a heading.
  _scaffold code_after_heading
  printf '# A\n' > "$tmp/code_after_heading/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/code_after_heading/docs/guide/index.md"
  printf '# Documentation\n    [Guide](docs/guide/index.md)\n' \
    > "$tmp/code_after_heading/README.md"
  _commit code_after_heading
  _case "indented code opens after a heading" 1 code_after_heading

  # 63. ...but it cannot INTERRUPT a paragraph: an indented continuation line
  #     is still paragraph text, and its link still counts.
  _scaffold para_continuation
  printf '# A\n' > "$tmp/para_continuation/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/para_continuation/docs/guide/index.md"
  printf 'Some prose text\n    continued [Guide](docs/guide/index.md)\n' \
    > "$tmp/para_continuation/README.md"
  _commit para_continuation
  _case "indented code cannot interrupt a paragraph" 0 para_continuation

  # 64. An inline link must CLOSE. `[Guide](path` renders as literal text.
  _scaffold unterminated_link
  printf '# A\n' > "$tmp/unterminated_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/unterminated_link/docs/guide/index.md"
  printf 'See [Guide](docs/guide/index.md\n' \
    > "$tmp/unterminated_link/README.md"
  _commit unterminated_link
  _case "unterminated link is not a link" 1 unterminated_link

  # 65. A link may carry a TITLE, and an index row written that way is a row.
  _scaffold row_link_title
  printf '# A\n' > "$tmp/row_link_title/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md "Alpha guide")\n' \
    > "$tmp/row_link_title/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/row_link_title/README.md"
  _commit row_link_title
  _case "row link with a title is still a row" 0 row_link_title

  # 66. ...but a link-shaped string INSIDE a title is not a link. Ending the
  #     outer link at the `)` within the title exposed it.
  _scaffold link_in_title
  printf '# A\n' > "$tmp/link_in_title/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/link_in_title/docs/guide/index.md"
  printf '[Other](other.md "title ) [Guide](docs/guide/index.md)")\n' \
    > "$tmp/link_in_title/README.md"
  _commit link_in_title
  _case "link inside a title is not a link" 1 link_in_title

  # 67. A thematic break ends the paragraph, so an indented line after one is
  #     code. Tracking only headings, fences and HTML missed it.
  _scaffold thematic_break
  printf '# A\n' > "$tmp/thematic_break/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/thematic_break/docs/guide/index.md"
  printf -- '---\n    [Guide](docs/guide/index.md)\n' \
    > "$tmp/thematic_break/README.md"
  _commit thematic_break
  _case "indented code opens after a thematic break" 1 thematic_break

  # 68. A block tag's NAME ends at whitespace, `>`, `/>` or end of line.
  #     `<div.class` is an ordinary paragraph, not raw HTML, and reading it
  #     as the tag `div` blanked the real link under it.
  _scaffold tag_delimiter
  printf '# A\n' > "$tmp/tag_delimiter/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/tag_delimiter/docs/guide/index.md"
  printf '<div.class\n[Guide](docs/guide/index.md)\n' \
    > "$tmp/tag_delimiter/README.md"
  _commit tag_delimiter
  _case "a tag name needs a delimiter to open a block" 0 tag_delimiter

  # 69. ...and the other direction: a REAL block opener still swallows what
  #     follows it, so case 68 cannot have been bought by disabling the rule.
  _scaffold tag_delimiter_real
  printf '# A\n' > "$tmp/tag_delimiter_real/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/tag_delimiter_real/docs/guide/index.md"
  printf '<div class="x">\n[Guide](docs/guide/index.md)\n' \
    > "$tmp/tag_delimiter_real/README.md"
  _commit tag_delimiter_real
  _case "a real block opener still blanks its block" 1 tag_delimiter_real

  # 70. A link-shaped REFERENCE inside a link's title is title text, not
  #     navigation. The inline pass already consumed that span, so the
  #     reference pass must not read back into it.
  _scaffold ref_in_title
  printf '# A\n' > "$tmp/ref_in_title/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_in_title/docs/guide/index.md"
  printf '[Other](other.md "see [Guide][catalog]")\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/ref_in_title/README.md"
  _commit ref_in_title
  _case "reference inside a title is not a link" 1 ref_in_title

  # 71. ...and the guard: blanking those spans must not eat a GENUINE
  #     reference link sitting next to one. Case 70 is worthless without it.
  _scaffold ref_beside_link
  printf '# A\n' > "$tmp/ref_beside_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_beside_link/docs/guide/index.md"
  printf '[X](y.md) [Guide][catalog]\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/ref_beside_link/README.md"
  _commit ref_beside_link
  _case "a reference beside an inline link still counts" 0 ref_beside_link

  # 72. CORRECTED in round 44. This case used to expect a PASS, and it was
  #     passing for the wrong reason: masking the link turned its lines into
  #     spaces, `_starts_block` read that as a blank line, and the glued
  #     definition resolved. Both cmark-gfm and markdown-it keep that line
  #     inside the paragraph and render the reference as plain text —
  #
  #       <p><a href="y.md" title="a b">X</a> [catalog]: docs/guide/index.md</p>
  #       <p>[Guide][catalog]</p>
  #
  #     — so the index is NOT reachable and the run must FAIL. The case had
  #     encoded the bug it was sitting next to.
  _scaffold ref_multiline_link
  printf '# A\n' > "$tmp/ref_multiline_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_multiline_link/docs/guide/index.md"
  printf '[X](y.md "a\nb")\n[catalog]: docs/guide/index.md\n\n[Guide][catalog]\n' \
    > "$tmp/ref_multiline_link/README.md"
  _commit ref_multiline_link
  _case "a definition glued to a multiline link defines nothing" 1 \
    ref_multiline_link

  # 72b. The newline guard the case above was written for, now with an
  #      expectation that holds: after a real blank line the definition IS
  #      real, which still fails if blanking a multi-line link span ate its
  #      newlines and dropped the `^` the definition scan anchors on.
  _scaffold ref_multiline_gap
  printf '# A\n' > "$tmp/ref_multiline_gap/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_multiline_gap/docs/guide/index.md"
  printf '[X](y.md "a\nb")\n\n[catalog]: docs/guide/index.md\n\n[Guide][catalog]\n' \
    > "$tmp/ref_multiline_gap/README.md"
  _commit ref_multiline_gap
  _case "a definition after a multiline link survives a gap" 0 ref_multiline_gap

  # 73. A destination may carry BALANCED parentheses. Ending the span at the
  #     first `)` left the title for the reference scan to misread.
  _scaffold balanced_dest
  printf '# A\n' > "$tmp/balanced_dest/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/balanced_dest/docs/guide/index.md"
  printf '[Other](other(foo).md "see [Guide][catalog]")\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/balanced_dest/README.md"
  _commit balanced_dest
  _case "balanced parens do not end a link early" 1 balanced_dest

  # 74. ...and the guard: a real link whose destination carries balanced
  #     parens is still a link, so case 73 is not bought by rejecting them.
  _scaffold balanced_dest_real
  printf '# A\n' > "$tmp/balanced_dest_real/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha(1).md)\n' \
    > "$tmp/balanced_dest_real/docs/guide/index.md"
  mv "$tmp/balanced_dest_real/docs/guide/alpha.md" \
     "$tmp/balanced_dest_real/docs/guide/alpha(1).md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/balanced_dest_real/README.md"
  _commit balanced_dest_real
  _case "a balanced-paren destination still resolves" 0 balanced_dest_real

  # 75. A REFERENCE-style row is a row. Recognising only the inline spelling
  #     reported the page as listed nowhere — a false failure on a perfectly
  #     ordinary index.
  _scaffold ref_row
  printf '# A\n' > "$tmp/ref_row/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][alpha]\n\n[alpha]: alpha.md\n' \
    > "$tmp/ref_row/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ref_row/README.md"
  _commit ref_row
  _case "a reference-style row is an entry" 0 ref_row

  # 76. The reason 75 matters more than convenience: an inline row and a
  #     reference row for the SAME page are two entries, and the "listed
  #     exactly once" guarantee has to see both.
  _scaffold ref_row_dup
  printf '# A\n' > "$tmp/ref_row_dup/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [A again][alpha]\n\n[alpha]: alpha.md\n' \
    > "$tmp/ref_row_dup/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ref_row_dup/README.md"
  _commit ref_row_dup
  _case "inline plus reference row is a duplicate" 1 ref_row_dup

  # 77. An UNDEFINED label is not a link, so the row is not an entry and the
  #     page is reported unlisted — the safe direction, and the one an
  #     unparseable inline row already takes.
  _scaffold ref_row_undef
  printf '# A\n' > "$tmp/ref_row_undef/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][nosuch]\n' \
    > "$tmp/ref_row_undef/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ref_row_undef/README.md"
  _commit ref_row_undef
  _case "an undefined label is not an entry" 1 ref_row_undef

  # 78. A row whose link does not start the content is prose, not an entry.
  #     Reference rows must not widen what counts as a row.
  _scaffold ref_row_prose
  printf '# A\n' > "$tmp/ref_row_prose/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- see the [alpha] page\n\n[alpha]: alpha.md\n' \
    > "$tmp/ref_row_prose/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ref_row_prose/README.md"
  _commit ref_row_prose
  _case "a mid-row reference is prose, not an entry" 0 ref_row_prose

  # 79. A definition-looking line inside a multi-line link TITLE is title
  #     text. Reading it as a definition let an undefined reference resolve,
  #     leaving the page unfindable while the gate passed.
  _scaffold defn_in_title
  printf '# A\n' > "$tmp/defn_in_title/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[Other](other.md "title\n[a]: alpha.md\n")\n' \
    > "$tmp/defn_in_title/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_in_title/README.md"
  _commit defn_in_title
  _case "a definition inside a title is not a definition" 1 defn_in_title

  # 80. CommonMark resolves a reference against the FIRST definition of a
  #     label. Keeping the last let a row resolve to a target the reader
  #     never reaches.
  _scaffold defn_first_wins
  printf '# A\n' > "$tmp/defn_first_wins/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]: ghost.md\n[a]: alpha.md\n' \
    > "$tmp/defn_first_wins/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_first_wins/README.md"
  _commit defn_first_wins
  _case "the first definition of a label wins" 1 defn_first_wins

  # 81. ...and the guard: one definition, and a second that is merely later,
  #     must still resolve. Case 80 is not bought by rejecting duplicates.
  _scaffold defn_first_good
  printf '# A\n' > "$tmp/defn_first_good/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]: alpha.md\n[a]: ghost.md\n' \
    > "$tmp/defn_first_good/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_first_good/README.md"
  _commit defn_first_good
  _case "a later duplicate definition is ignored" 0 defn_first_good

  # 82. `[a]: alpha.md#section` names the page `alpha.md`. Comparing the
  #     fragment as part of the filename rejected a valid row.
  _scaffold defn_fragment
  printf '# A\n' > "$tmp/defn_fragment/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]: alpha.md#section\n' \
    > "$tmp/defn_fragment/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_fragment/README.md"
  _commit defn_fragment
  _case "a fragment in a definition is stripped" 0 defn_fragment

  # 83. An angle-bracketed destination may carry a fragment, and a
  #     definition's target may be angle-bracketed at all. Both were read as
  #     inline HTML and blanked, so the row resolved to nothing. Neither was
  #     reported by review — this pins what probing turned up.
  _scaffold angle_fragment
  printf '# A\n' > "$tmp/angle_fragment/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/angle_fragment/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](<alpha.md#top>)\n- [B][b]\n\n[b]: <beta.md#top>\n' \
    > "$tmp/angle_fragment/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/angle_fragment/README.md"
  _commit angle_fragment
  _case "an angled destination may carry a fragment" 0 angle_fragment

  # 84. An ATX heading needs whitespace after its `#` run. `#not-a-heading`
  #     is a paragraph, and reading it as a heading let the next indented
  #     line open code and swallow a clickable link.
  _scaffold atx_prefix
  printf '# A\n' > "$tmp/atx_prefix/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/atx_prefix/docs/guide/index.md"
  printf '#not-a-heading\n    [Guide](docs/guide/index.md)\n' \
    > "$tmp/atx_prefix/README.md"
  _commit atx_prefix
  _case "a bare # prefix is not a heading" 0 atx_prefix

  # 85. ...and the guard: a REAL heading still ends the paragraph, so 84 is
  #     not bought by forgetting that headings exist.
  _scaffold atx_prefix_real
  printf '# A\n' > "$tmp/atx_prefix_real/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/atx_prefix_real/docs/guide/index.md"
  printf '# Heading\n    [Guide](docs/guide/index.md)\n' \
    > "$tmp/atx_prefix_real/README.md"
  _commit atx_prefix_real
  _case "a real heading still ends a paragraph" 1 atx_prefix_real

  # 86. A reference definition may not cross a blank line. `\s*` did, so a
  #     row resolved through text CommonMark renders as plain characters.
  _scaffold defn_blank_line
  printf '# A\n' > "$tmp/defn_blank_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]:\n\nalpha.md\n' \
    > "$tmp/defn_blank_line/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_blank_line/README.md"
  _commit defn_blank_line
  _case "a definition cannot cross a blank line" 1 defn_blank_line

  # 87. ...and the guard: ONE line ending between the colon and the
  #     destination is allowed, and must still resolve.
  _scaffold defn_one_newline
  printf '# A\n' > "$tmp/defn_one_newline/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]:\nalpha.md\n' \
    > "$tmp/defn_one_newline/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_one_newline/README.md"
  _commit defn_one_newline
  _case "a definition may use one line ending" 0 defn_one_newline

  # 88. A type-7 HTML tag cannot interrupt a paragraph. After prose, a lone
  #     `<span>` is inline HTML, so the lines under it are still paragraph
  #     text — blanking them as a raw block swallowed a clickable link.
  _scaffold type7_paragraph
  printf '# A\n' > "$tmp/type7_paragraph/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/type7_paragraph/docs/guide/index.md"
  printf 'Some prose\n<span>\n[Guide](docs/guide/index.md)\n' \
    > "$tmp/type7_paragraph/README.md"
  _commit type7_paragraph
  _case "a type-7 tag cannot interrupt a paragraph" 0 type7_paragraph

  # 89. ...and the guard, twice over: the SAME tag after a blank line does
  #     open a block, and a type-6 tag interrupts a paragraph even though
  #     type 7 cannot. Case 88 is not bought by ignoring HTML blocks.
  _scaffold type7_guard
  printf '# A\n' > "$tmp/type7_guard/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/type7_guard/docs/guide/index.md"
  printf 'Some prose\n\n<span>\n[Guide](docs/guide/index.md)\n' \
    > "$tmp/type7_guard/README.md"
  _commit type7_guard
  _case "a type-7 tag after a blank line opens a block" 1 type7_guard

  _scaffold type6_paragraph
  printf '# A\n' > "$tmp/type6_paragraph/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/type6_paragraph/docs/guide/index.md"
  printf 'Some prose\n<div>\n[Guide](docs/guide/index.md)\n' \
    > "$tmp/type6_paragraph/README.md"
  _commit type6_paragraph
  _case "a type-6 tag does interrupt a paragraph" 1 type6_paragraph

  # 90. Nothing but an optional title may follow a definition's destination.
  #     `[a]: alpha.md trailing garbage` is not a definition, so the row
  #     referencing it renders as plain text.
  _scaffold defn_trailing
  printf '# A\n' > "$tmp/defn_trailing/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]: alpha.md trailing garbage\n' \
    > "$tmp/defn_trailing/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_trailing/README.md"
  _commit defn_trailing
  _case "trailing garbage is not a definition" 1 defn_trailing

  # 91. ...and the guard: a real title is not garbage, in all three
  #     spellings CommonMark allows.
  _scaffold defn_title_ok
  printf '# A\n' > "$tmp/defn_title_ok/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]: alpha.md "Alpha guide"\n' \
    > "$tmp/defn_title_ok/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/defn_title_ok/README.md"
  _commit defn_title_ok
  _case "a definition may carry a title" 0 defn_title_ok

  # 92. A new LEVEL-ONE heading ends the section, so rows appended under it
  #     are under no `## ` and must be reported. A `### ` does not reset.
  _scaffold section_reset
  printf '# A\n' > "$tmp/section_reset/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/section_reset/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n# Appendix\n\n- [B](beta.md)\n' \
    > "$tmp/section_reset/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/section_reset/README.md"
  _commit section_reset
  _case "a level-one heading ends the section" 1 section_reset

  _scaffold section_subheading
  printf '# A\n' > "$tmp/section_subheading/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/section_subheading/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n### Sub\n\n- [B](beta.md)\n' \
    > "$tmp/section_subheading/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/section_subheading/README.md"
  _commit section_subheading
  _case "a level-three subheading keeps the section" 0 section_subheading

  # 93. The type-7 guard from case 88, defeated by the line above it: the
  #     paragraph rule tested `HTML_OPEN` directly, so a tag that did NOT
  #     open a block still ended the paragraph and the indented line under
  #     it became code. The decision is made once now, before either use.
  _scaffold type7_paragraph_state
  printf '# A\n' > "$tmp/type7_paragraph_state/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/type7_paragraph_state/docs/guide/index.md"
  printf 'Some prose\n<span>\n    [Guide](docs/guide/index.md)\n' \
    > "$tmp/type7_paragraph_state/README.md"
  _commit type7_paragraph_state
  _case "a type-7 tag does not end the paragraph" 0 type7_paragraph_state

  # 94. ...and the guard: a type-6 tag DOES end it, so the line under it is
  #     code and its link is not clickable.
  _scaffold type6_paragraph_state
  printf '# A\n' > "$tmp/type6_paragraph_state/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/type6_paragraph_state/docs/guide/index.md"
  printf 'Some prose\n<div>\n    [Guide](docs/guide/index.md)\n' \
    > "$tmp/type6_paragraph_state/README.md"
  _commit type6_paragraph_state
  _case "a type-6 tag does end the paragraph" 1 type6_paragraph_state

  # 95. Every level-one spelling ends the section, not just `# Title`: a
  #     bare `#`, a tab after the `#`, and the Setext `===` form.
  _scaffold section_reset_bare
  printf '# A\n' > "$tmp/section_reset_bare/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/section_reset_bare/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n#\n\n- [B](beta.md)\n' \
    > "$tmp/section_reset_bare/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/section_reset_bare/README.md"
  _commit section_reset_bare
  _case "a bare # ends the section" 1 section_reset_bare

  _scaffold section_reset_setext
  printf '# A\n' > "$tmp/section_reset_setext/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/section_reset_setext/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\nAppendix\n===\n\n- [B](beta.md)\n' \
    > "$tmp/section_reset_setext/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/section_reset_setext/README.md"
  _commit section_reset_setext
  _case "a Setext level-one heading ends the section" 1 section_reset_setext

  # 96. ...and the guard that matters most, because resetting wrongly tells
  #     an index its rows are unplaced when they are not: `===` is only a
  #     heading when a PARAGRAPH sits directly above it. Under a blank line
  #     it is ordinary text.
  _scaffold setext_not_heading
  printf '# A\n' > "$tmp/setext_not_heading/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/setext_not_heading/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n===\n\n- [B](beta.md)\n' \
    > "$tmp/setext_not_heading/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/setext_not_heading/README.md"
  _commit setext_not_heading
  _case "a bare === is text, not a heading" 0 setext_not_heading

  # 97. Escape parity for an image opener. `\\!` is an escaped BACKSLASH
  #     followed by a live `!`, so the image opens and the link inside its
  #     alt text is only alt text.
  _scaffold image_escape_parity
  printf '# A\n' > "$tmp/image_escape_parity/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/image_escape_parity/docs/guide/index.md"
  printf '\\\\![alt [Guide](docs/guide/index.md)](preview.png)\n' \
    > "$tmp/image_escape_parity/README.md"
  _commit image_escape_parity
  _case "two backslashes still open an image" 1 image_escape_parity

  # 98. ...and the guard: ONE backslash does escape the `!`, so what follows
  #     is a link, not an image.
  _scaffold image_escape_single
  printf '# A\n' > "$tmp/image_escape_single/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/image_escape_single/docs/guide/index.md"
  printf '\\![alt](preview.png) [Guide](docs/guide/index.md)\n' \
    > "$tmp/image_escape_single/README.md"
  _commit image_escape_single
  _case "one backslash escapes an image opener" 0 image_escape_single

  # 99. A link may not cross a BLANK line: the blank line ends the
  #     paragraph, so the text renders as literal characters.
  _scaffold link_blank_line
  printf '# A\n' > "$tmp/link_blank_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/link_blank_line/docs/guide/index.md"
  printf '[Guide](docs/guide/index.md\n\n)\n' > "$tmp/link_blank_line/README.md"
  _commit link_blank_line
  _case "a link cannot cross a blank line" 1 link_blank_line

  # 100. ...and the guard: ONE line ending inside a link is fine, so 99 is
  #      not bought by requiring links to sit on a single line.
  _scaffold link_one_newline
  printf '# A\n' > "$tmp/link_one_newline/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/link_one_newline/docs/guide/index.md"
  printf '[Guide](docs/guide/index.md\n)\n' > "$tmp/link_one_newline/README.md"
  _commit link_one_newline
  _case "a link may span one line ending" 0 link_one_newline

  # 101. Escape parity for a LINK opener, the twin of case 97's image. A
  #      one-character lookbehind rejected `\\[Guide](…)`, which is an
  #      escaped BACKSLASH followed by a live link.
  _scaffold link_escape_parity
  printf '# A\n' > "$tmp/link_escape_parity/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/link_escape_parity/docs/guide/index.md"
  printf '\\\\[Guide](docs/guide/index.md)\n' > "$tmp/link_escape_parity/README.md"
  _commit link_escape_parity
  _case "two backslashes leave a live link" 0 link_escape_parity

  # 102. ...and the guard: ONE backslash does escape the `[`, so there is
  #      no link and the index is unreachable.
  _scaffold link_escape_single
  printf '# A\n' > "$tmp/link_escape_single/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/link_escape_single/docs/guide/index.md"
  printf '\\[Guide](docs/guide/index.md)\n' > "$tmp/link_escape_single/README.md"
  _commit link_escape_single
  _case "one backslash escapes a link opener" 1 link_escape_single

  # 103. A literal HTML block owns its whole CLOSING LINE. Stopping at the
  #      tag handed the rest of the line back to the scanner as markdown,
  #      though it renders as raw text.
  _scaffold literal_closing_line
  printf '# A\n' > "$tmp/literal_closing_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/literal_closing_line/docs/guide/index.md"
  printf '<pre></pre> [Guide](docs/guide/index.md)\n' \
    > "$tmp/literal_closing_line/README.md"
  _commit literal_closing_line
  _case "a literal block owns its closing line" 1 literal_closing_line

  # 104. ...and the guard: the block really does END there, so a link on a
  #      LATER line is clickable.
  _scaffold literal_after_close
  printf '# A\n' > "$tmp/literal_after_close/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/literal_after_close/docs/guide/index.md"
  printf '<pre></pre>\n\n[Guide](docs/guide/index.md)\n' \
    > "$tmp/literal_after_close/README.md"
  _commit literal_after_close
  _case "a link after the closing line is clickable" 0 literal_after_close

  # 105. A comment that BEGINS a line is a raw block and owns the line to
  #      its end, terminator included.
  _scaffold comment_closing_line
  printf '# A\n' > "$tmp/comment_closing_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/comment_closing_line/docs/guide/index.md"
  printf '<!-- hidden --> [Guide](docs/guide/index.md)\n' \
    > "$tmp/comment_closing_line/README.md"
  _commit comment_closing_line
  _case "a comment block owns its closing line" 1 comment_closing_line

  # 106. ...and the guard: a comment MID-line is inline and owns only
  #      itself, so a link beside it is still clickable.
  _scaffold comment_inline
  printf '# A\n' > "$tmp/comment_inline/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/comment_inline/docs/guide/index.md"
  printf 'see <!-- x --> [Guide](docs/guide/index.md)\n' \
    > "$tmp/comment_inline/README.md"
  _commit comment_inline
  _case "a mid-line comment keeps the link" 0 comment_inline

  # 107. The declaration block is the third sibling of the same rule, and
  #      was fixed with the other two rather than one round later.
  _scaffold decl_closing_line
  printf '# A\n' > "$tmp/decl_closing_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/decl_closing_line/docs/guide/index.md"
  printf '<!DOCTYPE html> [Guide](docs/guide/index.md)\n' \
    > "$tmp/decl_closing_line/README.md"
  _commit decl_closing_line
  _case "a declaration block owns its closing line" 1 decl_closing_line

  # 108. A reference DEFINITION vanishes from the rendered page, so a link
  #      inside its title is not navigation and not even text.
  _scaffold link_in_defn_title
  printf '# A\n' > "$tmp/link_in_defn_title/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/link_in_defn_title/docs/guide/index.md"
  printf '[other]: other.md "title [Guide](docs/guide/index.md)"\n' \
    > "$tmp/link_in_defn_title/README.md"
  _commit link_in_defn_title
  _case "a link in a definition title is not a link" 1 link_in_defn_title

  # 109. ...and the guard: a definition that RESOLVES to the index, used by
  #      a real reference, still counts. The inline pass reads a blanked
  #      copy; the reference pass needs them intact.
  _scaffold defn_still_resolves
  printf '# A\n' > "$tmp/defn_still_resolves/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_still_resolves/docs/guide/index.md"
  printf '[Guide][c]\n\n[c]: docs/guide/index.md\n' \
    > "$tmp/defn_still_resolves/README.md"
  _commit defn_still_resolves
  _case "a definition used by a reference still counts" 0 defn_still_resolves

  # 110. The same hole on the ENTRY side, which review did not report: a
  #      definition title may span lines, so a row-shaped line inside one
  #      listed a page with text that renders nowhere.
  _scaffold row_in_defn_title
  printf '# A\n' > "$tmp/row_in_defn_title/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/row_in_defn_title/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n[c]: o.md "t\n- [B](beta.md)\n"\n' \
    > "$tmp/row_in_defn_title/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/row_in_defn_title/README.md"
  _commit row_in_defn_title
  _case "a row inside a definition title is not a row" 1 row_in_defn_title

  # 111. A definition's title may begin on the LINE AFTER its destination,
  #      and the span must cover it. Ending at the destination left the
  #      title to be read as markdown, so a row inside it listed a page.
  _scaffold defn_next_line_title
  printf '# A\n' > "$tmp/defn_next_line_title/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/defn_next_line_title/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n[c]: other.md\n"title\n- [B](beta.md)\n"\n' \
    > "$tmp/defn_next_line_title/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/defn_next_line_title/README.md"
  _commit defn_next_line_title
  _case "a next-line title belongs to the definition" 1 defn_next_line_title

  # 112. ...and the guard: a BLANK line ends the definition, so a quoted
  #      line after one is ordinary text and the definition still resolves.
  _scaffold defn_blank_before_title
  printf '# A\n' > "$tmp/defn_blank_before_title/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/defn_blank_before_title/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n- [B](beta.md)\n\n[a]: alpha.md\n\n"T"\n' \
    > "$tmp/defn_blank_before_title/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/defn_blank_before_title/README.md"
  _commit defn_blank_before_title
  _case "a blank line ends the definition" 0 defn_blank_before_title

  # 113. Escape parity for REFERENCE openers. `readable()` already resolved
  #      the run, so re-testing one character rejected `\\[Guide][]` — an
  #      escaped backslash followed by a real reference link.
  _scaffold ref_escape_parity
  printf '# A\n' > "$tmp/ref_escape_parity/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_escape_parity/docs/guide/index.md"
  printf '\\\\[Guide][]\n\n[Guide]: docs/guide/index.md\n' \
    > "$tmp/ref_escape_parity/README.md"
  _commit ref_escape_parity
  _case "two backslashes leave a live reference" 0 ref_escape_parity

  # 114. ...and the guard: one backslash still escapes it away.
  _scaffold ref_escape_single
  printf '# A\n' > "$tmp/ref_escape_single/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_escape_single/docs/guide/index.md"
  printf '\\[Guide][]\n\n[Guide]: docs/guide/index.md\n' \
    > "$tmp/ref_escape_single/README.md"
  _commit ref_escape_single
  _case "one backslash escapes a reference opener" 1 ref_escape_single

  # 115. A label inside a definition's own TITLE is not a use of it. The
  #      inline pass read a blanked copy and the USE scan did not, so a
  #      definition nothing references looked referenced.
  _scaffold label_in_own_title
  printf '# A\n' > "$tmp/label_in_own_title/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/label_in_own_title/docs/guide/index.md"
  printf '[catalog]: docs/guide/index.md "title [catalog]"\n' \
    > "$tmp/label_in_own_title/README.md"
  _commit label_in_own_title
  _case "a label in its own title is not a use" 1 label_in_own_title

  # 116. ...and the guard: a real use elsewhere on the page still counts.
  _scaffold label_used_elsewhere
  printf '# A\n' > "$tmp/label_used_elsewhere/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/label_used_elsewhere/docs/guide/index.md"
  printf 'see [catalog]\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/label_used_elsewhere/README.md"
  _commit label_used_elsewhere
  _case "a real reference use still counts" 0 label_used_elsewhere

  # 117. An autolink's body is a URI, so a label inside it is part of that
  #      URI, not a reference use. Skipping it without blanking left the
  #      text for the label scan to find.
  _scaffold label_in_autolink
  printf '# A\n' > "$tmp/label_in_autolink/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/label_in_autolink/docs/guide/index.md"
  printf '<https://example.com/[catalog]>\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/label_in_autolink/README.md"
  _commit label_in_autolink
  _case "a label inside an autolink is not a use" 1 label_in_autolink

  # 118. ...and the guard that blanking autolinks must not break: an ANGLE
  #      destination is not an autolink, and a real link beside an autolink
  #      is still a link.
  _scaffold autolink_guard
  printf '# A\n' > "$tmp/autolink_guard/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](<alpha.md>)\n' \
    > "$tmp/autolink_guard/docs/guide/index.md"
  printf '<https://example.com> [Guide](docs/guide/index.md)\n' \
    > "$tmp/autolink_guard/README.md"
  _commit autolink_guard
  _case "an autolink hides neither destination nor link" 0 autolink_guard

  # 119. An escape neutralises `<` too. Blanking autolinks (case 117) made
  #      this a false failure the same round it was added, because the
  #      escapable set listed only `[` and `!` — the branch points that
  #      existed when it was written.
  _scaffold escaped_angle
  printf '# A\n' > "$tmp/escaped_angle/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/escaped_angle/docs/guide/index.md"
  printf '\\<https://example.com/[catalog]>\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/escaped_angle/README.md"
  _commit escaped_angle
  _case "an escaped < is not an autolink" 0 escaped_angle

  # 120. ...and the guard: an UNESCAPED `<` still opens one, so 119 is not
  #      bought by giving up on autolinks.
  _scaffold unescaped_angle
  printf '# A\n' > "$tmp/unescaped_angle/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/unescaped_angle/docs/guide/index.md"
  printf '<https://example.com/[catalog]>\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/unescaped_angle/README.md"
  _commit unescaped_angle
  _case "an unescaped < still opens an autolink" 1 unescaped_angle

  # 121. Link TEXT may contain BALANCED brackets. A flat run stopped at the
  #      inner `]`, so `- [A [advanced]](alpha.md)` — an ordinary row —
  #      reported its page as listed nowhere.
  _scaffold nested_brackets
  printf '# A\n' > "$tmp/nested_brackets/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A [advanced]](alpha.md)\n' \
    > "$tmp/nested_brackets/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/nested_brackets/README.md"
  _commit nested_brackets
  _case "balanced brackets in link text are a row" 0 nested_brackets

  # 122. ...and the guard: brackets that do NOT balance are not link text,
  #      so the row is reported rather than quietly half-read.
  _scaffold unbalanced_brackets
  printf '# A\n' > "$tmp/unbalanced_brackets/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A [x](alpha.md)\n' \
    > "$tmp/unbalanced_brackets/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/unbalanced_brackets/README.md"
  _commit unbalanced_brackets
  _case "unbalanced brackets are not link text" 1 unbalanced_brackets

  # 123. A title's own delimiter may be ESCAPED and is then content. Ending
  #      at it handed the rest of the title back as markdown, exposing a
  #      link that renders nowhere.
  _scaffold escaped_title_delim
  printf '# A\n' > "$tmp/escaped_title_delim/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/escaped_title_delim/docs/guide/index.md"
  printf '[Other](other.md "title \\" ) [Guide](docs/guide/index.md)")\n' \
    > "$tmp/escaped_title_delim/README.md"
  _commit escaped_title_delim
  _case "an escaped delimiter is title content" 1 escaped_title_delim

  # 124. ...and the guard: an ordinary title still closes at its own
  #      unescaped delimiter, so 123 is not bought by swallowing the line.
  _scaffold plain_title_closes
  printf '# A\n' > "$tmp/plain_title_closes/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/plain_title_closes/docs/guide/index.md"
  printf '[Guide](docs/guide/index.md "T") and more text\n' \
    > "$tmp/plain_title_closes/README.md"
  _commit plain_title_closes
  _case "an ordinary title still closes" 0 plain_title_closes

  # 125. Balanced text in a REFERENCE row. Case 121 gave inline rows this
  #      grammar and left `ENTRY_REF` on a flat run, so the reference
  #      spelling of the same row still reported its page unlisted.
  _scaffold nested_brackets_ref
  printf '# A\n' > "$tmp/nested_brackets_ref/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A [advanced]][alpha]\n\n[alpha]: alpha.md\n' \
    > "$tmp/nested_brackets_ref/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/nested_brackets_ref/README.md"
  _commit nested_brackets_ref
  _case "balanced text in a reference row is a row" 0 nested_brackets_ref

  # 126. ...and the guard: a link LABEL may not hold unescaped brackets, so
  #      an undefined one is still not an entry. Text nests; labels do not.
  _scaffold nested_label_undefined
  printf '# A\n' > "$tmp/nested_label_undefined/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A [advanced]][nosuch]\n' \
    > "$tmp/nested_label_undefined/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/nested_label_undefined/README.md"
  _commit nested_label_undefined
  _case "a nested-text row with no definition is not an entry" 1 nested_label_undefined

  # 127. A declaration opener is UPPERCASE. `<!foo>` is ordinary text, and
  #      blanking its line as a raw block swallowed a clickable link.
  _scaffold decl_lowercase
  printf '# A\n' > "$tmp/decl_lowercase/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/decl_lowercase/docs/guide/index.md"
  printf '<!foo> [Guide](docs/guide/index.md)\n' > "$tmp/decl_lowercase/README.md"
  _commit decl_lowercase
  _case "a lowercase <!foo> is not a declaration" 0 decl_lowercase

  # 128. Brackets nest to ANY depth. The one-level grammar of case 121 was
  #      a false failure one level further down, which is true of every
  #      fixed bound — hence a scanner rather than a deeper regex.
  _scaffold deep_nesting
  printf '# A\n' > "$tmp/deep_nesting/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A [one [two]]](alpha.md)\n' \
    > "$tmp/deep_nesting/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/deep_nesting/README.md"
  _commit deep_nesting
  _case "brackets nest to any depth" 0 deep_nesting

  # 129. ...and the same for a REFERENCE row, since the scanner replaced the
  #      grammar at every site rather than the one that was reported.
  _scaffold deep_nesting_ref
  printf '# A\n' > "$tmp/deep_nesting_ref/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A [one [two]]][alpha]\n\n[alpha]: alpha.md\n' \
    > "$tmp/deep_nesting_ref/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/deep_nesting_ref/README.md"
  _commit deep_nesting_ref
  _case "a deep reference row is a row" 0 deep_nesting_ref

  # 130. ...and the guard: brackets that never balance are not link text,
  #      so the row is reported rather than half-read.
  _scaffold unbalanced_deep
  printf '# A\n' > "$tmp/unbalanced_deep/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A [one [two](alpha.md)\n' \
    > "$tmp/unbalanced_deep/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/unbalanced_deep/README.md"
  _commit unbalanced_deep
  _case "unbalanced nesting is not link text" 1 unbalanced_deep

  # 131. Inline HTML is matched against CommonMark's tag grammar. `=` cannot
  #      begin an attribute name, so `<span = [catalog]>` is literal text and
  #      the reference in it is a real link.
  _scaffold invalid_tag
  printf '# A\n' > "$tmp/invalid_tag/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/invalid_tag/docs/guide/index.md"
  printf '<span = [catalog]>\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/invalid_tag/README.md"
  _commit invalid_tag
  _case "invalid tag syntax is literal text" 0 invalid_tag

  # 132. ...and the guard: a VALID tag is still blanked, so a label inside
  #      an attribute value is tag text rather than a reference.
  _scaffold valid_tag_blanked
  printf '# A\n' > "$tmp/valid_tag_blanked/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/valid_tag_blanked/docs/guide/index.md"
  printf '<span title="[catalog]">\n\n[catalog]: docs/guide/index.md\n' \
    > "$tmp/valid_tag_blanked/README.md"
  _commit valid_tag_blanked
  _case "a label inside an attribute is not a reference" 1 valid_tag_blanked

  # 133. A rendered link is a URL, and `check-docs-links.sh` already resolves
  #      one this way. Comparing the query, the encoding or the markdown
  #      escape as part of the filename rejected ordinary rows — and would
  #      have had the two gates disagree about what an index may contain.
  _scaffold url_spellings
  printf '# A\n' > "$tmp/url_spellings/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/url_spellings/docs/guide/beta.md"
  printf '# C\n' > "$tmp/url_spellings/docs/guide/gamma.md"
  printf '# Guide\n\n## S\n\n- [A](alpha\\.md)\n- [B](beta%%2Emd)\n- [C](gamma.md?plain=1)\n' \
    > "$tmp/url_spellings/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/url_spellings/README.md"
  _commit url_spellings
  _case "escaped, encoded and queried destinations resolve" 0 url_spellings

  # 134. Parentheses nest in a bare destination too, so the destination is
  #      scanned for the same reason link text is.
  _scaffold nested_parens_dest
  printf '# A\n' > "$tmp/nested_parens_dest/docs/guide/a((b)).md"
  printf '# Guide\n\n## S\n\n- [A](a((b)).md)\n' \
    > "$tmp/nested_parens_dest/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/nested_parens_dest/README.md"
  _commit nested_parens_dest
  _case "parentheses nest in a destination" 0 nested_parens_dest

  # 135. ...and the guard: an UNBALANCED paren does not swallow the close,
  #      so the row is reported rather than half-read.
  _scaffold unbalanced_parens_dest
  printf '# A\n' > "$tmp/unbalanced_parens_dest/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](a(b.md)\n' \
    > "$tmp/unbalanced_parens_dest/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/unbalanced_parens_dest/README.md"
  _commit unbalanced_parens_dest
  _case "an unbalanced paren is not a destination" 1 unbalanced_parens_dest

  # 136-138. Shapes `check-docs-links.sh` thought worth encoding, checked
  #     here because two gates over one corpus must agree about what a link
  #     is. All three already passed when first probed; they are pinned so
  #     they keep doing so, since nothing else in this suite covered them.
  #
  #     A linked IMAGE is the one that earned its own pattern next door: a
  #     badge row nests a link inside a link, and the outer one is the entry.
  _scaffold linked_image_row
  printf '# A\n' > "$tmp/linked_image_row/docs/guide/alpha.md"
  printf 'x' > "$tmp/linked_image_row/docs/guide/img.png"
  printf '# Guide\n\n## S\n\n- [![badge](img.png)](alpha.md)\n' \
    > "$tmp/linked_image_row/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/linked_image_row/README.md"
  _commit linked_image_row
  _case "a linked-image row targets the page" 0 linked_image_row

  _scaffold escaped_bracket_text
  printf '# A\n' > "$tmp/escaped_bracket_text/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [closing \\]](alpha.md)\n' \
    > "$tmp/escaped_bracket_text/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/escaped_bracket_text/README.md"
  _commit escaped_bracket_text
  _case "an escaped bracket stays inside link text" 0 escaped_bracket_text

  _scaffold single_quoted_title_row
  printf '# A\n' > "$tmp/single_quoted_title_row/docs/guide/alpha.md"
  printf "# Guide\n\n## S\n\n- [A](alpha.md 'why')\n" \
    > "$tmp/single_quoted_title_row/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/single_quoted_title_row/README.md"
  _commit single_quoted_title_row
  _case "a single-quoted title on a row is a row" 0 single_quoted_title_row

  # 139. Link TEXT may not cross a blank line. The destination, the title
  #      and the definition all learned this rule in earlier rounds; the
  #      text was the one construct left out of it.
  _scaffold text_blank_line
  printf '# A\n' > "$tmp/text_blank_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/text_blank_line/docs/guide/index.md"
  printf '[Guide\n\ntext](docs/guide/index.md)\n' > "$tmp/text_blank_line/README.md"
  _commit text_blank_line
  _case "link text cannot cross a blank line" 1 text_blank_line

  # 140. ...and the guard: ONE line ending inside link text is fine, so 139
  #      is not bought by requiring links to sit on a single line.
  _scaffold text_one_newline
  printf '# A\n' > "$tmp/text_one_newline/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/text_one_newline/docs/guide/index.md"
  printf '[Guide\ntext](docs/guide/index.md)\n' > "$tmp/text_one_newline/README.md"
  _commit text_one_newline
  _case "link text may span one line ending" 0 text_one_newline

  # 141. The same rule for a reference LABEL, found by checking the sibling
  #      construct rather than waiting for it to be reported.
  _scaffold label_blank_line
  printf '# A\n' > "$tmp/label_blank_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/label_blank_line/docs/guide/index.md"
  printf '[Guide][cat\n\nalog]\n\n[cat alog]: docs/guide/index.md\n' \
    > "$tmp/label_blank_line/README.md"
  _commit label_blank_line
  _case "a label cannot cross a blank line" 1 label_blank_line

  # 142. Labels fold with `casefold`, not `lower`: `[ẞ]` resolves against
  #      `[ss]:`. `check-docs-orphans.sh` had this already, with the
  #      rationale; writing the helper from scratch is what lost it.
  _scaffold label_casefold
  printf '# A\n' > "$tmp/label_casefold/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/label_casefold/docs/guide/index.md"
  printf '[Guide][\xe1\xba\x9e]\n\n[ss]: docs/guide/index.md\n' \
    > "$tmp/label_casefold/README.md"
  _commit label_casefold
  _case "a label folds with full Unicode case folding" 0 label_casefold

  # 143. A label over CommonMark's 999-character cap defines nothing.
  _scaffold label_too_long
  printf '# A\n' > "$tmp/label_too_long/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/label_too_long/docs/guide/index.md"
  long=$(python3 -c "print('a'*1000)")
  printf '[Guide][%s]\n\n[%s]: docs/guide/index.md\n' "$long" "$long" \
    > "$tmp/label_too_long/README.md"
  _commit label_too_long
  _case "a 1000-character label is not a label" 1 label_too_long

  # 144. A definition's bare destination must balance its parentheses, the
  #      same rule `dest_at` applies inline, and the same rule
  #      `check-docs-orphans.sh` applies to this construct.
  #
  #      Round 40 checked the premise this case was written on and found it
  #      only half true: cmark and cmark-gfm — what GitHub serves — DO define
  #      the label here and resolve `[Guide][x]`, so against GitHub this
  #      expectation is a false failure. markdown-it-py and the spec's
  #      "parentheses only if balanced" wording say otherwise. The behaviour
  #      is kept to stay consistent with the sibling gate and with
  #      `migration_guide_gate_rejects_an_unbalanced_paren_in_a_definition`;
  #      changing it is a cross-gate call, raised on the PR, not taken here.
  _scaffold defn_dest_parens
  printf '# A\n' > "$tmp/defn_dest_parens/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_dest_parens/docs/guide/index.md"
  printf '[x]: docs/guide/index.md#(unterminated\n\n[Guide][x]\n' \
    > "$tmp/defn_dest_parens/README.md"
  _commit defn_dest_parens
  _case "a definition destination must balance parens" 1 defn_dest_parens

  # 145. A LINK MAY NOT CONTAIN A LINK: the outer opener is deactivated and
  #      the inner link renders. Found by reading the sibling gate rather
  #      than from review — it credited a page nothing links to.
  _scaffold link_in_link
  printf '# A\n' > "$tmp/link_in_link/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/link_in_link/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [[x](beta.md)](alpha.md)\n- [B](beta.md)\n' \
    > "$tmp/link_in_link/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/link_in_link/README.md"
  _commit link_in_link
  _case "a link inside link text deactivates the outer" 1 link_in_link

  # 146. ...and the guard: an IMAGE inside the text does NOT deactivate it,
  #      because an image is not a link. A badge row still resolves.
  _scaffold image_in_link
  printf '# A\n' > "$tmp/image_in_link/docs/guide/alpha.md"
  printf 'x' > "$tmp/image_in_link/docs/guide/img.png"
  printf '# Guide\n\n## S\n\n- [![badge](img.png)](alpha.md)\n' \
    > "$tmp/image_in_link/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/image_in_link/README.md"
  _commit image_in_link
  _case "an image inside link text does not deactivate it" 0 image_in_link

  # 147. A definition must START A BLOCK. Glued to the line above it, the
  #      line stays in that paragraph and defines nothing.
  _scaffold defn_block_start
  printf '# A\n' > "$tmp/defn_block_start/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_block_start/docs/guide/index.md"
  printf 'Some prose\n[catalog]: docs/guide/index.md\n\n[Guide][catalog]\n' \
    > "$tmp/defn_block_start/README.md"
  _commit defn_block_start
  _case "a definition glued to a paragraph defines nothing" 1 defn_block_start

  # 148. ...and the guard: after a blank line it is a definition again.
  _scaffold defn_after_blank
  printf '# A\n' > "$tmp/defn_after_blank/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_after_blank/docs/guide/index.md"
  printf 'Some prose\n\n[catalog]: docs/guide/index.md\n\n[Guide][catalog]\n' \
    > "$tmp/defn_after_blank/README.md"
  _commit defn_after_blank
  _case "a definition after a blank line still defines" 0 defn_after_blank

  # 149. A RESOLVED inner reference deactivates the outer opener too. Case
  #      145 covered only the inline shape, which is the right boundary
  #      when the defined labels are unknown and too narrow when they are.
  _scaffold resolved_ref_in_link
  printf '# A\n' > "$tmp/resolved_ref_in_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/resolved_ref_in_link/docs/guide/index.md"
  printf '[outer [Other][o]](docs/guide/index.md)\n\n[o]: other.md\n' \
    > "$tmp/resolved_ref_in_link/README.md"
  _commit resolved_ref_in_link
  _case "a resolved inner reference deactivates the outer" 1 resolved_ref_in_link

  # 150. ...and the guard: an UNRESOLVED inner reference is literal text,
  #      so the outer link still renders.
  _scaffold unresolved_ref_in_link
  printf '# A\n' > "$tmp/unresolved_ref_in_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/unresolved_ref_in_link/docs/guide/index.md"
  printf '[outer [Other][nope]](docs/guide/index.md)\n' \
    > "$tmp/unresolved_ref_in_link/README.md"
  _commit unresolved_ref_in_link
  _case "an unresolved inner reference does not deactivate" 0 unresolved_ref_in_link

  # 151. A code span cannot cross a BLANK line. Searching past one paired an
  #      opener with a backtick in a later paragraph and blanked the link
  #      between them.
  _scaffold span_blank_line
  printf '# A\n' > "$tmp/span_blank_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/span_blank_line/docs/guide/index.md"
  printf '`opener\n\n[Guide](docs/guide/index.md) `\n' \
    > "$tmp/span_blank_line/README.md"
  _commit span_blank_line
  _case "a code span cannot cross a blank line" 0 span_blank_line

  # 152. A reference IMAGE is an image only if its label resolves — the rule
  #      the shortcut form already followed, applied to the form that was
  #      left out of it. Unresolved, the brackets are literal and the link
  #      inside them is real.
  _scaffold unresolved_ref_image
  printf '# A\n' > "$tmp/unresolved_ref_image/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/unresolved_ref_image/docs/guide/index.md"
  printf '![alt [Guide](docs/guide/index.md)][missing]\n' \
    > "$tmp/unresolved_ref_image/README.md"
  _commit unresolved_ref_image
  _case "an unresolved reference image is literal" 0 unresolved_ref_image

  # 153. ...and the guard: a RESOLVED one is a real image, so the link in
  #      its alt text is alt text.
  _scaffold resolved_ref_image
  printf '# A\n' > "$tmp/resolved_ref_image/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/resolved_ref_image/docs/guide/index.md"
  printf '![alt [Guide](docs/guide/index.md)][found]\n\n[found]: other.png\n' \
    > "$tmp/resolved_ref_image/README.md"
  _commit resolved_ref_image
  _case "a resolved reference image hides its alt text" 1 resolved_ref_image

  # 154. `blank_defns` must honour the block-start rule too. It blanked
  #      every definition-SHAPED line, so one glued to a paragraph — which
  #      defines nothing and whose links are live — lost its whole line.
  _scaffold blank_defn_block_start
  printf '# A\n' > "$tmp/blank_defn_block_start/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/blank_defn_block_start/docs/guide/index.md"
  printf 'Some prose\n[c]: other.md "title [Guide](docs/guide/index.md)"\n' \
    > "$tmp/blank_defn_block_start/README.md"
  _commit blank_defn_block_start
  _case "only a real definition is blanked" 0 blank_defn_block_start

  # 155. A backtick fence's info string may not contain a backtick, so
  #      ```` ```md`x ```` is paragraph text and does not end a paragraph.
  #      `readable()` knew this and `_starts_block` did not.
  _scaffold invalid_fence_boundary
  printf '# A\n' > "$tmp/invalid_fence_boundary/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/invalid_fence_boundary/docs/guide/index.md"
  printf '```md`invalid\n[c]: docs/guide/index.md\n\n[Guide][c]\n' \
    > "$tmp/invalid_fence_boundary/README.md"
  _commit invalid_fence_boundary
  _case "an invalid fence is not a block boundary" 1 invalid_fence_boundary

  # 156. A run of definitions defines nothing unless the FIRST starts a
  #      block. Found by sweeping for the rule's other homes rather than
  #      from review.
  _scaffold defn_chain_glued
  printf '# A\n' > "$tmp/defn_chain_glued/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_chain_glued/docs/guide/index.md"
  printf 'Some prose\n[a]: other.md\n[c]: docs/guide/index.md\n\n[Guide][c]\n' \
    > "$tmp/defn_chain_glued/README.md"
  _commit defn_chain_glued
  _case "a definition chain glued to prose defines nothing" 1 defn_chain_glued

  # 157. ...and the guard: the same chain after a blank line still defines,
  #      so 156 is not bought by rejecting consecutive definitions.
  _scaffold defn_chain_ok
  printf '# A\n' > "$tmp/defn_chain_ok/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_chain_ok/docs/guide/index.md"
  printf 'Some prose\n\n[a]: other.md\n[c]: docs/guide/index.md\n\n[Guide][c]\n' \
    > "$tmp/defn_chain_ok/README.md"
  _commit defn_chain_ok
  _case "a definition chain after a blank line defines" 0 defn_chain_ok

  # 158. An inline IMAGE must have a valid tail, not merely balanced
  #      parentheses. `![alt …](not a valid dest)` forms no image, so the
  #      link inside its brackets is real and blanking it deleted one.
  _scaffold invalid_image_tail
  printf '# A\n' > "$tmp/invalid_image_tail/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/invalid_image_tail/docs/guide/index.md"
  printf '![alt [Guide](docs/guide/index.md)](not a valid dest)\n' \
    > "$tmp/invalid_image_tail/README.md"
  _commit invalid_image_tail
  _case "a malformed image is not an image" 0 invalid_image_tail

  # 159. ...and the guard: a VALID image still masks its alt text, so 158
  #      is not bought by giving up on images.
  _scaffold valid_image_tail
  printf '# A\n' > "$tmp/valid_image_tail/docs/guide/alpha.md"
  printf 'x' > "$tmp/valid_image_tail/docs/guide/img.png"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/valid_image_tail/docs/guide/index.md"
  printf '![alt [Guide](docs/guide/index.md)](img.png)\n' \
    > "$tmp/valid_image_tail/README.md"
  _commit valid_image_tail
  _case "a valid image still masks its alt text" 1 valid_image_tail

  # 160. A ROOT-RELATIVE destination leaves the repository: on every README
  #      renderer `/docs/guide/index.md` addresses the host root. Dropping
  #      the empty leading segment accepted a link that reaches nothing,
  #      and disagreed with `check-docs-links.sh`, which rejects it.
  _scaffold root_relative
  printf '# A\n' > "$tmp/root_relative/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/root_relative/docs/guide/index.md"
  printf '[Guide](/docs/guide/index.md)\n' > "$tmp/root_relative/README.md"
  _commit root_relative
  _case "a root-relative destination does not resolve" 1 root_relative

  # 161. ...and the guard: a `./` prefix is repo-relative and still does.
  _scaffold dot_slash
  printf '# A\n' > "$tmp/dot_slash/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/dot_slash/docs/guide/index.md"
  printf '[Guide](./docs/guide/index.md)\n' > "$tmp/dot_slash/README.md"
  _commit dot_slash
  _case "a ./ prefix still resolves" 0 dot_slash

  # 162. A backslash never swallows the LINE ENDING. Skipping two characters
  #      unconditionally ate the newline before a blank line, so the
  #      paragraph boundary went unseen and the bracket stack survived it.
  _scaffold escaped_newline
  printf '# A\n' > "$tmp/escaped_newline/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/escaped_newline/docs/guide/index.md"
  printf '[Guide\\\n\ncontinued](docs/guide/index.md)\n' \
    > "$tmp/escaped_newline/README.md"
  _commit escaped_newline
  _case "an escape does not swallow a blank line" 1 escaped_newline

  # 163. A link with NO rendered content is not an entry and not a route:
  #      `- [](alpha.md)` gives a reader nothing to see or click, which is
  #      the whole thing this gate exists to guarantee.
  _scaffold empty_label
  printf '# A\n' > "$tmp/empty_label/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [](alpha.md)\n' \
    > "$tmp/empty_label/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/empty_label/README.md"
  _commit empty_label
  _case "an empty link label is not an entry" 1 empty_label

  # 164. ...and the guard: an IMAGE is content, so a badge row stays a row.
  #      This is what `IMAGE_MARK` exists for.
  _scaffold image_label
  printf '# A\n' > "$tmp/image_label/docs/guide/alpha.md"
  printf 'x' > "$tmp/image_label/docs/guide/img.png"
  printf '# Guide\n\n## S\n\n- [![badge](img.png)](alpha.md)\n' \
    > "$tmp/image_label/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/image_label/README.md"
  _commit image_label
  _case "an image label is rendered content" 0 image_label

  # 165. A definition inside a FENCE is code, not a definition.
  #      `_starts_block` sees a fence opener above it and says yes, which is
  #      right after a fence and wrong inside one — so the candidate set is
  #      read from a first `readable()` pass rather than the raw text.
  _scaffold fenced_definition
  printf '# A\n' > "$tmp/fenced_definition/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/fenced_definition/docs/guide/index.md"
  printf '```\n[missing]: other.md\n```\n\n![alt [Guide](docs/guide/index.md)][missing]\n' \
    > "$tmp/fenced_definition/README.md"
  _commit fenced_definition
  _case "a fenced definition does not resolve" 0 fenced_definition

  # 166. An ANGLE definition destination gets the same grammar an inline one
  #      does. `<index.md?>>` merely begins and ends with brackets:
  #      CommonMark closes at the FIRST `>` and the rest is trailing
  #      garbage, so nothing is defined.
  _scaffold angle_defn_garbage
  printf '# A\n' > "$tmp/angle_defn_garbage/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/angle_defn_garbage/docs/guide/index.md"
  printf '[Guide][x]\n\n[x]: <docs/guide/index.md?>>\n' \
    > "$tmp/angle_defn_garbage/README.md"
  _commit angle_defn_garbage
  _case "a malformed angle destination defines nothing" 1 angle_defn_garbage

  # 167. ...and the guard: a well-formed one still resolves, so 166 is not
  #      bought by rejecting the angle form.
  _scaffold angle_defn_ok
  printf '# A\n' > "$tmp/angle_defn_ok/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/angle_defn_ok/docs/guide/index.md"
  printf '[Guide][x]\n\n[x]: <docs/guide/index.md>\n' \
    > "$tmp/angle_defn_ok/README.md"
  _commit angle_defn_ok
  _case "a well-formed angle destination resolves" 0 angle_defn_ok

  # 168. A reference LABEL may not hold an UNESCAPED bracket. Round 28 said
  #      so in a comment and left `[` out of the pattern, so `[a[b]` both
  #      defined and resolved while rendering no link at all.
  _scaffold label_raw_bracket
  printf '# A\n' > "$tmp/label_raw_bracket/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [Alpha][a[b]\n\n[a[b]: alpha.md\n' \
    > "$tmp/label_raw_bracket/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/label_raw_bracket/README.md"
  _commit label_raw_bracket
  _case "an unescaped bracket is not a label" 1 label_raw_bracket

  # 169. ...and the guard: an ESCAPED bracket is ordinary label content, so
  #      168 is not bought by rejecting brackets outright.
  _scaffold label_escaped_bracket
  printf '# A\n' > "$tmp/label_escaped_bracket/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [Alpha][a\\[b]\n\n[a\\[b]: alpha.md\n' \
    > "$tmp/label_escaped_bracket/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/label_escaped_bracket/README.md"
  _commit label_escaped_bracket
  _case "an escaped bracket is label content" 0 label_escaped_bracket

  # 170. A backslash escapes only ASCII PUNCTUATION. Unescaping every
  #      character turned `docs/guide/\index.md` — which names no tracked
  #      file — into one that does, and passed a broken link.
  _scaffold escape_non_punct
  printf '# A\n' > "$tmp/escape_non_punct/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/escape_non_punct/docs/guide/index.md"
  printf '[Guide](docs/guide/\\index.md)\n' > "$tmp/escape_non_punct/README.md"
  _commit escape_non_punct
  _case "a backslash before a letter is not an escape" 1 escape_non_punct

  # 171. ANY URI scheme leaves the guide, not the three that occurred to me.
  #      A hard-coded list read `ftp://example.com/file` as the relative
  #      path `docs/guide/ftp:/example.com/file` and reported a page that
  #      does not exist — and missed case variants like `HTTPS:` besides.
  _scaffold uri_schemes
  printf '# A\n' > "$tmp/uri_schemes/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [FTP](ftp://example.com/f)\n- [X](HTTPS://example.com/x)\n' \
    > "$tmp/uri_schemes/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/uri_schemes/README.md"
  _commit uri_schemes
  _case "any URI scheme is outside the guide" 0 uri_schemes

  # 172. The prepass and the real scan must agree about what a definition is.
  #      They had drifted, so `[o]: bad(unbalanced` resolved in one and not
  #      the other; `_defn_entries` is the single scanner all three callers
  #      now use, so they cannot drift again.
  #
  #      Which answer they agree ON is the separate, cross-gate question
  #      case 144 records: cmark-gfm would define `o` here and make the index
  #      genuinely unreachable, while this gate and its sibling do not define
  #      it, leaving the outer link live. The consolidation is what would
  #      make that a one-line change instead of three.
  _scaffold prepass_validation
  printf '# A\n' > "$tmp/prepass_validation/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/prepass_validation/docs/guide/index.md"
  printf '[outer [Other][o]](docs/guide/index.md)\n\n[o]: bad(unbalanced\n' \
    > "$tmp/prepass_validation/README.md"
  _commit prepass_validation
  _case "one scanner decides what a definition is" 0 prepass_validation

  # 173. An empty REFERENCE label renders an anchor with nothing in it, so
  #      it is no more an entry than `- [](alpha.md)` is. Case 163 gave the
  #      inline spelling this rule and `ref_at` did not share it.
  _scaffold empty_ref_label
  printf '# A\n' > "$tmp/empty_ref_label/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [][a]\n\n[a]: alpha.md\n' \
    > "$tmp/empty_ref_label/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/empty_ref_label/README.md"
  _commit empty_ref_label
  _case "an empty reference label is not an entry" 1 empty_ref_label

  # 174. An AUTOLINK in a link's text does NOT deactivate the outer opener.
  #      Reported as a bug; it is not one. cmark, cmark-gfm and markdown-it
  #      all render the outer link, because an autolink never touches the
  #      bracket delimiter stack that the no-nested-links rule works on:
  #
  #        <a href="docs/guide/index.md">outer <a href="https://e.com">…</a></a>
  #
  #      An HTML parser closes the outer anchor at the inner one, so "outer "
  #      is clickable and lands on the index. The gate must PASS.
  _scaffold autolink_in_text
  printf '# A\n' > "$tmp/autolink_in_text/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/autolink_in_text/docs/guide/index.md"
  printf '[outer <https://example.com>](docs/guide/index.md)\n' \
    > "$tmp/autolink_in_text/README.md"
  _commit autolink_in_text
  _case "an autolink does not deactivate its outer link" 0 autolink_in_text

  # 175. A definition-SHAPED line that is not a definition is paragraph text,
  #      and the links in it are live. `blank_defns` blanked this line whole
  #      — taking the only route to the index with it — because it carried
  #      the block-start rule and neither of the other two. An empty label is
  #      not a label in cmark-gfm or markdown-it, so both render the Guide
  #      link and the gate must PASS.
  _scaffold blank_defns_validated
  printf '# A\n' > "$tmp/blank_defns_validated/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/blank_defns_validated/docs/guide/index.md"
  printf '[]: alpha.md "t [Guide](docs/guide/index.md)"\n' \
    > "$tmp/blank_defns_validated/README.md"
  _commit blank_defns_validated
  _case "a non-definition keeps its links" 0 blank_defns_validated

  # 176. A bare `===` underlines nothing, so it is ordinary text and the
  #      definition below it is that paragraph's second line. Both renderers
  #      leave `[Guide][x]` literal, so the index is unreachable and the run
  #      must FAIL. `_starts_block` called every `===` a block boundary and
  #      passed this README.
  _scaffold setext_needs_context
  printf '# A\n' > "$tmp/setext_needs_context/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/setext_needs_context/docs/guide/index.md"
  printf '===\n[x]: docs/guide/index.md\n\n[Guide][x]\n' \
    > "$tmp/setext_needs_context/README.md"
  _commit setext_needs_context
  _case "a bare setext underline is not a boundary" 1 setext_needs_context

  # 177. A real Setext heading IS a boundary — the pin for the other
  #      direction, so 176's fix cannot be "never treat `===` as one".
  _scaffold setext_real_heading
  printf '# A\n' > "$tmp/setext_real_heading/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/setext_real_heading/docs/guide/index.md"
  printf 'Title\n===\n[x]: docs/guide/index.md\n\n[Guide][x]\n' \
    > "$tmp/setext_real_heading/README.md"
  _commit setext_real_heading
  _case "a real setext heading is a boundary" 0 setext_real_heading

  # 178. Every marker CommonMark allows starts an index row. `- ` with at
  #      most three digits was a third of them, and the rest were reported
  #      "listed in no section" — a false failure on a correct index.
  _scaffold row_markers
  for p in a b c d; do printf '# %s\n' "$p" > "$tmp/row_markers/docs/guide/$p.md"; done
  printf '# Guide\n\n## S\n\n* [A](a.md)\n+ [B](b.md)\n1000. [C](c.md)\n-  [D](d.md)\n' \
    > "$tmp/row_markers/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/row_markers/README.md"
  _commit row_markers
  _case "every list marker starts a row" 0 row_markers

  # 179. The other direction: five spaces after the marker is an indented
  #      code block inside the item, so the link renders as literal text and
  #      the page really is listed nowhere.
  _scaffold row_indent_code
  printf '# A\n' > "$tmp/row_indent_code/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n-     [A](alpha.md)\n' \
    > "$tmp/row_indent_code/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/row_indent_code/README.md"
  _commit row_indent_code
  _case "five spaces after a marker is code, not a row" 1 row_indent_code

  # 180. An UNMATCHED inline `<!--` is literal text, not a comment that runs
  #      to end of file. This had the widest blast radius of any false
  #      failure in this file: one unmatched opener anywhere in README.md
  #      blanked every link after it. cmark-gfm and markdown-it both render
  #      `<p>prose &lt;!-- unmatched</p>` and leave the next link live.
  _scaffold comment_unmatched
  printf '# A\n' > "$tmp/comment_unmatched/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/comment_unmatched/docs/guide/index.md"
  printf 'prose <!-- unmatched\n\n[Guide index](docs/guide/index.md)\n' \
    > "$tmp/comment_unmatched/README.md"
  _commit comment_unmatched
  _case "an unmatched inline comment stays literal" 0 comment_unmatched

  # 181. A closer on the far side of a BLANK LINE closes nothing — inline raw
  #      HTML cannot contain one — so the opener is still literal.
  _scaffold comment_across_blank
  printf '# A\n' > "$tmp/comment_across_blank/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/comment_across_blank/docs/guide/index.md"
  printf 'prose <!-- a\n\nb --> [Guide index](docs/guide/index.md)\n' \
    > "$tmp/comment_across_blank/README.md"
  _commit comment_across_blank
  _case "a closer past a blank line closes nothing" 0 comment_across_blank

  # 182. The other direction, so 180 cannot become "never blank a comment":
  #      a comment closed on the NEXT line is still one paragraph, so it IS a
  #      comment and its contents are not navigation. The index is reached
  #      only by the link after `-->`.
  _scaffold comment_next_line
  printf '# A\n' > "$tmp/comment_next_line/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/comment_next_line/docs/guide/index.md"
  printf 'prose <!-- a\nb --> [Guide index](docs/guide/index.md)\n' \
    > "$tmp/comment_next_line/README.md"
  _commit comment_next_line
  _case "a comment closed on the next line is a comment" 0 comment_next_line

  # 183. And the case that proves 182 is not vacuous: when the ONLY link sits
  #      INSIDE a properly closed comment, it is not navigation and the index
  #      is unreachable.
  _scaffold comment_hides_link
  printf '# A\n' > "$tmp/comment_hides_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/comment_hides_link/docs/guide/index.md"
  printf 'prose <!-- [Guide index](docs/guide/index.md) --> tail\n' \
    > "$tmp/comment_hides_link/README.md"
  _commit comment_hides_link
  _case "a link inside a closed comment is not a route" 1 comment_hides_link

  # 184. A fence inside a BLOCK QUOTE is still a fence. Detection ran on the
  #      raw line, so `> ```markdown` opened nothing and a README link inside
  #      a quoted EXAMPLE counted as navigation. This corpus writes quoted
  #      fences — ten of them live in docs/guide/ — so the spelling is not
  #      hypothetical. cmark-gfm renders the link as `<pre><code>`.
  #
  #      The three-backtick case happened to fail already, by accident: the
  #      code-span scan paired its equal backtick runs. The unequal, tilde
  #      and unclosed spellings did not, which is why 185 exists.
  _scaffold quoted_fence
  printf '# A\n' > "$tmp/quoted_fence/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_fence/docs/guide/index.md"
  printf '> ~~~markdown\n> [Guide index](docs/guide/index.md)\n> ~~~\n' \
    > "$tmp/quoted_fence/README.md"
  _commit quoted_fence
  _case "a quoted fence is a fence" 1 quoted_fence

  # 185. The same, with an opener LONGER than its closer — the spelling the
  #      accidental code-span pairing could not cover.
  _scaffold quoted_fence_uneven
  printf '# A\n' > "$tmp/quoted_fence_uneven/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_fence_uneven/docs/guide/index.md"
  printf '> ````markdown\n> [Guide index](docs/guide/index.md)\n> ```\n' \
    > "$tmp/quoted_fence_uneven/README.md"
  _commit quoted_fence_uneven
  _case "a quoted fence outlives a short closer" 1 quoted_fence_uneven

  # 186. The FALSE-FAILURE direction, and the reason this fix is bounded: an
  #      unclosed quoted fence ends where the QUOTE ends. Running it to end
  #      of file would blank every link after it — the same mistake the
  #      unmatched `<!--` used to make. cmark-gfm keeps this link live.
  _scaffold quoted_fence_unclosed
  printf '# A\n' > "$tmp/quoted_fence_unclosed/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_fence_unclosed/docs/guide/index.md"
  printf '> ```md\n> x\n\n[Guide index](docs/guide/index.md)\n' \
    > "$tmp/quoted_fence_unclosed/README.md"
  _commit quoted_fence_unclosed
  _case "an unclosed quoted fence ends with its quote" 0 quoted_fence_unclosed

  # 187. And a link in quoted PROSE is an ordinary link — the fix must not
  #      turn "quoted" into "invisible".
  _scaffold quoted_prose_link
  printf '# A\n' > "$tmp/quoted_prose_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_prose_link/docs/guide/index.md"
  printf '> see [Guide index](docs/guide/index.md)\n' \
    > "$tmp/quoted_prose_link/README.md"
  _commit quoted_prose_link
  _case "a link in quoted prose is a link" 0 quoted_prose_link

  # 188. A raw HTML block inside a quote is a raw HTML block. Round 42 gave
  #      the quote strip to FENCE detection only and left its neighbours
  #      reading the raw line — my own one-of-several-sites divergence, one
  #      commit old. The strip now happens once, before anything measures or
  #      matches the line.
  _scaffold quoted_html_block
  printf '# A\n' > "$tmp/quoted_html_block/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_html_block/docs/guide/index.md"
  printf '> <div>\n> [Guide](docs/guide/index.md)\n' \
    > "$tmp/quoted_html_block/README.md"
  _commit quoted_html_block
  _case "a quoted html block is a block" 1 quoted_html_block

  # 189. Indentation is measured INSIDE the quote. `>     [Guide](x)` is four
  #      spaces past the marker, so CommonMark renders indented code; the raw
  #      line measures zero and looked like prose with a live link.
  _scaffold quoted_indent_code
  printf '# A\n' > "$tmp/quoted_indent_code/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_indent_code/docs/guide/index.md"
  printf '>     [Guide](docs/guide/index.md)\n' \
    > "$tmp/quoted_indent_code/README.md"
  _commit quoted_indent_code
  _case "quoted indentation is measured inside the quote" 1 quoted_indent_code

  # 190. Both of the above end WITH THE QUOTE. Running either to end of file
  #      would blank every link after it, which is the run-to-end-of-file
  #      false failure this scan has produced three times now.
  _scaffold quoted_block_bounded
  printf '# A\n' > "$tmp/quoted_block_bounded/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_block_bounded/docs/guide/index.md"
  printf '> <div>\n\n[Guide index](docs/guide/index.md)\n' \
    > "$tmp/quoted_block_bounded/README.md"
  _commit quoted_block_bounded
  _case "a quoted html block ends with its quote" 0 quoted_block_bounded

  # 191. An image's LABEL may not span a blank line. `bracket_pairs` already
  #      refused to pair link brackets across one; this balancing loop was
  #      the construct left out, so it masked a whole pseudo-image and
  #      deleted a link CommonMark renders live.
  _scaffold image_label_blank
  printf '# A\n' > "$tmp/image_label_blank/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/image_label_blank/docs/guide/index.md"
  printf '![alt\n\n[Guide](docs/guide/index.md)](image.png)\n' \
    > "$tmp/image_label_blank/README.md"
  _commit image_label_blank
  _case "an image label stops at a blank line" 0 image_label_blank

  # 192. The other direction: a REAL image still masks its alt text, so a
  #      link written inside the label is not a route.
  _scaffold image_label_intact
  printf '# A\n' > "$tmp/image_label_intact/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/image_label_intact/docs/guide/index.md"
  printf '![alt [Guide](docs/guide/index.md)](image.png)\n' \
    > "$tmp/image_label_intact/README.md"
  _commit image_label_intact
  _case "a real image still masks its alt text" 1 image_label_intact

  # 193. Masking a link must not invent a paragraph boundary. A line that is
  #      entirely one inline link becomes spaces under `blank_links`, and
  #      `_starts_block` read that as blank — so a definition glued to it
  #      resolved, where CommonMark keeps that line INSIDE the paragraph and
  #      renders the later reference as plain text. Block starts are now read
  #      from the unmasked text, which shares offsets with the masked copy.
  _scaffold mask_invents_block
  printf '# A\n' > "$tmp/mask_invents_block/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/mask_invents_block/docs/guide/index.md"
  printf '[ordinary](other.md)\n[x]: docs/guide/index.md\n\n[Guide][x]\n' \
    > "$tmp/mask_invents_block/README.md"
  _commit mask_invents_block
  _case "masking a link invents no block boundary" 1 mask_invents_block

  # 194. The other direction: after a real blank line the definition is real,
  #      so the fix cannot become "a definition after a link never counts".
  _scaffold mask_real_boundary
  printf '# A\n' > "$tmp/mask_real_boundary/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/mask_real_boundary/docs/guide/index.md"
  printf '[ordinary](other.md)\n\n[x]: docs/guide/index.md\n\n[Guide][x]\n' \
    > "$tmp/mask_real_boundary/README.md"
  _commit mask_real_boundary
  _case "a definition after a blank line still defines" 0 mask_real_boundary

  # 195. An unclosed comment INSIDE a quote ends with the quote. The previous
  #      round bounded the raw-tag branch and left the comment and
  #      declaration branches beside it unbounded — the same one-of-several
  #      miss, in the branch whose own comment warns about it. `quote_limit`
  #      is now the one place all three ask.
  _scaffold quoted_comment_bounded
  printf '# A\n' > "$tmp/quoted_comment_bounded/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_comment_bounded/docs/guide/index.md"
  printf '> <!-- unclosed\n\n[Guide index](docs/guide/index.md)\n' \
    > "$tmp/quoted_comment_bounded/README.md"
  _commit quoted_comment_bounded
  _case "a quoted comment ends with its quote" 0 quoted_comment_bounded

  # 196. UNQUOTED, the same unclosed opener really is an HTML block to end of
  #      file, so the bound must not leak out of block quotes.
  _scaffold unquoted_comment_eof
  printf '# A\n' > "$tmp/unquoted_comment_eof/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/unquoted_comment_eof/docs/guide/index.md"
  printf '<!-- unclosed\n\n[Guide index](docs/guide/index.md)\n' \
    > "$tmp/unquoted_comment_eof/README.md"
  _commit unquoted_comment_eof
  _case "an unquoted comment block still runs to EOF" 1 unquoted_comment_eof

  # 197. And a link sealed inside a CLOSED quoted comment is still not a
  #      route, so bounding did not stop comments hiding things.
  _scaffold quoted_comment_hides
  printf '# A\n' > "$tmp/quoted_comment_hides/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/quoted_comment_hides/docs/guide/index.md"
  printf '> <!-- [Guide index](docs/guide/index.md) -->\n' \
    > "$tmp/quoted_comment_hides/README.md"
  _commit quoted_comment_hides
  _case "a link inside a quoted comment is not a route" 1 quoted_comment_hides

  # 198. NOT A BUG, pinned so it stays that way. Reported as one: that
  #      `strip()`ping an angle destination "silently changes the URL",
  #      because `[Guide](< docs/guide/index.md>)` supposedly renders with a
  #      leading encoded space. It does not. cmark-gfm and markdown-it agree:
  #
  #        [Guide](< docs/guide/index.md>)  ->  <a href="docs/guide/index.md">
  #        [Guide](<docs/guide/index.md >)  ->  <a href="docs/guide/index.md">
  #
  #      Leading and trailing whitespace inside `<>` is stripped by both, so
  #      the index IS reached and this run must PASS. "Remove only the angle
  #      delimiters" would have introduced the false failure it warned of.
  _scaffold angle_pads
  printf '# A\n' > "$tmp/angle_pads/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](< alpha.md>)\n' \
    > "$tmp/angle_pads/docs/guide/index.md"
  printf '[Guide](<docs/guide/index.md >)\n' > "$tmp/angle_pads/README.md"
  _commit angle_pads
  _case "angle destinations ignore their padding" 0 angle_pads

  # 199. The property `strip()` must never break, and the half of the report
  #      that IS true: an INNER space is preserved and percent-encoded
  #      (`my%20file.md`) by both renderers, so a page whose name really
  #      contains a space still has to resolve. `strip()` touches only the
  #      ends, and `normalise` unquotes, so it does.
  _scaffold angle_inner_space
  printf '# A\n' > "$tmp/angle_inner_space/docs/guide/my file.md"
  printf '# Guide\n\n## S\n\n- [A](<my file.md>)\n' \
    > "$tmp/angle_inner_space/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/angle_inner_space/README.md"
  _commit angle_inner_space
  _case "a space inside a destination is part of the name" 0 angle_inner_space

  # 200. A definition after a CLOSED raw HTML block is real, so the reference
  #      image below it IS an image and its alt text is not navigation. Round
  #      44 gave `origin` to both definition callers when only `definitions`
  #      needed it — that one masks links, `candidate_labels` does not — and
  #      the extra argument blinded this scan to every HTML block. cmark-gfm
  #      renders the construct as `<img src="pic.png" alt="alt Guide">`: no
  #      clickable guide link, so the index is unreachable and this FAILS.
  _scaffold defn_after_html_block
  printf '# A\n' > "$tmp/defn_after_html_block/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_after_html_block/docs/guide/index.md"
  printf '<pre></pre>\n[img]: pic.png\n\n![alt [Guide](docs/guide/index.md)][img]\n' \
    > "$tmp/defn_after_html_block/README.md"
  _commit defn_after_html_block
  _case "a definition after an html block is real" 1 defn_after_html_block

  # 201. The same definition, used as a plain reference LINK, does reach the
  #      index — the other direction, so 200 cannot become "definitions after
  #      HTML blocks never count".
  _scaffold defn_after_html_reaches
  printf '# A\n' > "$tmp/defn_after_html_reaches/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/defn_after_html_reaches/docs/guide/index.md"
  printf '<pre></pre>\n[x]: docs/guide/index.md\n\n[Guide][x]\n' \
    > "$tmp/defn_after_html_reaches/README.md"
  _commit defn_after_html_reaches
  _case "a reference after an html block reaches" 0 defn_after_html_reaches

  # 202. A link whose whole label is a CODE SPAN renders visible, clickable
  #      text — `<a href="…"><code>Guide</code></a>` — but blanking every
  #      character of the label left `_text_renders` with nothing, so the
  #      gate rejected it. The most ordinary false failure this gate has
  #      had: naming a module or a command in code font is a normal way to
  #      write an index row.
  _scaffold code_span_label
  printf '# A\n' > "$tmp/code_span_label/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [`alpha`](alpha.md)\n' \
    > "$tmp/code_span_label/docs/guide/index.md"
  printf '[`Guide`](docs/guide/index.md)\n' > "$tmp/code_span_label/README.md"
  _commit code_span_label
  _case "a code span is visible link text" 0 code_span_label

  # 203. The other direction: a code span containing only a space renders an
  #      element with nothing to read or aim at, so it is still not content.
  _scaffold code_span_blank
  printf '# A\n' > "$tmp/code_span_blank/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/code_span_blank/docs/guide/index.md"
  printf '[` `](docs/guide/index.md)\n' > "$tmp/code_span_blank/README.md"
  _commit code_span_blank
  _case "a blank code span is not content" 1 code_span_blank

  # 204. An ANGLE definition destination may contain spaces. `\S+` took only
  #      `<my` and the rest of the line then stopped the pattern matching at
  #      all, so a page whose name contains a space was listed nowhere —
  #      while both renderers resolve the row to `my%20file.md`. Case 199
  #      pinned this for the inline spelling; this is the definition one.
  _scaffold angle_defn_space
  printf '# A\n' > "$tmp/angle_defn_space/docs/guide/my file.md"
  printf '# Guide\n\n## S\n\n- [A][a]\n\n[a]: <my file.md>\n' \
    > "$tmp/angle_defn_space/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/angle_defn_space/README.md"
  _commit angle_defn_space
  _case "an angle definition may hold a space" 0 angle_defn_space

  # 205. A label needs one NON-WHITESPACE character. `[ ]` folds to the empty
  #      string, and an empty key matched another empty key happily, so
  #      `- [A][ ]` with `[ ]: alpha.md` counted as an entry — while both
  #      renderers leave the row and the definition as literal text and the
  #      page is listed nowhere. `ref_at` already refused the bare `[]`; this
  #      is where that fix stopped.
  _scaffold blank_label_row
  printf '# A\n' > "$tmp/blank_label_row/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A][ ]\n\n[ ]: alpha.md\n' \
    > "$tmp/blank_label_row/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/blank_label_row/README.md"
  _commit blank_label_row
  _case "a whitespace-only label is not a label" 1 blank_label_row

  # 206. The README side of the same rule.
  _scaffold blank_label_readme
  printf '# A\n' > "$tmp/blank_label_readme/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/blank_label_readme/docs/guide/index.md"
  printf '[Guide][ ]\n\n[ ]: docs/guide/index.md\n' \
    > "$tmp/blank_label_readme/README.md"
  _commit blank_label_readme
  _case "a whitespace-only label reaches nothing" 1 blank_label_readme

  # 207. And the guard: a real label with INTERNAL whitespace still folds and
  #      still resolves, so the fix cannot become "labels with spaces fail".
  _scaffold folded_label
  printf '# A\n' > "$tmp/folded_label/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/folded_label/docs/guide/index.md"
  printf '[Guide][big  catalog]\n\n[BIG CATALOG]: docs/guide/index.md\n' \
    > "$tmp/folded_label/README.md"
  _commit folded_label
  _case "a folded label still resolves" 0 folded_label

  # 208. A TAB is list-marker padding. The row grammar accepted only literal
  #      spaces, so a tab-padded row was reported unlisted while both
  #      renderers show a linked item. Round 40 widened this grammar and left
  #      tabs out deliberately; that was the wrong call.
  _scaffold row_tab_pad
  printf '# A\n' > "$tmp/row_tab_pad/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n-\t[Alpha](alpha.md)\n' \
    > "$tmp/row_tab_pad/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/row_tab_pad/README.md"
  _commit row_tab_pad
  _case "a tab is list-marker padding" 0 row_tab_pad

  # 209. TWO tabs reach column eight, which is indented code inside the item,
  #      so the bound still holds and the page really is listed nowhere.
  _scaffold row_tab_code
  printf '# A\n' > "$tmp/row_tab_code/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n-\t\t[Alpha](alpha.md)\n' \
    > "$tmp/row_tab_code/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/row_tab_code/README.md"
  _commit row_tab_code
  _case "two tabs after a marker is code" 1 row_tab_code

  # 210. A colon after a SHORTCUT reference disqualifies it only when what
  #      follows really makes a definition. `the page` is no destination, so
  #      both renderers render `<a href="alpha.md">Alpha</a>: the page` and
  #      the row is an entry. Rejecting on the colon alone failed a correct
  #      index.
  _scaffold shortcut_colon_row
  printf '# A\n' > "$tmp/shortcut_colon_row/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [Alpha]: the page\n\n[Alpha]: alpha.md\n' \
    > "$tmp/shortcut_colon_row/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' \
    > "$tmp/shortcut_colon_row/README.md"
  _commit shortcut_colon_row
  _case "a shortcut before a colon is still a link" 0 shortcut_colon_row

  # 211. And the guard the colon test exists for: a row that IS a definition
  #      renders an empty list item and lists no page. `blank_defns` cannot
  #      catch this one — its pattern is anchored to the line start and this
  #      definition is nested in a list item — which is why `ref_at` parses.
  _scaffold definition_row
  printf '# A\n' > "$tmp/definition_row/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [Alpha]: alpha.md\n' \
    > "$tmp/definition_row/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/definition_row/README.md"
  _commit definition_row
  _case "a definition row lists nothing" 1 definition_row

  echo "self-test: $pass/$total passed"
  [ "$pass" -eq "$total" ]
}

root="$(cd "$(dirname "$0")/.." && pwd)"

case "${1-}" in
  --self-test)
    self_test
    ;;
  "")
    echo "Checking that every guide page is listed in the guide index..."
    if run_check "$root"; then
      echo "Guide index gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the guide index does not account for every page in docs/guide/.

Fix each one where it lives:
  - page listed nowhere   -> add one line to docs/guide/index.md, under the
                             `## ` section a reader with that question would
                             scan; write the line in the reader's words, not
                             the internal feature name
  - page listed twice     -> keep the entry under the section a reader would
                             look in first, and delete the other
  - link to nothing       -> the page moved or was deleted; point at the
                             current path, or drop the entry
  - entry above every `## ` heading -> move it under the section it belongs to
  - README link missing   -> restore the `docs/guide/index.md` link in
                             README.md's `## Documentation` list
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
