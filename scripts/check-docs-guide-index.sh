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
_TITLE = (r'''"(?:[^"\n]|\n(?!\s*\n))*"'''
          r"""|'(?:[^'\n]|\n(?!\s*\n))*'"""
          r"""|\((?:[^)\n]|\n(?!\s*\n))*\)""")
# An optional title, then the close. Titles are `"..."`, `'...'` or `(...)`,
# and the required whitespace before one is what keeps a parenthesised title
# from being read as more balanced destination.
_CLOSE = r"(?:" + _WS1 + r"(?:" + _TITLE + r"))?" + _WS + r"\)"

LINK = re.compile(r"(?<!!)\[[^\]]*\]\(" + _WS + r"(" + _ANGLE + r"|" + _DEST + r"*)" + _FRAG + _CLOSE)

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
_ROW = r"^(?:- |\d{1,3}[.)] )"
ENTRY = re.compile(_ROW + r"\[[^\]]+\]\(" + _WS + r"(" + _ANGLE + r"|" + _DEST + r"+)" + _FRAG + _CLOSE)

# The same row, written as a REFERENCE link: `- [A][alpha]`, `- [A][]` or the
# shortcut `- [A]`, with `[alpha]: alpha.md` defined elsewhere in the index.
# These are ordinary markdown rows, and recognising only the inline spelling
# was wrong in both directions at once: a reference-style row reported its
# page as listed nowhere, and — worse — an inline row plus a reference-style
# row for the SAME page counted once, so the "listed exactly once" guarantee
# silently did not hold. The duplicate is the entry that rots.
ENTRY_REF = re.compile(_ROW + r"\[([^\]]+)\](?:\[([^\]]*)\])?(?![(:])")

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
DEFN = re.compile(
    r"""^ {0,3}\[([^\]]+)\]:[ \t]*(?:\n[ \t]*)?(\S+)"""
    r"""(?:""" + _WS1 + r"""(?:""" + _TITLE + r"""))?[ \t]*$""",
    re.MULTILINE)


def label_key(raw):
    """Markdown reference labels fold case and collapse whitespace, so
    `[guide   catalog]` and `[Guide Catalog]` are the same label.

    Shared by the index-entry scan and the README reachability scan. They read
    different files, but a label is a label in both, and the two callers want
    exactly the same folding — the reason this is shared rather than copied.
    """
    return " ".join(raw.split()).lower()


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
    return LINK.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)


def blank_defns(text):
    """Blank every reference-definition span, space for space.

    Definitions vanish from the rendered page, so nothing inside one is
    visible and nothing inside one is clickable. Newlines survive, for the
    same reason they do in `blank_links`.
    """
    return DEFN.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)


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
    for m in DEFN.finditer(blank_links(text)):
        out.setdefault(label_key(m.group(1)), m.group(2))
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
DECL = ((re.compile(r"^ {0,3}<\?"), "?>"),
        (re.compile(r"^ {0,3}<!\[CDATA\["), "]]>"),
        (re.compile(r"^ {0,3}<![a-zA-Z]"), ">"))
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
INLINE_TAG = re.compile(
    r"""<[a-zA-Z/!?](?:"[^"]*"|'[^']*'|[^>'"\n]|\n(?!\s*\n))*>""")


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


def readable(text):
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

    while i < n:
        at_line_start = i == 0 or text[i - 1] == "\n"
        eol = line_end(i)
        line = text[i:eol]

        if at_line_start:
            # A tab indents to the next multiple of four, so measuring
            # spaces alone read a tab-indented code line as column zero.
            expanded = line.expandtabs(4)
            stripped = expanded.lstrip(" ")
            indent = len(expanded) - len(stripped)
            if not stripped:
                in_paragraph = False
                i = eol + 1 if eol < n else n
                continue
            if indent == 0 and not LIST_ITEM.match(line):
                in_list = False
            if indent >= 4 and not in_paragraph and not in_list:
                # Runs while the indent holds; a line back under four spaces
                # ends it.
                j = i
                while j < n:
                    stop = line_end(j)
                    seg = text[j:stop].expandtabs(4)
                    body = seg.lstrip(" ")
                    if body and len(seg) - len(body) < 4:
                        break
                    blank_to(j, stop)
                    j = stop + 1
                i = min(j, n)
                in_paragraph = False
                continue
            if LIST_ITEM.match(line):
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
            auto = AUTOLINK.match(line)
            hm = HTML_OPEN.match(line)
            if hm and not auto:
                tag = hm.group(2).lower()
                alone = bool(re.fullmatch(r"\s*" + INLINE_TAG.pattern + r"\s*",
                                          line, re.VERBOSE))
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
            in_paragraph = not (ATX.match(line)
                                or FENCE.match(line)
                                or hm
                                # A thematic break (`---`, `***`, `___`) and a
                                # Setext underline (`===`, `---`) both end the
                                # paragraph, so an indented line after one is
                                # code.
                                or THEMATIC.match(line)
                                or SETEXT.match(line))

            m = FENCE.match(line)
            # A backtick opener's info string may not contain a backtick.
            if m and not (m.group(1)[0] == "`" and "`" in m.group(2)):
                char, length = m.group(1)[0], len(m.group(1))
                j = eol + 1
                while j <= n:
                    stop = line_end(j)
                    c = FENCE.match(text[j:stop])
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
            if COMMENT_BLOCK.match(line):
                close = text.find("-->", i + 4)
                stop = n if close < 0 else line_end(close + 3)
                blank_to(i, stop)
                i = stop
                continue

            decl = next((end for pat, end in DECL if pat.match(line)), None)
            if decl is not None:
                close = text.find(decl, i + 2)
                stop = n if close < 0 else line_end(close + len(decl))
                blank_to(i, stop)
                i = stop
                continue

            # `hm` was decided above, and is already None for an autolink or
            # a type-7 tag that does not open a block.
            if hm:
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
                blank_to(i, stop)
                i = stop
                continue

        if text.startswith("<!--", i):
            close = text.find("-->", i + 4)
            stop = n if close < 0 else close + 3
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
            if (j - i) % 2 == 1 and j < n and text[j] in "[!":
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
                    blank_to(start, j)
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
                        k += 2
                        continue
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
                close = ")" if text[label] == "(" else "]"
                end = balanced(label, text[label], close)
                if end is not None:
                    blank_to(i, end)
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
    if len(target) > 1 and target.startswith("<") and target.endswith(">"):
        target = target[1:-1].strip()
    # `alpha.md#section` names the page `alpha.md`. Inline destinations are
    # already split by `_FRAG`, but a REFERENCE definition arrives whole, and
    # comparing the fragment as part of the filename rejected valid rows and
    # valid README links. Stripping here rather than at each call site is the
    # point: a caller cannot forget it.
    target = target.split("#", 1)[0].rstrip()
    target = target.rstrip("/")
    if not target or target.startswith(("http://", "https://", "mailto:")):
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
        if (SETEXT_H1.match(line) and line_above.strip()
                and not ATX.match(line_above)
                and not THEMATIC.match(line_above)
                and not LIST_ITEM.match(line_above)):
            section = None
            continue
        target = None
        m = ENTRY.match(line)
        if m:
            target = m.group(1)
        else:
            r = ENTRY_REF.match(line)
            if r:
                # `[text][label]` uses `label`; `[label][]` and the shortcut
                # `[label]` use the text itself. An undefined label is not a
                # link, so the row is not an entry and the page it meant to
                # list is reported as listed nowhere — the safe direction,
                # and the same one an unparseable inline row already takes.
                key = label_key(r.group(2) or r.group(1))
                target = defs.get(key)
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
    if any(normalise(m.group(1), "") == INDEX
           for m in LINK.finditer(blank_defns(text))):
        return True
    # Definitions, then the labels actually referenced by a full
    # (`[text][label]`), collapsed (`[label][]`) or shortcut (`[label]`)
    # reference. A definition nothing references is not a link.
    #
    # `definitions()` blanks inline-link spans itself; the USES below are
    # scanned over the same blanked text, since a `[Guide][catalog]` sitting
    # inside a link's title is title text and reaches nothing.
    defs = definitions(text)
    text = blank_links(text)
    if not defs:
        return False
    # No `\\` in these lookbehinds. `readable()` already resolved escape
    # PARITY and blanked any `[` a live escape applies to, so re-testing one
    # character here rejected `\\[Guide][]` — an escaped backslash followed
    # by a real reference link. `LINK` dropped this two rounds ago and these
    # two kept it, which is the same one-of-two-sites miss as four findings
    # before it; they are now the last of that shape in the file.
    used = {label_key(m.group(2)) or label_key(m.group(1))
            for m in re.finditer(r"(?<!!)\[([^\]]*)\]\[([^\]]*)\]", text)}
    used |= {label_key(m.group(1))
             for m in re.finditer(r"(?<!!)\[([^\]]+)\](?![\[(:])", text)}
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

  # 72. The newline guard. A link span may straddle lines; blanking its
  #     newline would join the next line to it and drop the `^` that the
  #     definition scan anchors on, losing a good definition.
  _scaffold ref_multiline_link
  printf '# A\n' > "$tmp/ref_multiline_link/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/ref_multiline_link/docs/guide/index.md"
  printf '[X](y.md "a\nb")\n[catalog]: docs/guide/index.md\n\n[Guide][catalog]\n' \
    > "$tmp/ref_multiline_link/README.md"
  _commit ref_multiline_link
  _case "a definition after a multiline link survives" 0 ref_multiline_link

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
