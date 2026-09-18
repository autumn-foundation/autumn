#!/usr/bin/env bash
# Retrieval gate: every reader question in the fixture must land on the page
# that answers it, using only the words the asker would type.
#
# WHY THIS EXISTS: the corpus already gates eleven things about a page a reader
# HAS REACHED — its links (`check-docs-links.sh`, a 404), its commands
# (`check-docs-cli.sh`), the `AUTUMN_*` variables it tells them to SET
# (`check-docs-config.sh`), the `autumn.toml` keys it tells them to WRITE
# (`check-docs-toml.sh`), the `autumn_web::…` paths they IMPORT
# (`check-docs-symbols.sh`), the `/actuator/…` URLs they CURL
# (`check-docs-routes.sh`), the macro arguments they copy
# (`check-docs-macro-args.sh`), the Cargo feature a gated snippet needs
# (`check-docs-features.sh`), the dependency pin all of it is relative to
# (`check-docs-versions.sh`), and the agreement between those corpora
# (`check-docs-scope.sh`). `check-docs-orphans.sh` adds the one thing that is
# not about the page's contents: that the page can be REACHED AT ALL, by
# clicking, from a surface a reader enters through.
#
# Reachable is not the same as findable. `check-docs-orphans.sh` proves a path
# exists from the README to the page. It cannot ask the question a reader
# actually arrives with, which is not "which link do I click" but "what do I
# type". A reader mid-task does not read a 162-entry index; they search their
# own words, and a page whose title and headings are spelled in the project's
# vocabulary instead of theirs is invisible to that search while remaining
# perfectly reachable, perfectly accurate, and perfectly linked. Every other
# gate stays green over it.
#
# The baseline run found exactly that. `docs/guide/logging-pii.md` carries the
# answer to "how do I change the log level" — `[log] level`, `log.format`, the
# access log switch — under the title "Logging & PII", and the README lists it
# under that name. The words "log level" appeared in the title or a heading of
# none of the 162 guide pages, so the question returned nothing, while the
# `[log]` section itself appeared in 9 fences across 7 pages. The runtime half
# of the same question — `PUT /actuator/loggers/{name}`, which changes a live
# `tracing` subscriber without a redeploy — appeared on NO reader-facing page
# at all: it was documented in `skills/autumn-web/SKILL.md` (a context pack for
# agents, not a page a person lands on), named in one comparison-table row, and
# otherwise mentioned only in `deployment.md`'s list of endpoints production
# turns OFF.
#
# HOW IT MODELS RETRIEVAL. A page announces itself to a search in three places,
# in descending weight: its slug (the filename, which is also the URL), its H1,
# and its other headings. Body text is deliberately NOT searched: a word buried
# in paragraph nine is what "the answer is in there somewhere" means, and it is
# the defect this gate exists to catch, not the pass condition. A question
# matches a page when every content word in the question appears in one of
# those three places on that page.
#
# Matching is deliberately crude — casefold, split on non-alphanumerics, drop a
# short stopword list, fold a trailing plural `s`. A reader's query is crude
# too, and a cleverer matcher would start passing questions a real search
# engine would fail.
#
# THE FIXTURE IS THE EVIDENCE. `scripts/docs-retrieval-questions.tsv` pairs a
# question with the page that answers it. A question is added when a reader
# failure is observed, not because a page looks like it deserves one, and it is
# removed only with the page. Because the fixture pins the ANSWERING page and
# not merely "some hit", a retitle that makes a page findable to a different
# question fails here rather than silently re-aiming it.
#
# WHAT A FAILURE MEANS. A question that lands nowhere, or lands on a page other
# than the one that answers it, is a findability defect, and the fix is a
# retitle, a heading, or a crosslink — rung 4. It is NOT a new page: a second
# page answering the same question splits the rank that would have let a reader
# find either, and the corpus then carries both forever.
#
# Run locally with:
#
#     ./scripts/check-docs-retrieval.sh
#     ./scripts/check-docs-retrieval.sh --list       # every question and where it lands
#     ./scripts/check-docs-retrieval.sh --self-test  # the matcher's own tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

read -r -d '' PYSRC <<'PYEOF' || true
import pathlib
import re
import subprocess
import sys

MODE = sys.argv[1]
ROOT = pathlib.Path(sys.argv[2])

# The guide is the corpus a question is asked OF. The reader-facing corpus the
# sibling gates share is wider (README, EXAMPLES, skills/, agents/, the example
# READMEs), but those are entry surfaces and context packs rather than pages a
# search is expected to land a mid-task reader on; `check-docs-orphans.sh`
# already treats the guide as the answering surface for the same reason.
GUIDE = 'docs/guide/'
FIXTURE = 'scripts/docs-retrieval-questions.tsv'

# CommonMark type-6 block tags: a line opening with one of these starts a raw
# HTML block that runs to the next blank line, so nothing inside is Markdown.
CONTAINER_TAGS = (
    r'(address|article|aside|base|blockquote|body|caption|center|col|colgroup'
    r'|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form'
    r'|frame|frameset|h1|h2|h3|h4|h5|h6|head|header|hr|html|iframe|legend|li'
    r'|link|main|menu|menuitem|nav|noframes|ol|optgroup|option|p|param|search'
    r'|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)'
)

# Words a reader types that carry no retrieval signal. Kept short on purpose:
# every entry here is a word the matcher stops requiring, so a long list turns
# a failing question into a passing one without changing a page.
STOPWORDS = {
    'a', 'an', 'and', 'are', 'at', 'be', 'by', 'can', 'do', 'does', 'for',
    'from', 'how', 'i', 'in', 'is', 'it', 'me', 'my', 'of', 'on', 'or', 'the',
    'to', 'use', 'using', 'what', 'when', 'where', 'with', 'you', 'your',
}


def code_span_end(text, i):
    """End index of the inline code span starting at `i`, or None.

    ONE implementation, used by both `uncomment` and `_unlink`. They each need
    to leave code spans alone — a `<!--` in one opens nothing, a `[x](y)` in
    one links nowhere — and two copies of this rule would be two things to
    keep in step by hand. That is exactly how the in-comment and
    out-of-comment paths drifted earlier in this gate's life.
    """
    if text[i:i + 1] != '`':
        return None
    j = i
    while j < len(text) and text[j] == '`':
        j += 1
    ticks = text[i:j]
    # CommonMark closes a span with a run of EXACTLY the same length, so a
    # candidate must be a WHOLE run: no backtick on either side of it.
    # Checking only the character after was not enough — for a one-backtick
    # opener and a later two-backtick run, the first candidate is rejected and
    # `find` then returns the run's SECOND backtick, which has no backtick
    # after it and was accepted as a closer that CommonMark does not have.
    close = text.find(ticks, j)
    while close != -1 and not (
            text[close - 1:close] != '`'
            and text[close + len(ticks):close + len(ticks) + 1] != '`'):
        close = text.find(ticks, close + 1)
    if close == -1:
        return None                        # unclosed: literal backticks
    return close + len(ticks)


def uncomment(line, in_comment):
    """Split one line into what a renderer shows, and the comment state after.

    Scanned, not substring-tested, for the reason `_unlink` is: `'-->' in line`
    answers "did a comment close somewhere" when the question is "what is the
    state at the END of this line". `--> <!-- another` does both, and reporting
    only the close hides the next line's heading from the index while the gate
    still calls it visible.

    One function for both directions, so the in-comment and out-of-comment
    paths cannot disagree again — they had to be kept in step by hand before,
    and were not.
    """
    out, i, n = [], 0, len(line)
    while i < n:
        if in_comment:
            k = line.find('-->', i)
            if k == -1:
                break                      # comment runs past this line
            in_comment = False
            i = k + 3
            continue

        # An inline CODE SPAN is literal text, so a `<!--` inside one opens
        # nothing: a heading like ``## The `<!--` marker`` is visible, and
        # treating its literal as an opener loses that heading AND leaves the
        # scanner "in a comment" for every line after it, until some later
        # `-->`. That is the losing-visible-text direction with a whole page
        # of blast radius, so code spans are recognised before comments.
        #
        # Backticks inside a comment are NOT a code span — they are comment
        # text — which is why this sits in the else branch.
        # A BACKSLASH-escaped character is literal, and that includes a
        # backtick: `\\`` opens no code span. `_unlink` already honoured
        # escapes; `uncomment` did not, so the two disagreed even after they
        # started sharing `code_span_end`. Sharing the span scanner was not
        # enough — the decision of WHERE a span can start has to be shared
        # too, and this is that decision.
        if line[i] == '\\' and i + 1 < n:
            out.append(line[i:i + 2])
            i += 2
            continue

        end = code_span_end(line, i)
        if end is not None:
            out.append(line[i:end])        # the span renders verbatim
            i = end
            continue

        if line.startswith('<!--', i):
            in_comment = True
            i += 4
            continue

        out.append(line[i])
        i += 1
    return ''.join(out), in_comment


TITLE_CLOSER = {'"': '"', "'": "'", '(': ')'}

# CommonMark escapes ASCII PUNCTUATION and nothing else. A backslash before
# anything else — a space, a letter — is a literal backslash, which is why
# `(secret\ runtime\ logger)` is not a destination and not a link: the spaces
# are real, so a reader sees all three words.
ASCII_PUNCT = frozenset('!"#$%&\'()*+,-./:;<=>?@[\\]^_`{|}~')


# CommonMark type 7: a line holding ONE complete tag and nothing else but
# whitespace. The "nothing else" is what keeps prose out — `<Foo> is the type`
# has text after the tag, so it is a paragraph, while `<Foo>` alone is a raw
# HTML block whose contents no reader sees.
_ATTR = (r'[a-zA-Z_:][a-zA-Z0-9_.:-]*'
         r"(?:\s*=\s*(?:[^\s\"'=<>`]+|'[^']*'|\"[^\"]*\"))?")
_TAG = (rf'<[a-zA-Z][a-zA-Z0-9-]*(?:\s+{_ATTR})*\s*/?>'
        rf'|</[a-zA-Z][a-zA-Z0-9-]*\s*>')
TYPE7_TAG = re.compile(rf'^ {{0,3}}(?:{_TAG})[ \t]*$')
# The same grammar inline. A tag name cannot be followed by `:` or `@`, so
# `<https://example.com>` and `<a@b.com>` are autolinks and stay put — there
# the URL IS the rendered text.
INLINE_TAG = re.compile(_TAG)
# CommonMark's inline raw HTML is a tag, a comment, a processing instruction,
# a declaration or a CDATA section. The last three render as markup too —
# `<?secret runtime logger?>` shows a reader nothing — so they are stripped
# beside tags. Comments are already gone by here: `uncomment` runs first,
# because its state has to be carried across lines and this does not.
INLINE_RAW = re.compile(
    r'<\?.*?\?>'                       # processing instruction
    r'|<!\[CDATA\[.*?\]\]>'            # CDATA section (before the next one)
    r'|<![A-Za-z][^>]*>',              # declaration
    re.S)


def _escaped(text, i):
    """Whether `text[i]` is a backslash that escapes the character after it."""
    return (text[i] == '\\' and i + 1 < len(text)
            and text[i + 1] in ASCII_PUNCT)


def _closes_at(text, i, closer):
    """Index just past `closer` in `text[i:]`, or -1 if it never closes.

    Backslash escapes are honoured, so a `\\"` inside a quoted title does not
    end it.
    """
    while i < len(text):
        if _escaped(text, i):
            i += 2
            continue
        if text[i] == closer:
            return i + 1
        i += 1
    return -1


def _after_title_open(text):
    """State after `text`, which may OPEN a definition title at its first char.

    `None` when it cannot be a title at all — the caller then knows the
    definition is over and the line is ordinary text. A title has to be the
    WHOLE of what is left: CommonMark reads `[foo]: /url` then `"title" ok`
    as a definition without a title followed by a paragraph, so a closer with
    anything after it is not a title either.
    """
    closer = TITLE_CLOSER.get(text[:1])
    if closer is None:
        return None
    end = _closes_at(text, 1, closer)
    if end < 0:
        return ('title', closer)          # runs on to the next line
    return False if not text[end:].strip() else None


def _dest_end(text):
    """Index just past a legal link destination at the start of `text`, -1 if
    there is none.

    This is what decides whether a `[label]:` line began a definition at all.
    A destination is `<…>` or an unbroken run of non-space characters with
    balanced parens — so `Secret runtime logger` is not one, `[Overview]:`
    over it is no definition, and the two lines are the paragraph a `---`
    then turns into a real heading.
    """
    if text.startswith('<'):
        end = _closes_at(text, 1, '>')
        if end < 0:
            return -1              # `<` with no `>` is not a destination
        return end
    i, depth = 0, 0
    while i < len(text):
        c = text[i]
        if _escaped(text, i):
            i += 2
            continue
        if c.isspace():
            break
        if ord(c) < 0x20 or c == '\x7f':
            return -1              # control characters are not allowed
        if c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
            if depth < 0:
                return -1
        i += 1
    return i if i and depth == 0 else -1


def _is_link_target(inner):
    """Whether what a link's `(…)` holds is a legal destination and title.

    The same question `_after_dest` asks of a reference definition, asked of
    an inline link — one rule, two syntaxes. `(secret runtime logger)` fails
    it because an unbracketed destination may not contain spaces, which is
    why CommonMark renders that construct as the literal text it looks like.
    """
    s = inner.strip()
    if not s:
        return True            # `[a]()` is a link with an empty destination
    end = _dest_end(s)
    if end < 0:
        return False
    rest = s[end:].strip()
    # A complete, closed title and nothing after it, or no title at all.
    return not rest or _after_title_open(rest) is False


def _title_runs(text, n, body_at):
    """Follow a definition title that opens at `text[0]` from line `n` on.

    Returns `(closed, next_line)`. A title may span lines, so this reads
    ahead until its closer — and a closer with anything after it does not
    count, the way `[foo]: /url "title" ok` is a paragraph.
    """
    closer = TITLE_CLOSER.get(text[:1])
    if closer is None:
        return False, n
    end = _closes_at(text, 1, closer)
    if end >= 0:
        return not text[end:].strip(), n
    while True:
        nxt = body_at(n)
        if nxt is None:
            return False, n              # ran out of lines: never a title
        n += 1
        end = _closes_at(nxt, 0, closer)
        if end >= 0:
            return not nxt[end:].strip(), n


def defn_extent(lines, i):
    """`(label, line count)` for a link reference definition starting at
    `lines[i]`, or `(None, 0)` if there is none.

    LOOKAHEAD, deliberately, where this used to consume a line at a time and
    hope. A definition whose destination is not a destination, or whose title
    opens and never closes, is not a definition at all — and every line it
    looked like it covered is paragraph text that a `---` may then turn into
    a real heading. The incremental version had to hand those lines back, and
    its own comment admitted the case it could not: "its earlier lines are
    already consumed by then, so there is nothing to hand back." Deciding the
    whole extent first means never owing anything.
    """
    depth, body = unquote(lines[i])
    label, after = _defn_at(body)
    if label is None:
        return None, 0

    def body_at(k):
        """Line `k`'s text inside the same quote, or `None` at the end of it.

        A blank line ends a definition, and so does leaving the quote that
        holds it — neither can appear in the middle of one.
        """
        if k >= len(lines):
            return None
        d, b = unquote(lines[k])
        return b.strip() if d == depth and b.strip() else None

    n = i + 1
    rest = body[after:].strip()
    if not rest:                         # `[label]:` with the rest below it
        rest = body_at(n)
        if rest is None:
            return None, 0
        n += 1

    end = _dest_end(rest)
    if end < 0:
        return None, 0
    rest = rest[end:].strip()

    if rest:
        # A title opening on the DESTINATION's line has to close, or nothing
        # on that line is a definition.
        closed, n = _title_runs(rest, n, body_at)
        return (label, n - i) if closed else (None, 0)

    # A title MAY open on the next line. If it does not, or it never closes,
    # the definition is already complete without one and those lines are not
    # ours — CommonMark reads `[foo]: /url` over `"title" ok` as exactly that.
    done = n - i
    nxt = body_at(n)
    if nxt is None:
        return label, done
    closed, n = _title_runs(nxt, n + 1, body_at)
    return (label, n - i) if closed else (label, done)


CONTAINER_MARKER = re.compile(r'^ {0,3}(?:>[ \t]?|(?:[-*+]|\d{1,9}[.)])[ \t]+)')


def uncontain(line):
    """A line with its block-container markers taken off the front.

    `> ## Heading` and `- ## Heading` are REAL headings: CommonMark renders
    and anchors them inside the quote or the list item, so a reader can land
    on one and the gate has to see it. Inside a container the rest of the
    block syntax applies too, which is why the FENCE and raw-HTML checks read
    this form as well — recognising `> # comment` as a heading while `> ```
    opened no fence would turn a false negative into the worse kind, a
    shell comment indexed as a page's heading.

    Only markers on the line itself come off. A heading that sits in a
    container by INDENTATION alone — a deeply nested list where four spaces
    are content rather than code — needs a parser tracking container widths,
    and is not claimed here. Nor does the setext path use this: `- item` over
    `---` is a list and a thematic break, not a heading, so `is_paragraph`
    goes on reading the raw line.
    """
    while True:
        m = CONTAINER_MARKER.match(line)
        if not m:
            return line
        line = line[m.end():]


QUOTE_MARKER = re.compile(r'^ {0,3}>[ \t]?')


def unquote(line):
    """`(depth, rest)`: how many block-quote markers a line carries, and what
    is left after them.

    The setext path needs the DEPTH, not just the text. `> Secret runtime
    logger` over `> ---` is a real heading; the same text over an unquoted
    `---` is a paragraph in a quote followed by a thematic break. Only a
    matching prefix on both lines makes the underline an underline, which is
    also why this stops at quote markers: a list item's setext heading is
    decided by content INDENTATION, the case `uncontain` already declines.
    """
    depth = 0
    while True:
        m = QUOTE_MARKER.match(line)
        if not m:
            return depth, line
        line = line[m.end():]
        depth += 1


def _label_end(text, i):
    """Index just past the `]` closing a link LABEL that opens at `text[i]`,
    or -1 if there is none.

    CommonMark: a label ends at the first `]` that is NOT backslash-escaped,
    holds no unescaped `[`, must contain at least one character that is not
    whitespace, and is at most 999 characters between the brackets. `[]:` is
    therefore no definition at all, and renders as the text it looks like.
    """
    j = i + 1
    while j < len(text):
        if _escaped(text, j):
            j += 2
            continue
        if text[j] == '[':
            return -1
        if text[j] == ']':
            inner = text[i + 1:j]
            if not inner.strip() or len(inner) > 999:
                return -1
            return j + 1
        j += 1
    return -1


def _defn_at(text):
    """The label of a link reference definition at the start of `text`, and
    the index just past its `]:` — or `(None, -1)` if there is none.

    A label takes backslash escapes, so `[Foo*bar\]]: /url` is a definition
    whose label is `Foo*bar]` (CommonMark example 194). A `[^\]]*` pattern
    stopped at that escaped bracket, called the line paragraph text, and a
    `---` under it then indexed a destination no reader sees.
    """
    m = re.match(r'^ {0,3}\[', text)
    if not m:
        return None, -1
    end = _label_end(text, m.end() - 1)
    if end < 0 or text[end:end + 1] != ':':
        return None, -1
    return text[m.end():end - 1], end + 1


def is_paragraph(line):
    """Whether a line is paragraph text, and so can carry a setext underline.

    CommonMark only makes an underline a heading when what precedes it is a
    paragraph. A list item, a block quote, a table row and indented code are
    all something else, and a `---` after them is a thematic break or part of
    the block — so treating them as setext text indexes words that render as
    anything but a heading.
    """
    if not line.strip():
        return False
    if line.startswith('    ') or line.startswith('\t'):
        return False            # indented code
    stripped = line.lstrip(' ')
    if stripped.startswith(('>', '|', '<')):
        return False            # block quote, table row, raw HTML
    if re.match(r'^([-*+_])\s', stripped):
        return False            # bullet list item
    if re.match(r'^\d+[.)]\s', stripped):
        return False            # ordered list item
    if re.match(r'^([-*_])(\s*\1){2,}\s*$', stripped):
        return False            # thematic break
    # A line that LOOKS like a link reference definition is not rejected
    # here. `defn_extent` has already claimed the real ones — and only the
    # real ones, so `[a]: not a url` reaches this point as the paragraph text
    # it renders as.
    return True


def rendered(title, defined=None):
    """A heading's visible text: what a renderer shows, not its source.

    A link's DESTINATION is markup, not words on the page — a reader sees
    "Overview", not `secret-runtime-logger.md` — so indexing the raw source
    lets a row match a path no reader can read. Same reasoning as the fenced
    and commented cases: if it is not shown, it is not findable.

    Autolinks (`<https://example.com>`) are deliberately left alone: there the
    URL *is* the rendered text.

    Real inline HTML tags ARE stripped, but only where they are markup. An
    earlier round left them alone, reasoning that every `<…>` in a guide
    heading today — 21 of them — sits inside a code span (`Auth<T>`,
    `Query<T>`, `autumn credentials edit [--env <env>]`) where it is literal
    text a reader sees, and that a naive `<[^>]*>` strip would take the
    visible half of all 21. The first half of that is right and the
    conclusion was wrong: "no page does this yet" is not a rule, and leaving
    tags in indexed `<span title="secret runtime logger">Overview</span>` as
    four searchable words a reader never sees. The strip happens inside the
    scanner, which already knows where code spans are, so both directions
    hold: markup goes, `Query<T>` stays.
    """
    return _unlink(title, defined).strip()


def ref_label(text):
    """A reference label in the form CommonMark compares them by.

    Case-folded, with internal whitespace collapsed, so `[Secret  Logger]`
    and `[secret logger]` name the same definition.
    """
    return ' '.join(text.split()).casefold()


def _unlink(text, defined=None):
    """Replace every `[label](dest)`, `![alt](src)` and `[label][ref]` with
    its label, leaving everything else untouched.

    Scanned rather than pattern-matched, on purpose. Three review rounds found
    three different markup shapes that a regex per shape did not cover, and a
    destination regex is the clearest case of why: `[^)]*` stops at the first
    `)`, so `[Overview](foo(bar)-secret.md)` leaves `-secret.md)` in the title
    and the invisible half is indexed again. CommonMark allows balanced
    parentheses in a destination, and a backslash escapes either. Counting
    depth handles every valid destination at once instead of adding an
    epicycle per counter-example.

    `defined` is the set of reference labels the page actually defines. A
    `[label][ref]` with no definition is NOT a link — CommonMark renders the
    whole construct literally — so reducing it to `label` deletes words a
    reader can see. `None` means "no page context", and then a reference is
    assumed to resolve.
    """
    out, i, n = [], 0, len(text)
    while i < n:
        ch = text[i]
        if ch == '\\' and i + 1 < n:          # an escape covers the next char
            out.append(text[i:i + 2])
            i += 2
            continue
        # A link written INSIDE a code span is literal text a reader sees, so
        # reducing it to its label would drop words that are on the page —
        # ``## `[Overview](secret.md)` `` renders the whole thing.
        span = code_span_end(text, i)
        if span is not None:
            out.append(text[i:span])
            i = span
            continue

        # A real inline tag renders as markup, not words: a reader of
        # `<span title="secret runtime logger">Overview</span>` sees
        # "Overview" and nothing else, so indexing the attribute is the
        # invisible-text failure again — and it was reproduced as a false
        # POSITIVE, the gate reporting a hit on words that are not on the
        # page. Reached only outside code spans, which is what keeps
        # `Auth<T>` and `Query<T>` intact.
        tag = INLINE_TAG.match(text, i) or INLINE_RAW.match(text, i)
        if tag:
            i = tag.end()
            continue
        # A label opens at `[`, or at `![` for an image.
        bang = ch == '!' and i + 1 < n and text[i + 1] == '['
        if ch == '[' or bang:
            label_start = i + (2 if bang else 1)
            j, depth = label_start, 1
            while j < n and depth:
                if text[j] == '\\':
                    j += 2
                    continue
                if text[j] == '[':
                    depth += 1
                elif text[j] == ']':
                    depth -= 1
                j += 1
            if depth == 0:
                label = text[label_start:j - 1]
                if j < n and text[j] == '(':         # inline: [label](dest)
                    k, pdepth = j + 1, 1
                    while k < n and pdepth:
                        c = text[k]
                        if c == '\\':
                            k += 2
                            continue
                        # An optional TITLE follows the destination after
                        # whitespace, quoted with `"` or `'`, and may contain
                        # an unmatched parenthesis: `(foo "a ( b")`. Counting
                        # its parens as nesting means the link never closes
                        # and the whole raw source stays in the title. A
                        # PARENTHESISED title needs no case — its parens are
                        # balanced, so depth counting already handles it.
                        if c in '"\'' and text[k - 1] in ' \t':
                            k += 1
                            while k < n:
                                if text[k] == '\\':
                                    k += 2
                                    continue
                                if text[k] == c:
                                    k += 1
                                    break
                                k += 1
                            continue
                        # `<…>` destination: parens inside are literal.
                        if c == '<' and (k == j + 1 or text[k - 1] in ' \t'):
                            k += 1
                            while k < n:
                                if text[k] == '\\':
                                    k += 2
                                    continue
                                if text[k] == '>':
                                    k += 1
                                    break
                                k += 1
                            continue
                        if c == '(':
                            pdepth += 1
                        elif c == ')':
                            pdepth -= 1
                        k += 1
                    if pdepth == 0 and _is_link_target(text[j + 1:k - 1]):
                        # Only a LEGAL target may be dropped. Balanced parens
                        # are not enough: `[Overview](secret runtime logger)`
                        # has spaces in an unbracketed destination, so it is
                        # no link and a reader sees every word of it.
                        # Reducing it to `Overview` deleted visible text from
                        # the index — the mirror image of indexing invisible
                        # text, and a MISS on a heading that matches by eye.
                        out.append(_unlink(label, defined))
                        i = k
                        continue
                elif j < n and text[j] == '[':       # reference: [label][ref]
                    # `[label][]` is the collapsed form: the label is its own
                    # reference, and the empty `[]` is the one place a label
                    # may be empty. Everything else goes through the same
                    # escape-aware scan as a definition's label, or an
                    # escaped `]` inside the reference ends it early and the
                    # link goes unrecognised — leaving its invisible label
                    # in the index.
                    if text[j:j + 2] == '[]':
                        k, ref = j + 2, label
                    else:
                        k = _label_end(text, j)
                        ref = text[j + 1:k - 1] if k != -1 else None
                    # An UNDEFINED reference is not a link at all, so it
                    # falls through and renders verbatim.
                    if ref is not None and (defined is None
                                            or ref_label(ref) in defined):
                        out.append(_unlink(label, defined))
                        i = k
                        continue
        out.append(ch)
        i += 1
    return ''.join(out)


def words(text):
    """Casefold, split on non-alphanumerics, drop stopwords, fold plurals."""
    out = []
    for raw in re.split(r'[^a-z0-9]+', text.casefold()):
        if not raw or raw in STOPWORDS:
            continue
        # `logs` and `log`, `levels` and `level`. Not a stemmer: only a
        # trailing `s` on a word long enough that dropping it is not a
        # different word (`as`, `is` are stopwords already).
        if len(raw) > 3 and raw.endswith('s') and not raw.endswith('ss'):
            raw = raw[:-1]
        out.append(raw)
    return out


def tracked(pattern):
    res = subprocess.run(['git', 'ls-files', pattern], cwd=ROOT,
                         capture_output=True, text=True, check=True)
    return [p for p in res.stdout.split('\n') if p]


def index():
    """Per guide page, the three places it announces itself to a search."""
    pages = {}
    for rel in tracked(GUIDE + '**/*.md') + tracked(GUIDE + '*.md'):
        if rel in pages:
            continue
        text = (ROOT / rel).read_text(encoding='utf-8')
        slug = pathlib.PurePath(rel).stem
        # Headings are collected RAW and rendered after the page, because
        # whether `[Overview][ref]` is a link depends on a definition that may
        # sit below the heading that uses it. Rendering as we go would have to
        # guess, and guessing "it resolves" deletes visible words.
        raw_headings = []        # (level, source text)
        defined = set()          # reference labels this page defines
        fence = None            # (delimiter run, owning quote depth)
        comment = False
        comment_depth = 0
        html_block = None       # (kind, owning quote depth)
        skip = 0                # lines a definition has already claimed
        para = []
        para_depth = 0
        lines = text.splitlines()
        for idx, line in enumerate(lines):
            # Every block below — a fence, a raw HTML block, a comment — ends
            # when the BLOCK QUOTE holding it ends, because none of the three
            # can be lazily continued. So each records the quote depth it
            # opened at, and a line shallower than that closes it wherever it
            # had got to. Without this, `> <script>` inside a quote waited for
            # a `</script>` that the quote's end had already made
            # unnecessary, and suppressed every heading to EOF.
            #
            # Quote depth, not `uncontain`'s marker count: a list item is
            # continued by INDENTATION, so its lines carry no marker and would
            # read as depth 0, closing every block it holds on its second
            # line. Read from the RAW line, before any comment is stripped,
            # because that is the line the container structure is made of.
            container = unquote(line)[0]

            # Lines a definition already claimed. Nothing in one is visible,
            # and nothing in one can open a block: the lookahead that claimed
            # them checked that.
            if skip:
                skip -= 1
                para = []
                continue
            # A heading inside an HTML comment is not a heading: no renderer
            # shows it and no reader can navigate to it, so indexing one is
            # the same false positive as indexing a fenced `#` line. This
            # corpus writes its gate waivers as multi-line `<!-- … -->`
            # blocks, which is exactly where a quoted heading would appear.
            # Checked INSIDE the fence check below only when not fenced: a
            # `<!--` inside a code fence is sample text, not a comment.
            # A `#` inside a fence is a shell comment, a TOML comment or a Rust
            # attribute, not a heading, and it must not be indexed: a `# Raise
            # the global level` comment in a curl fence would let a question
            # match the page that fence sits on, which is exactly the page a
            # fixture row names — so the false positive lands on the EXPECTED
            # page and the gate passes while the reader still finds nothing.
            if comment and comment_depth and container < comment_depth:
                comment = False         # the quote that held it ended
            if comment:
                # A line that BEGINS inside a comment can start no heading:
                # a `#` after a mid-line `-->` is not at the start of the
                # line. But the state still has to be carried correctly
                # across it, because `--> <!-- another` both closes and
                # reopens, and treating that as "closed" would index the
                # next line's hidden heading.
                visible, comment = uncomment(line, True)
                if not comment:
                    comment_depth = 0
                # What the comment left behind is ordinary text, and
                # throwing it away lost a heading a reader sees: `Secret
                # <!--`, `--> runtime logger`, `---` renders one setext
                # heading carrying both halves. It can start no BLOCK, as
                # above, but it can continue — or begin — a paragraph.
                if is_paragraph(visible):
                    if not para:
                        para_depth = container
                    para.append(visible)
                else:
                    para = []
                continue

            # A CommonMark type-1 raw HTML block — `<script>`, `<pre>`,
            # `<style>`, `<textarea>` — runs to its closing tag, and nothing
            # inside it is Markdown. A heading-shaped line in there renders
            # as script text, so indexing it is the fenced case again with
            # angle brackets.
            #
            # Type 6 — a container tag such as `<div>` or `<table>` — is a
            # raw HTML block too, and ends at a BLANK LINE rather than a
            # closing tag. An earlier round of this gate declined to track it,
            # on the grounds that a wrong blank-line rule would swallow real
            # headings. That was the right worry and the wrong conclusion: the
            # rule is one line, and "would get it wrong" is a thing to test,
            # not a reason to leave a hole. Both directions are pinned by the
            # self-test.
            #
            # Type 7 — one complete tag alone on its line, any name — is
            # tracked too, below the type-6 check so the named tags keep
            # their own rule. It also ends at a blank line.
            if (html_block is not None and html_block[1]
                    and container < html_block[1]):
                # The quote that held it ended, so the block did. This line is
                # ordinary Markdown again and has to be READ, not skipped: the
                # heading right after a quoted `<script>` is exactly the one
                # that went missing.
                html_block = None
            if html_block is not None:
                kind = html_block[0]
                if kind == '#pi':
                    if '?>' in line:
                        html_block = None
                elif kind == '#cdata':
                    if ']]>' in line:
                        html_block = None
                elif kind == '#decl':
                    if '>' in line:
                        html_block = None
                elif kind == '#blank':
                    # `>` alone is a blank line INSIDE the quote, and that is
                    # what ends the block. Testing the raw line kept `#blank`
                    # open past it and swallowed the heading after — the
                    # opener became container-aware in this round, so the
                    # terminator has to be, or the pair is inconsistent in
                    # the direction that hides real headings.
                    #
                    # Quote markers only, not `uncontain`: a list item's
                    # blank line is absolutely blank in CommonMark, so
                    # stripping `- ` here would end the block on an empty
                    # list item that ends nothing.
                    if not unquote(line)[1].strip():
                        html_block = None
                elif re.search(rf'</{kind}\s*>', line, re.I):
                    html_block = None
                para = []
                continue

            # Literal SPACES only: a tab in column one expands to four
            # columns, so it cannot precede a fence at all. `\s{0,3}` let a
            # tabbed line close a fence that is still open, exposing hidden
            # content to the index.
            bare = uncontain(line)
            # Whether a LIST marker was stripped to get there. A marker means
            # a new list item begins on this line, so its delimiter is that
            # item's first content and cannot close a fence that belongs to
            # anything else — which is how a `- ``` ` INSIDE a top-level
            # fence was closing it and exposing the rest as visible text.
            # Openers are unaffected: `- ``` ` does open a fence.
            #
            # Not a depth, the way quotes are: a list item is continued by
            # INDENTATION, so its later lines carry no marker at all. Which
            # also marks the limit here — a fence opened inside an item and
            # left unclosed there runs on past the list, rather than ending
            # with it, and that needs container widths to fix.
            listed = bare != unquote(line)[1]
            if fence is not None and fence[1] and container < fence[1]:
                fence = None        # the quote that held the fence ended
            marker = re.match(r'^ {0,3}(`{3,}|~{3,})(.*)$', bare)
            if marker:
                run, rest = marker.group(1), marker.group(2)
                if fence is None and run[0] == '`' and '`' in rest:
                    # CommonMark: a BACKTICK fence's info string may not
                    # contain a backtick, so ```` ```a`b ```` opens nothing.
                    # Treating it as an opener suppressed every real heading
                    # after it — a false negative with the rest of the page as
                    # its blast radius.
                    para = []
                    continue
                if fence is None:
                    # Keep the opener VERBATIM, character and length. A page
                    # documenting markdown opens with ```` so it can show a
                    # ``` block inside; recording that opener as three would
                    # let the inner line close it, and every heading after it
                    # would be indexed while still inside the outer fence —
                    # the same false positive this block exists to stop.
                    # `rest` here is the info string (```bash), which an
                    # OPENING fence may carry. The quote depth goes with it:
                    # a delimiter in ANOTHER container is content, not a
                    # closer, so a top-level fence containing `> ``` ` used to
                    # close on it and index everything after as visible text —
                    # a false positive `uncontain` introduced.
                    fence = (run, container)
                elif (container == fence[1] and not listed
                      and run[0] == fence[0][0]
                      and len(run) >= len(fence[0])
                      and rest.strip(' \t') == ''):
                    # CommonMark: a closing fence is the same character, at
                    # least as long as the opener, and carries NO info string.
                    # So a `~~~` never closes a ``` block, a shorter run is
                    # content, and `~~~bash` inside a `~~~` fence is content
                    # too — closing on it would reopen the whole hole, since
                    # every heading-shaped line after it would be indexed
                    # while still fenced.
                    fence = None
                para = []
                continue
            if fence is not None:
                para = []
                continue

            # `\b` also matches before a hyphen, so `<script-widget>` was read
            # as a `<script>` block and the scanner then waited for a
            # `</script>` that never comes — suppressing every heading to EOF.
            # A type-1 tag name ends at whitespace, `>` or end of line.
            opener = re.match(r'^ {0,3}<(script|pre|style|textarea)(?=[\s>]|$)',
                              bare, re.I)
            if opener:
                tag = opener.group(1).lower()
                html_block = (tag, container)
                para = []
                if not re.search(rf'</{tag}\s*>', line, re.I):
                    continue
                html_block = None
                para = []
                continue

            if re.match(r'^ {0,3}<\?', bare):
                # CommonMark type 3: a processing instruction runs to `?>`,
                # and nothing inside it is Markdown.
                html_block = ('#pi', container)
                para = []
                if '?>' in line:
                    html_block = None
                continue

            if re.match(r'^ {0,3}<!\[CDATA\[', bare):
                html_block = ('#cdata', container)   # type 5, ends at `]]>`
                para = []
                if ']]>' in line:
                    html_block = None
                continue

            if re.match(r'^ {0,3}<![A-Za-z]', bare):
                # type 4 (`<!DOCTYPE …`), ends at `>`
                html_block = ('#decl', container)
                para = []
                if '>' in line:
                    html_block = None
                continue

            if re.match(rf'^ {{0,3}}</?{CONTAINER_TAGS}(?=[\s/>]|$)', bare, re.I):
                html_block = ('#blank', container)
                para = []
                continue

            # Type 7, reached only once types 1-6 have declined: one COMPLETE
            # tag alone on its line, any name, ending at a blank line. An
            # earlier round declined to track it on the grounds that the rule
            # would catch `<Foo>` in prose. It does not — a tag with text
            # after it fails the "nothing else on the line" test — and
            # declining indexed a `# heading` inside `<custom>…</custom>` as
            # if a reader could see it, which is a false POSITIVE: the gate
            # passes and the reader still finds nothing.
            #
            # `not para` because type 7, alone among the seven, cannot
            # interrupt a paragraph.
            if not para and TYPE7_TAG.match(bare):
                html_block = ('#blank', container)
                continue

            # A four-space-indented line with no paragraph open is an
            # indented CODE block, so a `<!--` in it is literal and must not
            # put the scanner into comment state — which would suppress every
            # heading until some later `-->`.
            if not para and re.match(r'^(    |\t)', line):
                continue

            # Whether this is a heading is decided on the RAW line, before
            # any comment is removed. `<!-- editorial -->## Heading` is an
            # HTML block in CommonMark, not a heading — the `##` is block
            # content — so stripping the comment first would manufacture a
            # heading that no renderer shows and let a row match it. The
            # strip still runs, because the comment state has to be carried
            # across this line either way.
            # Up to THREE leading spaces still makes an ATX heading; four is
            # an indented code block. Requiring column one made an ordinary,
            # harmless indentation change fail the gate on a page that
            # renders perfectly — a false NEGATIVE, and the only defect here
            # that could stop a contributor rather than let one through.
            is_heading = re.match(r'^ {0,3}#{1,6}\s', bare) is not None

            # Outside a fence: what a renderer would show of this line, and
            # whether a comment is left open past it. The VISIBLE text is
            # what gets indexed, so `# Page <!-- Secret runtime logger -->`
            # contributes "Page" and nothing else — and `# Page <!-- note`
            # still contributes "Page" even though the comment runs on.
            was_comment = comment
            line, comment = uncomment(line, False)
            if comment and not was_comment:
                comment_depth = container

            # A SETEXT heading is its text on one line and `===` (h1) or
            # `---` (h2) on the next. Missing them is a false NEGATIVE of the
            # same kind as requiring column one: the page renders a heading,
            # the gate does not see it, and an ordinary reformat fails CI.
            #
            # An underline only counts after PARAGRAPH text, which is what
            # `is_paragraph` decides. The first version of this comment
            # claimed that and the code did not do it: every non-blank line
            # reached `prev_para`, so `- Secret runtime logger` over `---`
            # indexed a LIST ITEM as a heading. A comment asserting a
            # property is not the same as enforcing it.
            depth, body = unquote(line)
            if para and depth == para_depth:
                under = re.match(r'^ {0,3}(=+|-+)\s*$', body)
                if under:
                    # CommonMark promotes the WHOLE preceding paragraph, which
                    # may be wrapped over several lines. Keeping only the last
                    # one indexed "logger" for a heading that reads "Secret
                    # runtime logger" — a false negative that would fail CI on
                    # nothing worse than a rewrap.
                    raw = ' '.join(l.strip() for l in para)
                    para = []
                    raw_headings.append((1 if under.group(1)[0] == '=' else 2,
                                         raw))
                    continue

            # `is_heading` gates only the ATX path: it is what stops
            # `<!-- x -->## H` — an HTML block — from being read as a
            # heading once the comment is stripped. It must NOT gate the
            # setext path above it, which is how that check was dead on
            # arrival the first time it was written.
            m = re.match(r'^ {0,3}(#{1,6})\s+(.*\S)\s*$', uncontain(line)) \
                if is_heading else None
            if not m:
                # Only a plain, non-blank text line can be setext text. A
                # blank line ends the paragraph, so the next `---` is a
                # thematic break rather than an underline; and a line that
                # LOOKED like a heading is never setext text either.
                # A definition renders as NOTHING, and its extent is decided
                # in one go rather than a line at a time — see `defn_extent`.
                # Only where no paragraph is open: a definition cannot
                # INTERRUPT one, so after prose the same line is continuation
                # text and the `---` under it makes a heading of both.
                if not para:
                    label, span = defn_extent(lines, idx)
                    if label is not None:
                        defined.add(ref_label(label))
                        skip = span - 1
                        continue
                if is_paragraph(body) and not is_heading:
                    para.append(body)
                    para_depth = depth
                else:
                    para = []       # a blank line or a block ends the paragraph
                continue
            para = []
            raw_headings.append((len(m.group(1)), m.group(2)))
        h1, headings = '', []
        for level, raw in raw_headings:
            title = rendered(raw, defined)
            if not title:
                continue
            if level == 1 and not h1:
                h1 = title
            else:
                headings.append(title)
        pages[rel] = (slug, h1, headings)
    return pages


def lands_on(question, pages):
    """Pages whose slug, H1 or a heading carries every word of the question."""
    need = words(question)
    if not need:
        return []
    hits = []
    for rel, (slug, h1, headings) in sorted(pages.items()):
        for where, text in (('slug', slug), ('h1', h1)):
            if text and all(w in words(text) for w in need):
                hits.append((rel, where, text))
                break
        else:
            for h in headings:
                if all(w in words(h) for w in need):
                    hits.append((rel, 'heading', h))
                    break
    return hits


def fixture():
    path = ROOT / FIXTURE
    rows = []
    for n, line in enumerate(path.read_text(encoding='utf-8').splitlines(), 1):
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        parts = line.split('\t')
        if len(parts) != 2:
            sys.exit(f'{FIXTURE}:{n}: expected "question<TAB>page", got {line!r}')
        rows.append((parts[0].strip(), parts[1].strip(), n))
    return rows


def main():
    pages = index()
    rows = fixture()

    if MODE == '--list':
        for question, expected, _ in rows:
            hits = lands_on(question, pages)
            mark = 'OK  ' if any(h[0] == expected for h in hits) else 'MISS'
            print(f'{mark} {question!r} -> {expected}')
            for rel, where, text in hits:
                print(f'       {rel}  ({where}: {text})')
            if not hits:
                print('       (no page announces these words)')
        return 0

    defects = []
    for question, expected, n in rows:
        if not (ROOT / expected).exists():
            defects.append(f'{FIXTURE}:{n}: {expected} does not exist')
            continue
        hits = lands_on(question, pages)
        if any(rel == expected for rel, _, _ in hits):
            continue
        if hits:
            landed = ', '.join(rel for rel, _, _ in hits)
            defects.append(
                f'{FIXTURE}:{n}: "{question}" lands on {landed} '
                f'but {expected} answers it')
        else:
            defects.append(
                f'{FIXTURE}:{n}: "{question}" lands nowhere; {expected} '
                f'answers it but says so in neither its slug, its H1, '
                f'nor any heading')

    print(f'guide pages indexed: {len(pages)}')
    print(f'reader questions checked: {len(rows)}')
    print(f'defects: {len(defects)}')
    for d in defects:
        print(f'  {d}')
    return 1 if defects else 0


sys.exit(main())
PYEOF

run_check() {
  python3 -c "$PYSRC" "${2---check}" "$1"
}

self_test() {
  local tmp pass=0 total=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  make_corpus() {
    local dir="$1"
    mkdir -p "$dir/docs/guide" "$dir/scripts"
    git -C "$dir" init -q 2>/dev/null || true
    git -C "$dir" config user.email t@t >/dev/null
    git -C "$dir" config user.name t >/dev/null
  }

  check() {
    local name="$1" want="$2" dir="$3"
    total=$((total + 1))
    git -C "$dir" add -A >/dev/null 2>&1 || true
    git -C "$dir" commit -qm fixture >/dev/null 2>&1 || true
    if python3 -c "$PYSRC" --check "$dir" >/dev/null 2>&1; then
      got=pass
    else
      got=fail
    fi
    if [[ "$got" == "$want" ]]; then
      pass=$((pass + 1))
    else
      echo "  self-test FAILED: $name (wanted $want, got $got)" >&2
    fi
  }

  # 1. A question whose words are in the page's slug lands.
  local c1="$tmp/c1"; make_corpus "$c1"
  printf '# Whatever\n' > "$c1/docs/guide/rate-limiting.md"
  printf 'rate limiting\tdocs/guide/rate-limiting.md\n' \
    > "$c1/scripts/docs-retrieval-questions.tsv"
  check "slug match lands" pass "$c1"

  # 2. A question answered only in body text does NOT land. This is the whole
  #    point of the gate: "it is in there somewhere" is the defect.
  local c2="$tmp/c2"; make_corpus "$c2"
  printf '# Logging & PII\n\nSet the log level with `[log] level`.\n' \
    > "$c2/docs/guide/logging-pii.md"
  printf 'log level\tdocs/guide/logging-pii.md\n' \
    > "$c2/scripts/docs-retrieval-questions.tsv"
  check "body-only answer does not land" fail "$c2"

  # 3. The same page, once a heading says it, lands.
  local c3="$tmp/c3"; make_corpus "$c3"
  printf '# Logging & PII\n\n## Set the log level\n\n`[log] level`.\n' \
    > "$c3/docs/guide/logging-pii.md"
  printf 'log level\tdocs/guide/logging-pii.md\n' \
    > "$c3/scripts/docs-retrieval-questions.tsv"
  check "heading match lands" pass "$c3"

  # 4. Landing on SOME page is not enough; it must be the answering page.
  local c4="$tmp/c4"; make_corpus "$c4"
  printf '# Log levels\n' > "$c4/docs/guide/other.md"
  printf '# Logging & PII\n' > "$c4/docs/guide/logging-pii.md"
  printf 'log level\tdocs/guide/logging-pii.md\n' \
    > "$c4/scripts/docs-retrieval-questions.tsv"
  check "landing on the wrong page is a defect" fail "$c4"

  # 5. Plural folding: the asker's "log levels" reaches "log level".
  local c5="$tmp/c5"; make_corpus "$c5"
  printf '# Logging\n\n## Set the log level\n' > "$c5/docs/guide/logging-pii.md"
  printf 'log levels\tdocs/guide/logging-pii.md\n' \
    > "$c5/scripts/docs-retrieval-questions.tsv"
  check "plural folds to singular" pass "$c5"

  # 6. A fixture naming a page that does not exist is a defect, not a pass.
  local c6="$tmp/c6"; make_corpus "$c6"
  printf '# Real\n' > "$c6/docs/guide/real.md"
  printf 'real\tdocs/guide/gone.md\n' \
    > "$c6/scripts/docs-retrieval-questions.tsv"
  check "fixture pointing at a missing page fails" fail "$c6"

  # 7. Stopwords do not carry the match: a question of only stopwords would
  #    otherwise land everywhere.
  local c7="$tmp/c7"; make_corpus "$c7"
  printf '# Anything\n' > "$c7/docs/guide/anything.md"
  printf 'how do i\tdocs/guide/anything.md\n' \
    > "$c7/scripts/docs-retrieval-questions.tsv"
  check "an all-stopword question lands nowhere" fail "$c7"

  # 8. A `#` comment inside a fence is not a heading. Without this, the curl
  #    fence in `logging-pii.md` would index "Raise the global level" onto the
  #    very page a fixture row names.
  local c8b="$tmp/c8b"; make_corpus "$c8b"
  printf '# Logging\n\n```bash\n# Raise the global level\ncurl ...\n```\n' \
    > "$c8b/docs/guide/logging-pii.md"
  printf 'raise the global level\tdocs/guide/logging-pii.md\n' \
    > "$c8b/scripts/docs-retrieval-questions.tsv"
  check "a comment inside a fence is not a heading" fail "$c8b"

  # 9. A four-backtick fence is not closed by a three-backtick line inside it.
  #     The heading after the inner block is still fenced, so it must not be
  #     indexed; recording the opener as three characters would index it.
  local c9="$tmp/c9"; make_corpus "$c9"
  printf '# Markdown\n\n````markdown\n```bash\n# Raise the global level\n```\n````\n\n## Something else\n' \
    > "$c9/docs/guide/md.md"
  printf 'raise the global level\tdocs/guide/md.md\n' \
    > "$c9/scripts/docs-retrieval-questions.tsv"
  check "an inner fence does not close a longer outer one" fail "$c9"

  # 10. A `~~~` line does not close a ``` fence.
  local c10="$tmp/c10"; make_corpus "$c10"
  printf '# Page\n\n```text\n~~~\n# Raise the global level\n```\n' \
    > "$c10/docs/guide/md.md"
  printf 'raise the global level\tdocs/guide/md.md\n' \
    > "$c10/scripts/docs-retrieval-questions.tsv"
  check "a tilde run does not close a backtick fence" fail "$c10"

  # 11. A same-character run carrying an info string is content, not a close:
  #     `~~~bash` inside a `~~~` fence leaves the fence open.
  local c11="$tmp/c11"; make_corpus "$c11"
  printf '# Page\n\n~~~\n~~~bash\n# Raise the global level\n~~~\n' \
    > "$c11/docs/guide/md.md"
  printf 'raise the global level\tdocs/guide/md.md\n' \
    > "$c11/scripts/docs-retrieval-questions.tsv"
  check "a run with an info string does not close a fence" fail "$c11"

  # 12. Trailing spaces on a closing fence are allowed, so a real close still
  #     closes and the heading after it IS indexed.
  local c12="$tmp/c12"; make_corpus "$c12"
  printf '# Page\n\n```bash\necho hi\n```   \n\n## Raise the global level\n' \
    > "$c12/docs/guide/md.md"
  printf 'raise the global level\tdocs/guide/md.md\n' \
    > "$c12/scripts/docs-retrieval-questions.tsv"
  check "trailing whitespace still closes a fence" pass "$c12"

  # 13. A heading inside a multi-line HTML comment is not indexed: no renderer
  #     shows it, so a reader can neither see nor navigate to it.
  local c13="$tmp/c13"; make_corpus "$c13"
  printf '# Page\n\n\u003c!--\n# Secret runtime logger\n--\u003e\n' \
    > "$c13/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c13/scripts/docs-retrieval-questions.tsv"
  check "a heading inside an HTML comment is not indexed" fail "$c13"

  # 14. The comment ends where it says it does: a real heading after `-->`
  #     is still indexed, so the fix cannot pass by swallowing the rest.
  local c14="$tmp/c14"; make_corpus "$c14"
  printf '# Page\n\n\u003c!--\nnote\n--\u003e\n\n## Secret runtime logger\n' \
    > "$c14/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c14/scripts/docs-retrieval-questions.tsv"
  check "a heading after the comment closes is indexed" pass "$c14"

  # 15. `<!--` inside a fence is sample text, not a comment, so it must not
  #     swallow the headings that follow the fence.
  local c15="$tmp/c15"; make_corpus "$c15"
  printf '# Page\n\n```html\n\u003c!-- unterminated in sample code\n```\n\n## Secret runtime logger\n' \
    > "$c15/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c15/scripts/docs-retrieval-questions.tsv"
  check "an HTML comment opener inside a fence is sample text" pass "$c15"

  # 16. An inline comment on a heading line is stripped before indexing: a
  #     renderer shows only the text outside it.
  local c16="$tmp/c16"; make_corpus "$c16"
  printf '# Page \u003c!-- Secret runtime logger --\u003e\n' \
    > "$c16/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c16/scripts/docs-retrieval-questions.tsv"
  check "an inline comment on a heading is not indexed" fail "$c16"

  # 17. Stripping the comment must leave the visible heading text indexed.
  local c17="$tmp/c17"; make_corpus "$c17"
  printf '# Page \u003c!-- an editorial note --\u003e\n' \
    > "$c17/docs/guide/md.md"
  printf 'page\tdocs/guide/md.md\n' \
    > "$c17/scripts/docs-retrieval-questions.tsv"
  check "the visible part of the heading is still indexed" pass "$c17"

  # 18. A link destination in a heading is markup, not words on the page.
  local c18="$tmp/c18"; make_corpus "$c18"
  printf '# Page\n\n## [Overview](secret-runtime-logger.md)\n' \
    > "$c18/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c18/scripts/docs-retrieval-questions.tsv"
  check "a link destination in a heading is not indexed" fail "$c18"

  # 19. The link TEXT is what a reader sees, so it must still be indexed.
  local c19="$tmp/c19"; make_corpus "$c19"
  printf '# Page\n\n## [Secret runtime logger](overview.md)\n' \
    > "$c19/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c19/scripts/docs-retrieval-questions.tsv"
  check "the link text in a heading is indexed" pass "$c19"

  # 20. A destination with BALANCED parentheses is still all destination.
  #     `[^)]*` stopped at the inner `)` and left the rest in the title.
  local c20="$tmp/c20"; make_corpus "$c20"
  printf '# Page\n\n## [Overview](foo(bar)-secret-runtime-logger.md)\n' \
    > "$c20/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c20/scripts/docs-retrieval-questions.tsv"
  check "balanced parens in a destination are not indexed" fail "$c20"

  # 21. Same for an ESCAPED paren, which CommonMark also allows.
  local c21="$tmp/c21"; make_corpus "$c21"
  printf '# Page\n\n## [Overview](foo\\)-secret-runtime-logger.md)\n' \
    > "$c21/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c21/scripts/docs-retrieval-questions.tsv"
  check "an escaped paren in a destination is not indexed" fail "$c21"

  # 22. The label after such a destination is still indexed, and so is text
  #     following the link — the scanner must resume, not swallow the rest.
  local c22="$tmp/c22"; make_corpus "$c22"
  printf '# Page\n\n## [Secret runtime logger](foo(bar).md) and more\n' \
    > "$c22/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c22/scripts/docs-retrieval-questions.tsv"
  check "the label of a nested-paren link is indexed" pass "$c22"

  # 23. Text AFTER the link survives too.
  local c23="$tmp/c23"; make_corpus "$c23"
  printf '# Page\n\n## [Overview](foo(bar).md) and the runtime logger\n' \
    > "$c23/docs/guide/md.md"
  printf 'runtime logger\tdocs/guide/md.md\n' \
    > "$c23/scripts/docs-retrieval-questions.tsv"
  check "text after a nested-paren link is indexed" pass "$c23"

  # 24. A bare `[` that opens no link must not eat the heading.
  local c24="$tmp/c24"; make_corpus "$c24"
  printf '# Page\n\n## Secret runtime logger [unclosed\n' \
    > "$c24/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c24/scripts/docs-retrieval-questions.tsv"
  check "an unclosed bracket does not swallow the heading" pass "$c24"

  # 25. A comment that closes and REOPENS on the same line is still open:
  #     `--> <!-- second` must hide the heading on the next line.
  local c25="$tmp/c25"; make_corpus "$c25"
  printf '# Page\n\n\u003c!--\nfirst\n--\u003e \u003c!-- second\n# Secret runtime logger\n--\u003e\n' \
    > "$c25/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c25/scripts/docs-retrieval-questions.tsv"
  check "a comment reopened on a closing line stays open" fail "$c25"

  # 26. Two complete comments on one line leave it CLOSED — the scanner must
  #     not treat the second opener as unterminated.
  local c26="$tmp/c26"; make_corpus "$c26"
  printf '# Page\n\n\u003c!-- a --\u003e \u003c!-- b --\u003e\n\n## Secret runtime logger\n' \
    > "$c26/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c26/scripts/docs-retrieval-questions.tsv"
  check "two complete comments on a line leave it closed" pass "$c26"

  # 27. A heading BEFORE an unterminated comment on the same line is visible,
  #     so its words are indexed even though the comment runs on.
  local c27="$tmp/c27"; make_corpus "$c27"
  printf '# Page\n\n## Secret runtime logger \u003c!-- note\n--\u003e\n' \
    > "$c27/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c27/scripts/docs-retrieval-questions.tsv"
  check "text before an unterminated comment is indexed" pass "$c27"

  # 28. An unmatched `(` inside a quoted link TITLE is not destination nesting.
  local c28="$tmp/c28"; make_corpus "$c28"
  printf '# Page\n\n## [Overview](foo "secret runtime logger (")\n' \
    > "$c28/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c28/scripts/docs-retrieval-questions.tsv"
  check "a paren inside a quoted title does not break the scan" fail "$c28"

  # 29. Same for a `<…>` destination, where parens are literal.
  local c29="$tmp/c29"; make_corpus "$c29"
  printf '# Page\n\n## [Overview](\u003csecret-runtime-logger (x).md\u003e)\n' \
    > "$c29/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c29/scripts/docs-retrieval-questions.tsv"
  check "parens inside an angle destination are literal" fail "$c29"

  # 30. A quote OUTSIDE any link is ordinary visible text and must survive —
  #     the title rule must not start swallowing prose.
  local c30="$tmp/c30"; make_corpus "$c30"
  printf '# Page\n\n## Secret runtime logger "quoted (" tail\n' \
    > "$c30/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c30/scripts/docs-retrieval-questions.tsv"
  check "a quote outside a link is ordinary text" pass "$c30"

  # 31. And the label of a titled link is still indexed.
  local c31="$tmp/c31"; make_corpus "$c31"
  printf '# Page\n\n## [Secret runtime logger](foo "a ( b")\n' \
    > "$c31/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c31/scripts/docs-retrieval-questions.tsv"
  check "the label of a titled link is indexed" pass "$c31"

  # 32. A complete comment BEFORE heading-shaped text makes the line an HTML
  #     block, not a heading — stripping it must not manufacture one.
  local c32="$tmp/c32"; make_corpus "$c32"
  printf '# Page\n\n\u003c!-- editorial --\u003e## Secret runtime logger\n' \
    > "$c32/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c32/scripts/docs-retrieval-questions.tsv"
  check "a comment before the hashes does not make a heading" fail "$c32"

  # 33. A comment WITHIN a real heading is still removed, and the heading
  #     itself still indexed — the raw-line check must not undo that.
  local c33="$tmp/c33"; make_corpus "$c33"
  printf '# Page\n\n## Secret runtime logger \u003c!-- editorial --\u003e\n' \
    > "$c33/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c33/scripts/docs-retrieval-questions.tsv"
  check "a comment inside a real heading is still stripped" pass "$c33"

  # 34. The HTML-block line must not break state: a real heading after it is
  #     still indexed.
  local c34="$tmp/c34"; make_corpus "$c34"
  printf '# Page\n\n\u003c!-- editorial --\u003e## Not a heading\n\n## Secret runtime logger\n' \
    > "$c34/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c34/scripts/docs-retrieval-questions.tsv"
  check "a heading after an HTML-block line is indexed" pass "$c34"

  # 35. Up to three leading spaces is still a heading — the false NEGATIVE
  #     that would fail the gate on a page that renders fine.
  local c35="$tmp/c35"; make_corpus "$c35"
  printf '# Page\n\n   ## Secret runtime logger\n' \
    > "$c35/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c35/scripts/docs-retrieval-questions.tsv"
  check "an indented ATX heading is indexed" pass "$c35"

  # 36. FOUR spaces is an indented code block, not a heading.
  local c36="$tmp/c36"; make_corpus "$c36"
  printf '# Page\n\n    ## Secret runtime logger\n' \
    > "$c36/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c36/scripts/docs-retrieval-questions.tsv"
  check "four spaces is code, not a heading" fail "$c36"

  # 37. A heading-shaped line inside a raw HTML block is script text.
  local c37="$tmp/c37"; make_corpus "$c37"
  printf '# Page\n\n\u003cscript\u003e\n# Secret runtime logger\n\u003c/script\u003e\n' \
    > "$c37/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c37/scripts/docs-retrieval-questions.tsv"
  check "a heading inside a script block is not indexed" fail "$c37"

  # 38. The block ends at its closing tag: a real heading after it is indexed.
  local c38="$tmp/c38"; make_corpus "$c38"
  printf '# Page\n\n\u003cscript\u003e\nvar x = 1;\n\u003c/script\u003e\n\n## Secret runtime logger\n' \
    > "$c38/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c38/scripts/docs-retrieval-questions.tsv"
  check "a heading after a script block is indexed" pass "$c38"

  # 39. `<script>` inside a FENCE is sample code, not a block opener, so it
  #     must not swallow the headings that follow the fence.
  local c39="$tmp/c39"; make_corpus "$c39"
  printf '# Page\n\n```html\n\u003cscript\u003e\n```\n\n## Secret runtime logger\n' \
    > "$c39/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c39/scripts/docs-retrieval-questions.tsv"
  check "a script tag inside a fence is sample code" pass "$c39"

  # 40. A heading inside a `<div>` block is raw HTML, not a heading.
  local c40="$tmp/c40"; make_corpus "$c40"
  printf '# Page\n\n\u003cdiv\u003e\n# Secret runtime logger\n\u003c/div\u003e\n' \
    > "$c40/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c40/scripts/docs-retrieval-questions.tsv"
  check "a heading inside a div block is not indexed" fail "$c40"

  # 41. That block ends at a BLANK LINE, not a closing tag — a heading after
  #     the blank must still be indexed, or the rule eats the rest of the page.
  local c41="$tmp/c41"; make_corpus "$c41"
  printf '# Page\n\n\u003cdiv\u003e\nraw\n\n## Secret runtime logger\n' \
    > "$c41/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c41/scripts/docs-retrieval-questions.tsv"
  check "a heading after the blank line ending a div is indexed" pass "$c41"

  # 42. A SETEXT h1 is a heading — the false negative an ordinary reformat
  #     would otherwise turn into a CI failure.
  local c42="$tmp/c42"; make_corpus "$c42"
  printf '# Page\n\nSecret runtime logger\n=====\n' \
    > "$c42/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c42/scripts/docs-retrieval-questions.tsv"
  check "a setext h1 is indexed" pass "$c42"

  # 43. And a setext h2.
  local c43="$tmp/c43"; make_corpus "$c43"
  printf '# Page\n\nSecret runtime logger\n-----\n' \
    > "$c43/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c43/scripts/docs-retrieval-questions.tsv"
  check "a setext h2 is indexed" pass "$c43"

  # 44. A `---` after a BLANK line is a thematic break, not an underline, so
  #     the paragraph above it is not a heading.
  local c44="$tmp/c44"; make_corpus "$c44"
  printf '# Page\n\nSecret runtime logger\n\n---\n' \
    > "$c44/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c44/scripts/docs-retrieval-questions.tsv"
  check "a thematic break is not a setext underline" fail "$c44"

  # 45. A table rule is not a setext underline either.
  local c45="$tmp/c45"; make_corpus "$c45"
  printf '# Page\n\n| Secret runtime logger |\n|---|\n' \
    > "$c45/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c45/scripts/docs-retrieval-questions.tsv"
  check "a table rule is not a setext underline" fail "$c45"

  # 46. A LIST ITEM over `---` is a list plus a thematic break, not a heading.
  local c46="$tmp/c46"; make_corpus "$c46"
  printf '# Page\n\n- Secret runtime logger\n---\n' \
    > "$c46/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c46/scripts/docs-retrieval-questions.tsv"
  check "a list item is not setext text" fail "$c46"

  # 47. Nor is a block quote.
  local c47="$tmp/c47"; make_corpus "$c47"
  printf '# Page\n\n\u003e Secret runtime logger\n---\n' \
    > "$c47/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c47/scripts/docs-retrieval-questions.tsv"
  check "a block quote is not setext text" fail "$c47"

  # 48. Nor indented code.
  local c48="$tmp/c48"; make_corpus "$c48"
  printf '# Page\n\n    Secret runtime logger\n---\n' \
    > "$c48/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c48/scripts/docs-retrieval-questions.tsv"
  check "indented code is not setext text" fail "$c48"

  # 49. Ordinary paragraph text still is — the rule must not reject everything.
  local c49="$tmp/c49"; make_corpus "$c49"
  printf '# Page\n\nSecret runtime logger\n---\n' \
    > "$c49/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c49/scripts/docs-retrieval-questions.tsv"
  check "paragraph text is still setext text" pass "$c49"

  # 50. A setext heading's text may be WRAPPED: the whole paragraph is the
  #     heading, not just its last line.
  local c50="$tmp/c50"; make_corpus "$c50"
  printf '# Page\n\nSecret runtime\nlogger\n---\n' \
    > "$c50/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c50/scripts/docs-retrieval-questions.tsv"
  check "a wrapped setext heading keeps all its words" pass "$c50"

  # 51. A BLANK line ends the paragraph, so only what follows it is the
  #     heading — the accumulator must not span the gap.
  local c51="$tmp/c51"; make_corpus "$c51"
  printf '# Page\n\nSecret runtime\n\nlogger\n---\n' \
    > "$c51/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c51/scripts/docs-retrieval-questions.tsv"
  check "a blank line ends the setext paragraph" fail "$c51"

  # 52. A fence between the lines ends it too.
  local c52="$tmp/c52"; make_corpus "$c52"
  printf '# Page\n\nSecret runtime\n```\nx\n```\nlogger\n---\n' \
    > "$c52/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c52/scripts/docs-retrieval-questions.tsv"
  check "a fence ends the setext paragraph" fail "$c52"

  # 53. A link-reference DEFINITION renders as nothing, so it is not setext
  #     text and its destination is not a heading.
  local c53="$tmp/c53"; make_corpus "$c53"
  printf '# Page\n\n[Overview]: /secret-runtime-logger.md\n---\n' \
    > "$c53/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c53/scripts/docs-retrieval-questions.tsv"
  check "a link-reference definition is not setext text" fail "$c53"

  # 54. Text that merely CONTAINS a colon after brackets is still paragraph
  #     text — the rule must not reject ordinary prose.
  local c54="$tmp/c54"; make_corpus "$c54"
  printf '# Page\n\nSecret runtime logger [see also] : notes\n---\n' \
    > "$c54/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c54/scripts/docs-retrieval-questions.tsv"
  check "prose with brackets is still setext text" pass "$c54"

  # 55. A `\u003c!--` inside an inline CODE SPAN is literal text, not a comment
  #     opener: the heading is visible and must be indexed.
  local c55="$tmp/c55"; make_corpus "$c55"
  printf '# Page\n\n## The `\u003c!--` secret runtime logger marker\n' \
    > "$c55/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c55/scripts/docs-retrieval-questions.tsv"
  check "a comment opener inside a code span is literal" pass "$c55"

  # 56. And it must not leave the scanner inside a comment: a LATER heading
  #     is still indexed. This is the blast radius, not just the one line.
  local c56="$tmp/c56"; make_corpus "$c56"
  printf '# Page\n\n## The `\u003c!--` marker\n\n## Secret runtime logger\n' \
    > "$c56/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c56/scripts/docs-retrieval-questions.tsv"
  check "a code-span opener does not swallow later headings" pass "$c56"

  # 57. A REAL comment on the same line still opens one, so the rule did not
  #     simply stop recognising comments.
  local c57="$tmp/c57"; make_corpus "$c57"
  printf '# Page\n\n## A `\u003c!--` marker \u003c!-- and a real one\n## Secret runtime logger\n--\u003e\n' \
    > "$c57/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c57/scripts/docs-retrieval-questions.tsv"
  check "a real comment after a code span still opens" fail "$c57"

  # 58. A link written inside a CODE SPAN is literal text a reader sees, so
  #     its destination must stay indexed rather than be reduced to a label.
  local c58="$tmp/c58"; make_corpus "$c58"
  printf '# Page\n\n## The `[Overview](secret-runtime-logger.md)` form\n' \
    > "$c58/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c58/scripts/docs-retrieval-questions.tsv"
  check "a link inside a code span keeps its text" pass "$c58"

  # 59. A REAL link outside a span is still reduced, so the fix did not just
  #     stop unlinking.
  local c59="$tmp/c59"; make_corpus "$c59"
  printf '# Page\n\n## [Overview](secret-runtime-logger.md)\n' \
    > "$c59/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c59/scripts/docs-retrieval-questions.tsv"
  check "a real link is still reduced to its label" fail "$c59"

  # 60. A LONGER backtick run does not close a shorter opener, so the comment
  #     after it is a real comment and its text is not indexed.
  local c60="$tmp/c60"; make_corpus "$c60"
  printf '# Page\n\n## ` \u003c!-- secret runtime logger --\u003e `` tail\n' \
    > "$c60/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c60/scripts/docs-retrieval-questions.tsv"
  check "a longer run does not close a shorter opener" fail "$c60"

  # 61. An equal-length run still closes, and the span is still literal — the
  #     rule must not stop recognising code spans altogether.
  local c61="$tmp/c61"; make_corpus "$c61"
  printf '# Page\n\n## The `\u003c!--` secret runtime logger marker\n' \
    > "$c61/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c61/scripts/docs-retrieval-questions.tsv"
  check "an equal-length run still closes a span" pass "$c61"

  # 62. ESCAPED backticks are literal, so they open no code span and the
  #     comment between them is a real, invisible comment.
  local c62="$tmp/c62"; make_corpus "$c62"
  printf '# Page\n\n## \\`\u003c!-- secret runtime logger --\u003e\\`\n' \
    > "$c62/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c62/scripts/docs-retrieval-questions.tsv"
  check "escaped backticks open no code span" fail "$c62"

  # 63. A link definition needs no space after the colon.
  local c63="$tmp/c63"; make_corpus "$c63"
  printf '# Page\n\n[Overview]:/secret-runtime-logger.md\n---\n' \
    > "$c63/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c63/scripts/docs-retrieval-questions.tsv"
  check "a definition with no space after the colon is not setext text" fail "$c63"

  # 64. A backtick fence opener may not carry a backtick in its info string,
  #     so it opens nothing and a LATER heading is still indexed.
  local c64="$tmp/c64"; make_corpus "$c64"
  printf '# Page\n\n```bad`info\n\n## Secret runtime logger\n' \
    > "$c64/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c64/scripts/docs-retrieval-questions.tsv"
  check "a backtick in the info string opens no fence" pass "$c64"

  # 65. A VALID fence still opens and still hides what is inside it.
  local c65="$tmp/c65"; make_corpus "$c65"
  printf '# Page\n\n```bash\n# Secret runtime logger\n```\n' \
    > "$c65/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c65/scripts/docs-retrieval-questions.tsv"
  check "a valid fence still hides its contents" fail "$c65"

  # 66. A tilde fence MAY carry a backtick in its info string.
  local c66="$tmp/c66"; make_corpus "$c66"
  printf '# Page\n\n~~~a`b\n# Secret runtime logger\n~~~\n' \
    > "$c66/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c66/scripts/docs-retrieval-questions.tsv"
  check "a tilde fence may carry a backtick" fail "$c66"

  # 67. A processing instruction runs to `?\u003e`; a heading inside is raw HTML.
  local c67="$tmp/c67"; make_corpus "$c67"
  printf '# Page\n\n\u003c?php\n# Secret runtime logger\n?\u003e\n' \
    > "$c67/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c67/scripts/docs-retrieval-questions.tsv"
  check "a heading inside a processing instruction is not indexed" fail "$c67"

  # 68. And it ends there: a heading after `?\u003e` is still indexed.
  local c68="$tmp/c68"; make_corpus "$c68"
  printf '# Page\n\n\u003c?php\nx\n?\u003e\n\n## Secret runtime logger\n' \
    > "$c68/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c68/scripts/docs-retrieval-questions.tsv"
  check "a heading after a processing instruction is indexed" pass "$c68"

  # 69. A TAB cannot precede a fence, so a tabbed run does not close one and
  #     the content after it stays hidden.
  local c69="$tmp/c69"; make_corpus "$c69"
  printf '# Page\n\n```\n\t```\n# Secret runtime logger\n```\n' \
    > "$c69/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c69/scripts/docs-retrieval-questions.tsv"
  check "a tab does not close a fence" fail "$c69"

  # 70. Up to three SPACES still closes one, so the rule did not break
  #     ordinary indented fences.
  local c70="$tmp/c70"; make_corpus "$c70"
  printf '# Page\n\n```\nx\n   ```\n\n## Secret runtime logger\n' \
    > "$c70/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c70/scripts/docs-retrieval-questions.tsv"
  check "three spaces still closes a fence" pass "$c70"

  # 71. `\u003cscript-widget\u003e` is NOT a `\u003cscript\u003e` block: it ends at a blank line,
  #     so a heading after it is visible and must be indexed.
  local c71="$tmp/c71"; make_corpus "$c71"
  printf '# Page\n\n\u003cscript-widget\u003e\n\n## Secret runtime logger\n' \
    > "$c71/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c71/scripts/docs-retrieval-questions.tsv"
  check "a hyphenated custom element is not a script block" pass "$c71"

  # 72. A real `\u003cscript\u003e` still is one.
  local c72="$tmp/c72"; make_corpus "$c72"
  printf '# Page\n\n\u003cscript\u003e\n# Secret runtime logger\n\u003c/script\u003e\n' \
    > "$c72/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c72/scripts/docs-retrieval-questions.tsv"
  check "a real script block still hides its contents" fail "$c72"

  # 73. FOUR spaces before `\u003c?` is indented code, not a processing
  #     instruction, so the scanner must not enter one and eat the page.
  local c73="$tmp/c73"; make_corpus "$c73"
  printf '# Page\n\n    \u003c?php\n\n## Secret runtime logger\n' \
    > "$c73/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c73/scripts/docs-retrieval-questions.tsv"
  check "four spaces before a PI is indented code" pass "$c73"

  # 74. A CDATA section hides its contents.
  local c74="$tmp/c74"; make_corpus "$c74"
  printf '# Page\n\n\u003c![CDATA[\n# Secret runtime logger\n]]\u003e\n' \
    > "$c74/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c74/scripts/docs-retrieval-questions.tsv"
  check "a heading inside CDATA is not indexed" fail "$c74"

  # 75. A declaration block does too, and ends at its `\u003e`.
  local c75="$tmp/c75"; make_corpus "$c75"
  printf '# Page\n\n\u003c!DOCTYPE html\n# Secret runtime logger\n\u003e\n' \
    > "$c75/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c75/scripts/docs-retrieval-questions.tsv"
  check "a heading inside a declaration is not indexed" fail "$c75"

  # 76. A container tag at END OF LINE still opens a type-6 block.
  local c76="$tmp/c76"; make_corpus "$c76"
  printf '# Page\n\n\u003cdiv\n# Secret runtime logger\n\n' \
    > "$c76/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c76/scripts/docs-retrieval-questions.tsv"
  check "a bare container tag at end of line opens a block" fail "$c76"

  # 77. A reference definition may put its destination on the NEXT line; that
  #     line is invisible too, so it is not setext text.
  local c77="$tmp/c77"; make_corpus "$c77"
  printf '# Page\n\n[Overview]:\n  /secret-runtime-logger.md\n---\n' \
    > "$c77/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c77/scripts/docs-retrieval-questions.tsv"
  check "a definition destination on the next line is not setext text" fail "$c77"

  # 78. Ordinary paragraph text is STILL setext text — none of the above may
  #     start swallowing prose.
  local c78="$tmp/c78"; make_corpus "$c78"
  printf '# Page\n\nSecret runtime logger\n---\n' \
    > "$c78/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c78/scripts/docs-retrieval-questions.tsv"
  check "paragraph text is still setext text" pass "$c78"

  # 79. A definition's optional TITLE may be on a further line; it is
  #     invisible too, so it is not setext text.
  local c79="$tmp/c79"; make_corpus "$c79"
  printf '# Page\n\n[Overview]:\n  /x\n  "secret runtime logger"\n---\n' \
    > "$c79/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c79/scripts/docs-retrieval-questions.tsv"
  check "a definition title on a later line is not setext text" fail "$c79"

  # 80. A `\u003c!--` inside an INDENTED CODE block is literal, so it must not
  #     swallow the heading that follows and ends the block.
  local c80="$tmp/c80"; make_corpus "$c80"
  printf '# Page\n\n    \u003c!-- literal\n\n## Secret runtime logger\n' \
    > "$c80/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c80/scripts/docs-retrieval-questions.tsv"
  check "a comment marker in indented code is literal" pass "$c80"

  # 81. A real comment at the MARGIN still opens one, so the indent rule did
  #     not disable comment tracking.
  local c81="$tmp/c81"; make_corpus "$c81"
  printf '# Page\n\n\u003c!-- literal\n# Secret runtime logger\n--\u003e\n' \
    > "$c81/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c81/scripts/docs-retrieval-questions.tsv"
  check "a comment at the margin still opens" fail "$c81"

  # 82. Paragraph text is STILL setext text after all of this.
  local c82="$tmp/c82"; make_corpus "$c82"
  printf '# Page\n\nSecret runtime logger\n---\n' \
    > "$c82/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c82/scripts/docs-retrieval-questions.tsv"
  check "paragraph text is still setext text" pass "$c82"

  # 83. The definition's title is OPTIONAL. A line that cannot open one is
  #     not part of the definition, so it is read as what it is — ordinary
  #     paragraph text, and here the text of a real setext heading.
  local c83="$tmp/c83"; make_corpus "$c83"
  printf '# Page\n\n[Overview]:\n  /x\nSecret runtime logger\n---\n' \
    > "$c83/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c83/scripts/docs-retrieval-questions.tsv"
  check "a line that cannot be a definition title is text again" pass "$c83"

  # 84. A title that RUNS ON to a further line is invisible for all of it.
  local c84="$tmp/c84"; make_corpus "$c84"
  printf '# Page\n\n[Overview]:\n  /x\n  "a\n  secret runtime logger"\n---\n' \
    > "$c84/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c84/scripts/docs-retrieval-questions.tsv"
  check "a title running onto a further line stays invisible" fail "$c84"

  # 85. …and once that title closes, the next line is text again — the
  #     positive control on 84, which "never leave title state" would fail.
  local c85="$tmp/c85"; make_corpus "$c85"
  printf '# Page\n\n[Overview]:\n  /x\n  "a\n  b"\nSecret runtime logger\n---\n' \
    > "$c85/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c85/scripts/docs-retrieval-questions.tsv"
  check "text after a closed multi-line title is indexed" pass "$c85"

  # 86. A bare label over a line that is NOT a legal destination is no
  #     definition at all, so BOTH lines are the paragraph a `---` promotes.
  local c86="$tmp/c86"; make_corpus "$c86"
  printf '# Page\n\n[Overview]:\nSecret runtime logger\n---\n' \
    > "$c86/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c86/scripts/docs-retrieval-questions.tsv"
  check "a label over a non-destination is ordinary text" pass "$c86"

  # 87. …while a legal `\u003c…\u003e` destination on that line still is one, and
  #     stays invisible. The positive control on 86.
  local c87="$tmp/c87"; make_corpus "$c87"
  printf '# Page\n\n[Overview]:\n  \u003c/secret-runtime-logger.md\u003e\n---\n' \
    > "$c87/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c87/scripts/docs-retrieval-questions.tsv"
  check "an angle-bracket destination is still invisible" fail "$c87"

  # 88. A title has to be ALL that is left on its line: CommonMark reads
  #     `[foo]: /url` then `"title" ok` as a definition and a paragraph.
  local c88="$tmp/c88"; make_corpus "$c88"
  printf '# Page\n\n[Overview]:\n  /x\n  "a" secret runtime logger\n---\n' \
    > "$c88/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c88/scripts/docs-retrieval-questions.tsv"
  check "a closer with text after it is not a title" pass "$c88"

  # 89. A heading inside a BLOCK QUOTE is a heading: it renders, it anchors,
  #     a reader lands on it.
  local c89="$tmp/c89"; make_corpus "$c89"
  printf '# Page\n\n\u003e ## Secret runtime logger\n' \
    > "$c89/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c89/scripts/docs-retrieval-questions.tsv"
  check "a heading inside a block quote is indexed" pass "$c89"

  # 90. So is one inside a LIST ITEM.
  local c90="$tmp/c90"; make_corpus "$c90"
  printf '# Page\n\n- ## Secret runtime logger\n' \
    > "$c90/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c90/scripts/docs-retrieval-questions.tsv"
  check "a heading inside a list item is indexed" pass "$c90"

  # 91. And a FENCE inside a block quote still hides what is in it — the
  #     control that stops 89 from indexing every `# comment` in the quoted
  #     code blocks this corpus already has.
  local c91="$tmp/c91"; make_corpus "$c91"
  printf '# Page\n\n\u003e \u0060\u0060\u0060\n\u003e # Secret runtime logger\n\u003e \u0060\u0060\u0060\n' \
    > "$c91/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c91/scripts/docs-retrieval-questions.tsv"
  check "a fence inside a block quote still hides its contents" fail "$c91"

  # 92. A SETEXT heading inside a block quote is a heading: both lines carry
  #     the same quote prefix, so the underline is an underline.
  local c92="$tmp/c92"; make_corpus "$c92"
  printf '# Page\n\n\u003e Secret runtime logger\n\u003e ---\n' \
    > "$c92/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c92/scripts/docs-retrieval-questions.tsv"
  check "a setext heading inside a block quote is indexed" pass "$c92"

  # 93. A MISMATCHED prefix is not. Test 47 has the unquoted underline; this
  #     is the other direction, and the pair is what makes 92 depth-matching
  #     rather than prefix-blind.
  local c93="$tmp/c93"; make_corpus "$c93"
  printf '# Page\n\nSecret runtime logger\n\u003e ---\n' \
    > "$c93/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c93/scripts/docs-retrieval-questions.tsv"
  check "a quoted underline does not promote unquoted text" fail "$c93"

  # 94. A raw HTML block nested in a quote ends at a line that is blank
  #     INSIDE the quote, so the heading after it is visible again.
  local c94="$tmp/c94"; make_corpus "$c94"
  printf '# Page\n\n\u003e \u003cdiv\u003e\n\u003e # Hidden\n\u003e\n\u003e ## Secret runtime logger\n' \
    > "$c94/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c94/scripts/docs-retrieval-questions.tsv"
  check "a nested html block ends at a quoted blank line" pass "$c94"

  # 95. …and without that blank line it still hides what follows.
  local c95="$tmp/c95"; make_corpus "$c95"
  printf '# Page\n\n\u003e \u003cdiv\u003e\n\u003e ## Secret runtime logger\n' \
    > "$c95/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c95/scripts/docs-retrieval-questions.tsv"
  check "a nested html block still hides its contents" fail "$c95"

  # 96. An inline link whose target is NOT a legal destination is not a link:
  #     a reader sees every word of it, so every word is indexed.
  local c96="$tmp/c96"; make_corpus "$c96"
  printf '# Page\n\n## [Overview](secret runtime logger)\n' \
    > "$c96/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c96/scripts/docs-retrieval-questions.tsv"
  check "an invalid link target is visible text" pass "$c96"

  # 97. …and a legal one is still reduced to its label. The control on 96,
  #     which "never reduce anything" would pass.
  local c97="$tmp/c97"; make_corpus "$c97"
  printf '# Page\n\n## [Overview](secret-runtime-logger.md "a title")\n' \
    > "$c97/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c97/scripts/docs-retrieval-questions.tsv"
  check "a legal link target is still dropped" fail "$c97"

  # 98. A raw HTML block opened in a quote ends when the QUOTE does: nothing
  #     can lazily continue it, so it does not wait for its closing tag.
  local c98="$tmp/c98"; make_corpus "$c98"
  printf '# Page\n\n\u003e \u003cscript\u003e\n\u003e var x\n\n## Secret runtime logger\n' \
    > "$c98/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c98/scripts/docs-retrieval-questions.tsv"
  check "a quoted html block ends with its quote" pass "$c98"

  # 99. …and while the quote continues it still hides what is inside.
  local c99="$tmp/c99"; make_corpus "$c99"
  printf '# Page\n\n\u003e \u003cscript\u003e\n\u003e # Secret runtime logger\n\u003e var x\n' \
    > "$c99/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c99/scripts/docs-retrieval-questions.tsv"
  check "a quoted html block still hides its contents" fail "$c99"

  # 100. A fence delimiter in ANOTHER container is content, not a closer. A
  #      top-level fence holding `\u003e \u0060\u0060\u0060` used to close on it and index the
  #      hidden heading after — a false positive, the worse direction.
  local c100="$tmp/c100"; make_corpus "$c100"
  printf '# Page\n\n\u0060\u0060\u0060\n\u003e \u0060\u0060\u0060\n# Secret runtime logger\n\u0060\u0060\u0060\n' \
    > "$c100/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c100/scripts/docs-retrieval-questions.tsv"
  check "a quoted delimiter does not close a top-level fence" fail "$c100"

  # 101. …and a delimiter at the SAME depth still closes it. The control on
  #      100, which "never close on a marker" would fail.
  local c101="$tmp/c101"; make_corpus "$c101"
  printf '# Page\n\n\u0060\u0060\u0060\n# hidden\n\u0060\u0060\u0060\n\n## Secret runtime logger\n' \
    > "$c101/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c101/scripts/docs-retrieval-questions.tsv"
  check "a same-depth delimiter still closes a fence" pass "$c101"

  # 102. A fence opened INSIDE a quote is closed by the quote ending, or it
  #      would swallow the rest of the page.
  local c102="$tmp/c102"; make_corpus "$c102"
  printf '# Page\n\n\u003e \u0060\u0060\u0060\n\u003e code\n\n## Secret runtime logger\n' \
    > "$c102/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c102/scripts/docs-retrieval-questions.tsv"
  check "a quoted fence ends with its quote" pass "$c102"

  # 103. A comment opened in a quote ends with it too — same rule, third
  #      block type.
  local c103="$tmp/c103"; make_corpus "$c103"
  printf '# Page\n\n\u003e \u003c!-- note\n\n## Secret runtime logger\n' \
    > "$c103/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c103/scripts/docs-retrieval-questions.tsv"
  check "a quoted comment ends with its quote" pass "$c103"

  # 104. …and hides a heading while the quote lasts.
  local c104="$tmp/c104"; make_corpus "$c104"
  printf '# Page\n\n\u003e \u003c!-- note\n\u003e # Secret runtime logger\n\u003e --\u003e\n' \
    > "$c104/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c104/scripts/docs-retrieval-questions.tsv"
  check "a quoted comment still hides its contents" fail "$c104"

  # 105. An UNRESOLVED reference is not a link: CommonMark renders the whole
  #      construct, so every word of it is on the page and in the index.
  local c105="$tmp/c105"; make_corpus "$c105"
  printf '# Page\n\n## [Overview][secret-runtime-logger]\n' \
    > "$c105/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c105/scripts/docs-retrieval-questions.tsv"
  check "an unresolved reference is visible text" pass "$c105"

  # 106. …and a RESOLVED one is still reduced to its label, wherever on the
  #      page the definition sits. The control on 105.
  local c106="$tmp/c106"; make_corpus "$c106"
  printf '# Page\n\n## [Overview][secret-runtime-logger]\n\n[secret-runtime-logger]: /x.md\n' \
    > "$c106/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c106/scripts/docs-retrieval-questions.tsv"
  check "a resolved reference is still dropped" fail "$c106"

  # 107. A definition whose destination is not one defines nothing, so the
  #      reference using it stays visible text.
  local c107="$tmp/c107"; make_corpus "$c107"
  printf '# Page\n\n## [Overview][secret-runtime-logger]\n\n[secret-runtime-logger]: not a url\n' \
    > "$c107/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c107/scripts/docs-retrieval-questions.tsv"
  check "a bad definition does not resolve a reference" pass "$c107"

  # 108. A backslash escapes ASCII PUNCTUATION and nothing else, so `\ ` is a
  #      literal backslash and a real space: the target has spaces, is not a
  #      destination, and the whole construct is words on the page.
  local c108="$tmp/c108"; make_corpus "$c108"
  printf '# Page\n\n## [Overview](secret\\ runtime\\ logger)\n' \
    > "$c108/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c108/scripts/docs-retrieval-questions.tsv"
  check "a backslash before a space escapes nothing" pass "$c108"

  # 109. …while a backslash before PUNCTUATION does escape it, so this really
  #      is a link and its destination really is invisible.
  local c109="$tmp/c109"; make_corpus "$c109"
  printf '# Page\n\n## [Overview](secret\\(runtime\\)logger.md)\n' \
    > "$c109/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c109/scripts/docs-retrieval-questions.tsv"
  check "an escaped paren keeps the target a link" fail "$c109"

  # 110. A complete type-7 tag opens a raw HTML block: what is inside renders
  #      as HTML, not as a heading, so indexing it is a false POSITIVE.
  local c110="$tmp/c110"; make_corpus "$c110"
  printf '# Page\n\n<custom>\n# Secret runtime logger\n\n' \
    > "$c110/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c110/scripts/docs-retrieval-questions.tsv"
  check "a type-7 tag hides the heading after it" fail "$c110"

  # 111. …and it ends at its blank line, so the next heading is visible again.
  local c111="$tmp/c111"; make_corpus "$c111"
  printf '# Page\n\n<custom>\nhidden\n\n## Secret runtime logger\n' \
    > "$c111/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c111/scripts/docs-retrieval-questions.tsv"
  check "a type-7 block ends at its blank line" pass "$c111"

  # 112. A tag with TEXT after it is prose, not a block — the control that
  #      keeps `<Foo> is the type` from swallowing the page.
  local c112="$tmp/c112"; make_corpus "$c112"
  printf '# Page\n\n<Foo> is the type\n# Secret runtime logger\n' \
    > "$c112/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c112/scripts/docs-retrieval-questions.tsv"
  check "a tag with text after it opens no block" pass "$c112"

  # 113. Type 7 cannot INTERRUPT a paragraph — alone among the seven. After
  #      prose, `\u003ccustom\u003e` is more prose, and the ATX heading below it (which
  #      can interrupt one) is a heading a reader sees.
  local c113="$tmp/c113"; make_corpus "$c113"
  printf '# Page\n\nSome prose\n\u003ccustom\u003e\n# Secret runtime logger\n' \
    > "$c113/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c113/scripts/docs-retrieval-questions.tsv"
  check "a type-7 tag does not interrupt a paragraph" pass "$c113"

  # 114. A real inline TAG is markup: its attributes are not words on the
  #      page, so indexing them is a hit on text no reader can see.
  local c114="$tmp/c114"; make_corpus "$c114"
  printf '# Page\n\n## \u003cspan title="secret runtime logger"\u003eOverview\u003c/span\u003e\n' \
    > "$c114/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c114/scripts/docs-retrieval-questions.tsv"
  check "an inline tag attribute is not indexed" fail "$c114"

  # 115. …while `\u003c…\u003e` inside a CODE SPAN is literal text a reader sees. The
  #      control that keeps the strip from eating `Query\u003cT\u003e`.
  local c115="$tmp/c115"; make_corpus "$c115"
  printf '# Page\n\n## \u0060Secret\u003cruntime\u003elogger\u0060\n' \
    > "$c115/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c115/scripts/docs-retrieval-questions.tsv"
  check "angle brackets in a code span survive" pass "$c115"

  # 116. An AUTOLINK is not a tag: there the URL is the rendered text.
  local c116="$tmp/c116"; make_corpus "$c116"
  printf '# Page\n\n## \u003chttps://example.com/secret-runtime-logger\u003e\n' \
    > "$c116/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c116/scripts/docs-retrieval-questions.tsv"
  check "an autolink is still visible text" pass "$c116"

  # 117. A delimiter carrying a LIST MARKER starts an item; it cannot close a
  #      fence that belongs to something else, so what follows stays fenced.
  local c117="$tmp/c117"; make_corpus "$c117"
  printf '# Page\n\n\u0060\u0060\u0060\n- \u0060\u0060\u0060\n# Secret runtime logger\n\u0060\u0060\u0060\n' \
    > "$c117/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c117/scripts/docs-retrieval-questions.tsv"
  check "a list-marker delimiter does not close a fence" fail "$c117"

  # 118. …and a fence a list item OPENS is still closed by its own indented
  #      delimiter, which carries no marker. The control on 117.
  local c118="$tmp/c118"; make_corpus "$c118"
  printf '# Page\n\n- \u0060\u0060\u0060\n  hidden\n  \u0060\u0060\u0060\n\n## Secret runtime logger\n' \
    > "$c118/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c118/scripts/docs-retrieval-questions.tsv"
  check "an indented delimiter closes a list item's fence" pass "$c118"

  # 119. A definition cannot INTERRUPT a paragraph, so after prose it is
  #      continuation text and the `---` under it makes a heading of both.
  local c119="$tmp/c119"; make_corpus "$c119"
  printf '# Page\n\nIntro\n[foo]: /secret-runtime-logger\n---\n' \
    > "$c119/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c119/scripts/docs-retrieval-questions.tsv"
  check "a definition cannot interrupt a paragraph" pass "$c119"

  # 120. …and with NO paragraph open it is a definition, invisible as ever.
  #      The control on 119.
  local c120="$tmp/c120"; make_corpus "$c120"
  printf '# Page\n\n[foo]: /secret-runtime-logger\n---\n' \
    > "$c120/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c120/scripts/docs-retrieval-questions.tsv"
  check "a definition with no paragraph open is invisible" fail "$c120"

  # 121. A label takes backslash escapes, so `[foo\]]:` is a definition whose
  #      label is `foo]` — CommonMark example 194 — and renders as nothing.
  local c121="$tmp/c121"; make_corpus "$c121"
  printf '# Page\n\n[foo\\]]: /secret-runtime-logger\n---\n' \
    > "$c121/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c121/scripts/docs-retrieval-questions.tsv"
  check "an escaped bracket does not end a label" fail "$c121"

  # 122. …while an UNescaped one does, so `[foo]]:` is not a definition and
  #      the line is the visible text it looks like. The control on 121.
  local c122="$tmp/c122"; make_corpus "$c122"
  printf '# Page\n\n[foo]]: secret runtime logger\n---\n' \
    > "$c122/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c122/scripts/docs-retrieval-questions.tsv"
  check "an unescaped bracket still ends a label" pass "$c122"

  # 123. The BARE-label form cannot interrupt one either. This is what the
  #      caller's own guard covers: without it the line opens definition
  #      state, which discards the paragraph the `---` was going to promote.
  local c123="$tmp/c123"; make_corpus "$c123"
  printf '# Page\n\nIntro\n[foo]:\n/secret-runtime-logger\n---\n' \
    > "$c123/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c123/scripts/docs-retrieval-questions.tsv"
  check "a bare label cannot interrupt a paragraph" pass "$c123"

  # 124. A title that opens on the DESTINATION's line and never closes makes
  #      the whole thing not a definition, so the line is visible text.
  local c124="$tmp/c124"; make_corpus "$c124"
  printf '# Page\n\n[foo]: /x "Secret runtime logger\n---\n' \
    > "$c124/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c124/scripts/docs-retrieval-questions.tsv"
  check "an unclosed title is not a definition" pass "$c124"

  # 125. …and when it does close, the definition is real and invisible. The
  #      control on 124.
  local c125="$tmp/c125"; make_corpus "$c125"
  printf '# Page\n\n[foo]: /x "Secret runtime logger"\n---\n' \
    > "$c125/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c125/scripts/docs-retrieval-questions.tsv"
  check "a closed title is still a definition" fail "$c125"

  # 126. A title may SPAN lines, and every line of it is invisible. The
  #      line-at-a-time version indexed the tail of one as prose.
  local c126="$tmp/c126"; make_corpus "$c126"
  printf '# Page\n\n[foo]: /x "a\nsecret runtime logger"\n---\n' \
    > "$c126/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c126/scripts/docs-retrieval-questions.tsv"
  check "a title spanning lines stays invisible" fail "$c126"

  # 127. A title on a LATER line that never closes is not a title, but the
  #      definition above it is still one — CommonMark reads `[foo]: /url`
  #      over `"title" ok` as a definition and then a paragraph.
  local c127="$tmp/c127"; make_corpus "$c127"
  printf '# Page\n\n[foo]: /x\n"secret runtime logger\n---\n' \
    > "$c127/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c127/scripts/docs-retrieval-questions.tsv"
  check "an unclosed title below a definition is text" pass "$c127"

  # 128. An inline PROCESSING INSTRUCTION renders as markup, not words.
  local c128="$tmp/c128"; make_corpus "$c128"
  printf '# Page\n\n## <?secret runtime logger?> Overview\n' \
    > "$c128/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c128/scripts/docs-retrieval-questions.tsv"
  check "an inline processing instruction is not indexed" fail "$c128"

  # 129. So does an inline DECLARATION, and a CDATA section.
  local c129="$tmp/c129"; make_corpus "$c129"
  printf '# Page\n\n## <!A secret runtime logger> Overview\n## <![CDATA[secret runtime logger]]> Two\n' \
    > "$c129/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c129/scripts/docs-retrieval-questions.tsv"
  check "an inline declaration and CDATA are not indexed" fail "$c129"

  # 130. …and inside a CODE SPAN all three are literal text. The control.
  local c130="$tmp/c130"; make_corpus "$c130"
  printf '# Page\n\n## `<?secret runtime logger?>`\n' \
    > "$c130/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c130/scripts/docs-retrieval-questions.tsv"
  check "raw inline markup in a code span survives" pass "$c130"

  # 131. A reference USE takes the same escape rules as a definition label,
  #      or the link goes unrecognised and its invisible label is indexed.
  local c131="$tmp/c131"; make_corpus "$c131"
  printf '# Page\n\n## [Overview][secret\\] runtime logger]\n\n[secret\\] runtime logger]: /x\n' \
    > "$c131/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c131/scripts/docs-retrieval-questions.tsv"
  check "an escaped bracket does not end a reference" fail "$c131"

  # 132. …and with no such definition it is not a link, so every word of it
  #      is on the page. The control on 131.
  local c132="$tmp/c132"; make_corpus "$c132"
  printf '# Page\n\n## [Overview][secret\\] runtime logger]\n' \
    > "$c132/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c132/scripts/docs-retrieval-questions.tsv"
  check "an unresolved escaped reference is visible" pass "$c132"

  # 133. A label must hold a non-whitespace character, so `[]:` is no
  #      definition and the line renders as the text it looks like.
  local c133="$tmp/c133"; make_corpus "$c133"
  printf '# Page\n\n[]: /secret-runtime-logger\n---\n' \
    > "$c133/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c133/scripts/docs-retrieval-questions.tsv"
  check "an empty label is not a definition" pass "$c133"

  # 134. Text a comment leaves BEHIND is ordinary text: `Secret <!--`,
  #      `--> runtime logger`, `---` is one setext heading.
  local c134="$tmp/c134"; make_corpus "$c134"
  printf '# Page\n\nSecret <!--\n--> runtime logger\n---\n' \
    > "$c134/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c134/scripts/docs-retrieval-questions.tsv"
  check "text after a comment closes is indexed" pass "$c134"

  # 135. …while a comment that stays open still hides everything in it.
  local c135="$tmp/c135"; make_corpus "$c135"
  printf '# Page\n\nSecret <!--\nruntime logger\n---\n' \
    > "$c135/docs/guide/md.md"
  printf 'secret runtime logger\tdocs/guide/md.md\n' \
    > "$c135/scripts/docs-retrieval-questions.tsv"
  check "an open comment still hides its contents" fail "$c135"

  # 136. A comment line and a blank line in the fixture are skipped.
  local c8="$tmp/c8"; make_corpus "$c8"
  printf '# Pagination\n' > "$c8/docs/guide/pagination.md"
  printf '# a comment\n\npagination\tdocs/guide/pagination.md\n' \
    > "$c8/scripts/docs-retrieval-questions.tsv"
  check "comments and blanks are skipped" pass "$c8"

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]]
}

case "${1-}" in
  --self-test)
    self_test
    ;;
  --list)
    run_check "$root" --list
    ;;
  *)
    echo "Checking reader questions against the guide's own words..."
    if run_check "$root"; then
      echo "Docs retrieval gate OK."
    else
      cat >&2 <<'EOF'

FAIL: a reader question does not reach the page that answers it (listed above).

The answer exists and is right; the page does not say so in the words the
asker types. Fix it at the lowest rung that closes it:

  - the page's H1 or slug is spelled in project vocabulary
      -> retitle the H1 in the reader's words (do NOT rename the file: the
         path is the URL, and inbound links are the corpus's most valuable
         asset)
  - the answer is on the page but under no heading of its own
      -> give it a heading in the reader's words
  - the answer is genuinely split across pages
      -> fold it onto the strongest page and crosslink from the others

Do NOT add a new page. A second page answering the same question splits the
rank that would have let a reader find either, and both then drift.
EOF
      exit 1
    fi
    ;;
esac
