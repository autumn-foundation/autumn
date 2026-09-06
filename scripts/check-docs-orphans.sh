#!/usr/bin/env bash
# Reachability gate: every page in `docs/guide/` must be reachable by clicking,
# starting from a surface a reader actually enters through.
#
# WHY THIS EXISTS: the corpus already gates the three things a reader copies off
# a page — its links (`check-docs-links.sh`, a 404), its commands
# (`check-docs-cli.sh`, `unrecognized subcommand`), and its `AUTUMN_*` variables
# (`check-docs-config.sh`, a silent no-op). All three check a page the reader
# ALREADY REACHED. None of them can tell you whether the page can be reached at
# all, and a page nobody lands on fails without producing an error anywhere: no
# 404, no exit code, no ignored override. The reader simply concludes the
# feature does not exist and goes and builds it themselves.
#
# That is the quietest defect class in a docs corpus, and the guide is where it
# accumulates, because `docs/guide/` has no index page of its own. The 155 guide
# pages are discovered through the hand-maintained `## Documentation` list in
# `README.md` and the skill indexes — surfaces someone has to remember to
# update. Nothing noticed when they forgot.
#
# The baseline run found three pages unreachable from every reader entry point:
#
#   docs/guide/time-zones.md                    (251 lines)
#   docs/guide/active-search-and-autocomplete.md (314 lines)
#   docs/guide/outbound-webhooks.md             (201 lines)
#
# All three are ACCURATE — every symbol they name resolves in the current
# source — which is what makes them the expensive kind of defect rather than the
# cheap kind. The work was done and then filed where no reader can reach it.
# `outbound-webhooks.md` is the sharpest case: `README.md` indexes
# `signed-webhooks.md` (webhooks coming IN), so a reader searching the README
# for "webhook" finds the inbound page, concludes that is all there is, and
# never sees the page about sending signed webhooks OUT to their own customers
# — a surface with a filed SSRF report against it
# (`docs/security/2026-09-03-webhook-ssrf/`). Its only two mentions anywhere are
# that report and a `///` comment in `webhook_outbound.rs`; neither is something
# a reader can click.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   Build the graph a reader can actually walk, and assert every guide page is
#   in it.
#     - NODES are `docs/guide/**/*.md`.
#     - ROOTS are the surfaces a reader or agent ENTERS through: the root
#       `README.md`, `EXAMPLES.md`, `CONTRIBUTING.md`, `STABILITY.md`,
#       `docs/plugins.md`, each skill's `SKILL.md`, each top-level `agents/*.md`,
#       and each `examples/*/README.md`.
#     - WAYPOINTS are the other pages under `skills/` and `agents/` — a skill's
#       `references/*.md`. They are NOT roots: they are ordinary pages their
#       `SKILL.md` links to, so they carry edges only once something reaches
#       them. Seeding them as roots would let a reference file that nothing
#       links any more still confer reachability on every guide it names.
#     - EDGES run from a root, a waypoint, or another guide page to a guide page
#       (or waypoint) it names, as a markdown link or as a bare repo path.
#   A page is a defect when no path of edges reaches it from any root.
#
# REACHABILITY, NOT INBOUND-LINK COUNT: this walks the graph instead of asking
# "does anything mention this page", because the weaker question misses exactly
# the case worth catching — a page linked only from other unreachable pages, or
# only from a file that is itself off the reader's path. Counting mentions finds
# two of the three baseline defects; `outbound-webhooks.md` has two inbound
# mentions and is still unreachable, and it is the one with the security report
# attached.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - Pages outside `docs/guide/`. `skills/` and `agents/` are loaded by name by
#     the agent machinery rather than linked, and `examples/*/README.md` is
#     rendered by a link to its DIRECTORY (`[blog](examples/blog)`), so neither
#     is reachable-by-link even when it is perfectly reachable in practice.
#     Gating them would report structure as breakage. Guide pages are the
#     homogeneous set that exists only to be linked to.
#   - `CHANGELOG.md` and `docs/releases/` are not edge sources. A page named
#     only in a historical release note is still unreachable for a reader
#     working today, and treating history as navigation would let the gate be
#     satisfied by a record of the page having once been current.
#   - `.rs` doc comments are not edge sources. `docs/guide/outbound-webhooks.md`
#     inside a `///` block is a citation, not a path a reader can follow.
#   - Whether the page is any GOOD, or whether its link sits somewhere sensible.
#     This gate answers one question — can a reader get here at all — and a
#     reachable page can still be badly placed.
#
# EDGES ARE READ PERMISSIVELY, on purpose: both `[text](docs/guide/x.md)` and a
# bare `docs/guide/x.md` count, and a guide page may link a sibling by relative
# filename (`](jobs.md)`, `](./jobs.md)`, `](tutorial/03-forms.md)`,
# `](../guide/jobs.md)`). A gate that argues about link syntax becomes a tax on
# writing docs; this one only ever asks whether some findable path exists.
#
# THE LINE IS VISIBLE vs INVISIBLE, not clickable vs not. A path inside a code
# fence or a code span still counts: it renders as text the reader can see,
# read and paste, so the page is findable from it. A path inside an HTML
# comment does not, and neither does one whose link was disabled with `\[`:
# those render as nothing at all. That is the whole rule, and it is why the two
# cases are handled so differently a few hundred lines below — comments are
# stripped before extraction, fences are not.
#
# It also means this gate deliberately parts company with
# `check-docs-links.sh`, which ignores fenced content because a sample link is
# not a link it should validate. Everywhere else the two agree ON PURPOSE, down
# to sharing this file's destination grammar: they run in the same CI job, so a
# link that resolves in one and not the other would fail a contributor's page in
# one breath and call it unreachable in the next.
#
# WAIVERS: a guide page is occasionally meant to be unlinked — an appendix
# reached only from a release note, a page kept for an external inbound link.
# Waive it with a marker in the page itself:
#
#     <!-- orphan-allow: reached only from the 0.7.0 release notes, see #1234 -->
#
# The marker lives in the page it exempts, so it is deleted by the same commit
# that finally links the page. A central allowlist would outlive the exemption
# and silently re-admit the defect. Every waiver must carry a reason.
#
# Run locally with:
#
#     scripts/check-docs-orphans.sh
#     scripts/check-docs-orphans.sh --self-test   # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# Kept in Python for the same reason as the sibling gates: the traversal and the
# relative-path resolution are graph and path work that bash would render
# unreadable, and python3 is already a dependency of the docs-only CI job.
run_check() {
  local dir="$1"
  python3 - "$dir" <<'PYEOF'
import os, posixpath, re, subprocess, sys, urllib.parse

root = sys.argv[1]

GUIDE = 'docs/guide/'
ROOT_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md',
              'docs/plugins.md')
ROOT_DIRS = ('skills/', 'agents/')
# History is a record, not a route. See the header.
HISTORY = ('CHANGELOG.md', 'docs/releases/')

# NUL-delimited so a path containing whitespace is not split into fragments and
# git does not quote unusual paths — same reason as check-docs-links.sh.
tracked = [
    f for f in subprocess.run(['git', 'ls-files', '-z'], cwd=root,
                              capture_output=True, text=True).stdout.split('\0')
    if f
]

nodes = sorted(f for f in tracked if f.startswith(GUIDE) and f.endswith('.md'))


def is_root(f):
    """A surface a reader or agent ENTERS through, rather than one they are
    routed to. A skill's entry file is `SKILL.md` — the agent machinery loads it
    by name — but the `references/*.md` beside it are ordinary pages that
    `SKILL.md` links to. Seeding those as roots would let a reference file that
    nothing links any more still confer reachability on the guides it names, so
    they are waypoints (below) instead: traversable, but only once something
    reaches them."""
    if f in HISTORY or f.startswith(HISTORY):
        return False
    return (f in ROOT_FILES
            or (f.startswith(ROOT_DIRS) and f.endswith('.md')
                and posixpath.basename(f) in ('SKILL.md', 'AGENT.md'))
            or (f.startswith('agents/') and f.endswith('.md')
                and posixpath.dirname(f) == 'agents')
            # `examples/<app>/README.md` only. A deeper one
            # (`examples/reddit-clone/capsules/README.md`) is a supporting page
            # its example links to, not an entry surface — same reason as a
            # skill's `references/*.md` above.
            or (f.startswith('examples/') and posixpath.basename(f) == 'README.md'
                and f.count('/') == 2))


roots = [f for f in tracked if is_root(f)]
node_set = set(nodes)
# Pages that are neither entry surfaces nor things we assert on, but that a
# reader can be routed THROUGH — a skill's `references/*.md`. They carry edges
# only once something reachable links them.
# Any tracked markdown that is neither an entry surface nor a page we assert on
# can still sit in the middle of a clickable path: `README.md` links
# `docs/release-checklist.md`, which links two guide pages. Restricting
# traversal to a whitelist of directories drops those hops and would report a
# guide reachable ONLY through such a hub as an orphan — a false positive, and
# the "tax on writing docs" this gate's header warns against. Reachability, not
# directory, decides: a hub carries edges only once something reaches it, so a
# planning note or an incident report that nothing links still confers nothing.
# History stays out regardless — it is a record, not a route.
waypoints = {f for f in tracked
             if f.endswith('.md') and f not in roots and f not in node_set
             and f not in HISTORY and not f.startswith(HISTORY)}
traversable = node_set | waypoints


def read(f):
    try:
        with open(posixpath.join(root, f), encoding='utf-8', errors='ignore') as fh:
            return fh.read()
    except OSError:
        return ''


# `](target)`. The destination grammar is lifted from the sibling
# `check-docs-links.sh` deliberately: the two gates run in the same CI job, so a
# link that resolves there and not here would pass one and be called an orphan
# by the other. That means the angle form (the only way to write a path with
# spaces), one level of balanced parentheses (`](guide(v2).md)`), and an
# optional link title in any of its three delimiters.
DEST_BARE = r'(?:[^()\s]|\([^()\s]*\))+'
TITLE = r'''(?:\s+(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^()\\]|\\.)*\)))?'''
# `[\s--\n]` style bounds: whitespace inside a destination may span ONE newline
# but never a blank line — a blank line ends the paragraph, so `[x](y.md\n\n)`
# renders no link at all and must not record an edge.
_WS = r'(?:[ \t]*\n?[ \t]*)'
DEST = r'\(' + _WS + r'(?:<([^<>]*)>|(' + DEST_BARE + r'))' + TITLE + _WS + r'\)'
# The label has to be matched too, not just the `](…)` tail: `\[Mail](mail.md)`
# renders as literal text, so treating it as a route would let a link someone
# deliberately disabled go on hiding an orphan. `\.` keeps an escaped bracket
# inside the label. The nesting form exists because a linked image puts one
# link inside another — `[![alt](img.png)](page.md)` — where the flat pattern
# finds only the image.
FLAT = r'(?:[^\[\]\\]|\\.)'
# `!` joins the lookbehind because `![alt](x.md)` is an IMAGE: its destination
# is a resource the page loads, not a page the reader can navigate to, and the
# path is never visible on screen. By this gate's visible-or-clickable rule it
# is therefore not an edge, so a stale image reference cannot keep an orphan
# alive. The nested pattern still resolves the outer target of a linked image,
# `[![alt](img.png)](page.md)`, which IS navigation.
MD_LINK = re.compile(r'(?<![\\!])\[' + FLAT + r'*\]' + DEST)
MD_LINK_NESTED = re.compile(
    r'(?<![\\!])\[(?:' + FLAT + r'|\[' + FLAT + r'*\])*\]' + DEST)
# Markdown drops the backslash from an escaped ASCII punctuation character, so
# `guide\(v2\).md` addresses the file `guide(v2).md` — same rule as the sibling.
UNESCAPE = re.compile(r'\\([!-/:-@\[-`{-~])')
# A numeric character reference in a destination: `mail&#46;md` and
# `mail&#x2e;md` both address `mail.md`. The migration-guide gate already pins
# this behaviour (`repo_hygiene.rs`
# migration_guide_gate_resolves_a_character_reference_in_a_destination), and
# comparing the encoded spelling rejects a link that works for the reader.
CHAR_REF = re.compile(r'&#(?:[xX]([0-9a-fA-F]+)|(\d+));')


def decode_char_refs(s):
    def one(m):
        try:
            return chr(int(m.group(1), 16) if m.group(1) else int(m.group(2)))
        except (ValueError, OverflowError):
            return m.group(0)
    return CHAR_REF.sub(one, s)
# A reference definition: `[mail]: mail.md`, optionally `<…>`-wrapped. Markdown
# allows up to three leading spaces. Reference-style links are a syntax
# check-docs-links.sh already parses and self-tests, so a page linked only that
# way is genuinely reachable; without this the gate would report it as an
# orphan and block a docs change written in a spelling the corpus supports.
# The label honours escapes, so `[closing \]]: mail.md` is one definition whose
# label contains a bracket — `[^\]]+` would stop at the escaped one and lose the
# definition entirely. Same `FLAT` shape the sibling uses for exactly this.
REF_DEF = re.compile(
    r'^ {0,3}\[((?:[^\[\]\\]|\\.)+)\]:\s*(?:<([^<>]*)>|(\S+))', re.M)
# The same, extended to the optional title — on the destination's line or the
# one after it, per CommonMark. Used only to blank the definition's full span.
REF_DEF_FULL = re.compile(
    r'^ {0,3}\[(?:[^\[\]\\]|\\.)+\]:\s*(?:<[^<>]*>|\S+)'
    r'(?:[ \t]*\n?[ \t]*(?:"[^"]*"|\'[^\']*\'|\([^()]*\)))?', re.M)
# ...but a definition only becomes a link the reader can click when some label
# USES it. A leftover `[old]: mail.md` with no `[…][old]` anywhere renders as
# nothing at all, so counting it as an edge would let an obsolete line launder a
# genuinely orphaned page past this gate — the failure direction that matters,
# since it is the one the gate exists to catch. Collect the labels actually
# used, in all three CommonMark spellings, and resolve only those definitions.
#   full:      [text][label]
#   collapsed: [label][]
#   shortcut:  [label]        (not followed by `(`, `[` or `:`)
# `(?<![\\!])` for the same two reasons the inline pattern carries it:
# `\[mail][]` renders as literal text, and `![mail][]` is an image whose
# destination is a resource rather than a page. Neither must keep a definition
# alive and go on hiding an orphan.
# Labels honour escapes on BOTH sides of the match, or `[mail][closing \]]`
# would be read against a definition whose label the same escape kept whole,
# and the two would never line up.
_LBL = r'(?:[^\[\]\\]|\\.)'
REF_USE_FULL = re.compile(r'(?<![\\!])\[' + _LBL + r'*\]\[(' + _LBL + r'*)\]')
REF_USE_SHORTCUT = re.compile(r'(?<![\\!])\[(' + _LBL + r'+)\](?![\(\[:])')
# Every full-reference span, image or not. Used to blank them before the
# shortcut scan: in `![alt][mail]` the guard correctly stops REF_USE_FULL, but
# the trailing `[mail]` then looks exactly like a standalone shortcut link, so
# an image would resurrect the label the guard just rejected.
REF_USE_ANY = re.compile(r'\[' + _LBL + r'*\]\[' + _LBL + r'*\]')
# An image and its destination, in both spellings. Blanked before the bare-path
# scan for the same reason the inline pattern guards against `!`: the path in
# `![alt](docs/guide/x.md)` is a resource the page loads, never text on screen,
# so it is not something a reader can find the page by.
# The alt text may itself contain a bracketed span — `![nested [alt]](x.md)` —
# and a pattern that stops at the inner bracket leaves the image unmasked, so
# the bare scan picks its destination back up.
IMAGE_INLINE = re.compile(
    r'!\[(?:(?:[^\[\]\\]|\\.)|\[(?:[^\[\]\\]|\\.)*\])*\]' + DEST)
IMAGE_REF = re.compile(
    r'!\[(?:(?:[^\[\]\\]|\\.)|\[(?:[^\[\]\\]|\\.)*\])*\](?:\[[^\]]*\])?')
# Any inline link span, image or not. Blanked before the shortcut scan: links
# cannot nest, so in `[outer [mail]](https://example.com)` only the OUTER link
# renders and the inner `[mail]` is ordinary label text — not a shortcut
# reference that should keep `[mail]: mail.md` alive.
INLINE_SPAN_ANY = re.compile(
    r'\[(?:(?:[^\[\]\\]|\\.)|\[(?:[^\[\]\\]|\\.)*\])*\]' + DEST)


def ref_label(s):
    """CommonMark label matching: case-insensitive, internal whitespace collapsed."""
    return ' '.join(s.split()).lower()
# A bare repo path. The leading guard keeps `…/docs/guide/x.md` inside a longer
# path from matching at the wrong offset.
BARE = re.compile(
    r'(?<![\w/.-])((?:docs|skills|agents|examples)/[A-Za-z0-9._/-]+\.md)')


def normalize(p):
    """Collapse `.`/`..` without touching the filesystem (targets may not exist;
    that is check-docs-links.sh's defect to report, not this gate's)."""
    parts = []
    for seg in p.split('/'):
        if seg in ('', '.'):
            continue
        if seg == '..':
            if parts:
                parts.pop()
            else:
                return None
        else:
            parts.append(seg)
    return '/'.join(parts)


# An HTML comment renders as nothing, so a link inside one — `<!-- old:
# [Mail](mail.md) -->` — is not a route: the reader has nothing to click. Left
# in, a commented-out link would mark its target reachable and let precisely the
# obsolete-link case this gate exists to catch pass silently. Stripped for EDGE
# extraction only; the waiver scan below deliberately reads the raw text, since
# `<!-- orphan-allow: … -->` is itself an HTML comment.
# The second alternative matters: an UNCLOSED `<!--` comments out the rest of
# the file as far as Markdown is concerned, so a missing `-->` must not leave
# the links after it counting as routes. Closed form is tried first, so a
# terminated comment still strips only itself.
HTML_COMMENT_CLOSED = re.compile(r'<!--.*?-->', re.S)
UNCLOSED = '<!--'
# A fence opener/closer: ``` or ~~~ , up to three spaces of indent, plus the
# rest of the line — which decides whether the line is a fence at all.
FENCE = re.compile(r'^ {0,3}(`{3,}|~{3,})(.*)$', re.M)
# A code span is a run of backticks closed by an equal-length run; both
# delimiters must be complete runs. Same shape as the sibling gate's.
CODE_SPAN = re.compile(r'(?<!`)(`+)(?!`)(.*?)(?<!`)\1(?!`)', re.S)


def split_fences(txt):
    """Split into ('prose'|'fence', text) segments, in order."""
    parts, pos, in_fence, marker = [], 0, False, None
    for m in FENCE.finditer(txt):
        tok, rest = m.group(1), m.group(2)
        if not in_fence:
            # A backtick info string may not itself contain a backtick, so
            # ```` ```md`invalid ```` opens nothing — it is ordinary prose, and
            # treating it as a fence would swallow the real links after it.
            if tok[0] == '`' and '`' in rest:
                continue
            parts.append(('prose', txt[pos:m.start()]))
            # Keep the opener's character AND length: a ```` fence is not
            # closed by a ``` line inside it, so recording only three
            # characters would end the block early and treat the code after
            # it as live prose.
            in_fence, marker, pos = True, (tok[0], len(tok)), m.start()
        elif (tok[0] == marker[0] and len(tok) >= marker[1]
              and not rest.strip()):
            # A CLOSING fence may be followed only by whitespace, so
            # ```` ```not-a-closer ```` leaves the block open. Closing early
            # would hand the fence's contents back to the prose scanners.
            parts.append(('fence', txt[pos:m.end()]))
            in_fence, marker, pos = False, None, m.end()
    parts.append(('fence' if in_fence else 'prose', txt[pos:]))
    return parts


def strip_comments(txt):
    """Remove HTML comments, but only where Markdown would treat them as
    comments. Inside a fenced code block `<!--` is literal text — an
    illustrative unclosed one in a sample must not comment out the live links
    that follow the closing fence, which is a false positive that would block a
    docs change. So fences are passed through untouched, and the run-to-EOF rule
    for an unclosed comment applies only to prose spans.

    FOUR KNOWN LIMITATIONS, all narrow, all false POSITIVES, and all left
    deliberately because the available fixes cost more than the bugs:

    1. A four-space-INDENTED code block is not recognised, so an unclosed
       `<!--` inside one is read as a real comment. A naive "four spaces means
       code" rule would stop a genuinely commented-out link inside a list item
       from being stripped — a false NEGATIVE, and being unsatisfiable by a
       line no reader can follow is this gate's whole job.

       CORRECTION to what this note said for several commits: it claimed no
       gate in the repo masks indented code, having checked only
       `check-docs-links.sh`. That was wrong — `check-migration-guides.sh`
       does, list-aware, and pins it with
       `migration_guide_gate_ignores_links_in_indented_code_blocks`. The
       conclusion stands but the reason is different: that logic lives inside
       that gate's awk entry-walking state machine, measuring indentation
       against each list item's content column, so it is not a function to
       borrow. Porting it means re-deriving it, which is exactly what these
       limitations exist to avoid.

    2. `FENCE` matches at absolute column, so a fence opened inside a block
       quote (`> ```html`) or indented under a list item is not seen as one.
       An unclosed `<!--` in such a block is then read as a real comment and
       truncates the rest of the page.

    3. Link labels nest only one level deep. `[outer [inner [deep]]](x.md)` is
       valid Markdown that neither this gate nor `check-docs-links.sh` matches,
       because balanced nesting to arbitrary depth is not expressible as a
       regular expression — it needs a bracket-matching scan. Both gates share
       the limit, so neither disagrees with the other about such a link, and
       one level covers the shape that actually occurs (a linked image,
       `[![alt](img.png)](page.md)`).

    4. Code spans are masked before comments are located, so a comment whose
       terminator sits inside backticks — `<!-- note ``-->`` -->` — has that
       terminator blanked, reads as unclosed, and truncates the page. The
       ordering is deliberate and is what makes case (1) of the fenced/inline
       fixes work: a `<!--` shown as a sample must not open a comment. Getting
       both right needs comment and code-span parsing interleaved with
       CommonMark's precedence between raw HTML and code spans, which is a
       parser, not a guard — and the failure mode of getting that subtly wrong
       is silently dropping real comments, a false negative.

    The sibling DOES solve (2), in ~120 lines of container-aware tracking
    (`quote_depth`, `column`, `strip_fences`). Copying that here is the wrong
    move: it would put a third copy of subtle CommonMark logic in the tree, and
    the two copies would drift exactly the way this PR's own duplicated prose
    did. The right fix is to lift the sibling's fence handling into something
    both gates import — which is a shared-tooling change to the docs CI job,
    not a line to smuggle into a reachability gate. Until then this stays a
    documented gap rather than a re-derived near-copy."""
    parts = split_fences(txt)

    out = []
    for kind, seg in parts:
        if kind == 'fence':
            out.append(seg)
            continue
        # A `<!--` inside an inline code span is literal text too, so mask code
        # spans while LOCATING comments and slice the original — the contents
        # themselves stay, since a bare path in code is still findable.
        masked = CODE_SPAN.sub(lambda m: ' ' * len(m.group(0)), seg)
        for cm in reversed(list(HTML_COMMENT_CLOSED.finditer(masked))):
            seg = seg[:cm.start()] + ' ' * (cm.end() - cm.start()) + seg[cm.end():]
            masked = masked[:cm.start()] + ' ' * (cm.end() - cm.start()) + masked[cm.end():]
        idx = masked.find(UNCLOSED)
        if idx != -1:
            # Everything from here to the end of the document is commented out.
            out.append(seg[:idx])
            return ''.join(out)
        out.append(seg)
    return ''.join(out)


# Raw HTML whose contents the browser does not render as documentation, and
# resource attributes whose value never appears on screen. `<a href>` is
# deliberately absent: a raw anchor IS navigation and must keep conferring
# reachability.
# The unclosed alternative matters for the same reason it does for comments: an
# opener with no matching close makes the rest of the file raw HTML, so
# Markdown-shaped text after it renders as nothing.
HIDDEN_HTML = re.compile(
    r'<(script|style|template)\b[^>]*>.*?</\1\s*>'
    r'|<(?:script|style|template)\b[^>]*>.*', re.S | re.I)
# Every attribute value EXCEPT `href`. An attribute is not rendered text, so a
# path, a reference label or a comment marker parked in one — `<span
# data-note="[mail](mail.md)">` — is invisible and confers nothing. `href` is
# excluded because `<a href=…>` is real navigation the anchor extractor reads.
# This subsumes the old `src`-only rule, which was the same idea applied to one
# attribute name.
ATTR_VALUE = re.compile(
    r'\b(?!href\b)[A-Za-z_:][-\w:.]*\s*=\s*(?:"[^"]*"|\'[^\']*\'|[^\s>]+)', re.I)
# The same with no exception, for tags that are not anchors. `href` is spared
# only on `<a>`: on `<link rel="alternate" href="…">` it names a resource the
# page references invisibly, not somewhere the reader can click.
ATTR_VALUE_ANY = re.compile(
    r'\b[A-Za-z_:][-\w:.]*\s*=\s*(?:"[^"]*"|\'[^\']*\'|[^\s>]+)', re.I)
ANCHOR_TAG = re.compile(r'<a(?:\s[^>]*)?>', re.I)
# The `](…)` destination of a rendered link, blanked before the bare-path scan.
LINK_DEST = re.compile(r'\]' + DEST)
# A whole raw tag, used to bound where `src=` may be masked. Unscoped, that
# pattern also eats the query of `[Mail](mail.md?src=guide)`, which is an
# ordinary Markdown link the sibling gate resolves.
# Attribute values are consumed as units so a quoted `>` does not end the tag:
# `<span title="1 > 0" data-note="…">` is one tag, and stopping at the quoted
# character would leave the later attributes outside the masking bounds.
HTML_TAG = re.compile(
    r'<[A-Za-z][A-Za-z0-9-]*'
    r'(?:\s+[A-Za-z_:][-\w:.]*(?:\s*=\s*(?:"[^"]*"|\'[^\']*\'|[^\s>]+))?)*'
    r'\s*/?>')
# A raw HTML BLOCK of any tag — CommonMark type 6. Its contents are raw HTML,
# so `[mail]` inside `<div>…</div>` stays literal and resolves no reference.
# Unlike `HIDDEN_HTML` the text is still VISIBLE, so this bounds Markdown
# extraction only; bare paths inside it still count.
# A type-6 block starts on a line beginning with a tag — trailing content on
# that line is allowed, `<div>example` opens one — and ends at the next BLANK
# line, not at a closing tag. Requiring a stand-alone opener and a matching
# close missed both halves of that.
# CommonMark's type-6 block-tag list. ONLY these open a block from a line that
# merely STARTS with the tag — `<div>example` does, `<span>text` does not,
# because `span` is inline and its line is ordinary paragraph text where a
# Markdown link still renders. Accepting every tag name here masked live links.
BLOCK_TAGS = (
    'address|article|aside|base|basefont|blockquote|body|caption|center|col|'
    'colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|'
    'footer|form|frame|frameset|h1|h2|h3|h4|h5|h6|head|header|hr|html|iframe|'
    'legend|li|link|main|menu|menuitem|nav|noframes|ol|optgroup|option|p|'
    'param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|'
    'track|ul')
# Type 6 (a block tag, trailing content allowed) or type 7 (any complete tag
# ALONE on its line). Both run to the next blank line.
#
# Types 3, 4 and 5 are here too — a processing instruction `<? … ?>`, a
# declaration `<!DOCTYPE …>`, and `<![CDATA[ … ]]>`. Their contents are raw
# HTML, so Markdown inside them is literal; unlike 6 and 7 they end at their own
# closing delimiter rather than at a blank line. The repo's migration gate pins
# all three (`…_treats_a_cdata_block_as_literal` and its two siblings), and
# omitting them let a link inside any of the three count as navigation.
# Types 3-6 MAY interrupt a paragraph, so they need no context test.
RAW_BLOCK = re.compile(
    r'^ {0,3}<\?.*?\?>'
    r'|^ {0,3}<!\[CDATA\[.*?\]\]>'
    r'|^ {0,3}<![A-Za-z][^>]*>'
    r'|^ {0,3}</?(?:' + BLOCK_TAGS + r')(?:[\s/>][^\n]*)?$'
    r'(?:\n(?![ \t]*$)[^\n]*)*',
    re.M | re.I | re.S)
# Type 7 — any complete tag alone on its line — may NOT. Applied only where a
# block can start, or `prose` / `<span>` / `[Mail](mail.md)` masks a live link.
RAW_BLOCK_TYPE7 = re.compile(
    r'^ {0,3}</?[A-Za-z][A-Za-z0-9-]*(?:\s[^\n]*?)?/?>[ \t]*$'
    r'(?:\n(?![ \t]*$)[^\n]*)*',
    re.M)
# A raw anchor IS navigation, so its destination is resolved like any other —
# through `add_relative`, which means `<a href="mail.md">` and
# `<a href="../guide/mail.md">` work, not just the repo-root spelling the
# bare-path scan happens to catch.
ANCHOR_HREF = re.compile(
    r'<a\b[^>]*?\bhref\s*=\s*(?:"([^"]*)"|\'([^\']*)\'|([^\s>]+))', re.I)


ATX_HEADING = re.compile(r'^ {0,3}#{1,6}(?:\s|$)')
THEMATIC_BREAK = re.compile(r'^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$')
FENCE_LINE = re.compile(r'^ {0,3}(?:`{3,}|~{3,})')


def block_starts(txt):
    """Offsets of lines where a new block may begin — i.e. no paragraph is open.

    Two separate rules need this, which is why it is a function rather than
    another `\\n\\n` test. A CommonMark type-7 raw HTML block may NOT interrupt
    a paragraph, so `prose` / `<span>` / `[Mail](mail.md)` leaves the tag inline
    and the link live. A reference definition may not interrupt one either —
    but it MAY follow a completed block, so `## Links` / `[mail]: mail.md`
    defines, with no blank line between. Requiring a preceding blank line got
    the first case wrong in one direction and the second in the other.

    `check-migration-guides.sh` tracks the same state as `md_paragraph_open`,
    and pins both cases (`…_keeps_a_type_seven_tag_inline_inside_a_paragraph`,
    `…_lets_a_type_six_tag_interrupt_a_paragraph`).
    """
    starts, pos, para_open = set(), 0, False
    for line in txt.split('\n'):
        if not para_open:
            starts.add(pos)
        stripped = line.strip()
        if not stripped:
            para_open = False
        elif (ATX_HEADING.match(line) or THEMATIC_BREAK.match(line)
                or FENCE_LINE.match(line)):
            # A completed block of its own: it closes any paragraph above and
            # leaves none open below.
            para_open = False
        else:
            para_open = True
        pos += len(line) + 1
    return starts


def sub_in_prose(pat, txt, only_at_block_start=False):
    """Blank `pat` where Markdown renders it, leaving fences alone.

    Anything that decides "this text is not rendered" has to be scoped this
    way — a construct shown inside a fence is a sample whose path is on screen.
    Every time a rule in this file was applied document-wide instead, it
    deleted a visible path; this helper exists so the scoping is one call
    rather than something to remember.
    """
    out = []
    for kind, seg in split_fences(txt):
        if kind != 'prose':
            out.append(seg)
            continue
        # Code SPANS are visible too, and this helper originally missed them —
        # blanking link destinations erased an image sample written in
        # backticks. "Rendered" means outside fences AND outside code spans.
        protected = [(m.start(), m.end()) for m in CODE_SPAN.finditer(seg)]
        allowed = block_starts(seg) if only_at_block_start else None
        pieces, last = [], 0
        for m in pat.finditer(seg):
            if any(a <= m.start() < b for a, b in protected):
                continue
            if allowed is not None and m.start() not in allowed:
                continue
            pieces.append(seg[last:m.start()])
            pieces.append(' ' * (m.end() - m.start()))
            last = m.end()
        pieces.append(seg[last:])
        out.append(''.join(pieces))
    return ''.join(out)


def mask_invisible(txt):
    """Blank everything a reader cannot see — in PROSE, and not inside a code
    span. This is the ONE place that decides what is invisible, because scoping
    one masker and leaving a sibling document-wide is a bug this gate has
    shipped twice: raw HTML was scoped and images were not, so an image sample
    in a fence lost its visible destination while a `<script>` sample kept it.

    Inside a fence, `<script src="/static/js/x.js">` is a sample the reader can
    see and copy, which is the same reason a bare path in a fence counts. Five
    guide pages ship exactly that, and an inline `` `<script src="…">` `` is the
    same thing in miniature. Masking those would delete visible text; masking
    nothing lets a `<script>` block in prose, which renders as nothing, keep an
    orphan alive.

    Replacements are space-for-space so offsets stay valid for the callers that
    blank spans by position.
    """
    out = []
    for kind, seg in split_fences(txt):
        if kind != 'prose':
            out.append(seg)
            continue
        protected = [(m.start(), m.end()) for m in CODE_SPAN.finditer(seg)]
        tags = [(m.start(), m.end()) for m in HTML_TAG.finditer(seg)]

        def blank(pat, bound=None):
            nonlocal seg
            pieces, last = [], 0
            for m in pat.finditer(seg):
                if any(a <= m.start() < b for a, b in protected):
                    continue
                if bound is not None and not any(
                        a <= m.start() < b for a, b in bound):
                    continue
                pieces.append(seg[last:m.start()])
                pieces.append(' ' * (m.end() - m.start()))
                last = m.end()
            pieces.append(seg[last:])
            seg = ''.join(pieces)

        blank(HIDDEN_HTML)
        anchors = [(m.start(), m.end()) for m in ANCHOR_TAG.finditer(seg)]
        others = [t for t in tags
                  if not any(a <= t[0] < b for a, b in anchors)]
        blank(ATTR_VALUE, bound=anchors)
        blank(ATTR_VALUE_ANY, bound=others)
        # Images last, and here rather than at the bare-path scan, so a
        # `![alt](x.md)` SAMPLE in a fence or code span keeps its visible
        # destination while a rendered image does not.
        blank(IMAGE_INLINE)
        blank(IMAGE_REF)
        out.append(seg)
    return ''.join(out)


def edges_from(f):
    """Guide pages this file gives a reader a way to reach."""
    # THE ORDER OF THESE THREE STEPS IS LOAD-BEARING, and each was set by a
    # test that failed when it was wrong.
    #
    # 1. Escapes fold FIRST. An escaped backslash consumes the next one, so
    #    `\\\\[x](y.md)` renders a literal backslash and a LIVE link while
    #    `\\[x](y.md)` renders literal text; and `\![x](y.md)` is a link, not an
    #    image, because the backslash stops the `!`. Fixed-width lookbehinds
    #    cannot count a run or see past the `!`, so both are folded to
    #    same-LENGTH placeholders — same length because spans are blanked by
    #    offset further down and the positions must stay valid. This has to
    #    precede masking, or `mask_invisible` reads `\![x](y.md)` as an image
    #    and blanks a live link.
    #    `\<script>` is the same idea for a tag: the escape stops raw HTML, so
    #    prose showing literal tags around a live link must keep that link.
    txt = (read(f).replace('\\\\', '\x00\x00')
           .replace('\\!', '\x01\x01')
           .replace('\\<', '\x02\x02'))
    # 2. Raw HTML is identified BEFORE comments. A `<!--` inside a closed
    #    `<script>` is script data, not a Markdown comment, and parsing comments
    #    first truncated the document at it. This order also settles the
    #    unclosed-opener case: the block is masked, so nothing inside it can
    #    open a comment either.
    # 3. Comments last, over what survives.
    txt = strip_comments(mask_invisible(txt))
    out = set()
    base = posixpath.dirname(f)

    def add_relative(raw):
        # Percent-decode before comparing to tracked filenames: `mail%20guide.md`
        # addresses `mail guide.md`, and the sibling gate decodes it too.
        # Anchor first, then query — the sibling's order. `mail.md?view=all` and
        # `mail.md?view=all#frag` both address `mail.md`; leaving the query on
        # would fail the `.md` test below and call a live link an orphan.
        raw = decode_char_refs(raw)
        raw = raw.split('#', 1)[0].split('?', 1)[0].strip()
        raw = urllib.parse.unquote(raw)
        raw = (raw.replace('\x00\x00', '\\\\')
               .replace('\x01\x01', '\\!')
               .replace('\x02\x02', '\\<'))
        raw = UNESCAPE.sub(r'\1', raw)
        if not raw.endswith('.md'):
            return
        # An absolute-looking `/docs/guide/x.md` is a site path, not a file path.
        raw = raw.lstrip('/') if raw.startswith('/') else raw
        # Try both readings: relative to this file, and from the repo root. A
        # guide page writing `](docs/guide/mail.md)` means the latter, and a
        # base-join would silently produce `docs/guide/docs/guide/mail.md`.
        for cand in (posixpath.join(base, raw), raw):
            t = normalize(cand)
            if t in traversable:
                out.add(t)
                return

    # Reference definitions are resolved ONLY through the usage-aware path
    # below, so blank their spans first: a bare scan over `[old]: docs/guide/x.md`
    # would re-admit an unused definition that renders as nothing, which is the
    # hole the usage check exists to close.
    # Blanking uses the FULL definition, title included. `REF_DEF` stops at the
    # destination because that is all resolution needs, but a definition may
    # carry an optional title — `[old]: https://example.com "docs/guide/x.md"` —
    # and a title left behind is scanned as a bare path even though the whole
    # unused definition renders nothing.
    # A rendered link's DESTINATION is not on screen — the reader sees the
    # label. So `[search](https://example.test/?q=docs/guide/mail.md)` must not
    # feed the bare-path scan a path out of its query string; that destination
    # is `MD_LINK`'s to resolve, and it resolves to an external URL. Prose only:
    # the same text in a fence shows the path and still counts.
    scan = sub_in_prose(LINK_DEST, txt)
    scan = sub_in_prose(REF_DEF_FULL, scan)
    for m in BARE.finditer(scan):
        t = normalize(m.group(1))
        if t in traversable:
            out.add(t)

    # Markdown extraction runs over a view with raw HTML BLOCKS removed: inside
    # `<div>…</div>` a `[mail](x.md)` stays literal, so it is not navigation.
    # The bare-path scan above deliberately still sees that region, because the
    # path itself is visible text there — the same visible/invisible split that
    # governs fences.
    md_txt = sub_in_prose(RAW_BLOCK, txt)
    md_txt = sub_in_prose(RAW_BLOCK_TYPE7, md_txt, only_at_block_start=True)

    for pattern in (MD_LINK, MD_LINK_NESTED):
        for m in pattern.finditer(md_txt):
            add_relative(m.group(1) if m.group(1) is not None else m.group(2))

    # Anchors read `txt`, NOT the raw-block-stripped view. A type-6 block
    # suppresses MARKDOWN inside it, but its raw HTML is passed through and
    # rendered, so `<a href="mail.md">` on its own line — which is itself such a
    # block — is a link the reader can click. Stripping it here broke two
    # anchor tests, which is what said the distinction is real.
    for m in ANCHOR_HREF.finditer(txt):
        add_relative(next(g for g in m.groups() if g is not None))

    # A reference USE inside code — `` `[mail][]` `` — is the one code case that
    # does not count, and it is not an exception to the visible/invisible rule
    # but an application of it. A bare `docs/guide/x.md` in a fence counts
    # because the PATH is on screen; a reference use in a fence shows only the
    # label, while the path lives in a definition that renders as nothing at
    # all. Nothing a reader can see names the target, so it is not an edge.
    ref_txt = ''.join(
        seg for kind, seg in split_fences(md_txt) if kind == 'prose')
    ref_txt = CODE_SPAN.sub(' ', ref_txt)

    # A definition is not a USE of anything, including of a label sitting in its
    # own title — `[old]: https://example.com "[mail]"` renders nothing at all.
    # Definition spans are blanked for the usage scans only; resolution below
    # still reads `ref_txt`, where the definitions are intact. Blanking them for
    # the bare-path scan and not here was the same one-view-not-its-sibling
    # mistake that `sub_in_prose` and `mask_invisible` were each introduced for.
    uses_txt = REF_DEF_FULL.sub(lambda m: ' ' * len(m.group(0)), ref_txt)

    used = set()
    for m in REF_USE_FULL.finditer(uses_txt):
        # `[label][]` (collapsed) leaves group 1 empty; the label is the text.
        inner = m.group(1)
        used.add(ref_label(inner) if inner.strip()
                 else ref_label(m.group(0)[1:m.group(0).index(']')]))
    # Blank every full-reference span before looking for shortcuts, so the
    # `[mail]` tail of `![alt][mail]` is not re-read as a link of its own.
    shortcut_txt = REF_USE_ANY.sub(lambda m: ' ' * len(m.group(0)), uses_txt)
    shortcut_txt = INLINE_SPAN_ANY.sub(
        lambda m: ' ' * len(m.group(0)), shortcut_txt)
    for m in REF_USE_SHORTCUT.finditer(shortcut_txt):
        used.add(ref_label(m.group(1)))

    # Markdown resolves a duplicated label to its FIRST definition, so a stale
    # `[mail]: mail.md` sitting below a live `[mail]: https://example.com` names
    # a page the reader never reaches — and must not keep it out of the report.
    # Definitions come from the prose-only view for the same reason usages do:
    # a `[mail]: mail.md` DEMONSTRATED inside a fence defines nothing, and
    # resolving it would let a documentation sample keep an orphan alive.
    # A definition cannot INTERRUPT a paragraph: after an ordinary prose line
    # it is literal text, and the reference above it resolves to nothing. The
    # migration-guide gate pins the same rule
    # (`migration_guide_gate_ignores_a_definition_continuing_a_paragraph`).
    # So a definition counts only where a block can start — at the top of the
    # view, or after a blank line.
    seen_labels = set()
    ref_block_starts = block_starts(ref_txt)
    for m in REF_DEF.finditer(ref_txt):
        if m.start() not in ref_block_starts:
            continue
        label = ref_label(m.group(1))
        if label in seen_labels:
            continue
        seen_labels.add(label)
        if label not in used:
            continue
        add_relative(m.group(2) if m.group(2) is not None else m.group(3))
    return out


# Breadth-first from every root. Only roots and guide pages carry edges: that
# restriction is what keeps a security report or a `///` citation from standing
# in for a route a reader can walk.
seen = set()
frontier = list(roots)
while frontier:
    cur = frontier.pop()
    for nxt in edges_from(cur):
        if nxt not in seen:
            seen.add(nxt)
            frontier.append(nxt)

WAIVER = re.compile(r'<!--\s*orphan-allow:\s*(.+?)\s*-->', re.S)


def waiver_view(txt):
    """The text a waiver may legitimately live in: prose, with code removed.

    A page that DOCUMENTS the marker — showing `<!-- orphan-allow: … -->` in a
    fence or a code span, as this script's own header and failure message do —
    would otherwise exempt itself by explaining the escape hatch. The exemption
    is the one place a false negative is most expensive, since it is how a page
    opts out of the check entirely, so it is matched only where an HTML comment
    would actually be an HTML comment.
    """
    txt = mask_invisible(txt)
    kept = [seg for kind, seg in split_fences(txt) if kind == 'prose']
    return CODE_SPAN.sub(' ', ''.join(kept))


defects, waived = [], 0
for n in sorted(node_set - seen):
    m = WAIVER.search(waiver_view(read(n)))
    if m and m.group(1).strip():
        waived += 1
    else:
        defects.append(
            f"{n}: unreachable — no root links it, directly or through another guide page"
            + (" (orphan-allow marker has no reason after the colon)" if m else "")
        )

print(f"guide pages: {len(nodes)}")
print(f"roots: {len(roots)} reader entry surfaces")
print(f"reachable: {len(seen & node_set)}")
for d in defects:
    print(f"  {d}")
print(f"defects: {len(defects)}" + (f" ({waived} waived)" if waived else ""))
sys.exit(1 if defects else 0)
PYEOF
}

self_test() {
  local tmp pass=0 total=0
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064 -- expand now: $tmp is function-local.
  trap "rm -rf '$tmp'" EXIT

  # A corpus with one root that links one of two guide pages. The unlinked page
  # is the defect every case below perturbs.
  make_corpus() {
    local dir="$1"
    mkdir -p "$dir/docs/guide"
    git init -q "$dir"
    git -C "$dir" config user.email test@test
    git -C "$dir" config user.name test
    printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n' > "$dir/README.md"
    printf '# Jobs\n\ntext\n' > "$dir/docs/guide/jobs.md"
    printf '# Mail\n\ntext\n' > "$dir/docs/guide/mail.md"
    git -C "$dir" add -A
    git -C "$dir" commit -qm init
  }

  check() {
    local name="$1" expect="$2" dir="$3"
    total=$((total + 1))
    local got=pass
    run_check "$dir" >/dev/null 2>&1 || got=fail
    if [[ "$got" == "$expect" ]]; then
      pass=$((pass + 1))
      echo "  ok: $name"
    else
      echo "  FAILED: $name (expected $expect, got $got)" >&2
    fi
  }

  local c1="$tmp/c1"; make_corpus "$c1"
  check "an unlinked guide page is a defect" fail "$c1"

  local c2="$tmp/c2"; make_corpus "$c2"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n- [Mail](docs/guide/mail.md)\n' > "$c2/README.md"
  git -C "$c2" add -A && git -C "$c2" commit -qm link-both
  check "both linked from the root passes" pass "$c2"

  # The case an inbound-link count gets wrong: mail is linked, but only from a
  # page that is itself unreachable. Reader still cannot get there.
  local c3="$tmp/c3"; make_corpus "$c3"
  printf '# Orphan\n\nSee [mail](mail.md).\n' > "$c3/docs/guide/orphan.md"
  git -C "$c3" add -A && git -C "$c3" commit -qm linked-from-orphan
  check "a page linked only from an unreachable page is still a defect" fail "$c3"

  # Transitivity in the other direction: reachable through a chain of guide pages.
  local c4="$tmp/c4"; make_corpus "$c4"
  printf '# Jobs\n\nSee [mail](mail.md).\n' > "$c4/docs/guide/jobs.md"
  git -C "$c4" add -A && git -C "$c4" commit -qm chain
  check "reachable through another guide page passes" pass "$c4"

  # Relative spellings a guide page actually uses.
  local c5="$tmp/c5"; make_corpus "$c5"
  printf '# Jobs\n\nSee [mail](./mail.md).\n' > "$c5/docs/guide/jobs.md"
  git -C "$c5" add -A && git -C "$c5" commit -qm dot-slash
  check "./sibling.md resolves" pass "$c5"

  local c6="$tmp/c6"; make_corpus "$c6"
  printf '# Jobs\n\nSee [mail](../guide/mail.md).\n' > "$c6/docs/guide/jobs.md"
  git -C "$c6" add -A && git -C "$c6" commit -qm dot-dot
  check "../guide/sibling.md resolves" pass "$c6"

  # A subdirectory page (the tutorial shape) linked from its parent.
  local c7="$tmp/c7"; make_corpus "$c7"
  mkdir -p "$c7/docs/guide/tutorial"
  printf '# Step 1\n\ntext\n' > "$c7/docs/guide/tutorial/01-start.md"
  printf '# Jobs\n\nSee [mail](mail.md) and [start](tutorial/01-start.md).\n' \
    > "$c7/docs/guide/jobs.md"
  git -C "$c7" add -A && git -C "$c7" commit -qm subdir
  check "a subdirectory page linked from its parent passes" pass "$c7"

  # An anchor must not defeat the match.
  local c8="$tmp/c8"; make_corpus "$c8"
  printf '# Jobs\n\nSee [mail](mail.md#retries).\n' > "$c8/docs/guide/jobs.md"
  git -C "$c8" add -A && git -C "$c8" commit -qm anchor
  check "a link carrying an anchor still counts as an edge" pass "$c8"

  # A bare repo path is findable, so it counts.
  local c9="$tmp/c9"; make_corpus "$c9"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nSee docs/guide/mail.md.\n' > "$c9/README.md"
  git -C "$c9" add -A && git -C "$c9" commit -qm bare
  check "a bare repo path counts as an edge" pass "$c9"

  # Reference-style links are a spelling check-docs-links.sh already parses and
  # self-tests, so the gate must read them too — otherwise it reports a
  # perfectly reachable page as an orphan and blocks the docs change.
  local c9a="$tmp/c9a"; make_corpus "$c9a"
  printf '# Jobs\n\nSee [mail][mail].\n\n[mail]: mail.md\n' > "$c9a/docs/guide/jobs.md"
  git -C "$c9a" add -A && git -C "$c9a" commit -qm ref-style
  check "a relative reference-style link counts as an edge" pass "$c9a"

  local c9b="$tmp/c9b"; make_corpus "$c9b"
  printf '# Jobs\n\nSee [mail][mail].\n\n[mail]: <mail.md>\n' > "$c9b/docs/guide/jobs.md"
  git -C "$c9b" add -A && git -C "$c9b" commit -qm ref-style-angle
  check "an angle-wrapped reference definition counts as an edge" pass "$c9b"

  local c9c="$tmp/c9c"; make_corpus "$c9c"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n- [Mail][m]\n\n   [m]: docs/guide/mail.md#retries\n' \
    > "$c9c/README.md"
  git -C "$c9c" add -A && git -C "$c9c" commit -qm ref-style-indented-anchor
  check "an indented reference definition with an anchor counts as an edge" pass "$c9c"

  # An unused definition renders as nothing, so it must NOT confer reachability
  # — otherwise a leftover line launders a genuinely orphaned page past the gate.
  local c9e="$tmp/c9e"; make_corpus "$c9e"
  printf '# Jobs\n\ntext\n\n[old]: mail.md\n' > "$c9e/docs/guide/jobs.md"
  git -C "$c9e" add -A && git -C "$c9e" commit -qm ref-style-unused
  check "an unused reference definition does not make a page reachable" fail "$c9e"

  local c9f="$tmp/c9f"; make_corpus "$c9f"
  printf '# Jobs\n\nSee [mail][].\n\n[mail]: mail.md\n' > "$c9f/docs/guide/jobs.md"
  git -C "$c9f" add -A && git -C "$c9f" commit -qm ref-style-collapsed
  check "a collapsed reference link [label][] counts as an edge" pass "$c9f"

  local c9g="$tmp/c9g"; make_corpus "$c9g"
  printf '# Jobs\n\nSee [mail].\n\n[mail]: mail.md\n' > "$c9g/docs/guide/jobs.md"
  git -C "$c9g" add -A && git -C "$c9g" commit -qm ref-style-shortcut
  check "a shortcut reference link [label] counts as an edge" pass "$c9g"

  # Label matching is case-insensitive and collapses internal whitespace.
  local c9h="$tmp/c9h"; make_corpus "$c9h"
  printf '# Jobs\n\nSee [the  Mail][Mail  Guide].\n\n[mail guide]: mail.md\n' \
    > "$c9h/docs/guide/jobs.md"
  git -C "$c9h" add -A && git -C "$c9h" commit -qm ref-style-label-normalized
  check "reference labels match case- and whitespace-insensitively" pass "$c9h"

  # The definition must still name a real node — a reference to something else
  # does not launder an unreachable page into reachability.
  local c9d="$tmp/c9d"; make_corpus "$c9d"
  printf '# Jobs\n\nSee [other][o].\n\n[o]: https://example.com/mail.md\n' > "$c9d/docs/guide/jobs.md"
  git -C "$c9d" add -A && git -C "$c9d" commit -qm ref-style-external
  check "an external reference definition does not make a page reachable" fail "$c9d"

  # A commented-out link renders as nothing, so it is not a route either.
  local c9i="$tmp/c9i"; make_corpus "$c9i"
  printf '# Jobs\n\n<!-- old: [mail](mail.md) -->\n' > "$c9i/docs/guide/jobs.md"
  git -C "$c9i" add -A && git -C "$c9i" commit -qm html-comment-link
  check "a link inside an HTML comment does not make a page reachable" fail "$c9i"

  local c9j="$tmp/c9j"; make_corpus "$c9j"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n<!-- was: docs/guide/mail.md -->\n' \
    > "$c9j/README.md"
  git -C "$c9j" add -A && git -C "$c9j" commit -qm html-comment-bare
  check "a bare path inside an HTML comment does not make a page reachable" fail "$c9j"

  # ...but the waiver marker IS an HTML comment, so stripping must not eat it.
  local c9k="$tmp/c9k"; make_corpus "$c9k"
  printf '# Mail\n\n<!-- orphan-allow: appendix, linked from the release notes -->\n' \
    > "$c9k/docs/guide/mail.md"
  git -C "$c9k" add -A && git -C "$c9k" commit -qm waiver-survives-strip
  check "the waiver marker still works after comment stripping" pass "$c9k"

  # History is a record, not a route.
  local c10="$tmp/c10"; make_corpus "$c10"
  printf '# Changelog\n\n- added [mail](docs/guide/mail.md)\n' > "$c10/CHANGELOG.md"
  git -C "$c10" add -A && git -C "$c10" commit -qm changelog
  check "a link from CHANGELOG.md does not make a page reachable" fail "$c10"

  local c11="$tmp/c11"; make_corpus "$c11"
  mkdir -p "$c11/docs/releases"
  printf '# 0.7.0\n\n- added [mail](../guide/mail.md)\n' > "$c11/docs/releases/0.7.0.md"
  git -C "$c11" add -A && git -C "$c11" commit -qm release-note
  check "a link from docs/releases/ does not make a page reachable" fail "$c11"

  # A `///` citation in Rust is not something a reader can click.
  local c12="$tmp/c12"; make_corpus "$c12"
  mkdir -p "$c12/src"
  printf '/// see docs/guide/mail.md\npub fn f() {}\n' > "$c12/src/lib.rs"
  git -C "$c12" add -A && git -C "$c12" commit -qm rust-citation
  check "a mention in a .rs doc comment does not make a page reachable" fail "$c12"

  # Skills and agents are real entry surfaces.
  local c13="$tmp/c13"; make_corpus "$c13"
  mkdir -p "$c13/skills/x"
  printf '# Skill\n\nSee [mail](../../docs/guide/mail.md).\n' > "$c13/skills/x/SKILL.md"
  git -C "$c13" add -A && git -C "$c13" commit -qm skill-root
  check "a skill page is a root" pass "$c13"

  # A skill's reference page is a waypoint: it routes readers onward, but only
  # once its SKILL.md links it.
  local c13a="$tmp/c13a"; make_corpus "$c13a"
  mkdir -p "$c13a/skills/x/references"
  printf '# Skill\n\nSee [ref](references/notes.md).\n' > "$c13a/skills/x/SKILL.md"
  printf '# Notes\n\nSee [mail](../../../docs/guide/mail.md).\n' \
    > "$c13a/skills/x/references/notes.md"
  git -C "$c13a" add -A && git -C "$c13a" commit -qm waypoint-linked
  check "a skill reference page linked from SKILL.md routes onward" pass "$c13a"

  local c13b="$tmp/c13b"; make_corpus "$c13b"
  mkdir -p "$c13b/skills/x/references"
  printf '# Skill\n\nnothing linked here\n' > "$c13b/skills/x/SKILL.md"
  printf '# Notes\n\nSee [mail](../../../docs/guide/mail.md).\n' \
    > "$c13b/skills/x/references/notes.md"
  git -C "$c13b" add -A && git -C "$c13b" commit -qm waypoint-unlinked
  check "an unlinked skill reference page does not confer reachability" fail "$c13b"

  # An unused definition written as a repo-root path must not slip past the
  # usage check via the bare-path scan.
  local c9p="$tmp/c9p"; make_corpus "$c9p"
  printf '# Jobs\n\ntext\n\n[old]: docs/guide/mail.md\n' > "$c9p/docs/guide/jobs.md"
  git -C "$c9p" add -A && git -C "$c9p" commit -qm ref-unused-rootpath
  check "an unused repo-root reference definition confers nothing" fail "$c9p"

  # ...while a USED one written the same way still resolves.
  local c9q="$tmp/c9q"; make_corpus "$c9q"
  printf '# Jobs\n\nSee [mail][m].\n\n[m]: docs/guide/mail.md\n' > "$c9q/docs/guide/jobs.md"
  git -C "$c9q" add -A && git -C "$c9q" commit -qm ref-used-rootpath
  check "a used repo-root reference definition resolves" pass "$c9q"

  # A repo-root inline link written from inside the guide must not be
  # base-joined into docs/guide/docs/guide/....
  local c9r="$tmp/c9r"; make_corpus "$c9r"
  printf '# Jobs\n\nSee [mail](docs/guide/mail.md).\n' > "$c9r/docs/guide/jobs.md"
  git -C "$c9r" add -A && git -C "$c9r" commit -qm inline-rootpath
  check "a repo-root inline link from inside the guide resolves" pass "$c9r"

  # Angle-wrapped destinations, including one containing a space.
  local c9s="$tmp/c9s"; make_corpus "$c9s"
  printf '# Jobs\n\nSee [mail](<mail.md>).\n' > "$c9s/docs/guide/jobs.md"
  git -C "$c9s" add -A && git -C "$c9s" commit -qm angle-plain
  check "an angle-wrapped inline destination resolves" pass "$c9s"

  local c9t="$tmp/c9t"; make_corpus "$c9t"
  mv "$c9t/docs/guide/mail.md" "$c9t/docs/guide/mail guide.md"
  printf '# Jobs\n\nSee [mail](<mail guide.md>).\n' > "$c9t/docs/guide/jobs.md"
  git -C "$c9t" add -A && git -C "$c9t" commit -qm angle-spaces
  check "an angle-wrapped destination containing a space resolves" pass "$c9t"

  # An illustrative unclosed `<!--` inside a fence is literal code: it must not
  # comment out the live links after the closing fence.
  local c9u="$tmp/c9u"; make_corpus "$c9u"
  printf '# Jobs\n\n```html\n<!-- sample, not closed\n```\n\nSee [mail](mail.md).\n' \
    > "$c9u/docs/guide/jobs.md"
  git -C "$c9u" add -A && git -C "$c9u" commit -qm unclosed-in-fence
  check "an unclosed comment inside a fence does not hide later links" pass "$c9u"

  # Inline code is literal too: a `<!--` shown as a sample must not comment out
  # the live links after it.
  local c9v="$tmp/c9v"; make_corpus "$c9v"
  printf '# Jobs\n\nThe literal `<!--` marker opens a comment.\n\nSee [mail](mail.md).\n' \
    > "$c9v/docs/guide/jobs.md"
  git -C "$c9v" add -A && git -C "$c9v" commit -qm unclosed-in-code-span
  check "an unclosed comment in inline code does not hide later links" pass "$c9v"

  # Destination grammar shared with check-docs-links.sh: balanced parens, an
  # escaped-paren spelling of the same path, and a link title.
  local c9w="$tmp/c9w"; make_corpus "$c9w"
  mv "$c9w/docs/guide/mail.md" "$c9w/docs/guide/mail(v2).md"
  printf '# Jobs\n\nSee [mail](mail(v2).md).\n' > "$c9w/docs/guide/jobs.md"
  git -C "$c9w" add -A && git -C "$c9w" commit -qm balanced-parens
  check "balanced parentheses in a destination resolve" pass "$c9w"

  local c9x="$tmp/c9x"; make_corpus "$c9x"
  mv "$c9x/docs/guide/mail.md" "$c9x/docs/guide/mail(v2).md"
  printf '# Jobs\n\nSee [mail](mail\\(v2\\).md).\n' > "$c9x/docs/guide/jobs.md"
  git -C "$c9x" add -A && git -C "$c9x" commit -qm escaped-parens
  check "an escaped-parenthesis destination resolves" pass "$c9x"

  local c9y="$tmp/c9y"; make_corpus "$c9y"
  printf '# Jobs\n\nSee [mail](mail.md "the mail guide").\n' > "$c9y/docs/guide/jobs.md"
  git -C "$c9y" add -A && git -C "$c9y" commit -qm link-title
  check "a destination carrying a link title resolves" pass "$c9y"

  # An escaped opening bracket renders literal text, so a link someone
  # deliberately disabled must not go on conferring reachability.
  local c9z="$tmp/c9z"; make_corpus "$c9z"
  printf '# Jobs\n\nDisabled: \\[mail](mail.md)\n' > "$c9z/docs/guide/jobs.md"
  git -C "$c9z" add -A && git -C "$c9z" commit -qm escaped-opener
  check "an escaped link opener does not confer reachability" fail "$c9z"

  # A linked image nests one link inside another; the outer target is the page.
  local c9aa="$tmp/c9aa"; make_corpus "$c9aa"
  printf '# Jobs\n\n[![alt](img.png)](mail.md)\n' > "$c9aa/docs/guide/jobs.md"
  git -C "$c9aa" add -A && git -C "$c9aa" commit -qm nested-image-link
  check "a linked image resolves its outer page target" pass "$c9aa"

  # Percent-encoded destinations address the decoded filename.
  local c9ab="$tmp/c9ab"; make_corpus "$c9ab"
  mv "$c9ab/docs/guide/mail.md" "$c9ab/docs/guide/mail guide.md"
  printf '# Jobs\n\nSee [mail](<mail%%20guide.md>).\n' > "$c9ab/docs/guide/jobs.md"
  git -C "$c9ab" add -A && git -C "$c9ab" commit -qm percent-encoded
  check "a percent-encoded destination resolves" pass "$c9ab"

  # A ```` fence is not closed by a ``` line inside it. This matters for where
  # PROSE begins again: an unclosed `<!--` still inside the outer fence is
  # literal sample text, and mistaking the inner ``` for the closer would treat
  # it as a real comment and blank out the live link that follows the block.
  local c9ac="$tmp/c9ac"; make_corpus "$c9ac"
  printf '# Jobs\n\n````md\n```\n<!-- sample\n```\n````\n\nSee [mail](mail.md).\n' \
    > "$c9ac/docs/guide/jobs.md"
  git -C "$c9ac" add -A && git -C "$c9ac" commit -qm longer-fence
  check "a shorter run inside a longer fence does not end it" pass "$c9ac"

  # An ordinary docs page can sit mid-path: README -> hub -> guide. Dropping
  # that hop would report a reachable guide as an orphan.
  local c9n="$tmp/c9n"; make_corpus "$c9n"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n- [Hub](docs/hub.md)\n' > "$c9n/README.md"
  printf '# Hub\n\nSee [mail](guide/mail.md).\n' > "$c9n/docs/hub.md"
  git -C "$c9n" add -A && git -C "$c9n" commit -qm hub-linked
  check "an ordinary docs hub linked from a root routes onward" pass "$c9n"

  # ...but only once something reaches it. An unlinked hub confers nothing.
  local c9o="$tmp/c9o"; make_corpus "$c9o"
  printf '# Hub\n\nSee [mail](guide/mail.md).\n' > "$c9o/docs/hub.md"
  git -C "$c9o" add -A && git -C "$c9o" commit -qm hub-unlinked
  check "an unlinked docs hub does not confer reachability" fail "$c9o"

  # An unterminated `<!--` comments out the rest of the file for Markdown, so
  # a missing `-->` must not leave the links after it counting as routes.
  local c9l="$tmp/c9l"; make_corpus "$c9l"
  printf '# Jobs\n\n<!-- retired [mail](mail.md)\n' > "$c9l/docs/guide/jobs.md"
  git -C "$c9l" add -A && git -C "$c9l" commit -qm unclosed-comment
  check "an unclosed HTML comment hides the links after it" fail "$c9l"

  # ...but a properly closed comment must still strip only itself.
  local c9m="$tmp/c9m"; make_corpus "$c9m"
  printf '# Jobs\n\n<!-- note -->\n\nSee [mail](mail.md).\n' > "$c9m/docs/guide/jobs.md"
  git -C "$c9m" add -A && git -C "$c9m" commit -qm closed-comment-scoped
  check "a closed comment does not hide links after it" pass "$c9m"

  # A nested example README is a waypoint, not an entry surface.
  local c14a="$tmp/c14a"; make_corpus "$c14a"
  mkdir -p "$c14a/examples/blog/capsules"
  printf '# Blog\n\nnothing linked\n' > "$c14a/examples/blog/README.md"
  printf '# Capsules\n\nSee [mail](../../../docs/guide/mail.md).\n' \
    > "$c14a/examples/blog/capsules/README.md"
  git -C "$c14a" add -A && git -C "$c14a" commit -qm nested-example-unlinked
  check "an unlinked nested example README does not confer reachability" fail "$c14a"

  local c14b="$tmp/c14b"; make_corpus "$c14b"
  mkdir -p "$c14b/examples/blog/capsules"
  printf '# Blog\n\nSee [capsules](capsules/README.md).\n' > "$c14b/examples/blog/README.md"
  printf '# Capsules\n\nSee [mail](../../../docs/guide/mail.md).\n' \
    > "$c14b/examples/blog/capsules/README.md"
  git -C "$c14b" add -A && git -C "$c14b" commit -qm nested-example-linked
  check "a linked nested example README routes onward" pass "$c14b"

  local c14="$tmp/c14"; make_corpus "$c14"
  mkdir -p "$c14/examples/blog"
  printf '# Blog\n\nSee [mail](../../docs/guide/mail.md).\n' > "$c14/examples/blog/README.md"
  git -C "$c14" add -A && git -C "$c14" commit -qm example-root
  check "an examples/*/README.md is a root" pass "$c14"

  # Waivers.
  local c15="$tmp/c15"; make_corpus "$c15"
  printf '# Mail\n\n<!-- orphan-allow: appendix, reached from the 0.7.0 notes -->\n' \
    > "$c15/docs/guide/mail.md"
  git -C "$c15" add -A && git -C "$c15" commit -qm waiver
  check "a waiver with a reason exempts the page" pass "$c15"

  local c16="$tmp/c16"; make_corpus "$c16"
  printf '# Mail\n\n<!-- orphan-allow: -->\n' > "$c16/docs/guide/mail.md"
  git -C "$c16" add -A && git -C "$c16" commit -qm waiver-no-reason
  check "a waiver with no reason does not exempt the page" fail "$c16"

  # A disabled collapsed reference link must not keep its definition alive.
  local c9ad="$tmp/c9ad"; make_corpus "$c9ad"
  printf '# Jobs\n\nDisabled: \\[mail][]\n\n[mail]: mail.md\n' > "$c9ad/docs/guide/jobs.md"
  git -C "$c9ad" add -A && git -C "$c9ad" commit -qm escaped-collapsed-ref
  check "an escaped collapsed reference use confers nothing" fail "$c9ad"

  local c9ae="$tmp/c9ae"; make_corpus "$c9ae"
  printf '# Jobs\n\nDisabled: \\[mail]\n\n[mail]: mail.md\n' > "$c9ae/docs/guide/jobs.md"
  git -C "$c9ae" add -A && git -C "$c9ae" commit -qm escaped-shortcut-ref
  check "an escaped shortcut reference use confers nothing" fail "$c9ae"

  # A page DOCUMENTING the waiver syntax must not thereby exempt itself.
  local c9af="$tmp/c9af"; make_corpus "$c9af"
  printf '# Mail\n\nWrite `<!-- orphan-allow: why -->` to waive a page.\n' \
    > "$c9af/docs/guide/mail.md"
  git -C "$c9af" add -A && git -C "$c9af" commit -qm waiver-in-code-span
  check "a waiver shown in inline code does not exempt the page" fail "$c9af"

  local c9ag="$tmp/c9ag"; make_corpus "$c9ag"
  printf '# Mail\n\n```\n<!-- orphan-allow: example -->\n```\n' > "$c9ag/docs/guide/mail.md"
  git -C "$c9ag" add -A && git -C "$c9ag" commit -qm waiver-in-fence
  check "a waiver shown in a fence does not exempt the page" fail "$c9ag"

  # An even backslash run leaves the bracket UNescaped: `\\[x](y.md)` renders a
  # literal backslash and a live link.
  local c9ah="$tmp/c9ah"; make_corpus "$c9ah"
  printf '# Jobs\n\nLive: \\\\\\\\[mail](mail.md)\n' > "$c9ah/docs/guide/jobs.md"
  git -C "$c9ah" add -A && git -C "$c9ah" commit -qm even-backslash-run
  check "an even backslash run leaves the link live" pass "$c9ah"

  # An image destination is a resource the page loads, never a path on screen.
  local c9ai="$tmp/c9ai"; make_corpus "$c9ai"
  printf '# Jobs\n\n![mail](mail.md)\n' > "$c9ai/docs/guide/jobs.md"
  git -C "$c9ai" add -A && git -C "$c9ai" commit -qm image-destination
  check "an image destination is not a navigation edge" fail "$c9ai"

  # A query string addresses the same file; the sibling strips it too.
  local c9aj="$tmp/c9aj"; make_corpus "$c9aj"
  printf '# Jobs\n\nSee [mail](mail.md?view=all).\n' > "$c9aj/docs/guide/jobs.md"
  git -C "$c9aj" add -A && git -C "$c9aj" commit -qm query-string
  check "a destination carrying a query string resolves" pass "$c9aj"

  local c9ak="$tmp/c9ak"; make_corpus "$c9ak"
  printf '# Jobs\n\nSee [mail](mail.md?view=all#retries).\n' > "$c9ak/docs/guide/jobs.md"
  git -C "$c9ak" add -A && git -C "$c9ak" commit -qm query-and-anchor
  check "a destination carrying a query and an anchor resolves" pass "$c9ak"

  # A reference use in code shows only the label; the path lives in a
  # definition that renders as nothing, so nothing visible names the target.
  local c9al="$tmp/c9al"; make_corpus "$c9al"
  printf '# Jobs\n\nSyntax: `[mail][]`\n\n[mail]: mail.md\n' > "$c9al/docs/guide/jobs.md"
  git -C "$c9al" add -A && git -C "$c9al" commit -qm ref-use-in-code-span
  check "a reference use in inline code confers nothing" fail "$c9al"

  local c9am="$tmp/c9am"; make_corpus "$c9am"
  printf '# Jobs\n\n```\n[mail][]\n```\n\n[mail]: mail.md\n' > "$c9am/docs/guide/jobs.md"
  git -C "$c9am" add -A && git -C "$c9am" commit -qm ref-use-in-fence
  check "a reference use in a fence confers nothing" fail "$c9am"

  # ...but a bare PATH in a fence still counts: the path itself is on screen.
  local c9an="$tmp/c9an"; make_corpus "$c9an"
  printf '# Jobs\n\n```\nsee docs/guide/mail.md\n```\n' > "$c9an/docs/guide/jobs.md"
  git -C "$c9an" add -A && git -C "$c9an" commit -qm bare-path-in-fence
  check "a bare path in a fence still counts as an edge" pass "$c9an"

  # An image reference use is a resource, not navigation.
  local c9ao="$tmp/c9ao"; make_corpus "$c9ao"
  printf '# Jobs\n\n![mail][]\n\n[mail]: mail.md\n' > "$c9ao/docs/guide/jobs.md"
  git -C "$c9ao" add -A && git -C "$c9ao" commit -qm image-ref-use
  check "an image reference use confers nothing" fail "$c9ao"

  # A backtick info string may not contain a backtick, so this opens no fence
  # and the link after it is ordinary live prose.
  local c9ap="$tmp/c9ap"; make_corpus "$c9ap"
  printf '# Jobs\n\n```md`invalid\n\nSee [mail](mail.md).\n' > "$c9ap/docs/guide/jobs.md"
  git -C "$c9ap" add -A && git -C "$c9ap" commit -qm invalid-info-string
  check "an invalid backtick info string opens no fence" pass "$c9ap"

  # A closing fence may be followed only by whitespace, so a waiver between a
  # look-alike line and the real closer is still inside the fence.
  local c9aq="$tmp/c9aq"; make_corpus "$c9aq"
  printf '# Mail\n\n```\n```not-a-closer\n<!-- orphan-allow: example -->\n```\n' \
    > "$c9aq/docs/guide/mail.md"
  git -C "$c9aq" add -A && git -C "$c9aq" commit -qm trailing-text-closer
  check "a closing fence with trailing text does not end the block" fail "$c9aq"

  # A reference label may contain an escaped bracket.
  local c9ar="$tmp/c9ar"; make_corpus "$c9ar"
  printf '# Jobs\n\nSee [mail][closing \\]].\n\n[closing \\]]: mail.md\n' \
    > "$c9ar/docs/guide/jobs.md"
  git -C "$c9ar" add -A && git -C "$c9ar" commit -qm escaped-bracket-label
  check "an escaped bracket in a reference label resolves" pass "$c9ar"

  # `![alt][mail]` is an image; its trailing label must not read as a shortcut.
  local c9as="$tmp/c9as"; make_corpus "$c9as"
  printf '# Jobs\n\n![alt][mail]\n\n[mail]: mail.md\n' > "$c9as/docs/guide/jobs.md"
  git -C "$c9as" add -A && git -C "$c9as" commit -qm image-full-ref
  check "an explicit image reference confers nothing" fail "$c9as"

  # The bare-path scan must honour the image guard too.
  local c9at="$tmp/c9at"; make_corpus "$c9at"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n![diagram](docs/guide/mail.md)\n' \
    > "$c9at/README.md"
  git -C "$c9at" add -A && git -C "$c9at" commit -qm image-rootpath
  check "a repo-root image destination is not a bare-path edge" fail "$c9at"

  # A duplicated label resolves to its FIRST definition.
  local c9au="$tmp/c9au"; make_corpus "$c9au"
  printf '# Jobs\n\nSee [mail].\n\n[mail]: https://example.com\n[mail]: mail.md\n' \
    > "$c9au/docs/guide/jobs.md"
  git -C "$c9au" add -A && git -C "$c9au" commit -qm duplicate-definition
  check "a shadowed duplicate definition confers nothing" fail "$c9au"

  # ...and the first definition still resolves when it is the real one.
  local c9av="$tmp/c9av"; make_corpus "$c9av"
  printf '# Jobs\n\nSee [mail].\n\n[mail]: mail.md\n[mail]: https://example.com\n' \
    > "$c9av/docs/guide/jobs.md"
  git -C "$c9av" add -A && git -C "$c9av" commit -qm first-definition-wins
  check "the first of duplicate definitions resolves" pass "$c9av"

  # An escaped `!` does not open an image, so the bracket is a real link.
  local c9aw="$tmp/c9aw"; make_corpus "$c9aw"
  printf '# Jobs\n\nLiteral bang: \\![mail](mail.md)\n' > "$c9aw/docs/guide/jobs.md"
  git -C "$c9aw" add -A && git -C "$c9aw" commit -qm escaped-image-marker
  check "an escaped image marker leaves a live link" pass "$c9aw"

  # ...and an unescaped one still does not confer reachability.
  local c9ax="$tmp/c9ax"; make_corpus "$c9ax"
  printf '# Jobs\n\n![mail](mail.md)\n' > "$c9ax/docs/guide/jobs.md"
  git -C "$c9ax" add -A && git -C "$c9ax" commit -qm unescaped-image-still-excluded
  check "an unescaped image marker still excludes the destination" fail "$c9ax"

  # Links cannot nest, so the inner label of an inline link is ordinary text,
  # not a shortcut reference that keeps a definition alive.
  local c9ay="$tmp/c9ay"; make_corpus "$c9ay"
  printf '# Jobs\n\n[outer [mail]](https://example.com)\n\n[mail]: mail.md\n' \
    > "$c9ay/docs/guide/jobs.md"
  git -C "$c9ay" add -A && git -C "$c9ay" commit -qm inner-label-not-shortcut
  check "an inline link's inner label is not a shortcut use" fail "$c9ay"

  # Image alt text may contain a bracketed span; the image must still be masked.
  local c9az="$tmp/c9az"; make_corpus "$c9az"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n![nested [alt]](docs/guide/mail.md)\n' \
    > "$c9az/README.md"
  git -C "$c9az" add -A && git -C "$c9az" commit -qm nested-alt-image
  check "an image with nested alt text is still not an edge" fail "$c9az"

  # A path in a resource attribute or a non-rendered raw-HTML block is not on
  # screen, so it is not an edge.
  local c9ba="$tmp/c9ba"; make_corpus "$c9ba"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<img src="docs/guide/mail.md">\n' \
    > "$c9ba/README.md"
  git -C "$c9ba" add -A && git -C "$c9ba" commit -qm html-src-attr
  check "a raw-HTML src attribute is not an edge" fail "$c9ba"

  local c9bb="$tmp/c9bb"; make_corpus "$c9bb"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<script>var p = "docs/guide/mail.md";</script>\n' \
    > "$c9bb/README.md"
  git -C "$c9bb" add -A && git -C "$c9bb" commit -qm html-script-block
  check "a path inside a script block is not an edge" fail "$c9bb"

  # ...but a raw anchor IS navigation.
  local c9bc="$tmp/c9bc"; make_corpus "$c9bc"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<a href="docs/guide/mail.md">Mail</a>\n' \
    > "$c9bc/README.md"
  git -C "$c9bc" add -A && git -C "$c9bc" commit -qm html-anchor
  check "a raw HTML anchor still confers reachability" pass "$c9bc"

  # ...and a script shown INSIDE a fence is visible sample text.
  local c9bd="$tmp/c9bd"; make_corpus "$c9bd"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n```html\n<script src="docs/guide/mail.md"></script>\n```\n' \
    > "$c9bd/README.md"
  git -C "$c9bd" add -A && git -C "$c9bd" commit -qm html-in-fence
  check "raw HTML shown inside a fence is still visible text" pass "$c9bd"

  # A waiver hidden in a script block must not exempt the page.
  local c9be="$tmp/c9be"; make_corpus "$c9be"
  printf '# Mail\n\n<script>const s = "<!-- orphan-allow: example -->";</script>\n' \
    > "$c9be/docs/guide/mail.md"
  git -C "$c9be" add -A && git -C "$c9be" commit -qm waiver-in-script
  check "a waiver inside a script block does not exempt the page" fail "$c9be"

  # An HTML sample in inline code is visible text, not raw HTML to mask.
  local c9bf="$tmp/c9bf"; make_corpus "$c9bf"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nWrite `<script src="docs/guide/mail.md"></script>` to load it.\n' \
    > "$c9bf/README.md"
  git -C "$c9bf" add -A && git -C "$c9bf" commit -qm html-sample-in-code-span
  check "an HTML sample in inline code stays visible" pass "$c9bf"

  # Raw anchors resolve like any other destination, relative spellings included.
  local c9bg="$tmp/c9bg"; make_corpus "$c9bg"
  printf '# Jobs\n\n<a href="mail.md">Mail</a>\n' > "$c9bg/docs/guide/jobs.md"
  git -C "$c9bg" add -A && git -C "$c9bg" commit -qm anchor-relative
  check "a raw anchor with a relative href resolves" pass "$c9bg"

  local c9bh="$tmp/c9bh"; make_corpus "$c9bh"
  printf '# Jobs\n\n<a href="../guide/mail.md">Mail</a>\n' > "$c9bh/docs/guide/jobs.md"
  git -C "$c9bh" add -A && git -C "$c9bh" commit -qm anchor-dotdot
  check "a raw anchor with a ../ href resolves" pass "$c9bh"

  # A markdown link hidden in a script block exposes no navigation.
  local c9bi="$tmp/c9bi"; make_corpus "$c9bi"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<script>const x = "[mail](docs/guide/mail.md)";</script>\n' \
    > "$c9bi/README.md"
  git -C "$c9bi" add -A && git -C "$c9bi" commit -qm md-link-in-script
  check "a markdown link inside a script block is not an edge" fail "$c9bi"

  # A reference definition demonstrated inside a fence defines nothing.
  local c9bj="$tmp/c9bj"; make_corpus "$c9bj"
  printf '# Jobs\n\nSee [mail].\n\n```\n[mail]: mail.md\n```\n' > "$c9bj/docs/guide/jobs.md"
  git -C "$c9bj" add -A && git -C "$c9bj" commit -qm fenced-refdef
  check "a reference definition inside a fence defines nothing" fail "$c9bj"

  # A label inside a raw-HTML attribute is not a reference use.
  local c9bk="$tmp/c9bk"; make_corpus "$c9bk"
  printf '# Jobs\n\n<span data-note="[mail]">text</span>\n\n[mail]: mail.md\n' \
    > "$c9bk/docs/guide/jobs.md"
  git -C "$c9bk" add -A && git -C "$c9bk" commit -qm label-in-attribute
  check "a label inside an HTML attribute is not a reference use" fail "$c9bk"

  # ...and a real shortcut use beside raw HTML still resolves.
  local c9bl="$tmp/c9bl"; make_corpus "$c9bl"
  printf '# Jobs\n\n<span>text</span>\n\nSee [mail].\n\n[mail]: mail.md\n' \
    > "$c9bl/docs/guide/jobs.md"
  git -C "$c9bl" add -A && git -C "$c9bl" commit -qm shortcut-beside-html
  check "a shortcut use beside raw HTML still resolves" pass "$c9bl"

  # Prose comparisons are not tags.
  local c9bm="$tmp/c9bm"; make_corpus "$c9bm"
  printf '# Jobs\n\nWhen a < b, see [mail].\n\n[mail]: mail.md\n' \
    > "$c9bm/docs/guide/jobs.md"
  git -C "$c9bm" add -A && git -C "$c9bm" commit -qm less-than-in-prose
  check "a less-than in prose does not eat the reference use" pass "$c9bm"

  # An image SAMPLE in code is visible text; only a rendered image is masked.
  local c9bn="$tmp/c9bn"; make_corpus "$c9bn"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nWrite `![Mail](docs/guide/mail.md)` for an image.\n' \
    > "$c9bn/README.md"
  git -C "$c9bn" add -A && git -C "$c9bn" commit -qm image-sample-in-code-span
  check "an image sample in inline code stays visible" pass "$c9bn"

  local c9bo="$tmp/c9bo"; make_corpus "$c9bo"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n```md\n![Mail](docs/guide/mail.md)\n```\n' \
    > "$c9bo/README.md"
  git -C "$c9bo" add -A && git -C "$c9bo" commit -qm image-sample-in-fence
  check "an image sample in a fence stays visible" pass "$c9bo"

  # A `src` query parameter is not an HTML attribute.
  local c9bp="$tmp/c9bp"; make_corpus "$c9bp"
  printf '# Jobs\n\nSee [mail](mail.md?src=guide).\n' > "$c9bp/docs/guide/jobs.md"
  git -C "$c9bp" add -A && git -C "$c9bp" commit -qm src-query-param
  check "a src query parameter does not break the link" pass "$c9bp"

  # A comment-shaped string inside a closed script is script data, not a
  # Markdown comment, so links after the script stay live.
  local c9bq="$tmp/c9bq"; make_corpus "$c9bq"
  printf '# Jobs\n\n<script>const m = "<!--";</script>\n\nSee [mail](mail.md).\n' \
    > "$c9bq/docs/guide/jobs.md"
  git -C "$c9bq" add -A && git -C "$c9bq" commit -qm comment-marker-in-script
  check "a comment marker inside a script does not truncate the page" pass "$c9bq"

  # An unclosed raw-HTML opener makes the rest of the file raw HTML.
  local c9br="$tmp/c9br"; make_corpus "$c9br"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<script>\nconst x = "[mail](docs/guide/mail.md)";\n' \
    > "$c9br/README.md"
  git -C "$c9br" add -A && git -C "$c9br" commit -qm unclosed-script
  check "an unclosed script block hides the links after it" fail "$c9br"

  # An inline link parked in an attribute value is not navigation.
  local c9bs="$tmp/c9bs"; make_corpus "$c9bs"
  printf '# Jobs\n\n<span data-note="[mail](mail.md)">text</span>\n' \
    > "$c9bs/docs/guide/jobs.md"
  git -C "$c9bs" add -A && git -C "$c9bs" commit -qm link-in-attribute
  check "an inline link inside an HTML attribute is not an edge" fail "$c9bs"

  # Nor is a waiver parked in one.
  local c9bt="$tmp/c9bt"; make_corpus "$c9bt"
  printf '# Mail\n\n<span data-note="<!-- orphan-allow: fake -->">text</span>\n' \
    > "$c9bt/docs/guide/mail.md"
  git -C "$c9bt" add -A && git -C "$c9bt" commit -qm waiver-in-attribute
  check "a waiver inside an HTML attribute does not exempt the page" fail "$c9bt"

  # An unused definition's TITLE is part of the definition and renders nothing.
  local c9bu="$tmp/c9bu"; make_corpus "$c9bu"
  printf '# Jobs\n\ntext\n\n[old]: https://example.com "docs/guide/mail.md"\n' \
    > "$c9bu/docs/guide/jobs.md"
  git -C "$c9bu" add -A && git -C "$c9bu" commit -qm refdef-title
  check "a path in an unused definition's title confers nothing" fail "$c9bu"

  # A definition DEMONSTRATED in a fence still shows its path on screen.
  local c9bv="$tmp/c9bv"; make_corpus "$c9bv"
  printf '# Jobs\n\n```\n[mail]: docs/guide/mail.md\n```\n' > "$c9bv/docs/guide/jobs.md"
  git -C "$c9bv" add -A && git -C "$c9bv" commit -qm fenced-refdef-visible
  check "a definition shown in a fence keeps its visible path" pass "$c9bv"

  # `href` is navigation only on an anchor.
  local c9bw="$tmp/c9bw"; make_corpus "$c9bw"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<link rel="alternate" href="docs/guide/mail.md">\n' \
    > "$c9bw/README.md"
  git -C "$c9bw" add -A && git -C "$c9bw" commit -qm link-element-href
  check "an href on a non-anchor element is not an edge" fail "$c9bw"

  # A quoted `>` does not end a tag, so later attributes stay inside the mask.
  local c9bx="$tmp/c9bx"; make_corpus "$c9bx"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<span title="1 > 0" data-note="docs/guide/mail.md">x</span>\n' \
    > "$c9bx/README.md"
  git -C "$c9bx" add -A && git -C "$c9bx" commit -qm quoted-gt-in-tag
  check "a quoted > does not end the attribute mask" fail "$c9bx"

  # Inside a raw HTML block, Markdown stays literal.
  local c9by="$tmp/c9by"; make_corpus "$c9by"
  printf '# Jobs\n\n<div>\n[mail]\n</div>\n\n[mail]: mail.md\n' > "$c9by/docs/guide/jobs.md"
  git -C "$c9by" add -A && git -C "$c9by" commit -qm raw-block-reference
  check "a reference inside a raw HTML block is not a use" fail "$c9by"

  local c9bz="$tmp/c9bz"; make_corpus "$c9bz"
  printf '# Jobs\n\n<div>\n[mail](mail.md)\n</div>\n' > "$c9bz/docs/guide/jobs.md"
  git -C "$c9bz" add -A && git -C "$c9bz" commit -qm raw-block-link
  check "a markdown link inside a raw HTML block is not an edge" fail "$c9bz"

  # ...but a visible bare path inside one still counts.
  local c9ca="$tmp/c9ca"; make_corpus "$c9ca"
  printf '# Jobs\n\n<div>\nSee docs/guide/mail.md\n</div>\n' > "$c9ca/docs/guide/jobs.md"
  git -C "$c9ca" add -A && git -C "$c9ca" commit -qm raw-block-bare-path
  check "a bare path inside a raw HTML block still counts" pass "$c9ca"

  # A type-6 block opens even with trailing content and runs to a blank line.
  local c9cb="$tmp/c9cb"; make_corpus "$c9cb"
  printf '# Jobs\n\n<div>example\n[mail](mail.md)\n' > "$c9cb/docs/guide/jobs.md"
  git -C "$c9cb" add -A && git -C "$c9cb" commit -qm raw-block-trailing-opener
  check "a raw block opener with trailing content still opens one" fail "$c9cb"

  # ...and the block ends at the blank line, so later links are live again.
  local c9cc="$tmp/c9cc"; make_corpus "$c9cc"
  printf '# Jobs\n\n<div>example\ntext\n\nSee [mail](mail.md).\n' > "$c9cc/docs/guide/jobs.md"
  git -C "$c9cc" add -A && git -C "$c9cc" commit -qm raw-block-ends-at-blank
  check "a link after the blank line ending a raw block is live" pass "$c9cc"

  # An escaped `<` shows a literal tag and opens no raw HTML.
  local c9cd="$tmp/c9cd"; make_corpus "$c9cd"
  printf '# Jobs\n\nShow \\<script\\> around [mail](mail.md) \\</script\\>.\n' \
    > "$c9cd/docs/guide/jobs.md"
  git -C "$c9cd" add -A && git -C "$c9cd" commit -qm escaped-tag-opener
  check "an escaped tag opener leaves the link live" pass "$c9cd"

  # A label in a definition's title is not a use of it.
  local c9ce="$tmp/c9ce"; make_corpus "$c9ce"
  printf '# Jobs\n\n[old]: https://example.com "[mail]"\n\n[mail]: mail.md\n' \
    > "$c9ce/docs/guide/jobs.md"
  git -C "$c9ce" add -A && git -C "$c9ce" commit -qm label-in-refdef-title
  check "a label inside a definition title is not a use" fail "$c9ce"

  # An external link's query string is not a visible path.
  local c9cf="$tmp/c9cf"; make_corpus "$c9cf"
  printf '# Jobs\n\nSee [search](https://example.test/?q=docs/guide/mail.md).\n' \
    > "$c9cf/docs/guide/jobs.md"
  git -C "$c9cf" add -A && git -C "$c9cf" commit -qm external-query-path
  check "a path inside an external link URL is not an edge" fail "$c9cf"

  # ...but the same text in a fence shows the path.
  local c9cg="$tmp/c9cg"; make_corpus "$c9cg"
  printf '# Jobs\n\n```\n[search](https://example.test/?q=docs/guide/mail.md)\n```\n' \
    > "$c9cg/docs/guide/jobs.md"
  git -C "$c9cg" add -A && git -C "$c9cg" commit -qm external-query-in-fence
  check "the same URL shown in a fence keeps its visible path" pass "$c9cg"

  # A destination may wrap one newline but never a blank line.
  local c9ch="$tmp/c9ch"; make_corpus "$c9ch"
  printf '# Jobs\n\nSee [mail](mail.md\n\n).\n' > "$c9ch/docs/guide/jobs.md"
  git -C "$c9ch" add -A && git -C "$c9ch" commit -qm dest-across-blank-line
  check "a destination split by a blank line is not a link" fail "$c9ch"

  # An inline tag does not open a raw block, so the next line stays Markdown.
  local c9ci="$tmp/c9ci"; make_corpus "$c9ci"
  printf '# Jobs\n\n<span>text\n[mail](mail.md)\n' > "$c9ci/docs/guide/jobs.md"
  git -C "$c9ci" add -A && git -C "$c9ci" commit -qm inline-tag-not-block
  check "an inline tag with trailing text opens no raw block" pass "$c9ci"

  # A character reference in a destination decodes to the real path.
  local c9cj="$tmp/c9cj"; make_corpus "$c9cj"
  printf '# Jobs\n\nSee [mail](mail&#46;md).\n' > "$c9cj/docs/guide/jobs.md"
  git -C "$c9cj" add -A && git -C "$c9cj" commit -qm decimal-char-ref
  check "a decimal character reference in a destination resolves" pass "$c9cj"

  local c9ck="$tmp/c9ck"; make_corpus "$c9ck"
  printf '# Jobs\n\nSee [mail](mail&#x2e;md).\n' > "$c9ck/docs/guide/jobs.md"
  git -C "$c9ck" add -A && git -C "$c9ck" commit -qm hex-char-ref
  check "a hex character reference in a destination resolves" pass "$c9ck"

  # A definition cannot interrupt a paragraph.
  local c9cl="$tmp/c9cl"; make_corpus "$c9cl"
  printf '# Jobs\n\nSee [mail][upgrade].\nmore prose here.\n[upgrade]: mail.md\n' \
    > "$c9cl/docs/guide/jobs.md"
  git -C "$c9cl" add -A && git -C "$c9cl" commit -qm refdef-continues-paragraph
  check "a definition continuing a paragraph defines nothing" fail "$c9cl"

  # ...but one after a blank line is a real definition.
  local c9cm="$tmp/c9cm"; make_corpus "$c9cm"
  printf '# Jobs\n\nSee [mail][upgrade].\n\n[upgrade]: mail.md\n' \
    > "$c9cm/docs/guide/jobs.md"
  git -C "$c9cm" add -A && git -C "$c9cm" commit -qm refdef-after-blank
  check "a definition after a blank line resolves" pass "$c9cm"

  # CommonMark raw-HTML block types 3, 4 and 5 are literal too.
  local c9cn="$tmp/c9cn"; make_corpus "$c9cn"
  printf '# Jobs\n\n<![CDATA[\n[mail](mail.md)\n]]>\n' > "$c9cn/docs/guide/jobs.md"
  git -C "$c9cn" add -A && git -C "$c9cn" commit -qm cdata-block
  check "a link inside a CDATA block is not an edge" fail "$c9cn"

  local c9co="$tmp/c9co"; make_corpus "$c9co"
  printf '# Jobs\n\n<?php\n[mail](mail.md)\n?>\n' > "$c9co/docs/guide/jobs.md"
  git -C "$c9co" add -A && git -C "$c9co" commit -qm processing-instruction
  check "a link inside a processing instruction is not an edge" fail "$c9co"

  # A declaration runs to the next `>`, which may be lines later.
  local c9cp="$tmp/c9cp"; make_corpus "$c9cp"
  printf '# Jobs\n\n<!DOCTYPE demo\n[mail](mail.md)\n>\n' > "$c9cp/docs/guide/jobs.md"
  git -C "$c9cp" add -A && git -C "$c9cp" commit -qm declaration-block
  check "a link inside a declaration block is not an edge" fail "$c9cp"

  # ...and it CLOSES on the line carrying that `>`, including the opener, so a
  # self-contained `<!DOCTYPE html>` must not swallow the next line's link.
  local c9cq="$tmp/c9cq"; make_corpus "$c9cq"
  printf '# Jobs\n\n<!DOCTYPE html>\n\nSee [mail](mail.md).\n' > "$c9cq/docs/guide/jobs.md"
  git -C "$c9cq" add -A && git -C "$c9cq" commit -qm declaration-closes
  check "a closed declaration does not swallow the next link" pass "$c9cq"

  # A definition may follow a completed block with no blank line between.
  local c9cr="$tmp/c9cr"; make_corpus "$c9cr"
  printf '# Jobs\n\nSee [mail][m].\n\n## Links\n[m]: mail.md\n' > "$c9cr/docs/guide/jobs.md"
  git -C "$c9cr" add -A && git -C "$c9cr" commit -qm refdef-after-heading
  check "a definition right after a heading resolves" pass "$c9cr"

  local c9cs="$tmp/c9cs"; make_corpus "$c9cs"
  printf '# Jobs\n\nSee [mail][m].\n\n---\n[m]: mail.md\n' > "$c9cs/docs/guide/jobs.md"
  git -C "$c9cs" add -A && git -C "$c9cs" commit -qm refdef-after-break
  check "a definition right after a thematic break resolves" pass "$c9cs"

  # A type-7 tag cannot interrupt a paragraph, so the link stays live.
  local c9ct="$tmp/c9ct"; make_corpus "$c9ct"
  printf '# Jobs\n\nordinary prose\n<span>\n[mail](mail.md)\n' > "$c9ct/docs/guide/jobs.md"
  git -C "$c9ct" add -A && git -C "$c9ct" commit -qm type7-inline
  check "a type-7 tag inside a paragraph leaves the link live" pass "$c9ct"

  # ...but one starting a block does open a raw block.
  local c9cu="$tmp/c9cu"; make_corpus "$c9cu"
  printf '# Jobs\n\n<span>\n[mail](mail.md)\n' > "$c9cu/docs/guide/jobs.md"
  git -C "$c9cu" add -A && git -C "$c9cu" commit -qm type7-block
  check "a type-7 tag at a block start opens a raw block" fail "$c9cu"

  # A type-6 tag MAY interrupt a paragraph.
  local c9cv="$tmp/c9cv"; make_corpus "$c9cv"
  printf '# Jobs\n\nordinary prose\n<div>\n[mail](mail.md)\n' > "$c9cv/docs/guide/jobs.md"
  git -C "$c9cv" add -A && git -C "$c9cv" commit -qm type6-interrupts
  check "a type-6 tag interrupts a paragraph" fail "$c9cv"

  # An untracked file is not part of the corpus and cannot carry an edge.
  local c17="$tmp/c17"; make_corpus "$c17"
  printf '# Scratch\n\n[mail](docs/guide/mail.md)\n' > "$c17/NOTES.md"
  check "an untracked file does not make a page reachable" fail "$c17"

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]]
}

case "${1-}" in
  --self-test)
    self_test
    ;;
  *)
    echo "Checking that every guide page is reachable from a reader entry point..."
    if run_check "$root"; then
      echo "Guide reachability gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the guide pages listed above cannot be reached by clicking.

A reader who cannot reach a page concludes the feature does not exist — there is
no 404 and no error to tell them otherwise. Fix it where the reader looks:

  - index it in README.md's `## Documentation` list, in the words a reader
    would search for rather than the internal feature name
  - or link it from the guide page whose reader has the question it answers
  - or, if it is deliberately unlinked, mark it in the page with a reason:
      <!-- orphan-allow: why this page has no inbound link -->

Do NOT fix this by writing a new page: the answer would then exist twice, both
copies would drift, and the search rank that would have found either is split.
EOF
      exit 1
    fi
    ;;
esac
