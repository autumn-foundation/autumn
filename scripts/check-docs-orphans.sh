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
#       `docs/plugins.md`, each skill's `SKILL.md` (under `skills/` and under
#       `.claude/skills/`, both of which the agent machinery loads by name),
#       each top-level `agents/*.md`, and each `examples/*/README.md`.
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
import html, os, posixpath, re, subprocess, sys, urllib.parse

root = sys.argv[1]

GUIDE = 'docs/guide/'
ROOT_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md',
              'docs/plugins.md')
# INSTRUCTION FILES are entry surfaces wherever they sit. Agent tooling loads
# them by NAME on entering a directory, so no link leads to one — which is
# exactly what makes it a root rather than a waypoint. As a waypoint an
# instruction file is inert, and a guide indexed only from it was an orphan.
#
# By basename at any depth, not as two root-only entries: both conventions are
# per-DIRECTORY — tooling reads the one beside the code it is working on — so a
# nested `subproject/CLAUDE.md` is loaded exactly as the top-level one is. This
# repository has only the two at the root, so the rule changes nothing about
# the current graph and everything about the next file someone adds. Listing
# `AGENTS.md` alone and leaving `CLAUDE.md` out was the same omit-one-of-a-
# family mistake this file keeps making, caught one round later.
INSTRUCTION_FILES = ('AGENTS.md', 'CLAUDE.md')
# `.claude/skills/` too: the agent machinery loads a `SKILL.md` there by name
# exactly as it does one under `skills/`, so it is a surface an agent ENTERS
# through, not one it is routed to. It was being treated as a waypoint, which
# is inert — nothing links `.claude/`, so a guide indexed only from there would
# have been reported as an orphan. `.claude/agents/` is listed for the same
# reason though the repo has none today, because the omission of one member of
# a family is how most of this file's bugs started.
# The layouts themselves live in `_is_agent_entry`, which is where the two
# directories' differing conventions are applied. A flat tuple of prefixes
# stood here and was crossed with a tuple of basenames, which got both
# directions wrong at once — see that function.
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


def _is_agent_entry(f):
    """An entry file under `skills/` or `agents/`, in either location.

    THE TWO DIRECTORIES HAVE DIFFERENT CONVENTIONS, and crossing them was
    wrong in both directions at once:

      skills/<name>/SKILL.md      the skill's entry file, loaded by name
      agents/<name>.md            an agent definition, one file per agent

    Taking the cross-product of every prefix with both basenames seeded
    `skills/x/references/AGENT.md` — a supporting page, which as a root would
    let a file nothing links any more still confer reachability, an orphan
    passing — while leaving an ordinary `.claude/agents/reviewer.md` out
    entirely, which reports a guide indexed only from there as an orphan. One
    of those is a false negative and the other a false positive, from the same
    two lines.

    Depth is what distinguishes an entry file from a supporting one, so it is
    checked rather than the basename alone: `references/` sits one level
    deeper and no longer matches.

    `AGENT.md` is NOT an entry filename here, and neither is a nested
    `agents/<name>/AGENT.md`. Both were carried over from the cross-product
    this replaced, on the reasoning that keeping them was the conservative
    choice — which had it backwards. The repository tracks no such file and
    no convention here documents one, so they only ever added a way for an
    ordinary supporting page to confer reachability because of its basename,
    which is an orphan passing. Seeding a root on a guessed convention is not
    the safe direction; it is the one that hides defects.
    """
    if not f.endswith('.md'):
        return False
    for prefix in ('', '.claude/'):
        if not f.startswith(prefix):
            continue
        parts = f[len(prefix):].split('/')
        if parts[0] == 'skills':
            # `skills/<name>/SKILL.md` — the entry file, never a page beside it.
            if len(parts) == 3 and parts[2] == 'SKILL.md':
                return True
        elif parts[0] == 'agents':
            # `agents/<name>.md` — one file per agent, and no deeper.
            if len(parts) == 2:
                return True
    return False


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
            or posixpath.basename(f) in INSTRUCTION_FILES
            or _is_agent_entry(f)
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
# ...and parentheses NEST. `https://example.test/a(b(c))?q=…` is a valid
# destination; one level missed it, the link was never recognised, its
# destination was left unblanked, and the bare-path scan read a repo path out
# of the query string — an invisible non-route marking a page reachable.
# Built to a fixed depth because a regular expression cannot count, and built
# to THE SAME depth cmark uses — 32 — so the two agree about every destination
# either can parse. A shallower cap was the obvious shortcut and the wrong one:
# it left this file naming 32 in a comment while implementing six, and a
# destination between the two would have gone unrecognised and unblanked.
# Depth N accepts nesting N exactly, measured. At 32 the pattern is 684
# characters and the corpus run is unchanged.
#
# The two alternatives are disjoint on their first character — `(` or not — so
# there is no ambiguity for the engine to backtrack through, and the nesting
# costs nothing on input that does not balance (measured on unbalanced opens,
# unbalanced closes, and a deep run that fails at the end). An earlier version
# wrapped each level in an ATOMIC group to guard against a blowup that cannot
# happen, and `(?>...)` is Python 3.11 syntax: on 3.10 the gate aborted while
# compiling this pattern. A defence against a measured non-problem is not worth
# a runtime requirement the script never declares.
def _nested_parens(depth):
    body = r'[^()\s]'
    for _ in range(depth):
        body = r'(?:[^()\s]|\((?:' + body + r')*\))'
    return body


DEST_BARE = r'(?:' + _nested_parens(32) + r')+'
# A title may wrap lines but not contain a BLANK one, and the whitespace before
# it is bounded the same way as the destination's: a blank line ends the
# paragraph, so `[x](y.md\n\n "t")` renders no link at all. Bounding `_WS` and
# leaving this grammar open was the same half-a-rule mistake in one regex.
_TBODY = r'(?:[^{q}\\\n]|\\.|\n(?![ \t]*\n))*'
# The separator is REQUIRED: CommonMark says a title must be separated from
# the destination by whitespace, so `[Mail](<mail.md>"title")` renders no
# link at all. Allowing zero characters there accepted it and recorded the
# destination of something the reader cannot click. One space or tab, or a
# single line ending — never a blank line, which ends the paragraph.
TITLE = (r'(?:' + r'(?:[ \t]+|[ \t]*\n[ \t]*)' +
         r'(?:"' + _TBODY.format(q='"') + r'"'
         r"|'" + _TBODY.format(q="'") + r"'"
         r'|\(' + _TBODY.format(q='()') + r'\)))?')
# `[\s--\n]` style bounds: whitespace inside a destination may span ONE newline
# but never a blank line — a blank line ends the paragraph, so `[x](y.md\n\n)`
# renders no link at all and must not record an edge.
_WS = r'(?:[ \t]*\n?[ \t]*)'
# The angle form admits NO line ending at all — CommonMark allows the
# whitespace AROUND a destination to span one newline, but not the destination
# itself. `[Mail](<mail.md\n>)` renders no link, and `[^<>]*` matched it, then
# `add_relative` stripped the newline and recorded the target: a malformed link
# concealing an orphan. This is the same bound `_WS` and the title body already
# carry, applied to the third place a newline could sneak through.
ANGLE_DEST = r'<([^<>\r\n]*)>'
DEST = r'\(' + _WS + r'(?:' + ANGLE_DEST + r'|(' + DEST_BARE + r'))' + TITLE + _WS + r'\)'
# The label has to be matched too, not just the `](…)` tail: `\[Mail](mail.md)`
# renders as literal text, so treating it as a route would let a link someone
# deliberately disabled go on hiding an orphan. `\.` keeps an escaped bracket
# inside the label. The nesting form exists because a linked image puts one
# link inside another — `[![alt](img.png)](page.md)` — where the flat pattern
# finds only the image.
# A label may wrap across a line but not across a BLANK one: the blank ends the
# paragraph, and `[Mail` / blank / `](mail.md)` renders no link at all.
# `[^\[\]\\]` admitted the newline like any other character, so that non-link
# recorded an edge and could conceal an orphan. The destination and the title
# already carried this bound; the label is the third side of the same link.
FLAT = r'(?:[^\[\]\\\n]|\\.|\n(?![ \t]*\n))'
# CommonMark caps a reference label at 999 characters, so a longer one creates
# no definition and renders as literal text. `FLAT+` was unbounded, and an
# over-long label could mark its target reachable through a definition that
# does not exist. The cap counts REPETITIONS rather than characters, so an
# escaped character is counted once — the only direction that over-accepts, and
# it takes a label of 999 escapes to reach. The repo pins the rule
# (`migration_guide_gate_rejects_an_overlong_reference_label`).
LABEL_LIMIT = 999
LABEL_MAX = '{1,999}'
# The collapsed form `[label][]` has an EMPTY second label, so the two are
# not interchangeable: `{1,999}?` is a lazy quantifier, not an optional one.
LABEL_MAX_OPT = '{0,999}'
# `!` joins the lookbehind because `![alt](x.md)` is an IMAGE: its destination
# is a resource the page loads, not a page the reader can navigate to, and the
# path is never visible on screen. By this gate's visible-or-clickable rule it
# is therefore not an edge, so a stale image reference cannot keep an orphan
# alive. The nested pattern still resolves the outer target of a linked image,
# `[![alt](img.png)](page.md)`, which IS navigation.
# The LABEL is captured because an empty one is not a route: `[](x.md)`
# renders an anchor with no content, so there is nothing on screen and
# nothing to click, and counting it let an invisible link conceal a real
# orphan. Whitespace-only counts as empty, which is the rule the sibling
# already applies (`strip_empty_links`, check-migration-guides.sh:502-509).
# An IMAGE label is not empty — `[![alt](img.png)](page.md)` renders a
# clickable image — and needs no exception, since the label text is not
# blank.
# A label may nest brackets to any depth, so long as they balance:
# `[outer [middle [Mail]]](mail.md)` is a real link that renders
# `outer [middle [Mail]]`. This stood at ONE level for most of this PR and was
# recorded as a known limitation; it is a false positive, so a page whose only
# route was written that way was reported as an orphan.
#
# Bounded depth rather than true recursion, exactly as `_nested_parens` already
# does for destinations — the pattern grows linearly per level, so this stays a
# regular expression instead of becoming a scanner.

DEST_RE = re.compile(DEST)
_BLANK_NEXT = re.compile(r'[ \t]*\n')
# A blank line ends the paragraph, and with it whatever the paragraph was in
# the middle of — a link label, an inline HTML comment, a code span. This is
# `.*?` with that one exclusion, shared by all three. It lives HERE, above
# `_label_end`, because a pattern placed above the fragment it is built from
# has raised a NameError and failed every test at once three times in this
# file; `HTML_COMMENT_INLINE` reads it from far below.
_CBODY = r'(?:[^\n]|\n(?![ \t]*\n))*?'
# A code span is a run of backticks closed by an equal-length run; both
# delimiters must be complete runs. The body was `.*?` under `re.S`, which
# matched across a blank line — but `x \`a` / blank / `b\` y` is two
# paragraphs of literal backticks and no code span at all, measured against
# cmark-gfm and markdown-it-py alike. Blanking that "span" erased real text.
CODE_SPAN = re.compile(r'(?<!`)(`+)(?!`)(' + _CBODY + r')(?<!`)\1(?!`)')


def _label_end(txt, i):
    """Index of the `]` matching the `[` at `i`, or None.

    A backslash escapes the next character, and a BLANK LINE ends the
    paragraph and with it the label.

    A CODE SPAN is skipped whole, because it binds tighter than the link
    brackets: `[foo `]` Mail](mail.md)` is a link whose text is
    `foo <code>]</code> Mail`, in both cmark-gfm and markdown-it-py. Counting
    that `]` ended the label early, and the damage ran in both directions — a
    sibling-relative `[a `](x.md)` b](mail.md)` handed the scanner
    `x.md` as the destination, so the page the reader actually reaches was
    reported an orphan while the one they cannot reach was marked live. The
    escape check comes first: `\\`` is a literal backtick that opens
    nothing, so `[foo \\`]\\` Mail](mail.md)` really is not a link.

    This was recorded here as a deliberate non-goal for most of this PR, on
    the grounds that the bracket-counting rewrite was a separate question.
    It was a false positive the whole time.
    """
    depth, j, n = 1, i + 1, len(txt)
    while j < n:
        c = txt[j]
        if c == '\\':
            j += 2
            continue
        if c == '`':
            span = CODE_SPAN.match(txt, j)
            if span:
                j = span.end()
                continue
        if c == '\n' and _BLANK_NEXT.match(txt, j + 1):
            return None
        if c == '[':
            depth += 1
        elif c == ']':
            depth -= 1
            if not depth:
                return j
        j += 1
    return None


def _opens_label(txt, i):
    """Whether the `[` at `i` opens a link label.

    `\\[` is an escaped bracket and `![` opens an image; neither does. Same
    guard as the patterns' lookbehind.
    """
    return not (i and txt[i - 1] in '\\!')


def _openers(txt):
    """Every `[` that could open a label, in order, skipping what cannot.

    A backslash escapes the bracket after it, and a CODE SPAN is stepped over
    whole — `[outer `[x](other.md)`](mail.md)` is one link to `mail.md` whose
    label happens to display a second link as code. Hunting with `find('[')`
    saw the sample as a real inner link, and (once inner links deactivate the
    opener above them) that stranded the outer link's page.

    Yielding positions rather than driving the loop means a caller cannot
    advance past a span it already consumed; that is deliberate. The scanners
    below re-derive their own end from `_label_end`, and overlapping openers
    are exactly what the nesting cases need.
    """
    i, n = 0, len(txt)
    while i < n:
        c = txt[i]
        if c == '\\':
            i += 2
            continue
        if c == '`':
            span = CODE_SPAN.match(txt, i)
            i = span.end() if span else i + 1
            continue
        if c == '[':
            yield i
        i += 1


def inline_links(txt, resolved=None):
    """Every inline `[label](dest)`, with the label balanced to ANY depth.

    A regular expression cannot count, so the nesting pattern was bounded —
    first at one level, then at sixteen — and each bound was beaten by
    bound+1. There is no bound to pick: cmark-gfm renders a link whose label
    nests a THOUSAND deep, measured, so it has no limit of its own to match.
    Counting is the only fix that ends the sequence.

    Label semantics are `_label_end`'s: a backslash escapes the next
    character, a BLANK LINE ends the paragraph and with it the label, and a
    code span is skipped whole.

    A LINK MAY NOT CONTAIN A LINK. When the label holds one, CommonMark
    deactivates the OUTER opener and the inner link is what renders, so
    `[outer [Mail](mail.md)](https://example.test)` gives the reader a route
    to `mail.md` and none to `example.test`. Balancing straight through the
    label yielded the outer destination and skipped past the inner one —
    exactly backwards, and it reported a page the reader can click as an
    orphan. `resolved` is the set of defined reference labels; without it
    only an inner INLINE link deactivates, since that is the one shape that
    resolves unconditionally.

    The rule is narrower than "a bracket pair inside the label", and every
    boundary here was measured rather than reasoned:

    - An inner IMAGE does not deactivate: `[outer ![m](m.png)](x)` is a link
      wrapping an image. Images may contain links, too, so an outer `!`
      opener is never deactivated by anything.
    - An UNRESOLVED reference does not deactivate — it is not a link, just
      literal brackets — which is why `resolved` has to be threaded in.
    - A link inside a CODE SPAN does not deactivate, and `_label_end` already
      steps over those, so the inner scan must use it rather than a bare
      bracket search.
    - An inner AUTOLINK or RAW ANCHOR does not deactivate. cmark-gfm emits
      nested `<a>` there; invalid HTML, but not this gate's business.

    Yields `(label_start, label_end, angle_dest, bare_dest)`, matching what
    the patterns' groups 1, 2 and 3 gave the caller.
    """
    for i in _openers(txt):
        # `\[` is an escaped bracket and `![` opens an image; neither starts a
        # link label. Same guard as the patterns' lookbehind.
        if not _opens_label(txt, i):
            continue
        j = _label_end(txt, i)
        if j is None:
            continue
        m = DEST_RE.match(txt, j + 1)
        if m and not contains_link(txt[i + 1:j], resolved):
            yield i + 1, j, m.group(1), m.group(2), m.start(), m.end()


def reference_images(txt, defined):
    """Every `![alt][ref]`, `![ref][]` and `![ref]` whose label RESOLVES.

    Yields `(start, end)` over the whole span, `!` included.

    Balanced like every other label here — the bounded pattern this replaces
    took one nested pair, so a deeper alt left the image unmasked and the
    bare-path scan read a path out of it.

    An UNRESOLVED reference is not an image at all: it renders as literal
    text, so `![alt][nosuch]` puts its label on screen and masking it would
    invent an orphan. That is why `defined` is required rather than optional.
    """
    for i in _openers(txt):
        if not (i and txt[i - 1] == '!'):
            continue
        j = _label_end(txt, i)
        if j is None:
            continue
        first = txt[i + 1:j]
        m = REF_TAIL.match(txt, j + 1)
        end = m.end() if m else j + 1
        second = m.group(1) if m else None
        label = second if second is not None and second.strip() else first
        if ref_label(label) in defined:
            yield i - 1, end


def inline_images(txt):
    """Every inline `![alt](dest)` span, with the alt text balanced to ANY depth.

    Yields `(start, end)` over the whole span, `!` included.

    An image's alt text is a link label and nests like one, so the bounded
    pattern this replaces missed `![outer [middle [alt]]](x.md)` and left the
    destination for the bare-path scan to pick up as if it were prose. An
    image destination is a resource the page loads, never text on screen, so
    that made a genuinely orphaned page reachable.

    No deactivation rule here, and that asymmetry is measured, not assumed: a
    link may not contain a link, but an IMAGE may — `![outer [Mail](m.md)](x)`
    renders as one image whose alt text reads `outer Mail`.
    """
    for i in _openers(txt):
        if not (i and txt[i - 1] == '!'):
            continue
        j = _label_end(txt, i)
        if j is None:
            continue
        m = DEST_RE.match(txt, j + 1)
        if m:
            yield i - 1, m.end()


def contains_link(label, resolved=None):
    """Whether a link label holds a link, which deactivates the opener above it.

    Recursion is over a strictly shorter string every time — the label sits
    inside the brackets it is cut from — so it terminates, and `LABEL_MAX`
    bounds how deep it can go.
    """
    for _ in inline_links(label, resolved):
        return True
    if not resolved:
        return False
    # The reference scans are patterns, not scanners, so they cannot step over
    # a code span the way `_openers` does — and a reference SPELLED in code is
    # not a link. `[outer `[fake][m]`](mail.md)` with `[m]` defined kept the
    # outer link in both renderers, while this function suppressed it and
    # stranded the page. Blanked space for space, so the offsets the callers
    # read stay valid.
    label = CODE_SPAN.sub(lambda m: ' ' * len(m.group(0)), label)
    for _s, _e, lstart, lend, ref in full_references(label):
        # `[text][]` is collapsed: the label IS the reference.
        if ref_label(ref or label[lstart:lend]) in resolved:
            return True
    for m in REF_USE_SHORTCUT.finditer(label):
        if ref_label(m.group(1)) in resolved:
            return True
    return False
# Markdown drops the backslash from an escaped ASCII punctuation character, so
# `guide\(v2\).md` addresses the file `guide(v2).md` — same rule as the sibling.
UNESCAPE = re.compile(r'\\([!-/:-@\[-`{-~])')
# A character reference in a destination: `mail&#46;md`, `mail&#x2e;md` and
# `mail&period;md` all address `mail.md`. `html.unescape` covers the numeric and
# named forms together, so there is no table to maintain here.
#
# NEITHER FORM CAN ACTUALLY BITE TODAY, and that is worth writing down rather
# than discovering later. `check-docs-links.sh` does not decode references at
# all: it reports `mail&#46;md` and `mail&period;md` as broken link targets, so
# a page using one fails the docs job on that gate before reachability is ever
# in question. Decoding here is therefore about not disagreeing with the
# reader's view of the page, not about a case a green corpus can contain.
#
# The two siblings disagree on this point: `check-migration-guides.sh` resolves
# numeric references deliberately, with a test
# (`…_resolves_a_character_reference_in_a_destination`), while
# `check-docs-links.sh` rejects them. Reconciling that is a change to the link
# gate's contract and belongs in its own PR, not smuggled into this one.
# CommonMark decodes only SEMICOLON-terminated references. `html.unescape` is
# HTML5-lenient and takes `&#46md` too, which renders literally — so decoding it
# invented a `.md` the reader never sees and marked an orphan reachable.
CHAR_REF = re.compile(r'&(?:#[0-9]{1,7}|#[xX][0-9A-Fa-f]{1,6}|[A-Za-z][A-Za-z0-9]*);')


def decode_char_refs(s):
    return CHAR_REF.sub(lambda m: html.unescape(m.group(0)), s)
# A reference definition: `[mail]: mail.md`, optionally `<…>`-wrapped. Markdown
# allows up to three leading spaces. Reference-style links are a syntax
# check-docs-links.sh already parses and self-tests, so a page linked only that
# way is genuinely reachable; without this the gate would report it as an
# orphan and block a docs change written in a spelling the corpus supports.
# The label honours escapes, so `[closing \]]: mail.md` is one definition whose
# label contains a bracket — `[^\]]+` would stop at the escaped one and lose the
# definition entirely. Same `FLAT` shape the sibling uses for exactly this.
REF_DEF = re.compile(
    r'^ {0,3}\[(' + FLAT + LABEL_MAX + r')\]:(?:[ \t]*\n?[ \t]*)'
    # The bare form must have BALANCED parentheses, exactly as an inline
    # destination does. `\\S+` took `mail.md#(unterminated`, which defines
    # nothing, and `add_relative` then dropped the fragment and recorded
    # `mail.md` — an orphan reachable through a definition that does not
    # exist. Pinned by the repo's
    # `migration_guide_gate_rejects_an_unbalanced_paren_in_a_definition`.
    r'(?:' + ANGLE_DEST + r'|(' + DEST_BARE + r'))'
    # A title that OPENS and never closes makes the whole line a paragraph —
    # there is no definition at all. Truncating at the destination recorded a
    # target the reader never reaches. Pinned by the repo's
    # `…_rejects_a_definition_with_an_unterminated_title`.
    r'(?![ \t]*(?:"[^"\n]*$|\'[^\'\n]*$|\([^)\n]*$))'
    # ...and nothing but whitespace and an optional title may follow the
    # destination. This resolved the `mail.md` PREFIX of `[m]: mail.md trailing
    # garbage`, which is a paragraph and defines nothing, so a page reachable
    # only through that label was recorded as reachable and a real orphan
    # passed. Matching a valid-looking prefix is the whole failure.
    + r'(?=' + TITLE + r'[ \t]*$)', re.M)
# The same, extended to the optional title — on the destination's line or the
# one after it, per CommonMark. Used only to blank the definition's full span.
# The title is `TITLE` itself, not a second spelling of it. Its own copy let a
# quoted body cross a blank line, so `[old]: url "title` / blank / `[Mail][m]`
# swallowed a LIVE reference use into the definition's blanked span — the
# definition it used then looked unused, and the page it pointed at was
# reported as an orphan the reader can in fact click. A blank line ends the
# paragraph and there is no title, which is what `TITLE` already encodes.
REF_DEF_FULL = re.compile(
    r'^ {0,3}\[(?:' + FLAT + r')' + LABEL_MAX + r'\]:(?:[ \t]*\n?[ \t]*)'
    r'(?:<[^<>\r\n]*>|' + DEST_BARE + r')'
    # A definition ENDS at the end of its line: after the destination only
    # whitespace and an optional title may follow. `[m]: mail.md trailing
    # garbage` is a paragraph, not a definition, so blanking its span hid text
    # the reader can see — and the matching rule in `REF_DEF` recorded the
    # `mail.md` prefix as a route, letting a real orphan through. Requiring the
    # line to end also subsumes the unterminated-title case for free: a title
    # that opens and never closes leaves the line unfinished.
    + TITLE + r'(?=[ \t]*$)', re.M)
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
_LBL = FLAT  # see LABEL_MAX above: one label grammar, not two
# The FIRST label is captured too, because the collapsed form `[label][]`
# takes its label from there. Finding it with `index(']')` picked the
# ESCAPED bracket in `[a\\]b][]` and produced `a\\`, which matches no
# definition — a link the reader can click, reported as an orphan.
# The FIRST label is link TEXT and may nest balanced brackets, exactly as an
# inline label may — see `full_references`, which scans rather than matches for
# that reason. The SECOND is a reference LABEL, which by CommonMark ends at the
# first unescaped `]` and so cannot nest — two grammars sharing a delimiter.
REF_USE_SHORTCUT = re.compile(
    r'(?<![\\!])\[(' + _LBL + LABEL_MAX + r')\](?![\(\[:])')
# `full_references(images=True)` blanks every full-reference span before the
# shortcut scan: in `![alt][mail]` the image guard correctly rejects the link,
# but the trailing `[mail]` then looks exactly like a standalone shortcut, so
# an image would resurrect the label the guard just refused. The same applies
# to a hidden nested label: when the blank misses, the `[m]` of
# `[<span hidden>[x]</span>][m]` resurrects as a route the reader cannot see.

# Placed HERE, not beside the other link scanners, because `REF_TAIL` is
# built at import time from `_LBL` and `LABEL_MAX_OPT` above it. Defining it
# earlier raised a NameError that failed every test at once — the third time
# this file has been bitten by a pattern placed above what it is made of.

REF_TAIL = re.compile(r'\[(' + _LBL + LABEL_MAX_OPT + r')\]')


def full_references(txt, images=False):
    """Every `[label][ref]`, with the FIRST label balanced to any depth.

    The first label is link TEXT and nests; the second is a reference LABEL,
    which by CommonMark ends at the first unescaped `]` and cannot. Two
    grammars sharing a delimiter, so only the first is scanned.

    `images` includes the `![alt][ref]` spelling, which the blanking pass
    needs and the extraction pass must not have.

    Yields `(start, end, label_start, label_end, ref)`.
    """
    i = 0
    while True:
        i = txt.find('[', i)
        if i == -1:
            return
        start = i
        if images and i and txt[i - 1] == '!':
            start = i - 1
        elif not _opens_label(txt, i):
            i += 1
            continue
        j = _label_end(txt, i)
        if j is None:
            i += 1
            continue
        m = REF_TAIL.match(txt, j + 1)
        if not m:
            i += 1
            continue
        yield start, m.end(), i + 1, j, m.group(1)
        i = m.end()


# An image and its destination, in both spellings. Blanked before the bare-path
# scan for the same reason the inline pattern guards against `!`: the path in
# `![alt](docs/guide/x.md)` is a resource the page loads, never text on screen,
# so it is not something a reader can find the page by.
# The alt text may itself contain a bracketed span — `![nested [alt]](x.md)` —
# and a pattern that stops at the inner bracket leaves the image unmasked, so
# the bare scan picks its destination back up.
# An image label is a link label: `FLAT`, blank-line bound included. Its own
# copy admitted one, so `![alt` / blank / `docs/guide/mail.md](x.png)` — which
# renders as a broken image and then the PATH as visible text — was masked
# whole before the bare-path scan, hiding a route the reader can read.
# The inline spelling is scanned by `inline_images`, not matched: its own
# bounded pattern admitted one nested bracket pair, so
# `![outer [middle [alt]]](docs/guide/mail.md)` was left unmasked and its
# destination — a resource, never text on screen — reached the bare-path scan
# as if a reader could read it there.
# The label is captured twice because an image reference names its
# definition in one of three places: `![alt][ref]` (the second), `![ref][]`
# and `![ref]` (the first). Which one is used decides whether this is an
# image at all — an UNRESOLVED reference renders as literal text, so
# `![alt][docs/guide/mail.md]` with no such definition puts that path on
# screen, and masking it unconditionally reported the page as an orphan.
# The REFERENCE spellings are scanned by `reference_images`, for the reason the
# inline one is: this pattern admitted a single nested bracket pair, so
# `![outer [middle [docs/guide/mail.md]]][logo]` went unmasked and the bare-path
# scan read a path out of an image's alt text.
# Any inline link span, image or not. Blanked before the shortcut scan: links
# cannot nest, so in `[outer [mail]](https://example.com)` only the OUTER link
# renders and the inner `[mail]` is ordinary label text — not a shortcut
# reference that should keep `[mail]: mail.md` alive.
INLINE_SPAN_ANY = re.compile(
    r'\[(?:(?:[^\[\]\\]|\\.)|\[(?:[^\[\]\\]|\\.)*\])*\]' + DEST)


def ref_label(s):
    """CommonMark label matching: case-insensitive, internal whitespace collapsed.

    Returns None for a label over the 999-character cap, which defines and uses
    nothing. The cap is on SOURCE characters, so it cannot be a repetition
    bound in the pattern: 500 `\\*` escapes are 500 repetitions and 1000
    characters, and were accepted. `LABEL_MAX` stays as a cheap ceiling — a
    valid label never needs more than 999 repetitions — and the exact count
    happens here, where the captured text is in hand.
    """
    if len(s) > LABEL_LIMIT:
        return None
    # `casefold`, not `lower`: CommonMark matches labels after a full Unicode
    # case fold, so `[Mail][\u1e9e]` resolves against `[ss]:` — `lower()`
    # gives `\u00df` there and matched nothing, reporting a page the reader
    # can click as an orphan. Verified against markdown-it-py.
    #
    # Case folding and whitespace are the WHOLE of it. Normalization does not
    # unescape, so `[Mail][x\\!]` does not resolve against `[x!]:` — a review
    # round proposed that it should, and the renderer says otherwise: only when
    # BOTH sides spell the escape does it link. Escapes fold to placeholders
    # before this point, uniformly across the document, so comparing them
    # literally is what keeps that true.
    return ' '.join(s.split()).casefold()
# A bare repo path. The leading guard keeps `…/docs/guide/x.md` inside a longer
# path from matching at the wrong offset.
# A URI scheme, for the destinations that leave the site entirely.
SCHEME = re.compile(r'[A-Za-z][A-Za-z0-9+.-]*:')
# The trailing guard is the mirror of the leading one: `docs/guide/mail.md.bak`
# and `docs/guide/mail.md5` name OTHER files, and stopping at the `.md`
# prefix recorded the tracked page as reachable from a path that does not
# point at it — an obsolete backup concealing a real orphan. The second
# lookahead is what keeps a sentence working: `See docs/guide/mail.md.`
# ends with a period and still counts, because what follows it is not a
# path character.
BARE = re.compile(
    r'(?<![\w/.-])((?:docs|skills|agents|examples)/[A-Za-z0-9._/-]+\.md)'
    r'(?![A-Za-z0-9_/-])(?!\.[A-Za-z0-9_/-])')


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
# Two grammars, because a comment MID-LINE is inline html and ends where the
# renderer says it does. `<!-->` and `<!--->` are COMPLETE comments — which
# is why `prose <!--> [Mail](mail.md) -->` renders the link and leaves the
# trailing `-->` as literal text — and any other `<!--` runs to its first
# `-->`, `--` inside and all.
#
# CORRECTION to what this said before: it forbade `--` in the body and
# called `<!-->` invalid. Both are wrong against markdown-it-py, which
# passes `<!-- a -- b -->` through as raw HTML. The outcome for `<!-->` was
# right by accident; the `--` rule was a live false NEGATIVE, since a path
# inside `<!-- docs/guide/mail.md -- x -->` went unstripped and counted as
# visible text that the browser does not show.
#
# At the START of a line the loose form still governs: a type-2 block opens
# on `<!--` whatever follows and ends at the first `-->`.
HTML_COMMENT_CLOSED = re.compile(r'<!--.*?-->', re.S)
# An INLINE comment cannot contain a BLANK LINE — that ends the paragraph,
# so `prose <!-- x` and `y --> tail` are two paragraphs of visible text and
# no comment at all (cmark-gfm escapes both). The line-initial form is
# unaffected: a type-2 block runs THROUGH blank lines to its first `-->`,
# which is why only this pattern carries the bound. Same idiom as `FLAT`.
# `_CBODY` itself is defined above `_label_end`, which needs it first.
HTML_COMMENT_INLINE = re.compile(
    r'<!--->|<!-->|<!--(?!>|->)' + _CBODY + r'-->')
UNCLOSED = '<!--'
# A paragraph break: one line with nothing on it.
BLANK_LINE = re.compile(r'\n[ \t]*\n')
# A fence opener/closer: ``` or ~~~ , up to three spaces of indent, plus the
# rest of the line — which decides whether the line is a fence at all.
FENCE = re.compile(r'^ {0,3}(`{3,}|~{3,})(.*)$', re.M)


# THE TAG VOCABULARY. Everything that walks an HTML tag reads from here, and
# it sits above every user for a plain reason: twice now a pattern has been
# added above the fragment it depends on, and Python raised a NameError that
# failed a hundred tests at once. One block, before anything needs it.
# Whitespace INSIDE a tag may wrap a line but never cross a blank one: the
# blank ends the paragraph, so `<a` / blank / ` href="b.md">` is not an anchor
# at all. `\s+` spanned it and recorded the destination of a tag that never
# renders — malformed prose concealing a real orphan. Same bound the
# destination, the title and the label already carry, applied to the fourth
# place a blank line could get through.
_TWS = r'(?:[ \t]|\n(?![ \t]*\n))'
# ...and so does the inside of a quoted value. Bounding only the whitespace
# BETWEEN attributes left the other half open: `<a title="hello` / blank /
# `world" href="mail.md">` is two paragraphs and no anchor, and its href was
# still being recorded. Same rule, the other side of the quote.
_Q = r'"(?:[^"\n]|\n(?![ \t]*\n))*"'
_Q1 = r"'(?:[^'\n]|\n(?![ \t]*\n))*'"
ATTR_ASSIGNED = (r'[A-Za-z_:][-\w:.]*' + _TWS + r'*=' + _TWS +
                 r'*(?:' + _Q + r'|' + _Q1 + r'|[^\s"\'=<>`]+)')
ATTR = (r'[A-Za-z_:][-\w:.]*(?:' + _TWS + r'*=' + _TWS +
        r'*(?:' + _Q + r'|' + _Q1 + r'|[^\s"\'=<>`]+))?')
# The same, confined to a single line — for the block start conditions,
# which require the whole tag on the opening line.
ATTR_1LINE = (r'[A-Za-z_:][-\w:.]*(?:[ \t]*=[ \t]*'
              r'(?:"[^"\n]*"|\'[^\'\n]*\'|[^\s"\'=<>`]+))?')

# The CommonMark type-1 tags. Declared here rather than beside the block
# pattern that used to own them, because the line scan below needs them
# first — one vocabulary, read in the order the file is executed.
TYPE1_TAGS = r'pre|textarea|script|style'
FENCE_ONLY = re.compile(r'^ {0,3}(`{3,}|~{3,})(.*)$')
# Type-1 openers and their end condition, line-anchored. The close need not
# match the opener — the spec says so — which is why this is one alternation.
TYPE1_OPEN_LINE = re.compile(
    r'^ {0,3}<(?:' + TYPE1_TAGS + r')(?=[\s>]|$)', re.I)
TYPE1_CLOSE_LINE = re.compile(r'</(?:' + TYPE1_TAGS + r')\s*>', re.I)


def raw_block_line_spans(txt):
    """Line ranges covered by a line-initial raw block — a comment, or type 1.

    Fences and raw blocks are all leaf blocks, so whichever OPENS first wins,
    and this scan is where that is decided for whole lines. It has been wrong
    in three directions in turn. Fences first let a ``` shown INSIDE a comment
    open a real fence that swallowed everything after the comment closed.
    Comments first let a fenced `<!--` sample comment out the file. And with
    only comments here, a ``` inside `<script>` split the script in two before
    `mask_invisible` could reach it, leaving a region the browser never renders
    marked as a visible fence — where a bare path then counted as a route.
    The repo's migration gate hit the same family in its own review rounds
    (`…_treats_fences_inside_comments_as_comment_content`).

    So the scan tracks fence state as well, and simply does not open a raw
    block inside one: `<script>` demonstrated in a fence is a code sample whose
    text is on screen, and the fence keeps it.
    """
    spans, pos, start, kind = [], 0, None, None
    in_fence, marker = False, None
    for line in txt.split('\n'):
        end = pos + len(line)
        if kind is not None:
            if ('-->' in line if kind == 'comment'
                    else TYPE1_CLOSE_LINE.search(line)):
                spans.append((start, end))
                start, kind = None, None
        elif in_fence:
            m = FENCE_ONLY.match(line)
            if (m and m.group(1)[0] == marker[0]
                    and len(m.group(1)) >= marker[1]
                    and not m.group(2).strip()):
                in_fence, marker = False, None
        else:
            m = FENCE_ONLY.match(line)
            stripped = line.lstrip()
            # A backtick info string may not contain a backtick, so ```` ```bad` ````
            # opens nothing and the line is ordinary paragraph text. `split_fences`
            # already knew; this scan accepted every fence-shaped line and closed
            # the paragraph, so a definition under one was blanked and the path it
            # holds — visible, because that definition cannot interrupt a
            # paragraph — stopped counting.
            if m and not (m.group(1)[0] == '`' and '`' in m.group(2)):
                in_fence, marker = True, (m.group(1)[0], len(m.group(1)))
            elif (stripped.startswith('<!--')
                  and len(line) - len(stripped) <= 3):
                start, kind = pos, 'comment'
                if '-->' in line:
                    spans.append((start, end))
                    start, kind = None, None
            elif TYPE1_OPEN_LINE.match(line):
                start, kind = pos, 'type1'
                if TYPE1_CLOSE_LINE.search(line):
                    spans.append((start, end))
                    start, kind = None, None
        pos = end + 1
    if start is not None:
        spans.append((start, len(txt)))
    return spans


def split_fences(txt):
    """Split into ('prose'|'fence', text) segments, in order."""
    blocked = raw_block_line_spans(txt)
    parts, pos, in_fence, marker = [], 0, False, None
    for m in FENCE.finditer(txt):
        tok, rest = m.group(1), m.group(2)
        # A fence delimiter inside a raw block is that block's content.
        if any(a <= m.start() < b for a, b in blocked):
            continue
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

    THREE REMAINING LIMITATIONS of the four once listed here, all narrow, all
    false POSITIVES, and all left deliberately because the available fixes cost
    more than the bugs. (3) has since been fixed and is kept as a note, because
    the argument that justified leaving it was wrong in a way worth seeing.

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

    3. FIXED, TWICE, and left here because the way it was wrong each time is
       worth more than the line it saved. It first said link labels nest only
       one level, because "balanced nesting to arbitrary depth is not
       expressible as a regular expression" — true, and beside the point, since
       `_nested_parens` in this same file was already matching nested
       DESTINATIONS by unrolling to a bounded depth. So labels were unrolled
       the same way, to 16. That bound was then beaten by 17, which is the
       thing a bound always invites: cmark-gfm has NO limit at all (measured, a
       label nesting a thousand deep still renders), so there was never a
       number to pick. Labels are now SCANNED (`inline_links`,
       `full_references`), which counts and so has no bound.

       Destinations are still unrolled at 32 parens and are beatable at 33 in
       exactly the same way. Left as is deliberately: cmark itself caps
       destination nesting at 32, so that bound matches the renderer instead of
       merely hoping. `check-docs-links.sh` still stops at one level of label
       nesting, so it is now the stricter of the two on this shape.

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
            # A comment that BEGINS a line is a type-2 raw HTML block, and the
            # line carrying its terminator belongs to the block: `<!-- note -->
            # [Mail](mail.md)` leaves the link literal. Mid-line the same
            # comment is inline HTML and the link after it renders, which is
            # why this reads the text before it rather than blanking to the end
            # of every comment. Same end-condition rule as types 1 and 3-5.
            start, end = cm.start(), cm.end()
            bol = masked.rfind('\n', 0, start) + 1
            line_initial = (masked[bol:start].strip() == ''
                            and start - bol <= 3)
            if line_initial:
                eol = masked.find('\n', end)
                end = len(masked) if eol == -1 else eol
            elif not HTML_COMMENT_INLINE.fullmatch(masked, start, end):
                # Mid-line it must be a well-formed INLINE comment. `<!-->` is
                # not one, so the text around it renders and the link inside
                # stays live; stripping through the later `-->` deleted it.
                continue
            elif BLANK_LINE.search(masked[start:end]):
                # Mid-line, this is INLINE html, and inline html cannot cross a
                # blank line: the paragraph ends there, so `prose <!-- note` /
                # blank / `[Mail](mail.md)` / `-->` is not a comment at all and
                # the link is live. Removing it took the link with it. A
                # LINE-INITIAL comment is a type-2 block and does span blank
                # lines, ending only at its `-->`, which is why this is the one
                # arm that checks.
                continue
            seg = seg[:start] + ' ' * (end - start) + seg[end:]
            masked = masked[:start] + ' ' * (end - start) + masked[end:]
        # Only a LINE-INITIAL opener runs to EOF. That is the type-2 block
        # start condition, and mid-line there is no block: `prose <!-- sample`
        # is an incomplete INLINE comment, which is literal text, and the links
        # below it stay live. Truncating at any `<!--` reported those pages as
        # orphans and would block a valid docs change.
        idx = -1
        pos = 0
        while True:
            found = masked.find(UNCLOSED, pos)
            if found == -1:
                break
            bol = masked.rfind('\n', 0, found) + 1
            if masked[bol:found].strip() == '' and found - bol <= 3:
                idx = found
                break
            pos = found + len(UNCLOSED)
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
# `iframe` is here for its CONTENTS: a browser renders the frame and ignores
# the fallback text inside it, so a path written there is not on screen. The
# ELEMENT still counts as link content (see `ANY_IMAGE`) — it paints — which
# is the same split `template` has and the reason this list is not the
# type-1 one.
HIDDEN_TAGS = r'script|style|template|iframe'
# Comments and hidden-HTML openers are found by ONE scan, because whichever
# opens FIRST wins and neither can decide that alone. `<!-- <script> -->` is a
# sample INSIDE a comment: matching hidden HTML first read it as an unclosed
# script and blanked the rest of the page, losing live links below it. The
# mirror was already true and is why the openers cannot simply be stripped in
# the other order — a `<!--` inside a closed `<script>` is script data, not a
# comment, and parsing comments first truncated the document at it.
# This is the third place in this file that needed the same rule; `split_fences`
# and `raw_block_line_spans` settle fences against comments the same way.
# `\b` is not the boundary this needs: a hyphen is a non-word character, so it
# sits happily between `script` and `-widget` and `<script-widget>` was read as
# an unclosed `<script>`, blanking the rest of the page. A custom element is a
# perfectly ordinary tag whose contents render. CommonMark's own start
# condition is the name followed by whitespace, `>` or the end of the line.
TAG_NAME_END = r'(?=[\s>]|$)'
# The opener is the tag NAME, with no `>` required: `<script` alone at the
# end of a line starts the block, and demanding `[^>]*>` left it unmasked
# whenever no angle bracket followed — the apparent markdown below it then
# counted as navigation. The sibling opens on the name alone for the same
# reason (check-migration-guides.sh:1062-1074).
# Two spellings, because the name-only form is a BLOCK start condition and
# nothing else: `prose <script` mid-line is an incomplete tag, which is
# ordinary visible text, and reading it as an opener masked through EOF.
# Mid-line it takes a complete tag to be raw HTML at all.
# The mid-line form is a complete, WELL-FORMED tag, attribute grammar and
# all: `prose <script a==>` is malformed, so the renderer escapes it and the
# browser never sees a script — the links below it stay live, and `[^>]*>`
# masked them away. A well-formed one still masks, because the browser DOES
# see that tag and swallows what follows it.
# `template` hides its contents in a browser, which is why it is a hidden
# tag — but it is NOT one of CommonMark's type-1 tags, and the name-only
# alternative is that block's start condition. `<template` with no `>` is
# escaped text, and the links under it stay live; only a COMPLETE
# `<template>` tag hides anything.
HIDDEN_TYPE1 = r'script|style'
FIRST_OPENER = re.compile(
    r'(<!--)'
    r'|(?:^ {0,3}(<(' + HIDDEN_TYPE1 + r')' + TAG_NAME_END + r'))'
    r'|(<(' + HIDDEN_TAGS + r')(?:' + _TWS + r'+' + ATTR + r')*'
    + _TWS + r'*/?>)',
    re.I | re.M)
# `<pre>` and `<textarea>` are CommonMark type-1 raw blocks alongside script and
# style, so Markdown inside them is literal — but unlike script and style their
# contents are SHOWN. A path in a `<pre>` block is on screen and still counts,
# which is why they belong here and not among the hidden tags: this bounds Markdown
# extraction only.
# The end condition is the LINE, not the tag: CommonMark ends a type-1 block on
# the line carrying a close tag, and that whole line is part of the block. So
# `</pre> [Mail](mail.md)` leaves the link literal, and stopping at the `>` fed
# the tail to the extractors as live Markdown. `check-migration-guides.sh`
# records the same rule in as many words ("The end-condition line is part of the
# block, so text sharing a line with `</script>` stays literal too").
# The close tag need not match the opener — the spec says so explicitly — so
# the alternation is deliberate rather than a backreference.
# Anchored to the start of a line (up to three spaces) because that is the
# START condition: a mid-line `see <pre> for details` opens no block, and the
# Markdown under it still renders.
PRE_BLOCK = re.compile(
    r'^ {0,3}<(?:' + TYPE1_TAGS + r')' + TAG_NAME_END +
    r'.*?</(?:' + TYPE1_TAGS + r')\s*>[^\n]*'
    r'|^ {0,3}<(?:' + TYPE1_TAGS + r')' + TAG_NAME_END + r'.*',
    re.S | re.I | re.M)
# One HTML attribute, as CommonMark defines it. The unquoted value is the part
# worth spelling out: it admits no whitespace and none of `"`, `'`, `=`, `<`,
# `>` or a backtick, so `<span a==>` is malformed and is ordinary paragraph
# text. `[^\s>]+` accepted it, a type-7 block opened on a line that opens none,
# and the link under it was masked away — a live route reported as an orphan.
# (A BLOCK tag is different: `<div a==>` opens a type-6 block whatever its
# attributes look like, because that start condition only reads the tag NAME.
# The distinction is why this belongs to type 7 and not to `RAW_BLOCK`.)
# Every place that walks a tag shares this, since the four that had their own
# copy of `[^\s>]+` are exactly the four this file has had to fix in lockstep.
# Every attribute value EXCEPT `href`. An attribute is not rendered text, so a
# path, a reference label or a comment marker parked in one — `<span
# data-note="[mail](mail.md)">` — is invisible and confers nothing. `href` is
# excluded because `<a href=…>` is real navigation the anchor extractor reads.
# This subsumes the old `src`-only rule, which was the same idea applied to one
# attribute name.
ATTR_VALUE = re.compile(r'\b(?!href\b)' + ATTR_ASSIGNED, re.I)
# The same with no exception, for tags that are not anchors. `href` is spared
# only on `<a>`: on `<link rel="alternate" href="…">` it names a resource the
# page references invisibly, not somewhere the reader can click.
ATTR_VALUE_ANY = re.compile(r'\b' + ATTR_ASSIGNED, re.I)
# The attribute grammar is HTML_TAG's, not `[^>]*`, for the reason HTML_TAG
# already carries: a quoted `>` does not end a tag. `<a title="1 > 0"
# data-note="docs/guide/mail.md">` ended here inside `title`, so `data-note`
# fell outside the anchor bounds, was never masked, and its invisible path kept
# an orphan alive. Fixing HTML_TAG and leaving this one is the same
# rule-applied-to-one-sibling mistake this file keeps making.
ANCHOR_TAG = re.compile(
    r'<a(?:' + _TWS + r'+' + ATTR + r')*' + _TWS + r'*/?>', re.I)
# The destination of a rendered link is blanked before the bare-path scan by
# `blank_link_dests`, which walks `inline_links` rather than matching a
# pattern. Two bounded patterns stood here and were deleted: they required the
# LABEL in front of the destination, because `](docs/guide/mail.md)` with no
# opening bracket renders literally and its path is visible text — but they
# admitted only ONE level of nesting, so
# `[outer [middle [label]]](https://example.test/?q=docs/guide/mail.md)` went
# unblanked and the bare scan read a repository path out of an EXTERNAL URL.
# The scanner also knows what a pattern cannot: an outer opener deactivated by
# an inner link is not a link, so its `](…)` is visible text and must survive.
# Only the destination is blanked, never the label: the label IS on screen,
# so `[see docs/guide/mail.md](https://example.com)` names a route in words
# the reader can read.
# A whole raw tag, used to bound where `src=` may be masked. Unscoped, that
# pattern also eats the query of `[Mail](mail.md?src=guide)`, which is an
# ordinary Markdown link the sibling gate resolves.
# Attribute values are consumed as units so a quoted `>` does not end the tag:
# `<span title="1 > 0" data-note="…">` is one tag, and stopping at the quoted
# character would leave the later attributes outside the masking bounds.
HTML_TAG = re.compile(
    r'<[A-Za-z][A-Za-z0-9-]*(?:' + _TWS + r'+' + ATTR + r')*' + _TWS + r'*/?>')
# A raw HTML BLOCK of any tag — CommonMark type 6. Its contents are raw HTML,
# so `[mail]` inside `<div>…</div>` stays literal and resolves no reference.
# Unlike the hidden tags the text is still VISIBLE, so this bounds Markdown
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
# Each of the three ends on the line carrying its terminator, and that whole
# line belongs to the block — `<?demo ?> [Mail][m]` leaves the reference
# literal, so a later `[m]: mail.md` cannot make it a route. Stopping at `?>`
# handed the tail to the reference scanner and marked the orphan reachable.
# Terminated forms first, then the run-to-EOF ones. An UNCLOSED `<?demo` keeps
# its block open through the end of the file, so everything below it is raw
# HTML and a `[Mail](mail.md)` there is literal. Requiring the closing
# delimiter left those blocks unmasked entirely, which is the direction that
# lets a real orphan pass — the same unclosed-opener rule comments and type-1
# blocks already carry.
RAW_DELIM = (
    r'^ {0,3}<\?.*?\?>[^\n]*'
    r'|^ {0,3}<!\[CDATA\[.*?\]\]>[^\n]*'
    # UPPERCASE only, and this alternation is the one place `re.I` must not
    # reach — see the note below `RAW_DELIM`.
    r'|^ {0,3}(?-i:<![A-Z])[^>]*>[^\n]*'
    r'|^ {0,3}<\?.*'
    r'|^ {0,3}<!\[CDATA\[.*'
    r'|^ {0,3}(?-i:<![A-Z]).*')
# CommonMark 0.30 opens a type-4 declaration on `<!` plus an UPPERCASE ASCII
# letter; 0.31 relaxed that to any ASCII letter. The renderers a reader
# actually meets — cmark-gfm on GitHub, and markdown-it — implement the
# 0.30 rule: `<!demo` is a PARAGRAPH there and the link under it is live,
# verified against markdown-it-py 4.2.0. Masking it reported a page the
# reader can click as an orphan.
#
# This DIVERGES from `check-migration-guides.sh`, which matches
# `^<![a-zA-Z]` and so follows 0.31. That is a real inconsistency between
# the two gates and is recorded here rather than papered over: what a
# reader sees decides this gate's answer, and no corpus page relies on
# either reading. I had declined this finding on the consistency argument
# before checking a renderer, which was the wrong order to do it in.
# The same three on their own, because `mask_invisible` needs them: nothing
# inside one is Markdown, and `<![CDATA[x]]>` happens to READ as an image
# reference — `![` … `]]` with one level of nesting. Blanking it there left a
# bare `<` and `>` behind, RAW_BLOCK no longer matched the line, and a link
# sharing it went back to counting as a route. Only these three are protected;
# a type 6/7 block still needs `hidden_spans` to reach a `<script>` inside it.
RAW_DELIM_BLOCK = re.compile(RAW_DELIM, re.M | re.S)
RAW_BLOCK = re.compile(
    RAW_DELIM +
    r'|^ {0,3}</?(?:' + BLOCK_TAGS + r')(?:[\s/>][^\n]*)?$'
    r'(?:\n(?![ \t]*$)[^\n]*)*',
    re.M | re.I | re.S)
# Type 7 — any complete tag alone on its line — may NOT. Applied only where a
# block can start, or `prose` / `<span>` / `[Mail](mail.md)` masks a live link.
RAW_BLOCK_TYPE7 = re.compile(
    # A malformed `<x =>` is ordinary text, not a tag, so it opens nothing —
    # the attribute grammar is HTML_TAG's rather than a permissive `[^\n]*?`.
    # An OPENING tag takes attributes and may self-close; a CLOSING tag takes
    # only its name, optional whitespace and `>`. Sharing one expression let
    # `</span a=x>` count as a tag, so a type-7 block opened on a line that
    # is ordinary paragraph text and swallowed the link below it.
    # ONE line. Type 7 wants a complete tag followed by nothing but
    # whitespace to the end of the line, so `<span` / ` title=x>` opens no
    # block at all and the markdown under it renders. `_TWS` permits a
    # newline — right for an INLINE tag, which may wrap — and using it here
    # let a split tag swallow the link below it.
    r'^ {0,3}(?:<[A-Za-z][A-Za-z0-9-]*(?:[ \t]+' + ATTR_1LINE + r')*'
    r'[ \t]*/?>'
    r'|</[A-Za-z][A-Za-z0-9-]*[ \t]*>)[ \t]*$'
    r'(?:\n(?![ \t]*$)[^\n]*)*',
    re.M)
# A raw anchor IS navigation, so its destination is resolved like any other —
# through `add_relative`, which means `<a href="mail.md">` and
# `<a href="../guide/mail.md">` work, not just the repo-root spelling the
# bare-path scan happens to catch.
# Attributes are skipped as whole units here too, for the mirror of the reason
# ANCHOR_TAG needs it: `<a title="1 > 0" href="mail.md">` hid the href behind a
# quoted `>`, so a link the reader CAN click stopped counting — the direction
# that reports a reachable page as an orphan.
ANCHOR_CLOSE = re.compile(r'</a' + _TWS + r'*>', re.I)
ANCHOR_HREF = re.compile(
    r'<a(?:' + _TWS + r'+' + ATTR + r')*?'
    + _TWS + r'+href' + _TWS + r'*=' + _TWS +
    r'*(?:"((?:[^"\n]|\n(?![ \t]*\n))*)"'
    r"|'((?:[^'\n]|\n(?![ \t]*\n))*)'"
    r'|([^\s"\'=<>`]+))', re.I)


ATX_HEADING = re.compile(r'^ {0,3}#{1,6}(?:\s|$)')
THEMATIC_BREAK = re.compile(r'^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$')
FENCE_LINE = re.compile(r'^ {0,3}(?:`{3,}|~{3,})')
# A Setext underline turns the paragraph ABOVE it into a heading, so it closes
# that paragraph and leaves none open. Only `---` was handled, and only by
# accident of being a thematic break too; `Links` / `=====` / `[m]: mail.md`
# left the paragraph open and the definition below it unrecognised, reporting a
# page the reader can click as an orphan. A single `=` or `-` underlines, which
# is why this is not the thematic-break rule with a different name.
SETEXT_UNDERLINE = re.compile(r'^ {0,3}(?:=+|-+)[ \t]*$')
# A bare container marker: a list bullet or a quote with nothing after it.
# Checked AFTER the Setext arm, because `-` under a paragraph underlines it
# rather than starting a list.
EMPTY_CONTAINER = re.compile(r'^ {0,3}(?:>|[-*+]|\d{1,9}[.)])[ \t]*$')
# Four columns of indent is a code block — but only where a paragraph is not
# already open, since indented code cannot interrupt one. Tabs expand to
# four-column stops, which is what makes a single leading tab count.
INDENTED_CODE = re.compile(r'^ {4}')
# A definition, as one line sees it. Same shape the sibling's
# `is_link_definition` uses, non-space after the colon included, so both agree
# on what keeps a RUN of definitions going.
# A definition-SHAPED line is not a definition. `[bad]: url trailing garbage`
# is a paragraph, so the `[mail]: mail.md` under it cannot interrupt one and
# defines nothing — but this test said both were definitions, left the
# paragraph closed, and let the second resolve. The complete syntax is what
# decides, the same as it does for `REF_DEF` itself.
REF_DEF_LINE = re.compile(
    r'^ {0,3}\[' + FLAT + LABEL_MAX + r'\]:[ \t]*'
    r'(?:<[^<>\r\n]*>|' + DEST_BARE + r')'
    r'(?P<title>[ \t]+(?:"[^"\n]*"|\'[^\'\n]*\'|\([^()\n]*\)))?[ \t]*$')
# A definition's title may sit on the line AFTER its destination, where it
# is still part of that definition. Read as a paragraph, it closed the run:
# `[x]: url` / `"title"` / `[m]: mail.md` rejected the third line and
# reported a page the reader can click as an orphan.
# A definition may also break BEFORE its destination: `[x]:` on one line and
# the URL on the next is one definition, and the definition after it is
# another. Reading the label line as a paragraph rejected that one, left it
# unblanked, and fed its path to the bare-path scan — so an unused
# definition, which renders nothing at all, marked its target reachable.
# The sibling collector supports the same three-line grammar
# (check-migration-guides.sh:1235-1243).
DEF_LABEL_ONLY = re.compile(
    r'^ {0,3}\[' + FLAT + LABEL_MAX + r'\]:[ \t]*$')
DEST_ONLY_LINE = re.compile(
    r'^ {0,3}(?:<[^<>\r\n]*>|' + DEST_BARE + r')'
    r'(?P<title>[ \t]+(?:"[^"\n]*"|\'[^\'\n]*\'|\([^()\n]*\)))?[ \t]*$')
TITLE_ONLY_LINE = re.compile(
    r'^ {0,3}(?:"[^"\n]*"|\'[^\'\n]*\'|\([^()\n]*\))[ \t]*$')


# Raw HTML blocks whose end condition is a DELIMITER rather than a blank line
# (types 1 and 3-5). `block_starts` has to know about these: it reads the
# unmasked text, where the lines of a visible `<pre>` block look like ordinary
# prose, so a definition directly under a `</pre>` was taken for paragraph
# continuation. It is a definition — the block ended at the close tag — and
# leaving it unblanked let its path count as a visible route.
# Types 6 and 7 need no entry: they end at a blank line, which already closes
# the paragraph, so the only line they could get wrong does not exist.
HTML_OPEN_TYPE1 = re.compile(
    r'^ {0,3}<(?:' + TYPE1_TAGS + r')(?=[\s>]|$)', re.I)
HTML_CLOSE_TYPE1 = re.compile(r'</(?:' + TYPE1_TAGS + r')\s*>', re.I)
HTML_DELIM_OPENERS = (
    (re.compile(r'^ {0,3}<!\[CDATA\['), ']]>'),
    (re.compile(r'^ {0,3}<\?'), '?>'),
    (re.compile(r'^ {0,3}<![A-Z]'), '>'),
)


def is_fence_line(line):
    """Whether the line opens or closes a fence — validated, not merely shaped.

    A backtick info string may not itself contain a backtick, so ```` ```bad` ````
    opens nothing and is ordinary paragraph text. `split_fences` and the line
    scan both check that; the paragraph tracker did not, and closed the
    paragraph on it — so a definition under such a line was blanked, and the
    path it holds stopped counting even though the whole line renders as
    visible text (a definition cannot interrupt a paragraph).
    """
    m = FENCE_ONLY.match(line)
    if not m:
        return False
    return not (m.group(1)[0] == '`' and '`' in m.group(2))


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

    The indented-code arm is measured from column 0, so it is right at the top
    level and wrong inside a list item, where four spaces is the item's own
    content indent rather than code. Getting that right needs the container
    tracking `check-migration-guides.sh` carries (`entry_content_indent`) and
    this file does not — the same gap already recorded for fences in
    `strip_comments`. Left out, the top-level case was wrong in the direction
    that hides an orphan: `    sample` / `<span>` / `[Mail](mail.md)` counted
    the link, when the tag opens a raw block and the link is literal.

    A block-quote line OPENS a paragraph rather than closing one, which is why
    it is not in the list below. `> note` / `[m]: mail.md` is lazy continuation:
    a definition cannot interrupt a paragraph, so the second line is ordinary
    text inside the quote and defines nothing. Treating the quote as a
    completed block admitted that non-definition as an edge, which is how an
    orphan would slip past. The migration gate agrees — its `block_body()`
    deliberately does not strip quote markers, so `> note` sets
    `md_paragraph_open`.
    """
    starts, pos, para_open = set(), 0, False
    # The end condition of an open delimiter-terminated raw block: a compiled
    # pattern for type 1, a literal terminator for types 3-5, or None.
    html_end = None
    # Set when the previous line was a definition with no title of its own, so
    # a title on this line continues it rather than starting a paragraph.
    title_may_follow = False
    # Set when the previous line was a label with no destination, so this line
    # completes that definition instead of starting a paragraph.
    dest_expected = False
    for line in txt.split('\n'):
        if not para_open:
            starts.add(pos)
        stripped = line.strip()
        if html_end is not None:
            # Inside such a block no paragraph is open, and the line carrying
            # the end condition belongs to the block — so the line AFTER it
            # starts a new one.
            para_open = False
            if (html_end.search(line) if hasattr(html_end, 'search')
                    else html_end in line):
                html_end = None
        elif HTML_OPEN_TYPE1.match(line):
            para_open = False
            if not HTML_CLOSE_TYPE1.search(line):
                html_end = HTML_CLOSE_TYPE1
        elif any(o.match(line) for o, _ in HTML_DELIM_OPENERS):
            para_open = False
            opener, term = next(
                (o, t) for o, t in HTML_DELIM_OPENERS if o.match(line))
            if term not in line[opener.match(line).end():]:
                html_end = term
        elif not stripped:
            para_open = False
        elif not para_open and INDENTED_CODE.match(line.expandtabs(4)):
            # Indented code. This arm is reachable only with no paragraph open,
            # which is exactly the rule: indented code cannot interrupt one, so
            # the same line under a paragraph is lazy continuation text and
            # falls through to the `else`.
            para_open = False
        elif dest_expected and DEST_ONLY_LINE.match(line):
            para_open = False
            title_may_follow = (
                DEST_ONLY_LINE.match(line).group('title') is None)
            dest_expected = False
            pos += len(line) + 1
            continue
        elif not para_open and DEF_LABEL_ONLY.match(line):
            para_open = False
            dest_expected = True
            title_may_follow = False
            pos += len(line) + 1
            continue
        elif title_may_follow and TITLE_ONLY_LINE.match(line):
            para_open = False
        elif not para_open and REF_DEF_LINE.match(line):
            # A definition is a block that opens no paragraph, which is what
            # keeps a RUN of them working — only the first follows a blank
            # line. Rejecting the second of `[first]: jobs.md` / `[mail]:
            # mail.md` reported a page the reader can reach as an orphan.
            para_open = False
            title_may_follow = REF_DEF_LINE.match(line).group('title') is None
            pos += len(line) + 1
            continue
        elif EMPTY_CONTAINER.match(line):
            # A container marker with nothing after it opens an EMPTY block —
            # `-` is a list item holding nothing, `>` a quote holding nothing —
            # so no paragraph is open under it and a definition there is a
            # definition. Read as prose, it left the definition unblanked and
            # its path counted as visible text. A marker WITH content still
            # opens a paragraph, and the definition below that one is lazy
            # continuation the reader can see; both are pinned.
            para_open = False
        elif para_open and SETEXT_UNDERLINE.match(line):
            # It needs an open paragraph to underline: with none, `===` is
            # ordinary text and `---` is a thematic break, which the next arm
            # already settles.
            para_open = False
        elif (ATX_HEADING.match(line) or THEMATIC_BREAK.match(line)
                or is_fence_line(line)):
            # A completed block of its own: it closes any paragraph above and
            # leaves none open below.
            para_open = False
        else:
            para_open = True
        title_may_follow = False
        dest_expected = False
        pos += len(line) + 1
    return starts


def decode_visible(txt):
    """Decode character references where a reader would see them decoded.

    `docs/guide/mail&#46;md` in prose is `docs/guide/mail.md` on screen, and the
    bare-path scan never saw the `.md` — so a page whose only route was written
    that way was reported as an orphan. Link destinations were already decoded;
    visible text was the sibling that was not.

    PROSE ONLY, and outside code spans, which is not a detail: inside a fence
    or backticks a character reference is literal — the reader sees
    `mail&#46;md` — so decoding there would invent a path nobody can read and
    mark a genuinely orphaned page reachable. Verified against markdown-it-py,
    which renders `&amp;#46;` in both.

    Offsets do not survive this, so it belongs at the end of the pipeline where
    nothing blanks by position any more.
    """
    out = []
    for kind, seg in split_fences(txt):
        if kind != 'prose':
            out.append(seg)
            continue
        def visible(t):
            # Backslash escapes go too: `docs/guide/mail\.md` in prose is
            # `docs/guide/mail.md` on screen, and the bare-path scan saw the
            # backslash and missed it. Same scoping as the references for the
            # same reason — inside a fence or backticks the escape is literal
            # and the reader sees `mail\.md`, so unescaping there would invent
            # a path nobody can read.
            return UNESCAPE.sub(r'\1', decode_char_refs(t))

        pieces, last = [], 0
        for m in CODE_SPAN.finditer(seg):
            pieces.append(visible(seg[last:m.start()]))
            pieces.append(m.group(0))
            last = m.end()
        pieces.append(visible(seg[last:]))
        out.append(''.join(pieces))
    return ''.join(out)


def _blank_href(m):
    """Blank an anchor's href VALUE, keeping the tag around it intact."""
    whole = m.group(0)
    for g in range(1, 4):
        if m.group(g) is not None:
            a = m.start(g) - m.start()
            return whole[:a] + ' ' * (m.end(g) - m.start(g)) + whole[a + (m.end(g) - m.start(g)):]
    return whole


def blank_link_dests(txt, resolved=None):
    """Blank the DESTINATION of every rendered link, leaving its label alone.

    `sub_in_prose` cannot do this: it blanks whole matches, and the label of a
    link is visible text that may itself name a path. So the match has to carry
    the label — to prove there IS a link — while only the destination's span is
    replaced.
    """
    out = []
    for kind, seg in split_fences(txt):
        if kind != 'prose':
            out.append(seg)
            continue
        protected = [(m.start(), m.end()) for m in CODE_SPAN.finditer(seg)]
        spans = []
        for lstart, _le, _a, _b, ds, de in inline_links(seg, resolved):
            if any(a <= lstart - 1 < b for a, b in protected):
                continue
            spans.append((ds, de))
        for a, b in spans:
            seg = seg[:a] + ' ' * (b - a) + seg[b:]
        out.append(seg)
    return ''.join(out)


QUOTE_PREFIX = re.compile(r'^ {0,3}(?:>[ \t]?)+', re.M)


def strip_quote_markers(txt):
    """Blank block-quote markers, space for space, so line-anchored patterns
    see what is inside the quote.

    A definition inside a quote is still a definition — `> [old]: x.md` renders
    NOTHING — but every pattern here anchors at `^ {0,3}` and so could not
    reach past the marker. The unused ones were then left in the bare-path
    scan and their destinations counted as visible navigation, which is an
    orphan passing; the used ones were not resolved at all, which is a
    reachable page reported as an orphan. Same length, so offsets stay valid
    and the ORIGINAL text is what gets blanked.

    List items are deliberately not handled. Four spaces under a `- ` item is
    the item's content indent and a definition there is invisible; the same
    four spaces at the top level are an indented CODE block whose text is on
    screen. Telling those apart needs the container tracking
    `check-migration-guides.sh` carries and this file does not — the gap
    already recorded for fences in `strip_comments`. Left alone, indented code
    stays correct.

    KNOWN GAP, and the only one here that is a false NEGATIVE — the four in
    `strip_comments` all merely block a valid change, while this one can let a
    real orphan through. Reported with a worked case and confirmed against
    markdown-it:

        - note
        ⏎
            [old]: docs/guide/mail.md

    renders as `<p>note</p>` alone. The definition is inside the item (four
    absolute spaces is two past a `- ` item's content column, short of the four
    that would make it code), so nothing is on screen — but `REF_DEF_FULL`
    anchors at `^ {0,3}`, does not match at four, and leaves the destination in
    the bare-path scan, where it counts as navigation. Six spaces there IS code
    and must keep counting, so the two cannot be told apart by indent alone.

    Not fixed here, and the reason is the shape of the fix rather than its
    size. Blanking cannot do it: unlike a `>` marker, this indentation is
    ALREADY spaces, so a length-preserving substitution changes nothing. It
    needs the content column threaded into the anchor of both definition
    patterns and into every arm of `block_starts`, across the four views built
    on this function — the block layer, which is where the rules derived from
    the spec in this file have gone wrong most often, and where a mistake
    blanks text the reader can see. That is the port of the awk container
    tracker this PR already names as a follow-up, not a patch.
    """
    return QUOTE_PREFIX.sub(lambda m: ' ' * len(m.group(0)), txt)


def sub_in_prose(pat, txt, only_at_block_start=False, view=None):
    """Blank `pat` where Markdown renders it, leaving fences alone.

    Anything that decides "this text is not rendered" has to be scoped this
    way — a construct shown inside a fence is a sample whose path is on screen.
    Every time a rule in this file was applied document-wide instead, it
    deleted a visible path; this helper exists so the scoping is one call
    rather than something to remember.
    """
    # `view` is a same-length rendering of `txt` that the pattern is matched
    # against while `txt` is what gets blanked — how a construct inside a
    # container is found without its marker having to be part of the pattern.
    src = txt if view is None else view
    out = []
    for (kind, seg), (_, vseg) in zip(split_fences(txt), split_fences(src)):
        if kind != 'prose':
            out.append(seg)
            continue
        # Code SPANS are visible too, and this helper originally missed them —
        # blanking link destinations erased an image sample written in
        # backticks. "Rendered" means outside fences AND outside code spans.
        protected = [(m.start(), m.end()) for m in CODE_SPAN.finditer(seg)]
        allowed = block_starts(vseg) if only_at_block_start else None
        pieces, last = [], 0
        for m in pat.finditer(vseg):
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


def hidden_spans(seg, protected):
    """Spans of raw HTML the reader never sees, resolved against comments by
    which construct OPENS FIRST.

    Consuming each construct whole and continuing past it is what makes the
    precedence fall out: a `<script>` reached while inside a comment is never
    the leftmost opener, and a `<!--` inside a script is skipped for the same
    reason. An unclosed opener of either kind runs to the end of the segment,
    which is what Markdown does with it.
    """
    spans, pos = [], 0
    while True:
        m = FIRST_OPENER.search(seg, pos)
        if not m:
            return spans
        if any(a <= m.start() < b for a, b in protected):
            # A sample in a code span opens nothing; step past just the marker
            # so an opener AFTER the span is still found.
            pos = m.start() + 1
            continue
        if m.group(1):
            # ...but only where a comment can actually OPEN. Line-initial it is
            # a type-2 block whatever it contains; mid-line it must be a
            # well-formed inline comment. `prose <!--> <style>…</style> -->`
            # skipped to the trailing `-->` and never saw the `<style>`, whose
            # contents the browser does not render — so a path inside it
            # counted as visible and let an orphan pass.
            bol = seg.rfind('\n', 0, m.start()) + 1
            line_initial = (seg[bol:m.start()].strip() == ''
                            and m.start() - bol <= 3)
            if line_initial:
                # A type-2 block: opens on `<!--` whatever follows, ends at the
                # first `-->`, and an unclosed one runs to the end.
                end = seg.find('-->', m.end())
                if end == -1:
                    return spans
                pos = end + 3
                continue
            # Mid-line the comment ends where the INLINE grammar says it does,
            # and that is the whole point here: `<!-->` is a complete comment,
            # so the scan must resume right after it and find the `<style>` in
            # `prose <!--> <style>…</style> -->` — searching for a later `-->`
            # skipped straight past the element whose contents are hidden.
            cm = HTML_COMMENT_INLINE.match(seg, m.start())
            pos = cm.end() if cm else m.start() + 1
            continue
        name = m.group(3) if m.group(3) is not None else m.group(5)
        if name.lower() in RAW_TEXT_NAMES:
            # `script`, `style` and `iframe` hold TEXT, so a `<script>` spelled
            # inside one is not an opener and the first close ends it.
            close = re.search(r'</' + name + r'\s*>', seg[m.end():], re.I)
            end = len(seg) if close is None else m.end() + close.end()
        else:
            # `template` is the odd one out: its contents are PARSED (a
            # `</span>` inside is consumed as markup, not kept as text), so it
            # nests for real and the first close is the wrong end. In
            # `<template><template></template> docs/guide/mail.md </template>`
            # the path is still inside the outer, inert template — Chromium
            # renders no body text at all — but ending at the first close
            # exposed it to the bare-path scan as visible navigation.
            depth, at, end = 1, m.end(), len(seg)
            nest = re.compile(
                r'<(/?)' + re.escape(name) + r'(?=[\s/>])[^>]*>', re.I)
            while depth:
                t = nest.search(seg, at)
                if not t:
                    break
                depth += -1 if t.group(1) else 1
                at = t.end()
                if not depth:
                    end = t.end()
        spans.append((m.start(), end))
        pos = end


def fold_escapes(txt):
    """Collapse the three backslash escapes that change what a construct IS.

    An escaped backslash consumes the next one, so `\\\\\\\\[x](y.md)` renders a
    literal backslash and a LIVE link while `\\\\[x](y.md)` renders literal text;
    `\\!` stops an image from being one; and `\\<` stops raw HTML, so prose
    showing literal tags around a live link keeps that link. Fixed-width
    lookbehinds cannot count a run or see past the `!`, so each folds to a
    same-LENGTH placeholder — same length because spans are blanked by offset
    downstream and the positions must stay valid.

    This is a function because two callers need it and only one had it: the
    waiver scan read unfolded text, so a page DISPLAYING `\\<!-- orphan-allow:
    … -->` — visible text, not a comment — exempted itself from the check. That
    is the most expensive false negative this gate has, since it is how a page
    opts out entirely.
    """
    return (txt.replace('\\\\', '\x00\x00')
            .replace('\\!', '\x01\x01')
            .replace('\\<', '\x02\x02'))


def definition_labels(txt):
    """Labels of definitions that actually DEFINE something.

    Prose only, code spans removed, and only where a block can start — the same
    three conditions reference resolution applies, because a definition
    demonstrated in a fence or continuing a paragraph defines nothing. Reading
    every `REF_DEF` in the raw document instead made a fenced `[img]: x.png`
    turn a literal `![docs/guide/mail.md][img]` into an image and hid a path
    the reader can see.
    """
    prose = ''.join(seg for kind, seg in split_fences(txt) if kind == 'prose')
    prose = strip_quote_markers(CODE_SPAN.sub(' ', prose))
    starts = block_starts(prose)
    out = set()
    for m in REF_DEF.finditer(prose):
        if m.start() not in starts:
            continue
        lbl = ref_label(m.group(1))
        if lbl is not None:
            out.add(lbl)
    return out


# Content a link can show that is not TEXT: a Markdown image, or an element
# that paints. An icon-only link is a real link — `<a href=…><svg>…</svg></a>`
# is visible and clickable — and stripping every tag left it looking empty.
# Only elements that reliably render are listed; an `<input type=hidden>`
# is markup, not content.
#
# `progress` and `meter` are here because they paint with no attributes at all
# and are not interactive: `<a href=…><progress></progress></a>` gives the
# anchor a 160x17 box that hit-tests inside it, so it is a link the reader can
# see and click.
#
# Measured and NOT added: `button`, `input`, `select` and `textarea` also paint
# and also hit-test inside a wrapping anchor. They are left out because
# interactive content inside `<a>` is invalid HTML that a docs page will not
# contain, and because "clicking navigates" cannot tell them apart — in a
# synthetic click every one of these navigates, since the event simply bubbles.
# Painting-without-attributes and non-interactive is the line that actually
# separates them.
#
# `picture` was on this list and is not now. An EMPTY one paints nothing and
# gives its anchor a 0x0 box, so it made `<a href=…><picture></picture></a>` a
# route — the same nothing as the empty container already rejected. Removing it
# loses no real case: the spec requires a `<picture>` to contain an `img`, and
# that `img` is what paints and is matched here anyway (measured: a picture
# wrapping one gives the anchor 20x17 and navigates).
#
# `iframe` was in this list for one round and is deliberately not now, for ONE
# reason rather than the two given here before: its CONTENTS are hidden (see
# `HIDDEN_TAGS`), so the whole element is blanked before this test ever runs
# and listing it did nothing but conflict. The second reason this comment used
# to give — that an anchor wrapping one "has no clickable area of its own" —
# is simply false, and measuring for `progress` is what exposed it: an iframe
# gives its anchor a 304x17 box that hit-tests inside it, exactly like the
# elements that are listed. The conclusion held up; that half of the argument
# for it never should have been written down unchecked.
#
# `hr` and the FORM CONTROLS join the list on the same evidence everything
# else here rests on — an anchor wrapping one, measured in Chromium for its
# box and hit-tested at its centre:
#
#     hr 1264x2 IN    input 185x17 IN    select 22x17 IN
#     textarea 182x17 IN                 button 16x17 IN
#
# `progress` and `meter` were already listed and are the same family, so the
# other four were the omission. `hr` is not that family: it is a block-level
# rule that paints a line, and an anchor wrapping one is a wide, thin, wholly
# clickable target.
#
# DELIBERATELY STILL OUT, each measured rather than reasoned:
#   `br` 0x17 and `wbr` 0x17 — full line height, ZERO width, nothing to click.
#   `output`, `datalist`, `template` — 0x0; they do not paint at all.
#   `audio` — 0x0 BARE, and 300x17 only with `controls`. This list matches tag
#   names and cannot read attributes, so listing it would call a silent,
#   invisible element content and let an orphan through. `video` is listed and
#   is not the same case: bare `<video></video>` is already 300x17. The
#   sibling-symmetry argument for adding `audio` is wrong here, and only
#   measuring the bare forms of both shows why.
ANY_IMAGE = re.compile(
    r'!\[|<(?:img|svg|video|canvas|object|embed|progress|meter'
    r'|hr|select|textarea|button)\b',
    re.I)
# `input` was in that list for one round and had to come out: it is the one
# element here whose painting depends on an ATTRIBUTE. `<input type="hidden">`
# is 0x0 with no clickable area, so a tag-name match called an empty anchor
# content and let an orphan through — the very failure the `audio` note above
# describes, which I wrote in the same commit that added `input`. The argument
# was right and I did not apply it to the tag next to it.
#
# The type is read rather than the tag rejected outright, because every OTHER
# type paints — text 185x17, checkbox 20x17, submit 57x17, image 57x17, and a
# bare `<input>` 185x17 — so dropping the element would strand real links.
_INPUT_TAG = re.compile(
    r'<input\b(?:' + _TWS + r'+' + ATTR + r')*' + _TWS + r'*/?>', re.I)
# Measured, and none of it is guessable from the spec text alone:
#   the value must be EXACTLY `hidden` — `type=" hidden "`, `type="hidden "`
#   and `type="hiddenx"` all fall back to `text` and paint, so no stripping
#   the value;
#   matching is case-insensitive, attribute name and value both, and the value
#   may be unquoted: `<input TYPE=HIDDEN>` is 0x0;
#   the FIRST `type` wins when an author repeats it — `type="hidden"
#   type="text"` is 0x0 while `type="text" type="hidden"` paints — which is why
#   this searches for one rather than scanning them all.
# The global `hidden` attribute needs nothing here: `<input hidden>` is already
# blanked by the hidden-subtree pass before `has_content` sees the span,
# verified against a corpus rather than assumed.
_TYPE_ATTR = re.compile(
    r'\btype' + _TWS + r'*=' + _TWS + r'*'
    r'(?:"([^"]*)"|\'([^\']*)\'|([^\s"\'=<>`]+))', re.I)


def decode_attribute(value):
    """An attribute value as the DOM holds it, references resolved.

    THE ONE RULE EVERY ATTRIBUTE READER HERE NEEDS, stated once because it has
    now been missed twice in three commits — for `style` and then, in the very
    next commit, for `type`. The HTML tokenizer resolves character references
    in an attribute value before anything downstream sees it, so
    `type="hidd&#101;n"` IS the hidden type and `style="display&#58;none"` IS
    `display:none`. Comparing the encoded source instead misses both.

    `html.unescape`, never `decode_char_refs`: an attribute is tokenizer
    territory, so the lenient rules apply — a numeric reference without its
    semicolon decodes here and would not in a Markdown destination. Checked
    against Chromium's own `getAttribute` on seven spellings, semicolonless
    and hex among them; they agree, including on leaving an unknown named
    reference like `&NotAThing;` alone.

    DECODE, THEN COMPARE EXACTLY. Decoding can introduce whitespace that
    changes the answer rather than being noise to trim: `type="&#32;hidden"`
    becomes ` hidden`, which is not the hidden type and paints 185x17.
    """
    return html.unescape(value)


def _input_paints(tag):
    """Whether an `<input>` renders anything the reader could click."""
    m = _TYPE_ATTR.search(tag)
    if not m:
        return True
    value = next(g for g in m.groups() if g is not None)
    return decode_attribute(value).lower() != 'hidden'


def _paints_an_input(view, raw_span):
    """Whether the span holds an `<input>` the reader can see.

    The tags are FOUND in the masked view, so one inside a comment or a hidden
    subtree is already blanked and cannot count. The type is READ from `raw`
    at the same offsets, because attribute VALUES are blanked upstream —
    `<input type="hidden">` arrives here as `<input type="      ">`, which
    reads as an ordinary text input and calls an empty anchor content.

    Same split, and the same reason, as the `hidden` attribute and the inline
    `display:none` a few lines above: a bare attribute NAME survives masking
    and a value does not. Offsets line up because every masker replaces space
    for space.
    """
    for m in _INPUT_TAG.finditer(view):
        tag = m.group(0)
        if raw_span is not None and len(raw_span) == len(view):
            tag = raw_span[m.start():m.end()]
        if _input_paints(tag):
            return True
    return False
# An element carrying `hidden`, and its OPENING tag only — the matching close
# is found by a scan, because same-name nesting cannot be balanced by a regular
# expression. `<span hidden><span></span>Mail</span>` closed at the INNER
# `</span>` and handed `Mail` back as though it were a label.
HIDDEN_OPEN = re.compile(
    r'<([A-Za-z][A-Za-z0-9-]*)(?:' + _TWS + r'+' + ATTR + r')*?'
    + _TWS + r'+hidden(?:' + _TWS + r'*=' + _TWS + r'*(?:' + _Q + r'|'
    + _Q1 + r'|[^\s"\'=<>`]+))?(?:' + _TWS + r'+' + ATTR + r')*'
    + _TWS + r'*/?>', re.I)

# The other way an element is not shown: `style="display:none"`. Unlike the
# `hidden` ATTRIBUTE this is a CSS declaration, so it reaches SVG and MathML
# too — Chromium reports `<svg style="display:none" width=40 height=40>` as
# `display: none` with a 0x0 box, where the same element carrying `hidden`
# still paints. That is why `FOREIGN_ROOTS` is not consulted for this pattern:
# the exclusion is a property of the attribute, not of hiding.
# Deliberately narrow — only a literal `display:none` in an inline style. This
# is not a CSS engine and does not pretend to be one: a stylesheet, a class, or
# a computed rule is invisible to it, and a page hiding a link that way is a
# gap this cannot see.
#
# WHICH declaration wins is decided rather than pattern-matched, because a
# style attribute is a cascade and the last valid declaration takes effect.
# `display:none; display:block` renders a 1264px wide, clickable link, so a
# pattern matching any occurrence of `display:none` rejected a live one. The
# rules below were measured in Chromium, not read off a spec.
STYLE_ATTR_OPEN = re.compile(
    r'<([A-Za-z][A-Za-z0-9-]*)(?:' + _TWS + r'+' + ATTR + r')*?'
    + _TWS + r'+style' + _TWS + r'*=' + _TWS + r'*'
    r'(?:"([^"]*)"|\'([^\']*)\'|([^\s"\'=<>`]+))'
    r'(?:' + _TWS + r'+' + ATTR + r')*' + _TWS + r'*/?>', re.I)
# The UNQUOTED branch is the general attribute grammar's, and leaving it out
# meant `<a style=display:none href=mail.md>` — valid HTML that Chromium hides
# with a 0x0 box — was read as having no inline style at all, so an empty
# anchor counted as a route. Its own grammar is what bounds it: an unquoted
# value ends at the first space, so `style=display: none` really does set only
# `display:` and really does paint, measured, and that is not a case to
# "fix" by allowing spaces.
# The property boundary matters on its own: `--display:none` is a custom
# PROPERTY and changes nothing, so an unbounded match rejected a live link.
_DISPLAY_DECL = re.compile(r'\s*display' + _TWS + r'*:([^;]*)', re.I)
# `visibility` hides differently and has to be asked separately: the element
# keeps its BOX — a hidden anchor is still 30x17 — but paints nothing and
# hit-tests through to whatever is behind it, so it is not a route.
_VISIBILITY_DECL = re.compile(r'\s*visibility' + _TWS + r'*:([^;]*)', re.I)
# The values that actually restore a hidden ancestor's visibility — an
# ALLOWLIST, because everything else leaves it hidden and the blocklist this
# replaces got that backwards. Measured on a `visibility:hidden` anchor, each
# value set on a span inside it and hit-tested:
#
#   visible   visible   initial   visible      <- these two, and only these
#   hidden    hidden    collapse  hidden
#   inherit   hidden    unset     hidden       <- CSS-wide, and `visibility`
#   revert    hidden    revert-layer hidden       inherits, so they stay hidden
#   bogus     hidden                           <- invalid, declaration dropped
#
# `initial` is in because `visibility`'s initial value IS `visible`, which is
# the one place the CSS-wide keywords part company. Allowlisting is also the
# safe direction: an unrecognised value keeps the anchor rejected, which is a
# false orphan — loud, and fixed the moment someone reads the failure — where
# the blocklist made it a silently recorded route.
_VISIBILITY_SHOWN = ('visible', 'initial')
_IMPORTANT = re.compile(r'!' + _TWS + r'*important' + _TWS + r'*$', re.I)
_CSS_COMMENT = re.compile(r'/\*.*?\*/|/\*.*', re.S)


def _declarations(style):
    """Split a style attribute on TOP-LEVEL semicolons.

    A semicolon inside a string or a `url()` does not end a declaration, and
    neither does the text around it start one. Scanning for the `display`
    property anywhere in the attribute read a declaration out of another
    declaration's VALUE, and every spelling below leaves `display: none` in
    force — measured in Chromium, which reports `display: none` and a 0x0 box
    for all four:

        display:none; --x:"display:block;"     a quoted custom property
        display:none; --x:display:block        an unquoted one
        display:none; background:url("display:block;")
        display:none; content:"display:block"

    Only the first was reported; the other three are the same hole, and a
    property-boundary lookbehind closes none of them. This is the fifth round
    on this attribute, so it is now split before it is searched rather than
    searched with a cleverer pattern.

    A `(` opens a nesting level that `)` closes, a quote runs to its matching
    quote, and a backslash escapes the next character inside one. Unclosed
    runs to the end, which is what a browser does with them.
    """
    out, buf, depth, quote = [], [], 0, None
    i, n = 0, len(style)
    while i < n:
        c = style[i]
        if quote:
            buf.append(c)
            if c == '\\' and i + 1 < n:
                buf.append(style[i + 1])
                i += 2
                continue
            if c == quote:
                quote = None
        elif c in '"\'':
            quote = c
            buf.append(c)
        elif c == '(':
            depth += 1
            buf.append(c)
        elif c == ')':
            depth = max(0, depth - 1)
            buf.append(c)
        elif c == ';' and not depth:
            out.append(''.join(buf))
            buf = []
        else:
            buf.append(c)
        i += 1
    out.append(''.join(buf))
    return out


def _display_none(style):
    """Whether the EFFECTIVE `display` in an inline style is `none`.

    Measured, all four in Chromium:
      `display:none; display:block`             -> block  (later wins)
      `display:block; display:none`             -> none
      `display:none !important; display:block`  -> none   (important wins)
      `display:none; display:block !important`  -> block

    KNOWN LIMIT, and it is a false negative: a browser DROPS an invalid
    declaration, so `display:none; display:bogus` stays hidden, while this
    takes the later value at face value and calls the element visible. Telling
    valid `display` values from invalid ones means carrying the keyword list
    and its multi-keyword forms, which is a CSS engine — the thing this
    deliberately is not. Inline CSS in this corpus is rare and invalid inline
    CSS rarer still.
    """
    # CSS comments are not declarations. `color:red; /* display:none; */`
    # renders a visible 30px link, but reading the comment as a declaration
    # rejected it. An unterminated `/*` runs to the end of the attribute, so
    # both forms are blanked — space for space, since nothing here depends on
    # the length but the habit is what keeps offsets safe elsewhere.
    return _effective(style, _DISPLAY_DECL) == 'none'


def _effective(style, decl):
    """The winning value of one property in an inline style, lowercased.

    The cascade, shared rather than written once per property — `display` and
    `visibility` differ only in which declaration they look for, and this file
    has learned what happens when a rule lives in one place and not its
    sibling.
    """
    # CSS comments are not declarations. `color:red; /* display:none; */`
    # renders a visible 30px link, but reading the comment as a declaration
    # rejected it. An unterminated `/*` runs to the end of the attribute, so
    # both forms are blanked — space for space, since nothing here depends on
    # the length but the habit is what keeps offsets safe elsewhere.
    style = _CSS_COMMENT.sub(lambda c: ' ' * len(c.group(0)), style)
    winner, winner_important = None, False
    for piece in _declarations(style):
        m = decl.match(piece)
        if not m:
            continue
        value = m.group(1).strip()
        important = bool(_IMPORTANT.search(value))
        if important:
            value = _IMPORTANT.sub('', value).strip()
        # A later declaration wins, unless an earlier one was `!important`
        # and this one is not.
        if important or not winner_important:
            winner, winner_important = value, important
    return winner.lower() if winner is not None else None


def _visibility_hidden(style):
    """Whether an inline style makes an element paint nothing.

    `hidden` and `collapse` both do — measured in Chromium: the anchor keeps a
    30x17 box, but a hit test at its centre lands on the BODY behind it, so
    there is nothing to click.
    """
    return _effective(style, _VISIBILITY_DECL) in ('hidden', 'collapse')


def mask_invisible_subtrees(view, src):
    """Blank every `visibility:hidden` subtree, MINUS what re-shows inside it.

    Kept apart from `mask_hidden_subtrees` for the one reason that makes
    `visibility` different from `display` and the `hidden` attribute: it
    INHERITS, and a descendant can set it back, so a subtree is not simply
    gone. `<span style="visibility:hidden"><span style="visibility:visible">
    Mail</span></span>` paints `Mail` and hit-tests inside it.

    Blanked space for space, like every other masker here, so the offsets the
    callers share stay valid. `src` carries the same span with attribute
    values intact, since the view has already had them blanked.
    """
    at = 0
    while True:
        m = style_hidden_search(src, at, _VISIBILITY_DECL, _VISIBILITY_HIDDEN)
        if not m:
            return view
        # The same guard `hidden_open` applies: a tag the view has already
        # blanked — inside a comment or a script — hides nothing.
        if not (m.start() < len(view) and view[m.start()] == '<'):
            at = m.start() + 1
            continue
        end = element_end(view, m, m.group(1))
        keep = _shown_spans(view, src, m.end(), end)
        blanked = list(view[m.start():end])
        for a, b in keep:
            for i in range(a - m.start(), b - m.start()):
                blanked[i] = view[m.start() + i]
        for i in range(len(blanked)):
            off = m.start() + i
            if not any(a <= off < b for a, b in keep):
                blanked[i] = ' '
        view = view[:m.start()] + ''.join(blanked) + view[end:]
        at = end


def _shown_spans(view, src, start, end):
    """Subtrees inside `[start, end)` that set `visibility` back to visible."""
    spans, at = [], start
    while at < end:
        m = style_hidden_search(src, at, _VISIBILITY_DECL, _VISIBILITY_SHOWN)
        if not m or m.start() >= end:
            return spans
        if not (m.start() < len(view) and view[m.start()] == '<'):
            at = m.start() + 1
            continue
        stop = min(element_end(view, m, m.group(1)), end)
        spans.append((m.start(), stop))
        at = stop
    return spans


def _invisible_anchor(tag_src, content_src):
    """Whether an anchor paints nothing because of `visibility`.

    UNLIKE `display:none`, this one has an escape, and the escape is why it
    cannot just be added to the hidden-tag machinery: `visibility` INHERITS,
    and a descendant may set it back. Measured in Chromium —
    `<a style="visibility:hidden"><span style="visibility:visible">Mail</span></a>`
    paints `Mail` and a hit test on it lands inside the anchor, so it IS a
    route, while the same anchor without that span hit-tests through to the
    BODY behind it and is not.

    So: hidden by the anchor's own style, and no tag inside its content
    setting a visible `visibility`. The content is read from RAW, like the
    anchor's own style, because values are blanked upstream.
    """
    if not _visibility_hidden(_style_value_str(tag_src)):
        return False
    for m in STYLE_ATTR_OPEN.finditer(content_src):
        value = _style_value(m)
        if value and _effective(value, _VISIBILITY_DECL) in _VISIBILITY_SHOWN:
            return False
    return True


def _style_value_str(tag_src):
    """The inline style of a single tag, or `''` if it has none."""
    m = STYLE_ATTR_OPEN.fullmatch(tag_src)
    return (_style_value(m) or '') if m else ''


def _style_value(m):
    """The style attribute's text from a `STYLE_ATTR_OPEN` match, DECODED.

    The HTML tokenizer resolves character references in an attribute value
    before CSS ever sees it, so `style="display&#58;none"` reaches the engine
    as `display:none` and hides the element. Handing the encoded source to the
    cascade missed the declaration and recorded the href as a live route — an
    orphan passing.

    `html.unescape`, not `decode_char_refs`: this is an HTML attribute, so the
    tokenizer's lenient rules apply rather than CommonMark's strict ones. That
    is the same split `add_relative` already makes between a raw href and a
    Markdown destination, and it matters — all of these hide the anchor,
    measured in Chromium, and only the first is a CommonMark reference:

        display&#58;none      display&#58none      &#100;isplay:none
        display&colon;none    display:&#110;one    display:none&#59;color:red

    Decoding before the split, not after, so `content:&quot;a;display:block&quot;`
    becomes a real quoted value that `_declarations` can see the string in.

    Length changes here, which is safe only because nothing reads offsets
    INSIDE the attribute: both callers mask by the whole tag's span.

    KNOWN LIMIT: inside an attribute the tokenizer does NOT decode a
    semicolonless NAMED reference followed by `=` or an alphanumeric, and
    `html.unescape` decodes it anyway. Reaching a wrong answer through that
    would take a style whose undecoded form is not `display:none` and whose
    over-decoded form is, which is a contrivance; writing an attribute-aware
    tokenizer to rule it out is not worth it here.
    """
    raw = next((g for g in m.group(2, 3, 4) if g is not None), None)
    return decode_attribute(raw) if raw else raw


_VISIBILITY_HIDDEN = ('hidden', 'collapse')


def style_hidden_search(txt, at, decl=None, wanted=None):
    """Next opening tag whose inline style computes to a given value.

    Defaults to `display:none`, which is every caller but the visibility
    passes; they ask for `_VISIBILITY_DECL` with the value set they care
    about, since that property is asked in both directions — which subtrees
    are hidden, and which ones inside them are shown again.
    """
    pos = at
    while True:
        m = STYLE_ATTR_OPEN.search(txt, pos)
        if not m:
            return None
        value = _style_value(m) or ''
        if decl is None:
            if _display_none(value):
                return m
        elif _effective(value, decl) in wanted:
            return m
        pos = m.start() + 1


def style_hidden_fullmatch(tag):
    """Whether a complete opening tag hides itself with inline CSS."""
    m = STYLE_ATTR_OPEN.fullmatch(tag)
    return m if m and _display_none(_style_value(m) or '') else None


def hidden_open(view, src, at):
    """The next opening tag that hides itself, by attribute or by inline CSS.

    Two strings because neither alone works. `hidden` is a bare attribute NAME
    and survives into the masked view, but `display:none` lives in an attribute
    VALUE, and those are blanked upstream so a tag boundary can be found before
    quoted markup is mistaken for markup — by here the tag reads
    `style="        "`. So the CSS is looked for in `src`, where the text is
    intact, and a hit counts only where the masked view still has a `<` at that
    offset. That second test is what keeps a `<span style="display:none">`
    written inside a comment or a script from masking anything: the view has
    already blanked it, and the offsets line up because every view here is
    length-preserving.
    """
    a = HIDDEN_OPEN.search(view, at)
    b, pos = None, at
    while True:
        c = style_hidden_search(src, pos)
        if not c:
            break
        if c.start() < len(view) and view[c.start()] == '<':
            b = c
            break
        pos = c.start() + 1
    if a and b:
        return a if a.start() <= b.start() else b
    return a or b


# HTML void elements. They have no close tag and no contents, so a `hidden` one
# ENDS AT ITS OPENING TAG — it hides itself and nothing after it. Searching for
# the close that does not exist ran the scan below off the end of the span, so
# `<a href="mail.md"><input hidden>Mail</a>` — a link the browser renders as the
# clickable label `Mail` — came back empty and its target was reported orphaned.
# Legacy `param`/`keygen` are here because browsers still parse them as void.
VOID_ELEMENTS = frozenset((
    'area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta',
    'source', 'track', 'wbr', 'param', 'keygen'))
# The two elements that enter FOREIGN CONTENT, where XML rules apply instead of
# HTML ones. Two consequences here, and both are needed — an earlier round used
# one of them alone and got the other wrong.
#
# 1. `hidden` DOES NOT HIDE THEM. The attribute has no power of its own: it
#    works through the one UA-stylesheet rule `[hidden] { display: none }`, and
#    that stylesheet is namespaced to HTML, so it never matches an SVG or
#    MathML element. Chromium reports `<svg hidden width=40 height=40>` as
#    `display: inline` with a 40x40 box — it paints, and an anchor wrapping one
#    is an icon link the reader can click. Inline `display:none` is CSS rather
#    than an attribute and DOES hide them, so this applies to `HIDDEN_OPEN`
#    only.
# 2. A trailing `/>` REALLY DOES CLOSE THEM, where on every HTML element the
#    parser drops the slash and the element stays open. So a `<svg/>` nested
#    inside a hidden one opens no new level, and counting it as one left the
#    outer close unable to return depth to zero: in
#    `<svg style="display:none"><svg/></svg>Mail`, the visible `Mail` was
#    masked to the end of the anchor and its target reported as an orphan.
#
# These were briefly a single `SELF_CLOSING_ROOTS` set applying (2) to the
# `hidden` path, which was the wrong pairing — (2) is real but only matters
# once something has actually hidden the element, and `hidden` never does.
FOREIGN_ROOTS = frozenset(('svg', 'math'))

# Elements whose CONTENTS the tokenizer reads as text, so a tag spelled inside
# one is not a tag. `<span hidden><textarea></span>Secret</textarea></span>`
# renders nothing — that `</span>` is textarea text — but the balancing scan
# stopped at it and handed `Secret` back as a label, hiding an orphan.
# Membership was measured, not taken from the spec: for each candidate,
# `<E id=E></span>Secret</E>` and then whether `E.textContent` still contains
# the literal `</span>`. `pre` and `code` do not (they only LOOK verbatim);
# these ten do.
# Elements that CANNOT nest in themselves: a second opener implicitly closes
# the first, it does not open a level inside it. `<p hidden>Secret<p>Mail</p>`
# leaves `Mail` visible and clickable, but the balancing scan counted that
# second `<p>` as depth 2, never returned to zero, and masked the label to the
# end of the anchor — a live link reported as an orphan.
#
# Measured one at a time, not listed from the spec: for each, whether the
# FIRST element's `textContent` still contains text written after the second
# opener. `div` and `span` do (they nest properly and are correctly absent);
# these fifteen do not.
_OPTIONAL_END = (
    'p', 'li', 'dd', 'dt', 'td', 'th', 'tr', 'rt', 'rp', 'option', 'optgroup',
    'thead', 'tbody', 'tfoot', 'caption', 'colgroup')
# `p` is the one with a WIDE closer set: any block element ends it, not just
# another `p`. `<p hidden>Secret<div>Mail</div>` leaves `Mail` visible, and
# recognising only a second `<p>` masked it to the end of the anchor.
# Measured, not read off the spec, and the spec would have misled: `details`
# is listed there as closing a paragraph and in Chromium does NOT, while
# `dir`, `listing` and `search` do. Inline elements (`span`, `em`, `a`, `code`)
# do not, which is what keeps this from swallowing ordinary markup.
_P_CLOSERS = frozenset((
    'address', 'article', 'aside', 'blockquote', 'dir', 'div', 'dl',
    'fieldset', 'figcaption', 'figure', 'footer', 'form', 'h1', 'h2', 'h3',
    'h4', 'h5', 'h6', 'header', 'hgroup', 'hr', 'listing', 'main', 'menu',
    'nav', 'ol', 'p', 'pre', 'search', 'section', 'table', 'ul', 'xmp'))
# What ENDS each element that has an optional end tag. The rest close only on
# their own name, which was measured too: a `<div>` does not end an `<li>`, and
# `dd`/`dt` end each other but nothing else. `p` is genuinely the odd one.
IMPLICIT_CLOSERS = {name: frozenset((name,)) for name in _OPTIONAL_END}
IMPLICIT_CLOSERS['p'] = _P_CLOSERS
IMPLICIT_CLOSERS['dd'] = IMPLICIT_CLOSERS['dt'] = frozenset(('dd', 'dt'))

RAW_TEXT_NAMES = frozenset((
    'textarea', 'title', 'script', 'style', 'xmp', 'iframe', 'noembed',
    'noframes', 'noscript', 'plaintext'))
RAW_TEXT_OPEN = re.compile(
    r'<(' + '|'.join(sorted(RAW_TEXT_NAMES)) + r')(?=[\s/>])(?:' + _TWS + r'+'
    + ATTR + r')*' + _TWS + r'*/?>', re.I)


def _raw_text_end(txt, name, at):
    """Where an element's raw-text contents stop.

    `plaintext` has no end: the tokenizer never leaves it, so everything after
    it is text — including its own apparent close tag.
    """
    if name.lower() == 'plaintext':
        return len(txt)
    m = re.compile(r'</' + re.escape(name) + r'(?=[\s>])[^>]*>', re.I).search(
        txt, at)
    return m.end() if m else len(txt)


# `inert` does not hide, it DEACTIVATES. An inert anchor still paints — 30x17
# in Chromium, same as any other — but focus and activation are suppressed, so
# clicking it does nothing and the reader cannot follow it. A link nobody can
# follow is not a route, so it cannot make a page reachable.
#
# The attribute applies to the whole SUBTREE, which is why this is a span scan
# rather than a test on the anchor alone: an `<a>` inside `<div inert>` is just
# as dead, and that was measured too.
INERT_OPEN = re.compile(
    r'<([A-Za-z][A-Za-z0-9-]*)(?:' + _TWS + r'+' + ATTR + r')*?'
    + _TWS + r'+inert(?:' + _TWS + r'*=' + _TWS + r'*(?:' + _Q + r'|'
    + _Q1 + r'|[^\s"\'=<>`]+))?(?:' + _TWS + r'+' + ATTR + r')*'
    + _TWS + r'*/?>', re.I)


def inert_spans(view, src):
    """Spans of the document that `inert` has deactivated.

    Read off `src`, where attribute values are intact, but a hit counts only
    where the masked `view` still has a `<` — so an inert element written
    inside a comment or a script deactivates nothing. Same guard, and same
    reason, as the inline-CSS path in `hidden_open`.

    Bare PATHS inside an inert subtree still count elsewhere: the text is on
    screen and a reader can retype it. Only the links are dead.
    """
    spans, at = [], 0
    while True:
        m = INERT_OPEN.search(src, at)
        if not m:
            return spans
        if not (m.start() < len(view) and view[m.start()] == '<'):
            at = m.start() + 1
            continue
        # BALANCED over the masked view, not `src`. Only the OPENING tag has
        # to be read from `src` (for its attributes); the close that ends the
        # subtree is structure, and a `</div>` written inside a comment or a
        # script is not structure. Balancing over `src` ended the span at the
        # spelling in `<div inert><!-- </div> --><a href=…>Mail</a></div>`,
        # letting an anchor the browser will not activate count as a route.
        # Offsets line up because both views are length-preserving.
        end = element_end(view, m, m.group(1))
        spans.append((m.start(), end))
        at = max(end, m.end())


ANY_ELEMENT_TAG = re.compile(r'<(/?)([A-Za-z][A-Za-z0-9-]*)(?=[\s/>]|$)')


def _open_names(txt, at):
    """Element names still open at offset `at`.

    An unmatched end tag is IGNORED by the parser — `</bogus>` with no `bogus`
    open closes nothing — so a close tag can only end the scanned element if
    the thing it names is actually open around it. Without this the scan
    treated every close it did not recognise as an ancestor's, and a stray end
    tag inside an inert or hidden subtree ended it early.
    """
    stack, pos = [], 0
    while pos < at:
        t = ANY_ELEMENT_TAG.search(txt, pos)
        if not t or t.start() >= at:
            break
        r = RAW_TEXT_OPEN.search(txt, pos)
        if r and r.start() < at and r.start() <= t.start():
            pos = _raw_text_end(txt, r.group(1), r.end())
            continue
        tname = t.group(2).lower()
        gt = txt.find('>', t.end())
        if t.group(1):
            if tname in stack:
                del stack[len(stack) - 1 - stack[::-1].index(tname):]
        elif (tname not in VOID_ELEMENTS
              and not (tname in FOREIGN_ROOTS and gt != -1
                       and txt[gt - 1] == '/')):
            while stack and tname in IMPLICIT_CLOSERS.get(stack[-1], ()):
                stack.pop()
            stack.append(tname)
        pos = t.end()
    return set(stack)


def element_end(txt, m, name):
    """Where the element opened by match `m` ends, contents and all.

    A STACK, not a depth counter. Depth counting only ever saw this element's
    own name, so it could not tell an ancestor's close tag from anything else
    — and an ancestor's close ends this element too. `<div><span hidden>Secret
    </div>Mail` leaves `Mail` visible, and so does `<ul><li hidden>Secret</ul>
    Mail`, because in both the parent's close takes the open child with it.
    Six separate findings arrived against the counter, each another HTML rule;
    tracking what is actually open answers them together.

    The rules it applies, each measured rather than read off the spec (see
    `VOID_ELEMENTS`, `RAW_TEXT_NAMES`, `FOREIGN_ROOTS`, `IMPLICIT_CLOSERS`):

      - a void element is the whole of itself, and opens nothing;
      - a raw-text element's contents are text, so tags inside it are not tags;
      - in foreign content `/>` really closes, so `<svg/>` opens no level;
      - an element with an optional end tag is ended by an opener that
        implicitly closes it — any block element for a `<p>`, another `<li>`
        for an `<li>`;
      - a close tag for something NOT open inside this element belongs to an
        ancestor, and ends this element where it appears.

    Running out of tags is not a bug: an unclosed element is closed implicitly
    by its parent, so ending at the end of the span is what the browser shows
    — `<a …><span hidden>Mail</a>` is an empty link.
    """
    lname = name.lower()
    if lname in VOID_ELEMENTS:
        return m.end()
    if lname in RAW_TEXT_NAMES:
        return _raw_text_end(txt, name, m.end())
    closers = IMPLICIT_CLOSERS.get(lname)
    outer = _open_names(txt, m.start())
    stack, pos = [], m.end()
    while True:
        t = ANY_ELEMENT_TAG.search(txt, pos)
        # A raw-text element opening first means the tags after it are text.
        r = RAW_TEXT_OPEN.search(txt, pos)
        # `<=`, not `<`: both patterns match a raw-text opener at the SAME
        # offset, and losing that tie pushed `<textarea>` on the stack as an
        # ordinary element instead of skipping its contents, so a `</span>`
        # written inside it counted as structure again.
        if r and (not t or r.start() <= t.start()):
            pos = _raw_text_end(txt, r.group(1), r.end())
            continue
        if not t:
            return len(txt)
        tname = t.group(2).lower()
        gt = txt.find('>', t.end())
        if t.group(1):
            if tname in stack:
                # A close for something opened inside: unwind to the INNERMOST
                # one. `index` finds the oldest, so closing the inner `div` of
                # `<div><span><div></div></span>` cleared the whole stack and
                # the following `</span>` was then mistaken for an ancestor's
                # close, ending the subtree early.
                del stack[len(stack) - 1 - stack[::-1].index(tname):]
                pos = t.end()
                continue
            if tname == lname:
                return len(txt) if gt == -1 else gt + 1
            if tname in outer:
                # Nothing HERE opened it but something around us did, so it
                # closes an ANCESTOR — which takes this element with it.
                return t.start()
            # Matches nothing open in either direction: the parser drops it,
            # and so must this. `<div inert></bogus><a …>Mail</a></div>` keeps
            # the anchor inert, and ending the span at `</bogus>` made a dead
            # link a route.
            pos = t.end()
            continue
        if not stack and closers and tname in closers:
            # Only while this element is still the innermost one: an implicit
            # closer nested deeper belongs to whatever is open there.
            return t.start()
        if (tname not in VOID_ELEMENTS
                and not (tname in FOREIGN_ROOTS and gt != -1
                         and txt[gt - 1] == '/')):
            stack.append(tname)
        pos = t.end()


def mask_hidden_subtrees(txt, src=None):
    """Blank every `hidden` element, contents and all, space for space.

    Nesting is why this is a scan: the close that ends a `hidden` element is
    the one that balances it, not the first one with the same name.
    """
    out = txt
    # `src` carries the same span with attribute VALUES intact, for the inline
    # CSS that the masked view has already blanked. Both are blanked in step so
    # a subtree is not found twice.
    ref = out if src is None else src
    # Where to resume. A tag can be passed over rather than masked, so the
    # scan cannot restart from zero and rely on blanks to make progress.
    at = 0
    while True:
        m = hidden_open(out, ref, at)
        if not m:
            return out
        name = m.group(1)
        lname = name.lower()
        # Only the ATTRIBUTE spares svg and math. `display:none` is CSS and
        # hides them like anything else, so the exclusion is scoped to the
        # pattern that matched rather than to the tag name alone.
        if lname in FOREIGN_ROOTS and m.re is HIDDEN_OPEN:
            # The attribute does not apply, so this is ordinary visible
            # content: step over the tag without masking anything.
            at = m.end()
            continue
        end = element_end(out, m, name)
        out = out[:m.start()] + ' ' * (end - m.start()) + out[end:]
        ref = ref[:m.start()] + ' ' * (end - m.start()) + ref[end:]
        at = end


# Any HTML tag, open or close, quote-aware so a `>` inside an attribute
# does not end it early.
ANY_TAG = re.compile(
    r'<[A-Za-z][A-Za-z0-9-]*(?:' + _TWS + r'+' + ATTR + r')*' + _TWS + r'*/?>'
    r'|</[A-Za-z][A-Za-z0-9-]*' + _TWS + r'*>', re.I)


# Characters that occupy no width, so a label made only of them is a link with
# nothing to click. `[&#8203;](mail.md)` decodes to U+200B, which Python's
# `.strip()` keeps because it is not whitespace, so the anchor counted as a
# label and its target as reachable.
#
# Measured one by one as the sole content of an anchor, not taken from a
# category: these six give it a 0px box. U+00A0 NBSP is deliberately NOT among
# them — it paints 4px — which also means Python's `.strip()`, which DOES treat
# it as whitespace, is wrong about it in the other direction. That is a false
# positive of its own, unreported and left alone rather than folded in here.
_ZERO_WIDTH = {ord(c): None for c in '\u200b\u200c\u200d\u2060\ufeff\u00ad'}


def has_content(image_span, masked_span, raw_span=None):
    """Whether a link's content leaves the reader anything to click.

    Two views, because neither alone is right. The MASKED one has comments,
    hidden HTML AND images blanked, so it answers "is there text". The other
    keeps images but has comments and hidden HTML removed, so it answers "is
    there an image" — and only one the page actually shows: reading the raw
    source instead counted the `![fake](x.png)` inside
    `<a href=…><!-- ![fake](x.png) --></a>`, an anchor that renders nothing.
    """
    # Tags are stripped before asking whether there is text: an empty element
    # is markup, not content. `<a href=…><span></span></a>` has no text and no
    # clickable area, and counting its tag SOURCE as content made it a route —
    # the same nothing as `<a href=…></a>`, spelled with more bytes.
    # A `hidden` subtree is not shown, so neither its text nor an image inside
    # it is content. `<a href=…><span hidden>Mail</span></a>` renders an empty
    # link, and stripping tags left `Mail` behind looking like a label.
    #
    # KNOWN GAP, a false negative, and one that a text rule provably cannot
    # close. `[**<!-- c -->**](mail.md)` renders `<strong>` around nothing, so
    # the link is empty — but `[**   **](mail.md)` renders the six visible
    # characters `**   **`, because an opener followed by whitespace is not
    # left-flanking and no emphasis forms. After the comment is masked space
    # for space the two are the SAME STRING, so no test applied here can
    # separate them: masking destroys the character that decided it. Stripping
    # leftover delimiters would call the second one empty and orphan a page
    # whose link works. Closing it means running emphasis parsing, not adding
    # a rule — the same answer as the list-item definitions in
    # `strip_quote_markers`, and for the same reason.
    # The RAW span goes along only so inline `display:none` can be read; the
    # views themselves are what get blanked.
    image_span = mask_hidden_subtrees(image_span, raw_span)
    masked_span = mask_hidden_subtrees(masked_span, raw_span)
    # `visibility` is masked separately and AFTER, because it is the one form
    # of hiding a descendant can undo, so the subtree is not simply gone.
    # `<a href=…><span style="visibility:hidden">Mail</span></a>` paints
    # nothing a reader can see — the anchor keeps a 30x17 box and is even
    # hit-testable, but there is no ink in it — and counting that text as a
    # label recorded a route nobody can find. Visible-vs-invisible is the rule
    # this gate runs on, and it decides this one.
    if raw_span is not None and len(raw_span) == len(image_span):
        image_span = mask_invisible_subtrees(image_span, raw_span)
        masked_span = mask_invisible_subtrees(masked_span, raw_span)
    # `&#32;` is a space on screen, so `[&#32;](mail.md)` is an anchor with
    # nothing in it — the same nothing as `[ ](mail.md)`, which was already
    # rejected. Testing the undecoded source saw four non-blank characters and
    # called it a label, so the entity spelling let an orphan through.
    #
    # ONLY THE TEXT VIEW, and only AFTER the tags come out. Both halves matter:
    # a decoded reference is literal TEXT, never markup. markdown-it renders
    # `<a href=…>&#33;&#91;x](y.png)</a>` as the visible characters
    # `![x](y.png)`, not an image, so decoding the image view would invent one
    # and mark a real orphan reachable; and `&#60;span&#62;` is the visible
    # text `<span>`, so decoding before `ANY_TAG` ran would let it be stripped
    # as though the reader saw no such thing. `decode_visible` leaves code
    # spans alone, which is right here too: `` [`&#32;`](mail.md) `` shows the
    # reader `&#32;` and is a genuine label.
    # ASCII whitespace only. U+00A0 and the other Unicode spaces PAINT — an
    # anchor holding just `&nbsp;` is 4px wide and clickable — so a bare
    # `.strip()`, which counts them as whitespace, called a live link empty.
    # The zero-width characters are removed above precisely because they do
    # not paint; these are the opposite case and must survive.
    return bool(decode_visible(ANY_TAG.sub('', masked_span))
                .translate(_ZERO_WIDTH).strip(' \t\n\r\f\v')
                or ANY_IMAGE.search(image_span)
                or _paints_an_input(image_span, raw_span))


def mask_invisible(txt, keep_images=False):
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
    defined = definition_labels(txt)
    out = []
    for kind, seg in split_fences(txt):
        if kind != 'prose':
            out.append(seg)
            continue
        protected = [(m.start(), m.end()) for m in CODE_SPAN.finditer(seg)]
        protected += [(m.start(), m.end())
                      for m in RAW_DELIM_BLOCK.finditer(seg)]
        tags = [(m.start(), m.end()) for m in HTML_TAG.finditer(seg)]

        def blank(pat, bound=None, when=None):
            nonlocal seg
            pieces, last = [], 0
            for m in pat.finditer(seg):
                if any(a <= m.start() < b for a, b in protected):
                    continue
                if bound is not None and not any(
                        a <= m.start() < b for a, b in bound):
                    continue
                if when is not None and not when(m):
                    continue
                pieces.append(seg[last:m.start()])
                pieces.append(' ' * (m.end() - m.start()))
                last = m.end()
            pieces.append(seg[last:])
            seg = ''.join(pieces)

        # Attributes are blanked BEFORE hidden HTML, because a tag boundary
        # has to be known before script-shaped TEXT can be treated as a script.
        # In `<span title="<script>">`, the quoted `<script>` is attribute text
        # and every link below it is still live; resolving hidden HTML first
        # made that span open a raw block and erased the rest of the page —
        # or everything up to an unrelated later `</script>`.
        anchors = [(m.start(), m.end()) for m in ANCHOR_TAG.finditer(seg)]
        others = [t for t in tags
                  if not any(a <= t[0] < b for a, b in anchors)]
        blank(ATTR_VALUE, bound=anchors)
        blank(ATTR_VALUE_ANY, bound=others)
        # Hidden HTML is resolved AFTER attribute masking, so script-shaped
        # attribute TEXT is already spaces and cannot open anything, and by a
        # comment-aware scan rather than a bare pattern, so a script shown
        # inside a comment cannot either.
        for a, b in hidden_spans(seg, protected):
            seg = seg[:a] + ' ' * (b - a) + seg[b:]
        # Images last, and here rather than at the bare-path scan, so a
        # `![alt](x.md)` SAMPLE in a fence or code span keeps its visible
        # destination while a rendered image does not.
        if keep_images:
            out.append(seg)
            continue
        # `protected` is the same guard `blank()` applies: an image SAMPLE
        # inside a code span keeps its visible destination, so the scan must
        # skip one exactly as the pattern did.
        for a, b in [s for s in inline_images(seg)
                     if not any(p <= s[0] < q for p, q in protected)]:
            seg = seg[:a] + ' ' * (b - a) + seg[b:]
        # An image REFERENCE is an image only if its label resolves. The
        # definition set is read from the whole text rather than the resolved
        # one computed later in `edges_from`, which is not available this
        # early; it over-accepts only a definition that a block-start check
        # would reject, and that is the direction that keeps a real image
        # masked.
        for a, b in [sp for sp in reference_images(seg, defined)
                     if not any(p <= sp[0] < q for p, q in protected)]:
            seg = seg[:a] + ' ' * (b - a) + seg[b:]
        out.append(seg)
    return ''.join(out)


def edges_from(f):
    """Guide pages this file gives a reader a way to reach."""
    # THE ORDER OF THESE THREE STEPS IS LOAD-BEARING, and each was set by a
    # test that failed when it was wrong.
    #
    # 1. Escapes fold FIRST — see `fold_escapes` for which three and why. It
    #    has to precede masking, or `mask_invisible` reads `\![x](y.md)` as an
    #    image and blanks a live link.
    raw = fold_escapes(read(f))
    txt = raw
    # Comments and hidden HTML gone, images kept: the one view that can say
    # whether an image is something the reader can see and click.
    img_view = strip_comments(mask_invisible(raw, keep_images=True))
    # 2. Raw HTML is identified BEFORE comments. A `<!--` inside a closed
    #    `<script>` is script data, not a Markdown comment, and parsing comments
    #    first truncated the document at it. This order also settles the
    #    unclosed-opener case: the block is masked, so nothing inside it can
    #    open a comment either.
    # 3. Comments last, over what survives.
    txt = strip_comments(mask_invisible(txt))
    out = set()
    base = posixpath.dirname(f)

    def add_relative(raw, markdown=True):
        # Percent-decode before comparing to tracked filenames: `mail%20guide.md`
        # addresses `mail guide.md`, and the sibling gate decodes it too.
        # Anchor first, then query — the sibling's order. `mail.md?view=all` and
        # `mail.md?view=all#frag` both address `mail.md`; leaving the query on
        # would fail the `.md` test below and call a live link an orphan.
        # HTML's rules for a raw href, CommonMark's for a Markdown
        # destination. The tokenizer decodes `mail&#46md` without its
        # semicolon and the browser navigates to `mail.md`, so a raw anchor
        # written that way IS a route; the same text in a Markdown
        # destination renders literally and is not.
        raw = decode_char_refs(raw) if markdown else html.unescape(raw)
        # ASCII whitespace only. URL processing discards exactly that;
        # U+00A0 stays IN the path, so `<a href="&nbsp;mail.md">` resolves
        # to `\xa0mail.md` and reaches the tracked file not at all —
        # Python's bare `.strip()` removed it and recorded the edge anyway.
        if not markdown:
            # URL parsing removes tab and line breaks from ANYWHERE in the
            # URL, not just its ends: `<a href="ma\nil.md">` navigates to
            # `mail.md`, and leaving the newline in reported a live link as an
            # orphan. Measured through `new URL()`, which also shows a space
            # and U+00A0 are NOT removed — they percent-encode and stay in the
            # path, which is why only these three go.
            raw = raw.translate({0x09: None, 0x0a: None, 0x0d: None})
            # A BACKSLASH IS A PATH SEPARATOR to the URL parser, so
            # `<a href="docs\guide\mail.md">` navigates to
            # `docs/guide/mail.md` and is a route the reader can click. This
            # gate compared the backslash spelling against tracked filenames,
            # matched nothing, and reported the page as an orphan.
            # Measured in Chromium through `new URL()`, under an `https:` base
            # and a `file:` base alike — they agree, so which one the reader
            # is on does not have to be decided here.
            #
            # MARKDOWN IS THE OTHER WAY, and that asymmetry is the whole
            # reason this sits in the raw branch: cmark-gfm percent-encodes
            # the backslash, emitting `href="docs%5Cguide%5Cmail.md"`, and
            # `%5C` is a literal backslash in the path that reaches the
            # tracked file not at all. Same source characters, two
            # destinations, because one passes through a Markdown renderer
            # first. Converting both would invent a route out of a link that
            # renders as a dead one.
            #
            # Before `unquote`, so a raw href that spells `%5C` keeps its
            # literal backslash rather than being folded into a separator.
            raw = raw.replace('\\', '/')
        raw = raw.split('#', 1)[0].split('?', 1)[0].strip(' \t\n\r\f\v')
        raw = urllib.parse.unquote(raw)
        raw = (raw.replace('\x00\x00', '\\\\')
               .replace('\x01\x01', '\\!')
               .replace('\x02\x02', '\\<'))
        # Only for a MARKDOWN destination: Markdown drops the backslash from
        # an escaped punctuation character, and a raw `href` has no Markdown
        # escapes to drop. Unescaping one recorded an edge no reader can
        # follow. Verified against markdown-it-py, which keeps the backslash
        # in the raw href and drops it from `[mail](docs/guide/mail\.md)`.
        #
        # CORRECTION to what this said before: it called the backslash in a
        # raw href "a literal character" pointing at `mail\.md`. It is not —
        # the URL parser reads it as a separator, so that href points at
        # `docs/guide/mail/.md`, which is a different wrong answer. The
        # decision was right and the reason was not, and the separator rule
        # above is where that measurement now lives.
        if markdown:
            raw = UNESCAPE.sub(r'\1', raw)
        if not raw.endswith('.md'):
            return
        # A destination that leaves the site cannot make a guide page
        # reachable, however much of a repo path it happens to spell.
        # `//docs/guide/mail.md` is PROTOCOL-RELATIVE: the browser reads `docs`
        # as a hostname and navigates away. Stripping its slashes produced
        # exactly the tracked path and recorded an edge to a page the link does
        # not go to — a real orphan passing the gate. A destination with an
        # explicit scheme is the same thing said out loud; it survives today
        # only because `normalize` happens not to collapse it onto a tracked
        # path, which is an accident to stop depending on.
        # A LEADING SLASH resolves against the host, not the checkout, so
        # `/docs/guide/mail.md` addresses `github.com/docs/guide/mail.md` and
        # reaches this repository not at all. Stripping it spelled the tracked
        # path exactly and recorded an edge no reader can follow.
        #
        # The comment here used to say "a site path, not a file path" and then
        # strip the slash anyway, which was the bug: the sentence was right and
        # the line under it did the opposite. `check-docs-links.sh` is the
        # repo's authority on whether a destination resolves, and it already
        # calls this one `link target escapes the repository` — `os.path.join`
        # discards the left side on an absolute path, so it lands outside the
        # root and is reported. A destination that gate rejects cannot make a
        # page reachable here either.
        #
        # This subsumes the protocol-relative case rather than sitting beside
        # it: `//docs/guide/mail.md` reads `docs` as a HOSTNAME and navigates
        # away, and it starts with a slash too. An explicit scheme is the same
        # thing said out loud.
        if raw.startswith('/') or SCHEME.match(raw):
            return
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
    # is `inline_links`' to resolve, and it resolves to an external URL. Prose
    # only: the same text in a fence shows the path and still counts.
    scan = blank_link_dests(txt, definition_labels(txt))
    # An `href` value is not visible text either. It is spared from masking
    # so the anchor extractor can read it — and that extractor now REJECTS
    # an anchor with no content, which left `<a href="docs/guide/mail.md">`
    # `</a>` recording its path here instead, through the very check that
    # had just refused it. Blanked for this view only; the anchor loop still
    # reads `txt`.
    scan = ''.join(
        seg if kind != 'prose' else ANCHOR_HREF.sub(_blank_href, seg)
        for kind, seg in split_fences(scan))
    # Only where a block can START. A definition cannot interrupt a
    # paragraph, so after ordinary prose `[mail]: docs/guide/mail.md` is not
    # one — it renders as visible text, and its path is a route the reader
    # can read. Blanking it here while the resolution loop below applied
    # `block_starts` was the same rule in one view and not its sibling, and
    # it reported a reachable page as an orphan.
    scan = sub_in_prose(REF_DEF_FULL, scan, only_at_block_start=True,
                        view=strip_quote_markers(scan))
    # Decoded LAST: the blanking above works by offset, and decoding moves
    # every offset after the first reference.
    for m in BARE.finditer(decode_visible(scan)):
        t = normalize(m.group(1))
        if t in traversable:
            out.add(t)

    # Markdown extraction runs over a view with raw HTML BLOCKS removed: inside
    # `<div>…</div>` a `[mail](x.md)` stays literal, so it is not navigation.
    # The bare-path scan above deliberately still sees that region, because the
    # path itself is visible text there — the same visible/invisible split that
    # governs fences.
    # `<pre>` and `<textarea>` are type-1 raw blocks, so they suppress Markdown
    # the same way — but they end at their own closing tag rather than at a
    # blank line, and their contents are on screen, so they are stripped from
    # this view only and the bare-path scan above still reads them.
    md_txt = sub_in_prose(PRE_BLOCK, txt)
    md_txt = sub_in_prose(RAW_BLOCK, md_txt)
    md_txt = sub_in_prose(RAW_BLOCK_TYPE7, md_txt, only_at_block_start=True)

    # The definition set decides whether an inner REFERENCE is a link — and so
    # whether it deactivates the opener around it. Definitions are
    # document-scoped, so this is read from the whole view.
    _md_defined = definition_labels(md_txt)
    for lstart, lend, angle, bare, _ds, _de in inline_links(md_txt, _md_defined):
        # Offsets line up across both views because every masker replaces
        # space for space.
        if not has_content(img_view[lstart:lend], md_txt[lstart:lend],
                           raw[lstart:lend]):
            continue
        add_relative(angle if angle is not None else bare)

    # Anchors read `txt`, NOT the raw-block-stripped view. A type-6 block
    # suppresses MARKDOWN inside it, but its raw HTML is passed through and
    # rendered, so `<a href="mail.md">` on its own line — which is itself such a
    # block — is a link the reader can click. Stripping it here broke two
    # anchor tests, which is what said the distinction is real.
    # The href is read only from a span `ANCHOR_TAG` already accepted as a
    # COMPLETE tag. On its own `ANCHOR_HREF` stops at the value and never sees
    # what follows, so `<a href="mail.md" =>` — malformed, and rendered as
    # literal text rather than a link — recorded an edge. One grammar decides
    # what an anchor is; the other only says where its destination sits.
    dead = inert_spans(txt, raw)

    def is_inert(at):
        return any(a <= at < b for a, b in dead)

    for tag in ANCHOR_TAG.finditer(txt):
        m = ANCHOR_HREF.search(tag.group(0))
        if not m:
            continue
        # An inert anchor paints but cannot be activated, so it is not a route
        # — nor is one anywhere inside an inert subtree.
        if is_inert(tag.start()):
            continue
        # An anchor with no content is nothing to click, the same as `[](x.md)`
        # — the href is metadata and the reader is left with an empty element.
        # An `<img>` inside IS content, which falls out of testing for
        # non-whitespace rather than for text.
        # The anchor's OWN `hidden` is checked here rather than in
        # `has_content`, which is handed the content slice and so never sees
        # the opening tag. `<a hidden href="mail.md">Mail</a>` has a label but
        # no link: Chromium gives it a 0x0 box, and the edge was recorded from
        # the `Mail` inside it.
        # `hidden` is a bare attribute name and survives masking, but the
        # inline CSS lives in a VALUE, which is blanked upstream — so that half
        # is read off `raw` at the same offsets. A tag cannot contain a comment,
        # so taking the raw slice here needs no further guard.
        if (HIDDEN_OPEN.fullmatch(tag.group(0))
                or style_hidden_fullmatch(raw[tag.start():tag.end()])
                or INERT_OPEN.fullmatch(raw[tag.start():tag.end()])):
            continue
        # The close is looked for in the MASKED view, not `raw`: an apparent
        # `</a>` inside a script or a comment is content, not a boundary, and
        # the browser keeps the label after it inside the live anchor.
        # `<a href="mail.md"><script>const fake="</a>";</script>Mail</a>`
        # renders a 30px clickable `Mail`, but stopping at the spelling left
        # the anchor looking empty and reported a real route as an orphan.
        close = ANCHOR_CLOSE.search(txt, tag.end())
        stop = close.start() if close else len(raw)
        # An anchor cannot contain an anchor: the parser closes this one the
        # moment the next `<a` opens, so the outer element ends there and its
        # content is whatever came before. `<a href="mail.md"><a
        # href="https://example.com">Inner</a></a>` leaves the outer anchor
        # with an EMPTY innerHTML in Chromium, but the first `</a>` was read
        # as its close and `Inner` counted as its label — an edge to a page
        # nothing actually links. The opener is looked for in `txt` rather
        # than `raw` so a commented-out `<a` cannot cut a live link short.
        nested = ANCHOR_TAG.search(txt, tag.end())
        if nested and nested.start() < stop:
            stop = nested.start()
        if not has_content(img_view[tag.end():stop], txt[tag.end():stop],
                           raw[tag.end():stop]):
            continue
        # `visibility:hidden` is checked LAST, and separately from the hidden
        # machinery above, because it is the one form of hiding a descendant
        # can undo — so the answer needs the anchor's content, which the
        # checks above do not have.
        if _invisible_anchor(raw[tag.start():tag.end()], raw[tag.end():stop]):
            continue
        add_relative(next(g for g in m.groups() if g is not None),
                     markdown=False)

    # A reference USE inside code — `` `[mail][]` `` — is the one code case that
    # does not count, and it is not an exception to the visible/invisible rule
    # but an application of it. A bare `docs/guide/x.md` in a fence counts
    # because the PATH is on screen; a reference use in a fence shows only the
    # label, while the path lives in a definition that renders as nothing at
    # all. Nothing a reader can see names the target, so it is not an edge.
    # Blanked, not dropped, and space for space. Dropping the fences shortened
    # this view, and its match offsets are used to slice `img_view` — so an
    # image-only reference after a fenced block read unrelated earlier bytes
    # and was rejected as empty. Every other view in this file preserves
    # length; this one stopped, and the check that grew to depend on it went
    # in the same commit.
    ref_txt = ''.join(
        seg if kind == 'prose' else ' ' * len(seg)
        for kind, seg in split_fences(md_txt))
    ref_txt = CODE_SPAN.sub(lambda m: ' ' * len(m.group(0)), ref_txt)

    # A definition is not a USE of anything, including of a label sitting in its
    # own title — `[old]: https://example.com "[mail]"` renders nothing at all.
    # Definition spans are blanked for the usage scans only; resolution below
    # still reads `ref_txt`, where the definitions are intact. Blanking them for
    # the bare-path scan and not here was the same one-view-not-its-sibling
    # mistake that `sub_in_prose` and `mask_invisible` were each introduced for.
    _ref_view = strip_quote_markers(ref_txt)
    _use_starts = block_starts(_ref_view)
    _def_spans = [m.span() for m in REF_DEF_FULL.finditer(_ref_view)
                  if m.start() in _use_starts]
    uses_txt = ref_txt
    for a, b in _def_spans:
        uses_txt = uses_txt[:a] + ' ' * (b - a) + uses_txt[b:]

    used = set()
    # `None` from `ref_label` means the label is over the character cap and is
    # no label at all; it must not join the used set, or an over-long
    # definition below would find a match for it.
    for _s, _e, lstart, lend, inner in full_references(uses_txt):
        # `[label][]` (collapsed) leaves the SECOND label empty; the label is
        # then the first one, which the pattern captures rather than this
        # having to find where it ends.
        # `[][m]` renders an empty anchor exactly as `[](x.md)` does, so the
        # rendered content — the FIRST label — has to carry something. The
        # inline form and the raw anchor already required it; this was the
        # third spelling of the same link and the one left out.
        if not has_content(img_view[lstart:lend], uses_txt[lstart:lend],
                           raw[lstart:lend]):
            continue
        lbl = (ref_label(inner) if inner.strip()
               else ref_label(uses_txt[lstart:lend]))
        if lbl is not None:
            used.add(lbl)
    # Blank every full-reference span before looking for shortcuts, so the
    # `[mail]` tail of `![alt][mail]` is not re-read as a link of its own.
    shortcut_txt = uses_txt
    for _s, _e, _ls, _le, _r in list(full_references(uses_txt, images=True)):
        shortcut_txt = (shortcut_txt[:_s] + ' ' * (_e - _s)
                        + shortcut_txt[_e:])
    # An inline span is blanked before the shortcut scan because links cannot
    # nest — but only when the OUTER link is the one that renders. CommonMark
    # resolves an inner shortcut at its `]` and deactivates the opener above
    # it, so in `[outer [Mail]](url)` with a `[mail]` definition the reader
    # gets a link to mail.md and the outer destination stays literal. Blanking
    # unconditionally deleted that live reference.
    _defined = definition_labels(ref_txt)

    def _blank_inline(m):
        for inner in REF_USE_SHORTCUT.finditer(m.group(0)[1:]):
            if ref_label(inner.group(1)) in _defined:
                return m.group(0)
        return ' ' * len(m.group(0))

    shortcut_txt = INLINE_SPAN_ANY.sub(_blank_inline, shortcut_txt)
    for m in REF_USE_SHORTCUT.finditer(shortcut_txt):
        # A shortcut's label IS its rendered content, so the same emptiness
        # rule applies: `[<span></span>]` renders an anchor with nothing in it.
        # The inline form, the raw anchor and the full reference all required
        # content already; this was the fourth spelling and the one left out.
        if not has_content(img_view[m.start(1):m.end(1)],
                           shortcut_txt[m.start(1):m.end(1)],
                           raw[m.start(1):m.end(1)]):
            continue
        lbl = ref_label(m.group(1))
        if lbl is not None:
            used.add(lbl)

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
    # Quote markers are blanked for the same reason they are in the bare-path
    # scan: a USED definition inside a quote is a route the reader can click,
    # and a pattern anchored at `^ {0,3}` could not see past the `>`.
    seen_labels = set()
    ref_view = strip_quote_markers(ref_txt)
    ref_block_starts = block_starts(ref_view)
    for m in REF_DEF.finditer(ref_view):
        if m.start() not in ref_block_starts:
            continue
        label = ref_label(m.group(1))
        if label is None:
            continue
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
# Seeded with the roots, not left empty: an instruction file may sit INSIDE
# the guide tree — `docs/guide/topic/AGENTS.md` is both an entry surface and a
# node — and processing a frontier item never adds that item itself. Such a
# root reported ITSELF as unreachable, which is the gate contradicting its own
# root list.
seen = set(roots)
frontier = list(roots)
while frontier:
    cur = frontier.pop()
    for nxt in edges_from(cur):
        if nxt not in seen:
            seen.add(nxt)
            frontier.append(nxt)

# The reason stops at the first `-->`, and may be EMPTY so the marker is still
# recognised when it has none. `(.+?)` did neither: needing at least one
# character, it stepped over the close of `<!-- orphan-allow: -->` and captured
# the page text plus the next comment's opener as a reason — an empty marker
# exempting a page from the check entirely, which is the most expensive false
# negative available here. Matching the marker with an empty reason is what
# keeps the "no reason after the colon" message pointing at the real problem.
WAIVER = re.compile(r'<!--\s*orphan-allow:\s*((?:(?!-->).)*?)\s*-->', re.S)


def waiver_view(txt):
    """The text a waiver may legitimately live in: prose, with code removed.

    A page that DOCUMENTS the marker — showing `<!-- orphan-allow: … -->` in a
    fence or a code span, as this script's own header and failure message do —
    would otherwise exempt itself by explaining the escape hatch. The exemption
    is the one place a false negative is most expensive, since it is how a page
    opts out of the check entirely, so it is matched only where an HTML comment
    would actually be an HTML comment.

    Escapes fold first, for the same reason and in the same order as edge
    extraction: `\\<!-- orphan-allow: … -->` is text the reader can see, not a
    comment, and must not waive anything.
    """
    txt = mask_invisible(fold_escapes(txt))
    kept = [seg for kind, seg in split_fences(txt) if kind == 'prose']
    return CODE_SPAN.sub(' ', ''.join(kept))


def find_waiver(txt):
    """The page's orphan-allow marker, if it has one that actually IS one.

    The marker must OPEN a comment. HTML comments do not nest, so in
    `<!-- sample: <!-- orphan-allow: x --> -->` the inner opener is content of
    the outer comment — text a page might well write while documenting the
    escape hatch — and matching it there let a sample silently switch the whole
    check off for that page. Comments are walked left to right and the marker
    is anchored at each opener, which is the same first-opener-wins rule the
    rest of this file settles fences and raw HTML with.
    """
    view = waiver_view(txt)
    pos = 0
    while True:
        start = view.find('<!--', pos)
        if start == -1:
            return None
        # The opener must actually OPEN a comment, by the same rule
        # `hidden_spans` applies. Line-initial it is a type-2 block and runs
        # through blank lines to its first `-->`; mid-line it must be a
        # well-formed inline comment, which CANNOT contain a blank line —
        # `prose <!-- orphan-allow: x` then a blank line then `-->` is two
        # paragraphs of visible text, and it was waiving the whole page.
        bol = view.rfind('\n', 0, start) + 1
        if view[bol:start].strip() == '' and start - bol <= 3:
            close = view.find('-->', start + 4)
            extent = len(view) if close == -1 else close + 3
        else:
            cm = HTML_COMMENT_INLINE.match(view, start)
            if not cm:
                pos = start + 1
                continue
            extent = cm.end()
        m = WAIVER.match(view, start)
        # ...and the marker has to fit INSIDE that comment, not run past it.
        if m and m.end() <= extent:
            return m
        pos = extent if extent > start else start + 1


defects, waived = [], 0
for n in sorted(node_set - seen):
    m = find_waiver(read(n))
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

  # ...and so is one under `.claude/skills/`, which the agent machinery loads
  # by name just the same. As a waypoint it was inert, since nothing links
  # `.claude/`, so a guide indexed only from there read as an orphan.
  local c13c="$tmp/c13c"; make_corpus "$c13c"
  mkdir -p "$c13c/.claude/skills/x"
  printf '# Skill\n\nSee [mail](../../../docs/guide/mail.md).\n' \
    > "$c13c/.claude/skills/x/SKILL.md"
  git -C "$c13c" add -A && git -C "$c13c" commit -qm claude-skill-root
  check "a .claude skill page is a root" pass "$c13c"

  # ...but only its SKILL.md. A reference page beside it is still a waypoint,
  # so the new prefix did not turn the whole directory into entry surfaces.
  local c13d="$tmp/c13d"; make_corpus "$c13d"
  mkdir -p "$c13d/.claude/skills/x/references"
  printf '# Notes\n\nSee [mail](../../../../docs/guide/mail.md).\n' \
    > "$c13d/.claude/skills/x/references/notes.md"
  git -C "$c13d" add -A && git -C "$c13d" commit -qm claude-skill-reference
  check "a .claude skill reference page is not a root" fail "$c13d"

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

  # A marker that starts MID-PARAGRAPH and crosses a blank line opens no
  # comment: cmark-gfm renders two paragraphs of visible text. It was waiving
  # the page anyway, which is the most expensive false negative here.
  local c9lf="$tmp/c9lf"; make_corpus "$c9lf"
  printf '# Mail\n\nprose <!-- orphan-allow: merely visible\n\n-->\n' \
    > "$c9lf/docs/guide/mail.md"
  git -C "$c9lf" add -A && git -C "$c9lf" commit -qm waiver-across-blank-line
  check "a marker crossing a blank line waives nothing" fail "$c9lf"

  # ...but the same marker mid-paragraph on ONE line is a real comment.
  local c9lg="$tmp/c9lg"; make_corpus "$c9lg"
  printf '# Mail\n\nprose <!-- orphan-allow: deliberate --> more\n' \
    > "$c9lg/docs/guide/mail.md"
  git -C "$c9lg" add -A && git -C "$c9lg" commit -qm waiver-inline-one-line
  check "an inline marker on one line still waives" pass "$c9lg"

  # ...and a LINE-INITIAL one is a type-2 block, which runs THROUGH blank
  # lines — so the bound belongs to the inline form only.
  local c9lh="$tmp/c9lh"; make_corpus "$c9lh"
  printf '# Mail\n\n<!-- orphan-allow: deliberate\n\n-->\n' \
    > "$c9lh/docs/guide/mail.md"
  git -C "$c9lh" add -A && git -C "$c9lh" commit -qm waiver-block-across-blank
  check "a line-initial marker may cross a blank line" pass "$c9lh"

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

  # CORRECTED, and the correction is the interesting part: this asserted that
  # the inner label of an inline link is always ordinary text. It is not. When
  # the inner shortcut RESOLVES, CommonMark makes it a link at its `]` and
  # deactivates the opener above it, so the reader gets a link to mail.md and
  # the outer destination renders literally. Verified against markdown-it-py:
  # `<p>[outer <a href="mail.md">mail</a>](https://example.com)</p>`.
  local c9ay="$tmp/c9ay"; make_corpus "$c9ay"
  printf '# Jobs\n\n[outer [mail]](https://example.com)\n\n[mail]: mail.md\n' \
    > "$c9ay/docs/guide/jobs.md"
  git -C "$c9ay" add -A && git -C "$c9ay" commit -qm inner-shortcut-wins
  check "a resolving inner shortcut wins over the outer link" pass "$c9ay"

  # ...and the original rule still holds where the inner label resolves to
  # NOTHING: the outer link is the one that renders, so `mail.md` named only in
  # its destination is not reachable through a bare-path read of the label.
  local c9ay2="$tmp/c9ay2"; make_corpus "$c9ay2"
  printf '# Jobs\n\n[outer [mail]](https://example.com)\n' \
    > "$c9ay2/docs/guide/jobs.md"
  git -C "$c9ay2" add -A && git -C "$c9ay2" commit -qm inner-label-unresolved
  check "an unresolved inner label is ordinary text" fail "$c9ay2"

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

  # A fence delimiter inside a comment is comment content, not a fence.
  local c9cw="$tmp/c9cw"; make_corpus "$c9cw"
  printf '# Jobs\n\n<!--\n```\n-->\n\nSee [mail](mail.md).\n' > "$c9cw/docs/guide/jobs.md"
  git -C "$c9cw" add -A && git -C "$c9cw" commit -qm fence-in-comment
  check "a fence inside a comment does not open a fence" pass "$c9cw"

  # ...and the mirror still holds: a comment sample in a fence opens nothing.
  local c9cx="$tmp/c9cx"; make_corpus "$c9cx"
  printf '# Jobs\n\n```\n<!--\n```\n\nSee [mail](mail.md).\n' > "$c9cx/docs/guide/jobs.md"
  git -C "$c9cx" add -A && git -C "$c9cx" commit -qm comment-in-fence
  check "a comment sample inside a fence opens no comment" pass "$c9cx"

  # A definition whose title never closes defines nothing.
  local c9cy="$tmp/c9cy"; make_corpus "$c9cy"
  printf '# Jobs\n\nSee [mail][m].\n\n[m]: mail.md "unterminated\n' \
    > "$c9cy/docs/guide/jobs.md"
  git -C "$c9cy" add -A && git -C "$c9cy" commit -qm unterminated-title
  check "a definition with an unterminated title defines nothing" fail "$c9cy"

  # ...but a properly terminated one still resolves.
  local c9cz="$tmp/c9cz"; make_corpus "$c9cz"
  printf '# Jobs\n\nSee [mail][m].\n\n[m]: mail.md "the mail guide"\n' \
    > "$c9cz/docs/guide/jobs.md"
  git -C "$c9cz" add -A && git -C "$c9cz" commit -qm terminated-title
  check "a definition with a closed title resolves" pass "$c9cz"

  # A named character reference decodes like the numeric forms.
  local c9da="$tmp/c9da"; make_corpus "$c9da"
  printf '# Jobs\n\nSee [mail](mail&period;md).\n' > "$c9da/docs/guide/jobs.md"
  git -C "$c9da" add -A && git -C "$c9da" commit -qm named-char-ref
  check "a named character reference in a destination resolves" pass "$c9da"

  # A title separated from its destination by a blank line renders no link.
  local c9db="$tmp/c9db"; make_corpus "$c9db"
  printf '# Jobs\n\nSee [mail](mail.md\n\n "title").\n' > "$c9db/docs/guide/jobs.md"
  git -C "$c9db" add -A && git -C "$c9db" commit -qm title-across-blank
  check "a title after a blank line is not a link" fail "$c9db"

  # ...nor does one whose own body contains a blank line.
  local c9dc="$tmp/c9dc"; make_corpus "$c9dc"
  printf '# Jobs\n\nSee [mail](mail.md "one\n\ntwo").\n' > "$c9dc/docs/guide/jobs.md"
  git -C "$c9dc" add -A && git -C "$c9dc" commit -qm title-body-blank
  check "a title containing a blank line is not a link" fail "$c9dc"

  # ...but a title wrapped across one line still resolves.
  local c9dd="$tmp/c9dd"; make_corpus "$c9dd"
  printf '# Jobs\n\nSee [mail](mail.md "the mail\nguide").\n' > "$c9dd/docs/guide/jobs.md"
  git -C "$c9dd" add -A && git -C "$c9dd" commit -qm title-wrapped
  check "a title wrapped across one line resolves" pass "$c9dd"

  # `<x =>` is not a tag at all — an attribute name cannot be empty — so it
  # opens no type-7 block and the link under it still renders.
  local c9de="$tmp/c9de"; make_corpus "$c9de"
  printf '# Jobs\n\n<x =>\nSee [mail](mail.md).\n' > "$c9de/docs/guide/jobs.md"
  git -C "$c9de" add -A && git -C "$c9de" commit -qm malformed-type7
  check "a malformed tag opens no raw block" pass "$c9de"

  # ...while a well-formed one alone on its line does, and suppresses it.
  local c9df="$tmp/c9df"; make_corpus "$c9df"
  printf '# Jobs\n\n<x a="1">\nSee [mail](mail.md).\n' > "$c9df/docs/guide/jobs.md"
  git -C "$c9df" add -A && git -C "$c9df" commit -qm wellformed-type7
  check "a well-formed tag alone on its line opens a raw block" fail "$c9df"

  # `<pre>` is a type-1 raw block, and each of these three is a case type 7
  # cannot reach — which is why the type-7 rule alone left the link live.
  # It MAY interrupt a paragraph...
  local c9dg="$tmp/c9dg"; make_corpus "$c9dg"
  printf '# Jobs\n\ntext\n<pre>\nSee [mail](mail.md).\n</pre>\n' \
    > "$c9dg/docs/guide/jobs.md"
  git -C "$c9dg" add -A && git -C "$c9dg" commit -qm pre-interrupts
  check "<pre> interrupting a paragraph makes the markdown literal" fail "$c9dg"

  # ...its opener may carry trailing content...
  local c9dg2="$tmp/c9dg2"; make_corpus "$c9dg2"
  printf '# Jobs\n\n<pre>example\nSee [mail](mail.md).\n</pre>\n' \
    > "$c9dg2/docs/guide/jobs.md"
  git -C "$c9dg2" add -A && git -C "$c9dg2" commit -qm pre-trailing
  check "<pre> with trailing content still opens a raw block" fail "$c9dg2"

  # ...and it ends at its closing tag, not at the next blank line.
  local c9dg3="$tmp/c9dg3"; make_corpus "$c9dg3"
  printf '# Jobs\n\n<pre>\ncode\n\nSee [mail](mail.md).\n</pre>\n' \
    > "$c9dg3/docs/guide/jobs.md"
  git -C "$c9dg3" add -A && git -C "$c9dg3" commit -qm pre-past-blank
  check "<pre> runs past a blank line to its closing tag" fail "$c9dg3"

  # ...but unlike a script block its contents are SHOWN, so a path there is
  # findable exactly as one in a fence is.
  local c9dh="$tmp/c9dh"; make_corpus "$c9dh"
  printf '# Jobs\n\n<pre>\ndocs/guide/mail.md\n</pre>\n' > "$c9dh/docs/guide/jobs.md"
  git -C "$c9dh" add -A && git -C "$c9dh" commit -qm pre-visible
  check "a bare path inside <pre> is still visible" pass "$c9dh"

  # A definition's destination may not sit across a blank line from its label.
  local c9di="$tmp/c9di"; make_corpus "$c9di"
  printf '# Jobs\n\nSee [mail][m].\n\n[m]:\n\nmail.md\n' > "$c9di/docs/guide/jobs.md"
  git -C "$c9di" add -A && git -C "$c9di" commit -qm def-across-blank
  check "a destination after a blank line defines nothing" fail "$c9di"

  # Script-SHAPED text inside an attribute value is text. Reading it as a raw
  # block opener erased every link below it, to the next `</script>` or to EOF.
  local c9dj="$tmp/c9dj"; make_corpus "$c9dj"
  printf '# Jobs\n\n<span title="<script>">note</span>\n\nSee [mail](mail.md).\n' \
    > "$c9dj/docs/guide/jobs.md"
  git -C "$c9dj" add -A && git -C "$c9dj" commit -qm script-in-attribute
  check "a script tag quoted in an attribute hides nothing" pass "$c9dj"

  # ...and a real script block still hides what it wraps.
  local c9dk="$tmp/c9dk"; make_corpus "$c9dk"
  printf '# Jobs\n\n<script>\nSee [mail](mail.md).\n</script>\n' \
    > "$c9dk/docs/guide/jobs.md"
  git -C "$c9dk" add -A && git -C "$c9dk" commit -qm real-script-block
  check "a real script block still hides its contents" fail "$c9dk"

  # A definition cannot interrupt the paragraph a block quote opens: the second
  # line lazily continues the quote and defines nothing.
  local c9dl="$tmp/c9dl"; make_corpus "$c9dl"
  printf '# Jobs\n\nSee [mail][m].\n\n> note\n[m]: mail.md\n' \
    > "$c9dl/docs/guide/jobs.md"
  git -C "$c9dl" add -A && git -C "$c9dl" commit -qm def-lazy-continuation
  check "a definition lazily continuing a block quote defines nothing" fail "$c9dl"

  # ...but one after the quote has closed still defines.
  local c9dm="$tmp/c9dm"; make_corpus "$c9dm"
  printf '# Jobs\n\nSee [mail][m].\n\n> note\n\n[m]: mail.md\n' \
    > "$c9dm/docs/guide/jobs.md"
  git -C "$c9dm" add -A && git -C "$c9dm" commit -qm def-after-quote
  check "a definition after a closed block quote resolves" pass "$c9dm"

  # A destination in angle brackets admits no line ending at all.
  local c9dn="$tmp/c9dn"; make_corpus "$c9dn"
  printf '# Jobs\n\nSee [mail](<mail.md\n>).\n' > "$c9dn/docs/guide/jobs.md"
  git -C "$c9dn" add -A && git -C "$c9dn" commit -qm angle-dest-newline
  check "an angle destination broken by a newline is not a link" fail "$c9dn"

  # ...and the same bound holds in a definition, which shares the grammar.
  local c9do="$tmp/c9do"; make_corpus "$c9do"
  printf '# Jobs\n\nSee [mail][m].\n\n[m]: <mail.md\n>\n' > "$c9do/docs/guide/jobs.md"
  git -C "$c9do" add -A && git -C "$c9do" commit -qm angle-def-newline
  check "an angle destination broken by a newline defines nothing" fail "$c9do"

  # A raw block ends on the line carrying its terminator, and that whole line
  # is part of the block — so markdown sharing the line stays literal.
  local c9dp="$tmp/c9dp"; make_corpus "$c9dp"
  printf '# Jobs\n\n<?demo ?> [mail][m]\n\n[m]: mail.md\n' > "$c9dp/docs/guide/jobs.md"
  git -C "$c9dp" add -A && git -C "$c9dp" commit -qm pi-terminator-line
  check "text sharing a line with ?> stays inside the block" fail "$c9dp"

  local c9dq="$tmp/c9dq"; make_corpus "$c9dq"
  printf '# Jobs\n\n<![CDATA[x]]> [mail](mail.md)\n' > "$c9dq/docs/guide/jobs.md"
  git -C "$c9dq" add -A && git -C "$c9dq" commit -qm cdata-terminator-line
  check "text sharing a line with ]]> stays inside the block" fail "$c9dq"

  local c9dr="$tmp/c9dr"; make_corpus "$c9dr"
  printf '# Jobs\n\n<pre>x</pre> [mail](mail.md)\n' > "$c9dr/docs/guide/jobs.md"
  git -C "$c9dr" add -A && git -C "$c9dr" commit -qm type1-terminator-line
  check "text sharing a line with </pre> stays inside the block" fail "$c9dr"

  # ...but a type-1 block needs its opener at the START of a line: mid-line
  # there is no block, and the markdown under it still renders.
  local c9ds="$tmp/c9ds"; make_corpus "$c9ds"
  printf '# Jobs\n\nsee <pre> for details\n\nSee [mail](mail.md).\n' \
    > "$c9ds/docs/guide/jobs.md"
  git -C "$c9ds" add -A && git -C "$c9ds" commit -qm type1-midline
  check "a mid-line <pre> opens no raw block" pass "$c9ds"

  # An ESCAPED waiver marker is text the reader can see, not a comment, so it
  # exempts nothing — the most expensive false negative this gate could have.
  local c9dt="$tmp/c9dt"; make_corpus "$c9dt"
  printf '# Mail\n\nWrite \\<!-- orphan-allow: why --> to waive a page.\n' \
    > "$c9dt/docs/guide/mail.md"
  git -C "$c9dt" add -A && git -C "$c9dt" commit -qm waiver-escaped
  check "an escaped waiver marker exempts nothing" fail "$c9dt"

  # ...while an escaped BACKSLASH before a real marker still waives.
  local c9du="$tmp/c9du"; make_corpus "$c9du"
  printf '# Mail\n\n\\\\<!-- orphan-allow: appendix, see the release notes -->\n' \
    > "$c9du/docs/guide/mail.md"
  git -C "$c9du" add -A && git -C "$c9du" commit -qm waiver-escaped-backslash
  check "an escaped backslash before a waiver still waives" pass "$c9du"

  # Indented code ends where the indentation does, so the tag below it opens a
  # raw block and the link inside that block is literal.
  local c9dv="$tmp/c9dv"; make_corpus "$c9dv"
  printf '# Jobs\n\n    sample\n<span>\nSee [mail](mail.md).\n' \
    > "$c9dv/docs/guide/jobs.md"
  git -C "$c9dv" add -A && git -C "$c9dv" commit -qm indented-code-then-tag
  check "a tag after indented code opens a raw block" fail "$c9dv"

  # ...but indented code cannot INTERRUPT a paragraph: under prose the same
  # line is continuation text, the paragraph stays open, and the tag is inline.
  local c9dw="$tmp/c9dw"; make_corpus "$c9dw"
  printf '# Jobs\n\nprose\n    sample\n<span>\nSee [mail](mail.md).\n' \
    > "$c9dw/docs/guide/jobs.md"
  git -C "$c9dw" add -A && git -C "$c9dw" commit -qm indented-code-in-paragraph
  check "an indented line inside a paragraph opens no code block" pass "$c9dw"

  # A run of definitions: the second is a definition too, so the route resolves.
  local c9dx="$tmp/c9dx"; make_corpus "$c9dx"
  printf '# Jobs\n\nSee [mail][mail].\n\n[first]: jobs.md\n[mail]: mail.md\n' \
    > "$c9dx/docs/guide/jobs.md"
  git -C "$c9dx" add -A && git -C "$c9dx" commit -qm definition-run
  check "the second definition in a run still defines" pass "$c9dx"

  # ...and the rule stays narrow: after real prose a definition still defines
  # nothing, because it cannot interrupt the paragraph.
  local c9dy="$tmp/c9dy"; make_corpus "$c9dy"
  printf '# Jobs\n\nSee [mail][mail].\n\nprose\n[mail]: mail.md\n' \
    > "$c9dy/docs/guide/jobs.md"
  git -C "$c9dy" add -A && git -C "$c9dy" commit -qm definition-after-prose
  check "a definition continuing a paragraph still defines nothing" fail "$c9dy"

  # A script SHOWN inside a comment opens nothing: the comment opened first.
  local c9dz="$tmp/c9dz"; make_corpus "$c9dz"
  printf '# Jobs\n\n<!-- <script> -->\n\nSee [mail](mail.md).\n' \
    > "$c9dz/docs/guide/jobs.md"
  git -C "$c9dz" add -A && git -C "$c9dz" commit -qm script-in-comment
  check "a script shown inside a comment hides nothing" pass "$c9dz"

  # ...and the mirror still holds: a comment inside a script is script data,
  # so it opens no comment and cannot truncate the page.
  local c9ea="$tmp/c9ea"; make_corpus "$c9ea"
  printf '# Jobs\n\n<script>\n<!--\n</script>\n\nSee [mail](mail.md).\n' \
    > "$c9ea/docs/guide/jobs.md"
  git -C "$c9ea" add -A && git -C "$c9ea" commit -qm comment-in-script
  check "a comment inside a script opens no comment" pass "$c9ea"

  # A quoted `>` does not end an anchor, so the attribute after it is still
  # masked and its invisible path confers nothing.
  local c9eb="$tmp/c9eb"; make_corpus "$c9eb"
  printf '# Jobs\n\n<a title="1 > 0" data-note="docs/guide/mail.md">x</a>\n' \
    > "$c9eb/docs/guide/jobs.md"
  git -C "$c9eb" add -A && git -C "$c9eb" commit -qm anchor-quoted-gt
  check "an attribute after a quoted > is still masked" fail "$c9eb"

  # ...and the href behind two of them is still found, since that IS
  # navigation. TWO, because with one the masking pass happened to blank the
  # offending attribute before `ANCHOR_HREF` ran and the bug did not show —
  # the single-attribute spelling verifies an accident, not the rule.
  local c9ec="$tmp/c9ec"; make_corpus "$c9ec"
  printf '# Jobs\n\n<a title="1 > 0" data-x="2 > 1" href="mail.md">mail</a>\n' \
    > "$c9ec/docs/guide/jobs.md"
  git -C "$c9ec" add -A && git -C "$c9ec" commit -qm anchor-href-after-quoted-gt
  check "an href after quoted > characters still counts as an edge" pass "$c9ec"

  # An empty marker does not reach forward to a LATER comment for its reason.
  local c9ed="$tmp/c9ed"; make_corpus "$c9ed"
  printf '# Mail\n\n<!-- orphan-allow: -->\n\ntext\n\n<!-- note -->\n' \
    > "$c9ed/docs/guide/mail.md"
  git -C "$c9ed" add -A && git -C "$c9ed" commit -qm waiver-empty-then-comment
  check "an empty marker does not borrow a later comment as its reason" fail "$c9ed"

  # ...and it is still recognised AS a marker, so the message says why.
  local c9ee="$tmp/c9ee"; make_corpus "$c9ee"
  printf '# Mail\n\n<!-- orphan-allow: -->\n\ntext\n\n<!-- note -->\n' \
    > "$c9ee/docs/guide/mail.md"
  git -C "$c9ee" add -A && git -C "$c9ee" commit -qm waiver-empty-message
  # Captured rather than piped: `run_check` exits 1 here by design, and under
  # `pipefail` that status is the pipeline's however well grep matched.
  total=$((total + 1))
  local out9ee; out9ee="$(run_check "$c9ee" 2>&1 || true)"
  if [[ "$out9ee" == *"has no reason after the colon"* ]]; then
    pass=$((pass + 1)); echo "  ok: an empty marker is reported as having no reason"
  else
    echo "  FAILED: an empty marker is reported as having no reason" >&2
  fi

  # `<span a==>` is malformed — an unquoted value admits no `=` — so it opens
  # no type-7 block and the link under it still renders.
  local c9ef="$tmp/c9ef"; make_corpus "$c9ef"
  printf '# Jobs\n\n<span a==>\nSee [mail](mail.md).\n' > "$c9ef/docs/guide/jobs.md"
  git -C "$c9ef" add -A && git -C "$c9ef" commit -qm bad-unquoted-attr
  check "an invalid unquoted attribute opens no raw block" pass "$c9ef"

  # ...but a BLOCK tag opens one whatever its attributes look like: that start
  # condition reads the tag name and nothing else.
  local c9eg="$tmp/c9eg"; make_corpus "$c9eg"
  printf '# Jobs\n\n<div a==>\nSee [mail](mail.md).\n' > "$c9eg/docs/guide/jobs.md"
  git -C "$c9eg" add -A && git -C "$c9eg" commit -qm bad-attr-block-tag
  check "a block tag opens a raw block whatever its attributes" fail "$c9eg"

  # A definition's title cannot cross a blank line, so a reference use below one
  # is not swallowed into the definition's span.
  # The closing quote is load-bearing: without one there is nothing for an
  # unbounded body to reach, the bug does not show, and the test would be
  # verifying its own gap rather than the rule.
  local c9eh="$tmp/c9eh"; make_corpus "$c9eh"
  printf '# Jobs\n\n[old]: https://example.test "title\n\nSee [mail][m].\n\n"\n\n[m]: mail.md\n' \
    > "$c9eh/docs/guide/jobs.md"
  git -C "$c9eh" add -A && git -C "$c9eh" commit -qm def-title-across-blank
  check "a use below a blank line is not swallowed by a title" pass "$c9eh"

  # A custom element is not a script: the tag name ends at the hyphen.
  local c9ei="$tmp/c9ei"; make_corpus "$c9ei"
  printf '# Jobs\n\n<script-widget> See [mail](mail.md). </script-widget>\n' \
    > "$c9ei/docs/guide/jobs.md"
  git -C "$c9ei" add -A && git -C "$c9ei" commit -qm custom-element
  check "a custom element is not a script tag" pass "$c9ei"

  # ...and a real one, needing no attributes to be one, still hides its body.
  local c9ej="$tmp/c9ej"; make_corpus "$c9ej"
  printf '# Jobs\n\n<script> See [mail](mail.md). </script>\n' \
    > "$c9ej/docs/guide/jobs.md"
  git -C "$c9ej" add -A && git -C "$c9ej" commit -qm real-script-inline
  check "a real script tag still hides its body" fail "$c9ej"

  # A Setext underline completes the heading above it, so the definition below
  # starts a block rather than continuing a paragraph.
  local c9ek="$tmp/c9ek"; make_corpus "$c9ek"
  printf '# Jobs\n\nSee [mail][m].\n\nLinks\n=====\n[m]: mail.md\n' \
    > "$c9ek/docs/guide/jobs.md"
  git -C "$c9ek" add -A && git -C "$c9ek" commit -qm setext-underline
  check "a definition under a Setext heading resolves" pass "$c9ek"

  # ...but `===` with no paragraph above it underlines nothing, so a definition
  # after it is still continuing ordinary text.
  local c9el="$tmp/c9el"; make_corpus "$c9el"
  printf '# Jobs\n\nSee [mail][m].\n\n=====\n[m]: mail.md\n' \
    > "$c9el/docs/guide/jobs.md"
  git -C "$c9el" add -A && git -C "$c9el" commit -qm setext-no-paragraph
  check "an underline with nothing above it completes no heading" fail "$c9el"

  # A line-initial comment is a raw block: the line carrying its terminator
  # belongs to it, so markdown sharing that line is literal.
  local c9em="$tmp/c9em"; make_corpus "$c9em"
  printf '# Jobs\n\n<!-- note --> [mail](mail.md)\n' > "$c9em/docs/guide/jobs.md"
  git -C "$c9em" add -A && git -C "$c9em" commit -qm comment-block-terminator-line
  check "text sharing a line with a block comment stays literal" fail "$c9em"

  # ...but MID-line the same comment is inline HTML, and the link renders.
  local c9en="$tmp/c9en"; make_corpus "$c9en"
  printf '# Jobs\n\nSee <!-- note --> [mail](mail.md)\n' > "$c9en/docs/guide/jobs.md"
  git -C "$c9en" add -A && git -C "$c9en" commit -qm comment-inline-keeps-link
  check "a link after an inline comment still counts" pass "$c9en"

  # An unterminated type-3 block runs to EOF, so the link below it is literal.
  local c9eo="$tmp/c9eo"; make_corpus "$c9eo"
  printf '# Jobs\n\n<?demo\n\nSee [mail](mail.md).\n' > "$c9eo/docs/guide/jobs.md"
  git -C "$c9eo" add -A && git -C "$c9eo" commit -qm unterminated-pi
  check "an unterminated processing instruction runs to EOF" fail "$c9eo"

  # A label cannot cross a blank line: the paragraph ends and no link renders.
  local c9ep="$tmp/c9ep"; make_corpus "$c9ep"
  printf '# Jobs\n\nSee [mail\n\n](mail.md).\n' > "$c9ep/docs/guide/jobs.md"
  git -C "$c9ep" add -A && git -C "$c9ep" commit -qm label-across-blank
  check "a label broken by a blank line is not a link" fail "$c9ep"

  # ...but one wrapped across a single line still resolves.
  local c9eq="$tmp/c9eq"; make_corpus "$c9eq"
  printf '# Jobs\n\nSee [the mail\nguide](mail.md).\n' > "$c9eq/docs/guide/jobs.md"
  git -C "$c9eq" add -A && git -C "$c9eq" commit -qm label-wrapped
  check "a label wrapped across one line resolves" pass "$c9eq"

  # A definition ends at its line: trailing garbage makes it a paragraph.
  local c9er="$tmp/c9er"; make_corpus "$c9er"
  printf '# Jobs\n\nSee [mail][m].\n\n[m]: mail.md trailing garbage\n' \
    > "$c9er/docs/guide/jobs.md"
  git -C "$c9er" add -A && git -C "$c9er" commit -qm def-trailing-garbage
  check "a definition with trailing garbage defines nothing" fail "$c9er"

  # A protocol-relative destination leaves the site: `docs` is a hostname.
  local c9es="$tmp/c9es"; make_corpus "$c9es"
  printf '# Jobs\n\n<a href="//docs/guide/mail.md">mail</a>\n' \
    > "$c9es/docs/guide/jobs.md"
  git -C "$c9es" add -A && git -C "$c9es" commit -qm protocol-relative
  check "a protocol-relative destination is not an edge" fail "$c9es"

  # ...and one with an explicit scheme is the same thing said out loud.
  local c9et="$tmp/c9et"; make_corpus "$c9et"
  printf '# Jobs\n\nSee [mail](https://example.test/docs/guide/mail.md).\n' \
    > "$c9et/docs/guide/jobs.md"
  git -C "$c9et" add -A && git -C "$c9et" commit -qm absolute-url
  check "an absolute URL spelling a repo path is not an edge" fail "$c9et"

  # ...and a SINGLE slash is the same story: it resolves against the host, so
  # `/docs/guide/mail.md` addresses `github.com/docs/guide/mail.md`.
  # This case asserted the opposite for several commits, on nothing but an
  # assertion — while the code comment beside it already said "a site path,
  # not a file path" and stripped the slash anyway. `check-docs-links.sh`
  # settles it: run on a page containing exactly this link it reports
  # `link target escapes the repository`, so one gate was calling the link
  # broken while this one counted it as navigation.
  local c9eu="$tmp/c9eu"; make_corpus "$c9eu"
  printf '# Jobs\n\n<a href="/docs/guide/mail.md">mail</a>\n' \
    > "$c9eu/docs/guide/jobs.md"
  git -C "$c9eu" add -A && git -C "$c9eu" commit -qm site-root-path
  check "a site-root path is not an edge" fail "$c9eu"

  # ...while the same path without the slash is the route it looks like.
  local c9ev="$tmp/c9ev"; make_corpus "$c9ev"
  printf '# Jobs\n\n<a href="mail.md">mail</a>\n' > "$c9ev/docs/guide/jobs.md"
  git -C "$c9ev" add -A && git -C "$c9ev" commit -qm relative-path
  check "the same path without the slash is an edge" pass "$c9ev"

  # A malformed anchor renders as literal text, so its href is not navigation.
  local c9ev="$tmp/c9ev"; make_corpus "$c9ev"
  printf '# Jobs\n\n<a href="mail.md" =>mail</a>\n' > "$c9ev/docs/guide/jobs.md"
  git -C "$c9ev" add -A && git -C "$c9ev" commit -qm malformed-anchor
  check "a malformed anchor confers no reachability" fail "$c9ev"

  # A bare destination needs balanced parentheses, in a definition as inline.
  local c9ew="$tmp/c9ew"; make_corpus "$c9ew"
  printf '# Jobs\n\nSee [mail][m].\n\n[m]: mail.md#(unterminated\n' \
    > "$c9ew/docs/guide/jobs.md"
  git -C "$c9ew" add -A && git -C "$c9ew" commit -qm unbalanced-def-paren
  check "an unbalanced paren in a definition defines nothing" fail "$c9ew"

  # A label over CommonMark's 999-character cap defines nothing.
  local c9ex="$tmp/c9ex"; make_corpus "$c9ex"
  local long; long="$(printf 'a%.0s' $(seq 1000))"
  printf '# Jobs\n\nSee [mail][%s].\n\n[%s]: mail.md\n' "$long" "$long" \
    > "$c9ex/docs/guide/jobs.md"
  git -C "$c9ex" add -A && git -C "$c9ex" commit -qm overlong-label
  check "an overlong reference label defines nothing" fail "$c9ex"

  # ...and one just inside the cap still resolves.
  local c9ey="$tmp/c9ey"; make_corpus "$c9ey"
  local ok999; ok999="$(printf 'a%.0s' $(seq 999))"
  printf '# Jobs\n\nSee [mail][%s].\n\n[%s]: mail.md\n' "$ok999" "$ok999" \
    > "$c9ey/docs/guide/jobs.md"
  git -C "$c9ey" add -A && git -C "$c9ey" commit -qm label-at-cap
  check "a reference label at the cap resolves" pass "$c9ey"

  # A reference USE cannot cross a blank line either.
  local c9ez="$tmp/c9ez"; make_corpus "$c9ez"
  printf '# Jobs\n\nSee [mail][a\n\nb].\n\n[a b]: mail.md\n' \
    > "$c9ez/docs/guide/jobs.md"
  git -C "$c9ez" add -A && git -C "$c9ez" commit -qm use-across-blank
  check "a reference use broken by a blank line is not a link" fail "$c9ez"

  # The cap is on SOURCE characters: 500 escapes are 500 repetitions and 1000
  # characters, which a repetition bound alone lets through.
  local c9fa="$tmp/c9fa"; make_corpus "$c9fa"
  # 500 TWO-character escapes: 500 repetitions, 1000 source characters. Three
  # characters each would be 1000 repetitions, which the repetition bound
  # already rejects — that spelling passes without the source-length check and
  # so proves nothing.
  local esc; esc="$(printf '\\*%.0s' $(seq 500))"
  printf '# Jobs\n\nSee [mail][%s].\n\n[%s]: mail.md\n' "$esc" "$esc" \
    > "$c9fa/docs/guide/jobs.md"
  git -C "$c9fa" add -A && git -C "$c9fa" commit -qm overlong-escaped-label
  check "an overlong label of escapes defines nothing" fail "$c9fa"

  # An unterminated comment runs to EOF only from the start of a line.
  local c9fb="$tmp/c9fb"; make_corpus "$c9fb"
  printf '# Jobs\n\nprose <!-- sample\n\nSee [mail](mail.md).\n' \
    > "$c9fb/docs/guide/jobs.md"
  git -C "$c9fb" add -A && git -C "$c9fb" commit -qm midline-unclosed-comment
  check "a mid-line unclosed comment hides nothing" pass "$c9fb"

  # ...but a line-initial one still comments out the rest of the file.
  local c9fc="$tmp/c9fc"; make_corpus "$c9fc"
  printf '# Jobs\n\n<!-- sample\n\nSee [mail](mail.md).\n' \
    > "$c9fc/docs/guide/jobs.md"
  git -C "$c9fc" add -A && git -C "$c9fc" commit -qm block-unclosed-comment
  check "a line-initial unclosed comment still runs to EOF" fail "$c9fc"

  # A tag's whitespace may wrap a line but not cross a blank one.
  local c9fd="$tmp/c9fd"; make_corpus "$c9fd"
  printf '# Jobs\n\n<a\n\n href="mail.md">mail</a>\n' > "$c9fd/docs/guide/jobs.md"
  git -C "$c9fd" add -A && git -C "$c9fd" commit -qm anchor-across-blank
  check "an anchor broken by a blank line is not an anchor" fail "$c9fd"

  # ...but one wrapped across a single line is still an anchor.
  local c9fe="$tmp/c9fe"; make_corpus "$c9fe"
  printf '# Jobs\n\n<a\n href="mail.md">mail</a>\n' > "$c9fe/docs/guide/jobs.md"
  git -C "$c9fe" add -A && git -C "$c9fe" commit -qm anchor-wrapped
  check "an anchor wrapped across one line still counts" pass "$c9fe"

  # A type-1 block opens on its tag NAME: no closing angle bracket needed.
  local c9ff="$tmp/c9ff"; make_corpus "$c9ff"
  printf '# Jobs\n\n<script\n\nSee [mail](mail.md).\n' > "$c9ff/docs/guide/jobs.md"
  git -C "$c9ff" add -A && git -C "$c9ff" commit -qm type1-name-only
  check "a type-1 opener with no > still opens a block" fail "$c9ff"

  # ...and `<pre` the same way, where the contents stay visible.
  local c9fg="$tmp/c9fg"; make_corpus "$c9fg"
  printf '# Jobs\n\n<pre\n\nSee [mail](mail.md).\n' > "$c9fg/docs/guide/jobs.md"
  git -C "$c9fg" add -A && git -C "$c9fg" commit -qm type1-pre-name-only
  check "a <pre opener with no > still opens a block" fail "$c9fg"

  # ...but the name must END there: `<scripted` is an ordinary custom element.
  local c9fh="$tmp/c9fh"; make_corpus "$c9fh"
  printf '# Jobs\n\n<scripted> See [mail](mail.md). </scripted>\n' \
    > "$c9fh/docs/guide/jobs.md"
  git -C "$c9fh" add -A && git -C "$c9fh" commit -qm longer-tag-name
  check "a longer tag name is not a type-1 opener" pass "$c9fh"

  # A quoted attribute value cannot cross a blank line either, so the tag is
  # not a tag and its href is not a route.
  local c9fi="$tmp/c9fi"; make_corpus "$c9fi"
  printf '# Jobs\n\n<a title="hello\n\nworld" href="mail.md">mail</a>\n' \
    > "$c9fi/docs/guide/jobs.md"
  git -C "$c9fi" add -A && git -C "$c9fi" commit -qm attr-across-blank
  check "an attribute value broken by a blank line is not an anchor" fail "$c9fi"

  # A MID-LINE comment is inline HTML and cannot span a blank line, so it is no
  # comment and the link inside it is live.
  local c9fj="$tmp/c9fj"; make_corpus "$c9fj"
  printf '# Jobs\n\nprose <!-- note\n\nSee [mail](mail.md).\n\n-->\n' \
    > "$c9fj/docs/guide/jobs.md"
  git -C "$c9fj" add -A && git -C "$c9fj" commit -qm inline-comment-across-blank
  check "a mid-line comment does not span a blank line" pass "$c9fj"

  # ...but a LINE-INITIAL one is a type-2 block and does, ending at its `-->`.
  local c9fk="$tmp/c9fk"; make_corpus "$c9fk"
  printf '# Jobs\n\n<!-- note\n\nSee [mail](mail.md).\n\n-->\n' \
    > "$c9fk/docs/guide/jobs.md"
  git -C "$c9fk" add -A && git -C "$c9fk" commit -qm block-comment-across-blank
  check "a line-initial comment spans blank lines to its close" fail "$c9fk"

  # An image label carries the same paragraph bound, so the path below the
  # break is visible text rather than part of a masked image span.
  local c9fl="$tmp/c9fl"; make_corpus "$c9fl"
  printf '# Jobs\n\n![alt\n\ndocs/guide/mail.md](x.png)\n' \
    > "$c9fl/docs/guide/jobs.md"
  git -C "$c9fl" add -A && git -C "$c9fl" commit -qm image-label-across-blank
  check "a path below a broken image label is still visible" pass "$c9fl"

  # A definition-shaped line that cannot interrupt a paragraph is visible text,
  # so its path is a route — it must not be blanked before the bare-path scan.
  local c9fm="$tmp/c9fm"; make_corpus "$c9fm"
  printf '# Jobs\n\nprose\n[mail]: docs/guide/mail.md\n' > "$c9fm/docs/guide/jobs.md"
  git -C "$c9fm" add -A && git -C "$c9fm" commit -qm def-in-paragraph-visible
  check "a definition continuing a paragraph is visible text" pass "$c9fm"

  # A LOWERCASE declaration opens nothing on the renderers a reader meets, so
  # the link below it is live. Verified against markdown-it-py, which renders
  # `<p>&lt;!demo</p>` and then the link. See the note on `RAW_DELIM` for why
  # this diverges from the sibling gate.
  local c9fn="$tmp/c9fn"; make_corpus "$c9fn"
  printf '# Jobs\n\n<!demo\n\nSee [mail](mail.md).\n' > "$c9fn/docs/guide/jobs.md"
  git -C "$c9fn" add -A && git -C "$c9fn" commit -qm lowercase-declaration
  check "a lowercase declaration opens no raw block" pass "$c9fn"

  # ...while an UPPERCASE one still does.
  local c9fn2="$tmp/c9fn2"; make_corpus "$c9fn2"
  printf '# Jobs\n\n<!DEMO\n\nSee [mail](mail.md).\n' > "$c9fn2/docs/guide/jobs.md"
  git -C "$c9fn2" add -A && git -C "$c9fn2" commit -qm uppercase-declaration
  check "an uppercase declaration opens a raw block" fail "$c9fn2"

  # A definition directly under a closed `<pre>` block IS a definition: the
  # block ended at the close tag, so the line below it starts a new block and
  # the definition renders as nothing. Its path must not count as visible.
  local c9fo="$tmp/c9fo"; make_corpus "$c9fo"
  printf '# Jobs\n\n<pre>\nx\n</pre>\n[mail]: docs/guide/mail.md\n' \
    > "$c9fo/docs/guide/jobs.md"
  git -C "$c9fo" add -A && git -C "$c9fo" commit -qm def-after-pre-block
  check "an unused definition under a closed <pre> is not a route" fail "$c9fo"

  # A title must be SEPARATED from its destination, or there is no link.
  local c9fp="$tmp/c9fp"; make_corpus "$c9fp"
  printf '# Jobs\n\nSee [mail](<mail.md>"title").\n' > "$c9fp/docs/guide/jobs.md"
  git -C "$c9fp" add -A && git -C "$c9fp" commit -qm title-unseparated
  check "a title jammed against its destination is not a link" fail "$c9fp"

  # ...but a properly separated one still resolves.
  local c9fq="$tmp/c9fq"; make_corpus "$c9fq"
  printf '# Jobs\n\nSee [mail](<mail.md> "title").\n' > "$c9fq/docs/guide/jobs.md"
  git -C "$c9fq" add -A && git -C "$c9fq" commit -qm title-separated
  check "a separated title still resolves" pass "$c9fq"

  # An invalid definition is a paragraph, so the valid-looking line under it
  # cannot interrupt one and defines nothing.
  local c9fr="$tmp/c9fr"; make_corpus "$c9fr"
  # A RELATIVE destination, so the bare-path scan cannot see it and the only
  # possible route is the definition itself. Spelled `docs/guide/mail.md`, the
  # line is visible paragraph text whose path counts on its own — correctly,
  # and the test would then pass either way and prove nothing.
  printf '# Jobs\n\nSee [mail][mail].\n\n[bad]: https://example.test trailing garbage\n[mail]: mail.md\n' \
    > "$c9fr/docs/guide/jobs.md"
  git -C "$c9fr" add -A && git -C "$c9fr" commit -qm def-after-invalid-def
  check "a definition under an invalid one defines nothing" fail "$c9fr"

  # ...while a run of VALID definitions still resolves, as before.
  local c9fs="$tmp/c9fs"; make_corpus "$c9fs"
  printf '# Jobs\n\nSee [mail][mail].\n\n[ok]: https://example.test\n[mail]: mail.md\n' \
    > "$c9fs/docs/guide/jobs.md"
  git -C "$c9fs" add -A && git -C "$c9fs" commit -qm def-after-valid-def
  check "a definition under a valid one still defines" pass "$c9fs"

  # A collapsed reference takes its label from the FIRST bracket, and an
  # escaped `]` inside it does not end that label.
  local c9ft="$tmp/c9ft"; make_corpus "$c9ft"
  printf '# Jobs\n\nSee [a\\]b][].\n\n[a\\]b]: mail.md\n' > "$c9ft/docs/guide/jobs.md"
  git -C "$c9ft" add -A && git -C "$c9ft" commit -qm collapsed-escaped-bracket
  check "a collapsed label with an escaped bracket resolves" pass "$c9ft"

  # A definition's title may sit on the next line, and the definition after it
  # still starts a block.
  local c9fu="$tmp/c9fu"; make_corpus "$c9fu"
  printf '# Jobs\n\nSee [mail][m].\n\n[x]: https://example.test\n"title"\n[m]: mail.md\n' \
    > "$c9fu/docs/guide/jobs.md"
  git -C "$c9fu" add -A && git -C "$c9fu" commit -qm continuation-title
  check "a definition after a continuation title resolves" pass "$c9fu"

  # A `](path)` tail with no opening bracket renders literally, so its path is
  # visible text and must not be blanked with real link destinations.
  local c9fv="$tmp/c9fv"; make_corpus "$c9fv"
  printf '# Jobs\n\nstray ](docs/guide/mail.md)\n' > "$c9fv/docs/guide/jobs.md"
  git -C "$c9fv" add -A && git -C "$c9fv" commit -qm stray-link-tail
  check "a stray link tail leaves its path visible" pass "$c9fv"

  # ...while a real link's destination is still not visible text.
  local c9fw="$tmp/c9fw"; make_corpus "$c9fw"
  printf '# Jobs\n\nSee [q](https://example.test/?x=docs/guide/mail.md).\n' \
    > "$c9fw/docs/guide/jobs.md"
  git -C "$c9fw" add -A && git -C "$c9fw" commit -qm real-link-dest-masked
  check "a real link destination is not scanned as a bare path" fail "$c9fw"

  # An unresolved image reference is literal text, so its path is on screen.
  local c9fx="$tmp/c9fx"; make_corpus "$c9fx"
  printf '# Jobs\n\n![alt][docs/guide/mail.md]\n' > "$c9fx/docs/guide/jobs.md"
  git -C "$c9fx" add -A && git -C "$c9fx" commit -qm unresolved-image-ref
  check "an unresolved image reference is visible text" pass "$c9fx"

  # ...but a RESOLVED one is an image, and neither its label nor its
  # destination is something the reader can read.
  local c9fy="$tmp/c9fy"; make_corpus "$c9fy"
  printf '# Jobs\n\n![alt][img]\n\n[img]: docs/guide/mail.md\n' \
    > "$c9fy/docs/guide/jobs.md"
  git -C "$c9fy" add -A && git -C "$c9fy" commit -qm resolved-image-ref
  check "a resolved image reference confers nothing" fail "$c9fy"

  # A closing tag admits no attributes, so this is not a tag and opens nothing.
  local c9fz="$tmp/c9fz"; make_corpus "$c9fz"
  printf '# Jobs\n\n</span a=x>\nSee [mail](mail.md).\n' > "$c9fz/docs/guide/jobs.md"
  git -C "$c9fz" add -A && git -C "$c9fz" commit -qm closing-tag-with-attrs
  check "a closing tag with attributes opens no raw block" pass "$c9fz"

  # ...but a well-formed closing tag alone on its line still does.
  local c9ga="$tmp/c9ga"; make_corpus "$c9ga"
  printf '# Jobs\n\n</span>\nSee [mail](mail.md).\n' > "$c9ga/docs/guide/jobs.md"
  git -C "$c9ga" add -A && git -C "$c9ga" commit -qm closing-tag-plain
  check "a well-formed closing tag opens a raw block" fail "$c9ga"

  # A name-only type-1 opener is a BLOCK start condition: mid-line it is an
  # incomplete tag, which is ordinary visible text.
  local c9gb="$tmp/c9gb"; make_corpus "$c9gb"
  printf '# Jobs\n\nprose <script\n\nSee [mail](mail.md).\n' > "$c9gb/docs/guide/jobs.md"
  git -C "$c9gb" add -A && git -C "$c9gb" commit -qm midline-script-name-only
  check "a mid-line name-only script opener hides nothing" pass "$c9gb"

  # ...while a complete tag mid-line is raw HTML wherever it sits.
  local c9gc="$tmp/c9gc"; make_corpus "$c9gc"
  printf '# Jobs\n\nprose <script>\n\nSee [mail](mail.md).\n' > "$c9gc/docs/guide/jobs.md"
  git -C "$c9gc" add -A && git -C "$c9gc" commit -qm midline-script-complete
  check "a complete mid-line script tag still hides what follows" fail "$c9gc"

  # A definition DEMONSTRATED in a fence defines nothing, so the image
  # reference below it is unresolved and its path is visible text.
  local c9gd="$tmp/c9gd"; make_corpus "$c9gd"
  printf '# Jobs\n\n```\n[img]: x.png\n```\n\n![docs/guide/mail.md][img]\n' \
    > "$c9gd/docs/guide/jobs.md"
  git -C "$c9gd" add -A && git -C "$c9gd" commit -qm fenced-def-image-ref
  check "a fenced definition does not turn text into an image" pass "$c9gd"

  # A backslash in a raw HTML href is a literal character, not a Markdown
  # escape, so the anchor points at a file that does not exist.
  local c9ge="$tmp/c9ge"; make_corpus "$c9ge"
  printf '# Jobs\n\n<a href="mail\\.md">mail</a>\n' > "$c9ge/docs/guide/jobs.md"
  git -C "$c9ge" add -A && git -C "$c9ge" commit -qm raw-href-backslash
  check "a backslash in a raw href is not unescaped" fail "$c9ge"

  # ...while the same escape in a MARKDOWN destination still resolves.
  local c9gf="$tmp/c9gf"; make_corpus "$c9gf"
  printf '# Jobs\n\nSee [mail](mail\\.md).\n' > "$c9gf/docs/guide/jobs.md"
  git -C "$c9gf" add -A && git -C "$c9gf" commit -qm markdown-dest-backslash
  check "a backslash in a markdown destination is unescaped" pass "$c9gf"

  # A definition may break before its destination, and the definition after it
  # is a definition too — so an UNUSED one renders nothing and is no route.
  local c9gg="$tmp/c9gg"; make_corpus "$c9gg"
  printf '# Jobs\n\n[x]:\nhttps://example.test\n[m]: docs/guide/mail.md\n' \
    > "$c9gg/docs/guide/jobs.md"
  git -C "$c9gg" add -A && git -C "$c9gg" commit -qm next-line-destination
  check "an unused definition after a wrapped one is not visible" fail "$c9gg"

  # ...but USED, the same definition is a route the reader can click.
  local c9gh="$tmp/c9gh"; make_corpus "$c9gh"
  printf '# Jobs\n\nSee [mail][m].\n\n[x]:\nhttps://example.test\n[m]: mail.md\n' \
    > "$c9gh/docs/guide/jobs.md"
  git -C "$c9gh" add -A && git -C "$c9gh" commit -qm next-line-destination-used
  check "a used definition after a wrapped one resolves" pass "$c9gh"

  # An UNUSED definition inside a block quote renders nothing, so its
  # destination is not visible text and confers no reachability.
  local c9gi="$tmp/c9gi"; make_corpus "$c9gi"
  printf '# Jobs\n\n> [old]: docs/guide/mail.md\n' > "$c9gi/docs/guide/jobs.md"
  git -C "$c9gi" add -A && git -C "$c9gi" commit -qm quoted-unused-def
  check "an unused definition in a block quote is not a route" fail "$c9gi"

  # ...while a USED one inside a quote is a link the reader can click.
  local c9gj="$tmp/c9gj"; make_corpus "$c9gj"
  printf '# Jobs\n\n> [old]: mail.md\n>\n> See [mail][old].\n' \
    > "$c9gj/docs/guide/jobs.md"
  git -C "$c9gj" add -A && git -C "$c9gj" commit -qm quoted-used-def
  check "a used definition in a block quote resolves" pass "$c9gj"

  # A path that merely BEGINS with a tracked page names a different file.
  local c9gk="$tmp/c9gk"; make_corpus "$c9gk"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nOld copy: docs/guide/mail.md.bak\n' \
    > "$c9gk/README.md"
  git -C "$c9gk" add -A && git -C "$c9gk" commit -qm bare-path-suffix
  check "a suffixed path does not reach the page it prefixes" fail "$c9gk"

  # ...and a sentence-ending period still leaves the path intact.
  local c9gl="$tmp/c9gl"; make_corpus "$c9gl"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nSee docs/guide/mail.md.\n' \
    > "$c9gl/README.md"
  git -C "$c9gl" add -A && git -C "$c9gl" commit -qm bare-path-sentence
  check "a path at the end of a sentence still counts" pass "$c9gl"

  # A waiver marker must OPEN its comment: nested inside another, it is the
  # outer comment's content and exempts nothing.
  local c9gm="$tmp/c9gm"; make_corpus "$c9gm"
  printf '# Mail\n\n<!-- sample: <!-- orphan-allow: example only --> -->\n' \
    > "$c9gm/docs/guide/mail.md"
  git -C "$c9gm" add -A && git -C "$c9gm" commit -qm nested-waiver
  check "a waiver nested in another comment exempts nothing" fail "$c9gm"

  # ...and a marker after an UNRELATED comment is still found.
  local c9gn="$tmp/c9gn"; make_corpus "$c9gn"
  printf '# Mail\n\n<!-- a note -->\n\n<!-- orphan-allow: appendix, see the notes -->\n' \
    > "$c9gn/docs/guide/mail.md"
  git -C "$c9gn" add -A && git -C "$c9gn" commit -qm waiver-after-comment
  check "a waiver after another comment still waives" pass "$c9gn"

  # Labels match after a full Unicode case fold, not `lower()`.
  local c9go="$tmp/c9go"; make_corpus "$c9go"
  printf '# Jobs\n\nSee [mail][\xe1\xba\x9e].\n\n[ss]: mail.md\n' \
    > "$c9go/docs/guide/jobs.md"
  git -C "$c9go" add -A && git -C "$c9go" commit -qm casefold-label
  check "reference labels match under Unicode case folding" pass "$c9go"

  # A destination with NESTED parentheses is still a destination, so the repo
  # path in its query string is invisible and confers nothing.
  local c9gp="$tmp/c9gp"; make_corpus "$c9gp"
  printf '# Jobs\n\n[ext](https://example.test/a(b(c))?q=docs/guide/mail.md)\n' \
    > "$c9gp/docs/guide/jobs.md"
  git -C "$c9gp" add -A && git -C "$c9gp" commit -qm nested-paren-dest
  check "a nested-paren destination is not scanned as a bare path" fail "$c9gp"

  # ...and a nested-paren path that IS the destination still resolves.
  local c9gq="$tmp/c9gq"; make_corpus "$c9gq"
  mkdir -p "$c9gq/docs/guide"
  printf '# V2\n\ntext\n' > "$c9gq/docs/guide/guide(v2).md"
  printf '# Jobs\n\nSee [mail](mail.md) and [v2](guide(v2).md).\n' \
    > "$c9gq/docs/guide/jobs.md"
  git -C "$c9gq" add -A && git -C "$c9gq" commit -qm paren-in-filename
  check "a parenthesised filename still resolves" pass "$c9gq"

  # `<!-->` is not a well-formed inline comment, so the link beside it renders.
  local c9gr="$tmp/c9gr"; make_corpus "$c9gr"
  printf '# Jobs\n\nprose <!--> [mail](mail.md) -->\n' > "$c9gr/docs/guide/jobs.md"
  git -C "$c9gr" add -A && git -C "$c9gr" commit -qm invalid-inline-comment
  check "an invalid inline comment strips nothing" pass "$c9gr"

  # ...but at the START of a line the same text opens a type-2 block.
  local c9gs="$tmp/c9gs"; make_corpus "$c9gs"
  printf '# Jobs\n\n<!--> [mail](mail.md) -->\n' > "$c9gs/docs/guide/jobs.md"
  git -C "$c9gs" add -A && git -C "$c9gs" commit -qm invalid-comment-line-initial
  check "the same text line-initial still opens a block" fail "$c9gs"

  # A fence marker inside `<script>` is the script's content, not a fence, so
  # the region stays hidden and a path in it is not on screen.
  local c9gt="$tmp/c9gt"; make_corpus "$c9gt"
  printf '# Jobs\n\n<script>\n```\ndocs/guide/mail.md\n```\n</script>\n' \
    > "$c9gt/docs/guide/jobs.md"
  git -C "$c9gt" add -A && git -C "$c9gt" commit -qm fence-in-script
  check "a fence marker inside a script opens no fence" fail "$c9gt"

  # ...and the mirror: a `<script>` shown INSIDE a fence is a code sample whose
  # text the reader can see, so the fence keeps it.
  local c9gu="$tmp/c9gu"; make_corpus "$c9gu"
  printf '# Jobs\n\n```\n<script>\ndocs/guide/mail.md\n</script>\n```\n' \
    > "$c9gu/docs/guide/jobs.md"
  git -C "$c9gu" add -A && git -C "$c9gu" commit -qm script-in-fence
  check "a script shown inside a fence stays visible" pass "$c9gu"

  # A destination may nest parentheses as deeply as cmark allows.
  local c9gv="$tmp/c9gv"; make_corpus "$c9gv"
  printf '# Jobs\n\n[ext](https://example.test/a(b(c(d(e(f(g(h)))))))?q=docs/guide/mail.md)\n' \
    > "$c9gv/docs/guide/jobs.md"
  git -C "$c9gv" add -A && git -C "$c9gv" commit -qm deep-nested-dest
  check "a deeply nested destination is not scanned as a bare path" fail "$c9gv"

  # A character reference in visible prose is decoded on screen, so the path
  # the reader sees is the tracked one.
  local c9gw="$tmp/c9gw"; make_corpus "$c9gw"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nSee docs/guide/mail&#46;md\n' \
    > "$c9gw/README.md"
  git -C "$c9gw" add -A && git -C "$c9gw" commit -qm entity-in-prose
  check "a character reference in prose still names the path" pass "$c9gw"

  # ...but inside a FENCE it is literal, so the reader sees `mail&#46;md` and
  # decoding there would invent a route nobody can read.
  local c9gx="$tmp/c9gx"; make_corpus "$c9gx"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n```\ndocs/guide/mail&#46;md\n```\n' \
    > "$c9gx/README.md"
  git -C "$c9gx" add -A && git -C "$c9gx" commit -qm entity-in-fence
  check "a character reference in a fence stays literal" fail "$c9gx"

  # ...and the same holds inside a code span.
  local c9gy="$tmp/c9gy"; make_corpus "$c9gy"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nSee `docs/guide/mail&#46;md`\n' \
    > "$c9gy/README.md"
  git -C "$c9gy" add -A && git -C "$c9gy" commit -qm entity-in-code-span
  check "a character reference in a code span stays literal" fail "$c9gy"

  # `[](x.md)` renders an anchor with no content: nothing to see, nothing to
  # click, so it is not a route.
  local c9gz="$tmp/c9gz"; make_corpus "$c9gz"
  printf '# Jobs\n\n[](mail.md)\n' > "$c9gz/docs/guide/jobs.md"
  git -C "$c9gz" add -A && git -C "$c9gz" commit -qm empty-label-link
  check "an empty link label is not an edge" fail "$c9gz"

  # ...and whitespace is empty, the rule the sibling gate already applies.
  local c9ha="$tmp/c9ha"; make_corpus "$c9ha"
  printf '# Jobs\n\n[ ](mail.md)\n' > "$c9ha/docs/guide/jobs.md"
  git -C "$c9ha" add -A && git -C "$c9ha" commit -qm blank-label-link
  check "a whitespace-only link label is not an edge" fail "$c9ha"

  # ...but an IMAGE label is content — a clickable image — even though the
  # image itself is masked as invisible before the link scan runs.
  local c9hb="$tmp/c9hb"; make_corpus "$c9hb"
  printf '# Jobs\n\n[![alt](img.png)](mail.md)\n' > "$c9hb/docs/guide/jobs.md"
  git -C "$c9hb" add -A && git -C "$c9hb" commit -qm image-label-link
  check "an image label is a clickable route" pass "$c9hb"

  # An empty raw anchor is the same nothing.
  local c9hc="$tmp/c9hc"; make_corpus "$c9hc"
  printf '# Jobs\n\n<a href="mail.md"></a>\n' > "$c9hc/docs/guide/jobs.md"
  git -C "$c9hc" add -A && git -C "$c9hc" commit -qm empty-anchor
  check "an empty raw anchor is not an edge" fail "$c9hc"

  # ...and one wrapping an image is a link the reader can click.
  local c9hd="$tmp/c9hd"; make_corpus "$c9hd"
  printf '# Jobs\n\n<a href="mail.md"><img src="i.png"></a>\n' \
    > "$c9hd/docs/guide/jobs.md"
  git -C "$c9hd" add -A && git -C "$c9hd" commit -qm anchor-wrapping-image
  check "a raw anchor wrapping an image is a route" pass "$c9hd"

  # `[][m]` renders an empty anchor, the same as `[](x.md)`.
  local c9he="$tmp/c9he"; make_corpus "$c9he"
  printf '# Jobs\n\n[][m]\n\n[m]: mail.md\n' > "$c9he/docs/guide/jobs.md"
  git -C "$c9he" add -A && git -C "$c9he" commit -qm empty-full-reference
  check "an empty full reference is not an edge" fail "$c9he"

  # ...but one whose content is an image still is.
  local c9hf="$tmp/c9hf"; make_corpus "$c9hf"
  printf '# Jobs\n\n[![alt](img.png)][m]\n\n[m]: mail.md\n' \
    > "$c9hf/docs/guide/jobs.md"
  git -C "$c9hf" add -A && git -C "$c9hf" commit -qm image-full-reference
  check "a full reference around an image is a route" pass "$c9hf"

  # An anchor holding only a COMMENT renders nothing: the source looks like
  # content, the page shows none.
  local c9hg="$tmp/c9hg"; make_corpus "$c9hg"
  printf '# Jobs\n\n<a href="mail.md"><!-- hidden --></a>\n' \
    > "$c9hg/docs/guide/jobs.md"
  git -C "$c9hg" add -A && git -C "$c9hg" commit -qm anchor-comment-only
  check "an anchor holding only a comment is not an edge" fail "$c9hg"

  # `<!-->` is no comment, so the `<style>` after it is real and its contents
  # are not on screen.
  local c9hh="$tmp/c9hh"; make_corpus "$c9hh"
  printf '# Jobs\n\nprose <!--> <style> docs/guide/mail.md </style> -->\n' \
    > "$c9hh/docs/guide/jobs.md"
  git -C "$c9hh" add -A && git -C "$c9hh" commit -qm bad-comment-shields-style
  check "an invalid comment does not shield hidden HTML" fail "$c9hh"

  # A malformed mid-line opener is escaped text, not a tag, so it hides nothing.
  local c9hi="$tmp/c9hi"; make_corpus "$c9hi"
  printf '# Jobs\n\nprose <script a==>\n\nSee [mail](mail.md).\n' \
    > "$c9hi/docs/guide/jobs.md"
  git -C "$c9hi" add -A && git -C "$c9hi" commit -qm midline-bad-attr
  check "a malformed mid-line opener hides nothing" pass "$c9hi"

  # ...while a well-formed one still does: the browser sees that tag and
  # swallows everything after it.
  local c9hj="$tmp/c9hj"; make_corpus "$c9hj"
  printf '# Jobs\n\nprose <script a=b>\n\nSee [mail](mail.md).\n' \
    > "$c9hj/docs/guide/jobs.md"
  git -C "$c9hj" add -A && git -C "$c9hj" commit -qm midline-ok-attr
  check "a well-formed mid-line opener still hides what follows" fail "$c9hj"

  # An unterminated character reference renders literally: no `.md` on screen.
  local c9hk="$tmp/c9hk"; make_corpus "$c9hk"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nSee docs/guide/mail&#46md\n' \
    > "$c9hk/README.md"
  git -C "$c9hk" add -A && git -C "$c9hk" commit -qm unterminated-char-ref
  check "an unterminated character reference decodes to nothing" fail "$c9hk"

  # A backtick info string may not hold a backtick, so this opens no fence and
  # the definition under it is visible paragraph text.
  local c9hl="$tmp/c9hl"; make_corpus "$c9hl"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n```bad`\n[old]: docs/guide/mail.md\n' \
    > "$c9hl/README.md"
  git -C "$c9hl" add -A && git -C "$c9hl" commit -qm invalid-fence-then-def
  check "a definition under an invalid fence stays visible" pass "$c9hl"

  # A comment body may contain `--`, so the path inside one is still hidden.
  local c9hm="$tmp/c9hm"; make_corpus "$c9hm"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nprose <!-- docs/guide/mail.md -- x -->\n' \
    > "$c9hm/README.md"
  git -C "$c9hm" add -A && git -C "$c9hm" commit -qm dashes-in-comment
  check "a path in a comment containing -- is still hidden" fail "$c9hm"

  # An image spelled inside a comment renders nothing, so the anchor holding
  # only that comment is still empty.
  local c9hn="$tmp/c9hn"; make_corpus "$c9hn"
  printf '# Jobs\n\n<a href="mail.md"><!-- ![fake](x.png) --></a>\n' \
    > "$c9hn/docs/guide/jobs.md"
  git -C "$c9hn" add -A && git -C "$c9hn" commit -qm image-inside-comment
  check "an image inside a comment is not anchor content" fail "$c9hn"

  # An image-only reference AFTER a fence is still a clickable link: the views
  # it is judged against have to line up.
  local c9ho="$tmp/c9ho"; make_corpus "$c9ho"
  # The fence has to be BIG. With a short one the shifted offset happens to
  # land on non-blank bytes and the case passes either way — proving nothing.
  # Interpolated into the FORMAT, not passed as `%s`: as an argument the
  # `\n` stay literal backslash-n and the fence collapses to one line.
  local fence; fence="$(printf 'yyyyyyyyyyyyyyyyyyyy\\n%.0s' $(seq 8))"
  printf "# Jobs\n\n\`\`\`\n${fence}\`\`\`\n\n[![alt](x.png)][m]\n\n[m]: mail.md\n" \
    > "$c9ho/docs/guide/jobs.md"
  git -C "$c9ho" add -A && git -C "$c9ho" commit -qm image-ref-after-fence
  check "an image reference after a fence is a route" pass "$c9ho"

  # A raw href is decoded by the HTML tokenizer, which does not need the
  # semicolon — the browser navigates there.
  local c9hp="$tmp/c9hp"; make_corpus "$c9hp"
  printf '# Jobs\n\n<a href="mail&#46md">mail</a>\n' > "$c9hp/docs/guide/jobs.md"
  git -C "$c9hp" add -A && git -C "$c9hp" commit -qm raw-href-no-semicolon
  check "a raw href decodes by HTML rules" pass "$c9hp"

  # `<template` with no `>` opens nothing: it is not a type-1 tag.
  local c9hq="$tmp/c9hq"; make_corpus "$c9hq"
  printf '# Jobs\n\n<template\n\nSee [mail](mail.md).\n' > "$c9hq/docs/guide/jobs.md"
  git -C "$c9hq" add -A && git -C "$c9hq" commit -qm template-name-only
  check "a name-only template opener hides nothing" pass "$c9hq"

  # ...while `<script` with no `>` still does, and a COMPLETE `<template>`
  # still hides what it wraps.
  local c9hr="$tmp/c9hr"; make_corpus "$c9hr"
  printf '# Jobs\n\n<template>\nSee [mail](mail.md).\n</template>\n' \
    > "$c9hr/docs/guide/jobs.md"
  git -C "$c9hr" add -A && git -C "$c9hr" commit -qm template-complete
  check "a complete template tag still hides its contents" fail "$c9hr"

  # A label matches LITERALLY, backslash and all: normalization is case
  # folding and whitespace, nothing else.
  local c9hs="$tmp/c9hs"; make_corpus "$c9hs"
  printf '# Jobs\n\nSee [mail][x\\!].\n\n[x\\!]: mail.md\n' > "$c9hs/docs/guide/jobs.md"
  git -C "$c9hs" add -A && git -C "$c9hs" commit -qm label-escape-both
  check "a label escaped on both sides resolves" pass "$c9hs"

  # ...and escaping only one side is a different label.
  local c9ht="$tmp/c9ht"; make_corpus "$c9ht"
  printf '# Jobs\n\nSee [mail][x\\!].\n\n[x!]: mail.md\n' > "$c9ht/docs/guide/jobs.md"
  git -C "$c9ht" add -A && git -C "$c9ht" commit -qm label-escape-use-only
  check "a label escaped on one side does not resolve" fail "$c9ht"

  local c9hu="$tmp/c9hu"; make_corpus "$c9hu"
  printf '# Jobs\n\nSee [mail][x!].\n\n[x\\!]: mail.md\n' > "$c9hu/docs/guide/jobs.md"
  git -C "$c9hu" add -A && git -C "$c9hu" commit -qm label-escape-def-only
  check "a label escaped only in the definition does not resolve" fail "$c9hu"

  # An empty element is markup, not content: nothing to see, nothing to click.
  local c9hv="$tmp/c9hv"; make_corpus "$c9hv"
  printf '# Jobs\n\n<a href="mail.md"><span></span></a>\n' > "$c9hv/docs/guide/jobs.md"
  git -C "$c9hv" add -A && git -C "$c9hv" commit -qm anchor-empty-markup
  check "an anchor holding empty markup is not an edge" fail "$c9hv"

  # ...but the same element with TEXT in it is a route.
  local c9hw="$tmp/c9hw"; make_corpus "$c9hw"
  printf '# Jobs\n\n<a href="mail.md"><span>go</span></a>\n' > "$c9hw/docs/guide/jobs.md"
  git -C "$c9hw" add -A && git -C "$c9hw" commit -qm anchor-markup-with-text
  check "an anchor holding text inside markup is a route" pass "$c9hw"

  # A type-7 block needs its whole tag on ONE line, so a split tag opens
  # nothing and the link under it renders.
  local c9hx="$tmp/c9hx"; make_corpus "$c9hx"
  printf '# Jobs\n\n<span\n title=x>\nSee [mail](mail.md).\n' > "$c9hx/docs/guide/jobs.md"
  git -C "$c9hx" add -A && git -C "$c9hx" commit -qm type7-split-tag
  check "a tag split across lines opens no type-7 block" pass "$c9hx"

  # ...while the same tag on one line still opens one.
  local c9hy="$tmp/c9hy"; make_corpus "$c9hy"
  printf '# Jobs\n\n<span title=x>\nSee [mail](mail.md).\n' > "$c9hy/docs/guide/jobs.md"
  git -C "$c9hy" add -A && git -C "$c9hy" commit -qm type7-one-line-tag
  check "the same tag on one line opens a type-7 block" fail "$c9hy"

  # An href value is not visible text, so an EMPTY anchor confers nothing even
  # when its destination spells a repo path.
  local c9hz="$tmp/c9hz"; make_corpus "$c9hz"
  printf '# Jobs\n\n<a href="docs/guide/mail.md"></a>\n' > "$c9hz/docs/guide/jobs.md"
  git -C "$c9hz" add -A && git -C "$c9hz" commit -qm empty-anchor-repo-href
  check "an empty anchor with a repo-path href is not an edge" fail "$c9hz"

  # ...while the same href in a NON-empty anchor is still a route.
  local c9ia="$tmp/c9ia"; make_corpus "$c9ia"
  printf '# Jobs\n\n<a href="docs/guide/mail.md">mail</a>\n' > "$c9ia/docs/guide/jobs.md"
  git -C "$c9ia" add -A && git -C "$c9ia" commit -qm anchor-repo-href-with-text
  check "a repo-path href in a real anchor still counts" pass "$c9ia"

  # An icon-only link is a link: an inline SVG paints and is clickable.
  local c9ib="$tmp/c9ib"; make_corpus "$c9ib"
  printf '# Jobs\n\n<a href="mail.md"><svg><circle r="2"/></svg></a>\n' \
    > "$c9ib/docs/guide/jobs.md"
  git -C "$c9ib" add -A && git -C "$c9ib" commit -qm icon-only-link
  check "an icon-only SVG link is a route" pass "$c9ib"

  # A browser renders the frame and ignores the fallback text inside it.
  local c9ic="$tmp/c9ic"; make_corpus "$c9ic"
  printf '# Jobs\n\n<iframe src="about:blank">docs/guide/mail.md</iframe>\n' \
    > "$c9ic/docs/guide/jobs.md"
  git -C "$c9ic" add -A && git -C "$c9ic" commit -qm iframe-fallback-text
  check "iframe fallback text is not visible" fail "$c9ic"

  # ...and an anchor wrapping one is not a route either: the element is hidden
  # content, and an embedded document gives the anchor no clickable area.
  local c9id="$tmp/c9id"; make_corpus "$c9id"
  printf '# Jobs\n\n<a href="mail.md"><iframe src="about:blank"></iframe></a>\n' \
    > "$c9id/docs/guide/jobs.md"
  git -C "$c9id" add -A && git -C "$c9id" commit -qm iframe-as-link-content
  check "an anchor wrapping only an iframe is not a route" fail "$c9id"

  # A bare container marker opens an EMPTY block, so the definition under it is
  # a definition and renders nothing.
  local c9ie="$tmp/c9ie"; make_corpus "$c9ie"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n-\n[unused]: docs/guide/mail.md\n' \
    > "$c9ie/README.md"
  git -C "$c9ie" add -A && git -C "$c9ie" commit -qm empty-list-marker
  check "a definition under an empty list marker is not visible" fail "$c9ie"

  local c9if="$tmp/c9if"; make_corpus "$c9if"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n>\n[unused]: docs/guide/mail.md\n' \
    > "$c9if/README.md"
  git -C "$c9if" add -A && git -C "$c9if" commit -qm empty-quote-marker
  check "a definition under an empty quote marker is not visible" fail "$c9if"

  # ...but a marker WITH content opens a paragraph, and the line under it is
  # lazy continuation the reader can see.
  local c9ig="$tmp/c9ig"; make_corpus "$c9ig"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n- item\n[unused]: docs/guide/mail.md\n' \
    > "$c9ig/README.md"
  git -C "$c9ig" add -A && git -C "$c9ig" commit -qm list-item-with-text
  check "a definition continuing a list item is visible text" pass "$c9ig"

  # Markdown eats the punctuation escape, so the reader sees the real path.
  local c9ih="$tmp/c9ih"; make_corpus "$c9ih"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\nSee docs/guide/mail\\.md\n' \
    > "$c9ih/README.md"
  git -C "$c9ih" add -A && git -C "$c9ih" commit -qm escaped-path-in-prose
  check "an escaped path in prose still names the page" pass "$c9ih"

  # ...but inside a fence the escape is literal, so the path is not on screen.
  local c9ii="$tmp/c9ii"; make_corpus "$c9ii"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n```\ndocs/guide/mail\\.md\n```\n' \
    > "$c9ii/README.md"
  git -C "$c9ii" add -A && git -C "$c9ii" commit -qm escaped-path-in-fence
  check "an escaped path in a fence stays literal" fail "$c9ii"

  # A shortcut reference's label is its rendered content, so an empty one is
  # an anchor with nothing in it.
  local c9ij="$tmp/c9ij"; make_corpus "$c9ij"
  printf '# Jobs\n\n[<span></span>]\n\n[<span></span>]: mail.md\n' \
    > "$c9ij/docs/guide/jobs.md"
  git -C "$c9ij" add -A && git -C "$c9ij" commit -qm empty-shortcut-reference
  check "an empty shortcut reference is not an edge" fail "$c9ij"

  # A `hidden` subtree shows nothing, so the anchor around it is empty.
  local c9ik="$tmp/c9ik"; make_corpus "$c9ik"
  printf '# Jobs\n\n<a href="mail.md"><span hidden>Mail</span></a>\n' \
    > "$c9ik/docs/guide/jobs.md"
  git -C "$c9ik" add -A && git -C "$c9ik" commit -qm hidden-subtree
  check "a hidden subtree is not link content" fail "$c9ik"

  # ...while the same span without `hidden` is a route.
  local c9il="$tmp/c9il"; make_corpus "$c9il"
  printf '# Jobs\n\n<a href="mail.md"><span>Mail</span></a>\n' \
    > "$c9il/docs/guide/jobs.md"
  git -C "$c9il" add -A && git -C "$c9il" commit -qm visible-subtree
  check "a visible subtree is link content" pass "$c9il"

  # Nesting: the close that ends a hidden element is the one that BALANCES it.
  local c9im="$tmp/c9im"; make_corpus "$c9im"
  printf '# Jobs\n\n<a href="mail.md"><span hidden><span></span>Mail</span></a>\n' \
    > "$c9im/docs/guide/jobs.md"
  git -C "$c9im" add -A && git -C "$c9im" commit -qm nested-hidden-subtree
  check "a nested hidden subtree is masked whole" fail "$c9im"

  # ...and text AFTER the hidden element closes is still content.
  local c9in="$tmp/c9in"; make_corpus "$c9in"
  printf '# Jobs\n\n<a href="mail.md"><span hidden>x</span>Mail</a>\n' \
    > "$c9in/docs/guide/jobs.md"
  git -C "$c9in" add -A && git -C "$c9in" commit -qm text-after-hidden
  check "text after a hidden element is still content" pass "$c9in"

  # A VOID element has no close tag, so a hidden one hides only itself and the
  # label after it is a live, clickable route. Searching for the `</input>`
  # that cannot exist masked the rest of the link and orphaned its target.
  local c9io="$tmp/c9io"; make_corpus "$c9io"
  printf '# Jobs\n\n<a href="mail.md"><input hidden>Mail</a>\n' \
    > "$c9io/docs/guide/jobs.md"
  git -C "$c9io" add -A && git -C "$c9io" commit -qm hidden-void-element
  check "a hidden void element does not hide the label after it" pass "$c9io"

  # Same for the void element that is also an image: hidden, so it is not
  # content itself, but the text beside it still is.
  local c9ip="$tmp/c9ip"; make_corpus "$c9ip"
  printf '# Jobs\n\n<a href="mail.md"><img hidden src="x.png">Mail</a>\n' \
    > "$c9ip/docs/guide/jobs.md"
  git -C "$c9ip" add -A && git -C "$c9ip" commit -qm hidden-void-image
  check "a hidden void image does not hide the label after it" pass "$c9ip"

  # ...but `/>` on an HTML element is NOT self-closing. The parser drops the
  # slash, the span stays open, and it swallows the label — pinned because
  # generalising the void fix to every `/>` would call this a route.
  local c9iq="$tmp/c9iq"; make_corpus "$c9iq"
  printf '# Jobs\n\n<a href="mail.md"><span hidden/>Mail</a>\n' \
    > "$c9iq/docs/guide/jobs.md"
  git -C "$c9iq" add -A && git -C "$c9iq" commit -qm html-slash-not-self-closing
  check "a slash does not self-close an HTML element" fail "$c9iq"

  # `hidden` does not reach an SVG: the UA rule that implements it is
  # namespaced to HTML, so the icon still paints and the anchor is a route.
  local c9ir="$tmp/c9ir"; make_corpus "$c9ir"
  printf '# Jobs\n\n<a href="mail.md"><svg hidden width="9" height="9"></svg></a>\n' \
    > "$c9ir/docs/guide/jobs.md"
  git -C "$c9ir" add -A && git -C "$c9ir" commit -qm hidden-does-not-reach-svg
  check "a hidden attribute does not hide an svg" pass "$c9ir"

  # ...and because it never masks, a self-closing nested SVG cannot unbalance
  # the scan and swallow the label after it.
  local c9ix="$tmp/c9ix"; make_corpus "$c9ix"
  printf '# Jobs\n\n<a href="mail.md"><svg hidden><svg/></svg>Mail</a>\n' \
    > "$c9ix/docs/guide/jobs.md"
  git -C "$c9ix" add -A && git -C "$c9ix" commit -qm nested-self-closing-svg
  check "a nested self-closing svg does not swallow the label" pass "$c9ix"

  # An unclosed non-void element is closed implicitly by its parent, so it
  # hides everything to the end of the link — masking that far is correct.
  local c9is="$tmp/c9is"; make_corpus "$c9is"
  printf '# Jobs\n\n<a href="mail.md"><span hidden>Mail</a>\n' \
    > "$c9is/docs/guide/jobs.md"
  git -C "$c9is" add -A && git -C "$c9is" commit -qm unclosed-hidden-element
  check "an unclosed hidden element hides the rest of the link" fail "$c9is"

  # `&#32;` is a space on screen, so this is an anchor with nothing in it —
  # the same nothing as `[ ](mail.md)`, which was already rejected.
  local c9it="$tmp/c9it"; make_corpus "$c9it"
  printf '# Jobs\n\n[&#32;](docs/guide/mail.md)\n' > "$c9it/docs/guide/jobs.md"
  git -C "$c9it" add -A && git -C "$c9it" commit -qm entity-space-label
  check "an entity-spelled space is not a label" fail "$c9it"

  # ...and the same in a raw anchor, which shares the content test.
  local c9iu="$tmp/c9iu"; make_corpus "$c9iu"
  printf '# Jobs\n\n<a href="mail.md">&#32;</a>\n' > "$c9iu/docs/guide/jobs.md"
  git -C "$c9iu" add -A && git -C "$c9iu" commit -qm entity-space-anchor
  check "an entity-spelled space is not anchor content" fail "$c9iu"

  # ...but a reference that decodes to a VISIBLE character is a real label,
  # so the decode must not be mistaken for "entities mean empty".
  local c9iv="$tmp/c9iv"; make_corpus "$c9iv"
  printf '# Jobs\n\n[&#65;](docs/guide/mail.md)\n' > "$c9iv/docs/guide/jobs.md"
  git -C "$c9iv" add -A && git -C "$c9iv" commit -qm entity-letter-label
  check "an entity-spelled letter is a label" pass "$c9iv"

  # ...and the decode must run AFTER the tags come out. `&#60;span&#62;` is
  # the visible text `<span>`, not a tag; decoding first would let `ANY_TAG`
  # strip it and report a live link as empty.
  # (The code-span half of the decode is deliberately not tested here: inside
  # this emptiness check the backticks are themselves non-blank text, so
  # `` [`&#32;`](mail.md) `` passes whether or not code spans are exempt. Such
  # a test would verify the delimiters, not the rule.)
  local c9iw="$tmp/c9iw"; make_corpus "$c9iw"
  printf '# Jobs\n\n<a href="mail.md">&#60;span&#62;</a>\n' > "$c9iw/docs/guide/jobs.md"
  git -C "$c9iw" add -A && git -C "$c9iw" commit -qm entity-spelled-tag-text
  check "an entity-spelled tag is visible text, not markup" pass "$c9iw"

  # Inline `display:none` hides a link as surely as the attribute does.
  local c9jl="$tmp/c9jl"; make_corpus "$c9jl"
  printf '# Jobs\n\n<a style="display:none" href="mail.md">Mail</a>\n' \
    > "$c9jl/docs/guide/jobs.md"
  git -C "$c9jl" add -A && git -C "$c9jl" commit -qm anchor-display-none
  check "an anchor hidden by inline CSS is not a route" fail "$c9jl"

  # ...and so does a descendant carrying it.
  local c9jm="$tmp/c9jm"; make_corpus "$c9jm"
  printf '# Jobs\n\n<a href="mail.md"><span style="display:none">Mail</span></a>\n' \
    > "$c9jm/docs/guide/jobs.md"
  git -C "$c9jm" add -A && git -C "$c9jm" commit -qm descendant-display-none
  check "a subtree hidden by inline CSS is not content" fail "$c9jm"

  # CSS is not the `hidden` attribute: it DOES reach an SVG. The pair of this
  # and `a hidden attribute does not hide an svg` is the whole distinction.
  local c9jn="$tmp/c9jn"; make_corpus "$c9jn"
  printf '# Jobs\n\n<a href="mail.md"><svg style="display:none" width="9" height="9"></svg></a>\n' \
    > "$c9jn/docs/guide/jobs.md"
  git -C "$c9jn" add -A && git -C "$c9jn" commit -qm svg-display-none
  check "inline CSS does hide an svg" fail "$c9jn"

  # A `<p>` cannot nest in itself: the second opener CLOSES the hidden first
  # one, so the label after it is visible and clickable.
  local c9kt="$tmp/c9kt"; make_corpus "$c9kt"
  printf '# Jobs\n\n<a href="mail.md"><p hidden>Secret<p>Mail</p></a>\n' \
    > "$c9kt/docs/guide/jobs.md"
  git -C "$c9kt" add -A && git -C "$c9kt" commit -qm p-reopened
  check "a reopened p ends the hidden one" pass "$c9kt"

  local c9ku="$tmp/c9ku"; make_corpus "$c9ku"
  printf '# Jobs\n\n<a href="mail.md"><ul><li hidden>Secret<li>Mail</li></ul></a>\n' \
    > "$c9ku/docs/guide/jobs.md"
  git -C "$c9ku" add -A && git -C "$c9ku" commit -qm li-reopened
  check "a reopened li ends the hidden one" pass "$c9ku"

  # U+00A0 PAINTS — an anchor holding just `&nbsp;` is 4px wide and clickable
  # — so a bare `.strip()`, which treats it as whitespace, called a live link
  # empty. The zero-width characters are removed for the opposite reason.
  local c9ln="$tmp/c9ln"; make_corpus "$c9ln"
  printf '# Jobs\n\n[&nbsp;](mail.md)\n' > "$c9ln/docs/guide/jobs.md"
  git -C "$c9ln" add -A && git -C "$c9ln" commit -qm nbsp-label
  check "a non-breaking space is a visible label" pass "$c9ln"

  # ...while an ASCII space paints nothing and is still empty.
  local c9lo="$tmp/c9lo"; make_corpus "$c9lo"
  printf '# Jobs\n\n[ ](mail.md)\n' > "$c9lo/docs/guide/jobs.md"
  git -C "$c9lo" add -A && git -C "$c9lo" commit -qm ascii-space-label
  check "an ASCII space is not a visible label" fail "$c9lo"

  # URL parsing removes tab and line breaks from ANYWHERE in the URL, so this
  # href navigates to `mail.md` and the page is reachable.
  local c9lp="$tmp/c9lp"; make_corpus "$c9lp"
  printf '# Jobs\n\n<a href="ma\nil.md">Mail</a>\n' > "$c9lp/docs/guide/jobs.md"
  git -C "$c9lp" add -A && git -C "$c9lp" commit -qm newline-in-href
  check "a line break inside an href is removed" pass "$c9lp"

  local c9lq="$tmp/c9lq"; make_corpus "$c9lq"
  printf '# Jobs\n\n<a href="ma\til.md">Mail</a>\n' > "$c9lq/docs/guide/jobs.md"
  git -C "$c9lq" add -A && git -C "$c9lq" commit -qm tab-in-href
  check "a tab inside an href is removed" pass "$c9lq"

  # ...but a SPACE is not removed, it percent-encodes and stays in the path.
  local c9lr="$tmp/c9lr"; make_corpus "$c9lr"
  printf '# Jobs\n\n<a href="ma il.md">Mail</a>\n' > "$c9lr/docs/guide/jobs.md"
  git -C "$c9lr" add -A && git -C "$c9lr" commit -qm space-in-href
  check "a space inside an href stays in the path" fail "$c9lr"

  # U+00A0 stays IN a URL path, so this href resolves to `\xa0mail.md` and
  # reaches the tracked file not at all. Python's bare `.strip()` removed it.
  local c9li="$tmp/c9li"; make_corpus "$c9li"
  printf '# Jobs\n\n<a href="&nbsp;mail.md">Mail</a>\n' > "$c9li/docs/guide/jobs.md"
  git -C "$c9li" add -A && git -C "$c9li" commit -qm nbsp-in-href
  check "a non-breaking space in an href is part of the path" fail "$c9li"

  # ...while ASCII padding IS discarded by URL processing, so it still resolves.
  local c9lj="$tmp/c9lj"; make_corpus "$c9lj"
  printf '# Jobs\n\n<a href="  mail.md  ">Mail</a>\n' > "$c9lj/docs/guide/jobs.md"
  git -C "$c9lj" add -A && git -C "$c9lj" commit -qm ascii-padded-href
  check "ASCII padding in an href is discarded" pass "$c9lj"

  # An UNMATCHED end tag closes nothing — the parser drops it — so it cannot
  # end an inert subtree either. Treating every unrecognised close as an
  # ancestor's ended the span at `</bogus>` and made a dead link a route.
  local c9ll="$tmp/c9ll"; make_corpus "$c9ll"
  printf '# Jobs\n\nx <div inert></bogus><a href="mail.md">Mail</a></div>\n' \
    > "$c9ll/docs/guide/jobs.md"
  git -C "$c9ll" add -A && git -C "$c9ll" commit -qm unmatched-close-inert
  check "an unmatched close does not end an inert subtree" fail "$c9ll"

  # ...and the same inside a hidden one.
  local c9lm="$tmp/c9lm"; make_corpus "$c9lm"
  printf '# Jobs\n\n<a href="mail.md"><span hidden></bogus>Secret</span></a>\n' \
    > "$c9lm/docs/guide/jobs.md"
  git -C "$c9lm" add -A && git -C "$c9lm" commit -qm unmatched-close-hidden
  check "an unmatched close does not end a hidden subtree" fail "$c9lm"

  # Unwinding must find the INNERMOST match: closing the inner `div` here left
  # the whole stack cleared, and the `</span>` after it read as an ancestor
  # close, ending the hidden subtree early and exposing its text.
  local c9lk="$tmp/c9lk"; make_corpus "$c9lk"
  printf '# Jobs\n\n<a href="mail.md"><section hidden><div><span><div></div></span>Secret</div></section></a>\n' \
    > "$c9lk/docs/guide/jobs.md"
  git -C "$c9lk" add -A && git -C "$c9lk" commit -qm repeated-descendant-name
  check "a repeated descendant name unwinds to the innermost" fail "$c9lk"

  # A PARENT's close takes its open child with it, so the label after it is
  # visible. The depth counter could not see this at all: it only ever looked
  # for the scanned element's own name.
  local c9lc="$tmp/c9lc"; make_corpus "$c9lc"
  printf '# Jobs\n\n<a href="mail.md"><ul><li hidden>Secret</ul>Mail</a>\n' \
    > "$c9lc/docs/guide/jobs.md"
  git -C "$c9lc" add -A && git -C "$c9lc" commit -qm li-ended-by-ul-close
  check "a parent close ends the hidden li" pass "$c9lc"

  # ...and this is not an optional-end-tag rule: a `</div>` ends an open
  # `<span>` the same way, which the reported case did not cover.
  local c9ld="$tmp/c9ld"; make_corpus "$c9ld"
  printf '# Jobs\n\n<a href="mail.md"><div><span hidden>Secret</div>Mail</a>\n' \
    > "$c9ld/docs/guide/jobs.md"
  git -C "$c9ld" add -A && git -C "$c9ld" commit -qm span-ended-by-div-close
  check "a parent close ends a hidden span too" pass "$c9ld"

  # ...but a DESCENDANT's close is not the parent's, and must not end it.
  local c9le="$tmp/c9le"; make_corpus "$c9le"
  printf '# Jobs\n\n<a href="mail.md"><span hidden><em>x</em>Secret</span>Mail</a>\n' \
    > "$c9le/docs/guide/jobs.md"
  git -C "$c9le" add -A && git -C "$c9le" commit -qm descendant-close
  check "a descendant close does not end the hidden span" pass "$c9le"

  # ...and a `p` is ended by any BLOCK element, not only another `p`.
  local c9kx="$tmp/c9kx"; make_corpus "$c9kx"
  printf '# Jobs\n\n<a href="mail.md"><p hidden>Secret<div>Mail</div></a>\n' \
    > "$c9kx/docs/guide/jobs.md"
  git -C "$c9kx" add -A && git -C "$c9kx" commit -qm p-closed-by-div
  check "a div ends a hidden p" pass "$c9kx"

  # ...but an INLINE element does not, which is what stops this from
  # swallowing ordinary markup inside a hidden paragraph.
  local c9ky="$tmp/c9ky"; make_corpus "$c9ky"
  printf '# Jobs\n\n<a href="mail.md"><p hidden>Secret<span>Mail</span></a>\n' \
    > "$c9ky/docs/guide/jobs.md"
  git -C "$c9ky" add -A && git -C "$c9ky" commit -qm p-not-closed-by-span
  check "a span does not end a hidden p" fail "$c9ky"

  # ...and the wide closer set belongs to `p` alone: a div does NOT end an li.
  local c9kz="$tmp/c9kz"; make_corpus "$c9kz"
  printf '# Jobs\n\n<a href="mail.md"><ul><li hidden>Secret<div>Mail</div></li></ul></a>\n' \
    > "$c9kz/docs/guide/jobs.md"
  git -C "$c9kz" add -A && git -C "$c9kz" commit -qm li-not-closed-by-div
  check "a div does not end a hidden li" fail "$c9kz"

  # ...but `span` and `div` DO nest, so this is a property of those elements
  # rather than a general "a second opener ends the first".
  local c9kv="$tmp/c9kv"; make_corpus "$c9kv"
  printf '# Jobs\n\n<a href="mail.md"><span hidden>Secret<span>Mail</span></span></a>\n' \
    > "$c9kv/docs/guide/jobs.md"
  git -C "$c9kv" add -A && git -C "$c9kv" commit -qm span-still-nests
  check "a nested span is still hidden" fail "$c9kv"

  # ...and a hidden one that is never reopened still hides everything.
  local c9kw="$tmp/c9kw"; make_corpus "$c9kw"
  printf '# Jobs\n\n<a href="mail.md"><p hidden>Secret</p></a>\n' \
    > "$c9kw/docs/guide/jobs.md"
  git -C "$c9kw" add -A && git -C "$c9kw" commit -qm p-hidden-alone
  check "a hidden p with no reopen is still hidden" fail "$c9kw"

  # A full reference's FIRST label is link text and nests too. When the blank
  # missed it, the trailing `[m]` was left looking like a standalone shortcut
  # link, resurrecting a label the reader cannot see.
  local c9kp="$tmp/c9kp"; make_corpus "$c9kp"
  printf '# Jobs\n\n[<span hidden>[x]</span>][m]\n\n[m]: mail.md\n' \
    > "$c9kp/docs/guide/jobs.md"
  git -C "$c9kp" add -A && git -C "$c9kp" commit -qm hidden-nested-ref-label
  check "a hidden nested reference label is not a route" fail "$c9kp"

  # ...while the same shape with a VISIBLE label is a route, so the nesting
  # support did not simply start rejecting full references.
  local c9kq="$tmp/c9kq"; make_corpus "$c9kq"
  printf '# Jobs\n\n[outer [x]][m]\n\n[m]: mail.md\n' > "$c9kq/docs/guide/jobs.md"
  git -C "$c9kq" add -A && git -C "$c9kq" commit -qm visible-nested-ref-label
  check "a visible nested reference label is a route" pass "$c9kq"

  # A label of only zero-width characters gives the anchor a 0px box.
  local c9kr="$tmp/c9kr"; make_corpus "$c9kr"
  printf '# Jobs\n\n[&#8203;](mail.md)\n' > "$c9kr/docs/guide/jobs.md"
  git -C "$c9kr" add -A && git -C "$c9kr" commit -qm zero-width-label
  check "a zero-width label is not a label" fail "$c9kr"

  # ...but a decoded character that PAINTS still is, so this removes the
  # zero-width ones rather than everything that decodes.
  local c9ks="$tmp/c9ks"; make_corpus "$c9ks"
  printf '# Jobs\n\n[&#65;](mail.md)\n' > "$c9ks/docs/guide/jobs.md"
  git -C "$c9ks" add -A && git -C "$c9ks" commit -qm painting-entity-label
  check "an entity that paints is still a label" pass "$c9ks"

  # No BOUND at all: the label is scanned, not matched, so depth 17 and 40 —
  # each of which beat a previous bound — work like any other.
  local c9ls="$tmp/c9ls"; make_corpus "$c9ls"
  python3 -c "print('# Jobs\n\n[' + '['*17 + 'Mail' + ']'*17 + '](mail.md)')" \
    > "$c9ls/docs/guide/jobs.md"
  git -C "$c9ls" add -A && git -C "$c9ls" commit -qm label-depth-17
  check "a label nested seventeen deep is a link" pass "$c9ls"

  local c9lt="$tmp/c9lt"; make_corpus "$c9lt"
  python3 -c "print('# Jobs\n\n[' + '['*40 + 'Mail' + ']'*40 + '][m]\n\n[m]: mail.md')" \
    > "$c9lt/docs/guide/jobs.md"
  git -C "$c9lt" add -A && git -C "$c9lt" commit -qm ref-label-depth-40
  check "a reference label nested forty deep is a link" pass "$c9lt"

  # A label may nest brackets to any depth so long as they balance, and this
  # matched only one level for most of the PR.
  local c9km="$tmp/c9km"; make_corpus "$c9km"
  printf '# Jobs\n\n[outer [middle [Mail]]](mail.md)\n' > "$c9km/docs/guide/jobs.md"
  git -C "$c9km" add -A && git -C "$c9km" commit -qm nested-label-three
  check "a label nested three deep is still a link" pass "$c9km"

  local c9kn="$tmp/c9kn"; make_corpus "$c9kn"
  printf '# Jobs\n\n[a [b [c [d [Mail]]]]](mail.md)\n' > "$c9kn/docs/guide/jobs.md"
  git -C "$c9kn" add -A && git -C "$c9kn" commit -qm nested-label-five
  check "a label nested five deep is still a link" pass "$c9kn"

  # ...and an empty label is still empty however deep the pattern reaches.
  local c9ko="$tmp/c9ko"; make_corpus "$c9ko"
  printf '# Jobs\n\n[](mail.md)\n' > "$c9ko/docs/guide/jobs.md"
  git -C "$c9ko" add -A && git -C "$c9ko" commit -qm empty-label-still-empty
  check "an empty label is still not a route" fail "$c9ko"

  # `inert` does not hide, it DEACTIVATES: the anchor still paints at 30x17
  # but cannot be activated, so the reader cannot follow it.
  local c9ke="$tmp/c9ke"; make_corpus "$c9ke"
  printf '# Jobs\n\n<a inert href="mail.md">Mail</a>\n' > "$c9ke/docs/guide/jobs.md"
  git -C "$c9ke" add -A && git -C "$c9ke" commit -qm inert-anchor
  check "an inert anchor is not a route" fail "$c9ke"

  # ...and it applies to the whole subtree, not just the tag carrying it.
  local c9kf="$tmp/c9kf"; make_corpus "$c9kf"
  printf '# Jobs\n\n<div inert><a href="mail.md">Mail</a></div>\n' \
    > "$c9kf/docs/guide/jobs.md"
  git -C "$c9kf" add -A && git -C "$c9kf" commit -qm inert-subtree
  check "an anchor inside an inert subtree is not a route" fail "$c9kf"

  # ...but the subtree ENDS, so a link after it is live.
  local c9kg="$tmp/c9kg"; make_corpus "$c9kg"
  printf '# Jobs\n\n<div inert>x</div><a href="mail.md">Mail</a>\n' \
    > "$c9kg/docs/guide/jobs.md"
  git -C "$c9kg" add -A && git -C "$c9kg" commit -qm inert-ends
  check "a link after an inert subtree is a route" pass "$c9kg"

  # ...and an `inert` written inside a comment deactivates nothing.
  local c9kh="$tmp/c9kh"; make_corpus "$c9kh"
  printf '# Jobs\n\nx <!-- <div inert> --> <a href="mail.md">Mail</a>\n' \
    > "$c9kh/docs/guide/jobs.md"
  git -C "$c9kh" add -A && git -C "$c9kh" commit -qm commented-inert
  check "a commented-out inert deactivates nothing" pass "$c9kh"

  # ...and a close spelled inside a COMMENT does not end the subtree, so the
  # anchor after it is still inert. The opening tag has to be read from the
  # raw text for its attributes; the close is structure and must be balanced
  # over the masked view, which is the half that was wrong.
  local c9kl="$tmp/c9kl"; make_corpus "$c9kl"
  printf '# Jobs\n\nx <div inert><!-- </div> --><a href="mail.md">Mail</a></div>\n' \
    > "$c9kl/docs/guide/jobs.md"
  git -C "$c9kl" add -A && git -C "$c9kl" commit -qm commented-close-in-inert
  check "a commented close does not end an inert subtree" fail "$c9kl"

  # A bare PATH inside an inert subtree is still on screen and still counts:
  # inert kills the link, not the text.
  local c9ki="$tmp/c9ki"; make_corpus "$c9ki"
  printf '# Jobs\n\n<div inert>see docs/guide/mail.md</div>\n' \
    > "$c9ki/docs/guide/jobs.md"
  git -C "$c9ki" add -A && git -C "$c9ki" commit -qm inert-bare-path
  check "a bare path inside an inert subtree still counts" pass "$c9ki"

  # An EMPTY picture paints nothing — the same nothing as an empty container.
  local c9kj="$tmp/c9kj"; make_corpus "$c9kj"
  printf '# Jobs\n\n<a href="mail.md"><picture></picture></a>\n' \
    > "$c9kj/docs/guide/jobs.md"
  git -C "$c9kj" add -A && git -C "$c9kj" commit -qm empty-picture
  check "an empty picture is not link content" fail "$c9kj"

  # ...while a real one contains the `img` that paints, and that is what counts.
  local c9kk="$tmp/c9kk"; make_corpus "$c9kk"
  printf '# Jobs\n\n<a href="mail.md"><picture><img src="x.png"></picture></a>\n' \
    > "$c9kk/docs/guide/jobs.md"
  git -C "$c9kk" add -A && git -C "$c9kk" commit -qm picture-with-img
  check "a picture wrapping an img is link content" pass "$c9kk"

  # `progress` paints with no attributes at all, so an anchor around one is a
  # link the reader can see and click (160x17, hit-testing inside the anchor).
  local c9kb="$tmp/c9kb"; make_corpus "$c9kb"
  printf '# Jobs\n\n<a href="mail.md"><progress></progress></a>\n' \
    > "$c9kb/docs/guide/jobs.md"
  git -C "$c9kb" add -A && git -C "$c9kb" commit -qm progress-content
  check "a progress bar is link content" pass "$c9kb"

  local c9kc="$tmp/c9kc"; make_corpus "$c9kc"
  printf '# Jobs\n\n<a href="mail.md"><meter></meter></a>\n' \
    > "$c9kc/docs/guide/jobs.md"
  git -C "$c9kc" add -A && git -C "$c9kc" commit -qm meter-content
  check "a meter is link content" pass "$c9kc"

  # ...but hiding it still hides it, so this is about painting rather than
  # about the tag name being on the list.
  local c9kd="$tmp/c9kd"; make_corpus "$c9kd"
  printf '# Jobs\n\n<a href="mail.md"><progress hidden></progress></a>\n' \
    > "$c9kd/docs/guide/jobs.md"
  git -C "$c9kd" add -A && git -C "$c9kd" commit -qm hidden-progress
  check "a hidden progress bar is not link content" fail "$c9kd"

  # A style attribute is a CASCADE: the later declaration wins, so this link
  # is visible and matching any `display:none` occurrence rejected it.
  local c9jw="$tmp/c9jw"; make_corpus "$c9jw"
  printf '# Jobs\n\n<a style="display:none; display:block" href="mail.md">Mail</a>\n' \
    > "$c9jw/docs/guide/jobs.md"
  git -C "$c9jw" add -A && git -C "$c9jw" commit -qm cascade-later-wins
  check "a later display declaration wins" pass "$c9jw"

  # ...in the other order it really is hidden, so the rule is order, not
  # "an override exists somewhere".
  local c9jx="$tmp/c9jx"; make_corpus "$c9jx"
  printf '# Jobs\n\n<a style="display:block; display:none" href="mail.md">Mail</a>\n' \
    > "$c9jx/docs/guide/jobs.md"
  git -C "$c9jx" add -A && git -C "$c9jx" commit -qm cascade-none-last
  check "a later display:none still hides" fail "$c9jx"

  # ...and `!important` beats declaration order, both ways round.
  local c9jy="$tmp/c9jy"; make_corpus "$c9jy"
  printf '# Jobs\n\n<a style="display:none !important; display:block" href="mail.md">Mail</a>\n' \
    > "$c9jy/docs/guide/jobs.md"
  git -C "$c9jy" add -A && git -C "$c9jy" commit -qm cascade-important-none
  check "an important display:none beats a later block" fail "$c9jy"

  local c9jz="$tmp/c9jz"; make_corpus "$c9jz"
  printf '# Jobs\n\n<a style="display:none; display:block !important" href="mail.md">Mail</a>\n' \
    > "$c9jz/docs/guide/jobs.md"
  git -C "$c9jz" add -A && git -C "$c9jz" commit -qm cascade-important-block
  check "an important display:block beats an earlier none" pass "$c9jz"

  # ...and CSS is case-insensitive, so the cascade must be read that way too.
  local c9ka="$tmp/c9ka"; make_corpus "$c9ka"
  printf '# Jobs\n\n<a style="DISPLAY:NONE; Display: Block" href="mail.md">Mail</a>\n' \
    > "$c9ka/docs/guide/jobs.md"
  git -C "$c9ka" add -A && git -C "$c9ka" commit -qm cascade-case-insensitive
  check "the cascade is read case-insensitively" pass "$c9ka"

  # A CSS comment is not a declaration, terminated or not.
  local c9la="$tmp/c9la"; make_corpus "$c9la"
  printf '# Jobs\n\n<a style="color:red; /* display:none; */" href="mail.md">Mail</a>\n' \
    > "$c9la/docs/guide/jobs.md"
  git -C "$c9la" add -A && git -C "$c9la" commit -qm css-comment
  check "a commented-out declaration does not hide" pass "$c9la"

  local c9lb="$tmp/c9lb"; make_corpus "$c9lb"
  printf '# Jobs\n\n<a style="color:red; /* display:none;" href="mail.md">Mail</a>\n' \
    > "$c9lb/docs/guide/jobs.md"
  git -C "$c9lb" add -A && git -C "$c9lb" commit -qm css-comment-unterminated
  check "an unterminated CSS comment runs to the end" pass "$c9lb"

  # `--display` is a custom PROPERTY and changes nothing, so the match needs a
  # property boundary — a substring test rejected this live link.
  local c9js="$tmp/c9js"; make_corpus "$c9js"
  printf '# Jobs\n\n<a style="--display:none" href="mail.md">Mail</a>\n' \
    > "$c9js/docs/guide/jobs.md"
  git -C "$c9js" add -A && git -C "$c9js" commit -qm custom-property
  check "a custom property that ends in display is not it" pass "$c9js"

  # ...and the value needs its own boundary: `none-such` is not `none`.
  local c9jt="$tmp/c9jt"; make_corpus "$c9jt"
  printf '# Jobs\n\n<a style="display:none-such" href="mail.md">Mail</a>\n' \
    > "$c9jt/docs/guide/jobs.md"
  git -C "$c9jt" add -A && git -C "$c9jt" commit -qm value-prefix
  check "a value that merely starts with none is not none" pass "$c9jt"

  # ...but a real declaration beside others still hides.
  local c9ju="$tmp/c9ju"; make_corpus "$c9ju"
  printf '# Jobs\n\n<a style="color:red;display:none" href="mail.md">Mail</a>\n' \
    > "$c9ju/docs/guide/jobs.md"
  git -C "$c9ju" add -A && git -C "$c9ju" commit -qm declaration-among-others
  check "a real declaration among others still hides" fail "$c9ju"

  # Inside a foreign-content root `/>` really closes, so a nested `<svg/>`
  # opens no level. Counting it as one swallowed the label after the outer
  # close — the same shape as the `hidden` case, reached by the CSS path.
  local c9jv="$tmp/c9jv"; make_corpus "$c9jv"
  printf '# Jobs\n\n<a href="mail.md"><svg style="display:none"><svg/></svg>Mail</a>\n' \
    > "$c9jv/docs/guide/jobs.md"
  git -C "$c9jv" add -A && git -C "$c9jv" commit -qm nested-selfclose-svg-css
  check "a self-closing svg opens no nesting level" pass "$c9jv"

  # It is the declaration that hides, not the word: `display:block` is a route.
  local c9jo="$tmp/c9jo"; make_corpus "$c9jo"
  printf '# Jobs\n\n<a style="display:block" href="mail.md">Mail</a>\n' \
    > "$c9jo/docs/guide/jobs.md"
  git -C "$c9jo" add -A && git -C "$c9jo" commit -qm display-block
  check "display:block is still a route" pass "$c9jo"

  # ...and a hidden span written inside a COMMENT hides nothing, which is what
  # the masked-view check on the tag position is for.
  local c9jp="$tmp/c9jp"; make_corpus "$c9jp"
  printf '# Jobs\n\n<a href="mail.md"><!-- <span style="display:none"> -->Mail</a>\n' \
    > "$c9jp/docs/guide/jobs.md"
  git -C "$c9jp" add -A && git -C "$c9jp" commit -qm commented-display-none
  check "a commented-out hidden span hides nothing" pass "$c9jp"

  # A `</a>` spelled inside a script is script text, so the anchor stays open
  # and the label after it is live.
  local c9jq="$tmp/c9jq"; make_corpus "$c9jq"
  printf '# Jobs\n\n<a href="mail.md"><script>const fake="</a>";</script>Mail</a>\n' \
    > "$c9jq/docs/guide/jobs.md"
  git -C "$c9jq" add -A && git -C "$c9jq" commit -qm scripted-fake-close
  check "a close spelled in a script does not end an anchor" pass "$c9jq"

  # ...and the same inside a comment.
  local c9jr="$tmp/c9jr"; make_corpus "$c9jr"
  printf '# Jobs\n\n<a href="mail.md"><!-- </a> -->Mail</a>\n' \
    > "$c9jr/docs/guide/jobs.md"
  git -C "$c9jr" add -A && git -C "$c9jr" commit -qm commented-fake-close
  check "a close spelled in a comment does not end an anchor" pass "$c9jr"

  # Whitespace just inside angle brackets is syntax, not part of the URL.
  # Reported as the opposite — that `[Mail](<mail.md >)` navigates to
  # `mail.md%20` and so is not a route — but both cmark-gfm (the renderer
  # GitHub runs) and markdown-it emit `href="mail.md"`. Only INTERIOR
  # whitespace is percent-encoded, and `.strip()` does not touch that. Keeping
  # the space would strand a page whose only link works fine.
  local c9jj="$tmp/c9jj"; make_corpus "$c9jj"
  printf '# Jobs\n\n[Mail](<mail.md >)\n' > "$c9jj/docs/guide/jobs.md"
  git -C "$c9jj" add -A && git -C "$c9jj" commit -qm bracketed-trailing-space
  check "a trailing space in a bracketed destination is syntax" pass "$c9jj"

  local c9jk="$tmp/c9jk"; make_corpus "$c9jk"
  printf '# Jobs\n\n[Mail](< mail.md>)\n' > "$c9jk/docs/guide/jobs.md"
  git -C "$c9jk" add -A && git -C "$c9jk" commit -qm bracketed-leading-space
  check "a leading space in a bracketed destination is syntax" pass "$c9jk"

  # `template` contents are PARSED, so templates nest and the first close is
  # the wrong end — the path is still inside the outer, inert one.
  local c9jg="$tmp/c9jg"; make_corpus "$c9jg"
  printf '# Jobs\n\n<template><template></template> docs/guide/mail.md </template>\n' \
    > "$c9jg/docs/guide/jobs.md"
  git -C "$c9jg" add -A && git -C "$c9jg" commit -qm nested-template
  check "a nested template does not end at the first close" fail "$c9jg"

  # ...but `script` and `iframe` hold TEXT, so the inner spelling is not an
  # opener, the FIRST close really does end them, and the path after it is on
  # screen. Balancing these too would strand a page nothing else links.
  local c9jh="$tmp/c9jh"; make_corpus "$c9jh"
  printf '# Jobs\n\n<script><script></script> docs/guide/mail.md </script>\n' \
    > "$c9jh/docs/guide/jobs.md"
  git -C "$c9jh" add -A && git -C "$c9jh" commit -qm nested-script-raw-text
  check "a raw-text element ends at its first close" pass "$c9jh"

  local c9ji="$tmp/c9ji"; make_corpus "$c9ji"
  printf '# Jobs\n\n<iframe><iframe></iframe> docs/guide/mail.md </iframe>\n' \
    > "$c9ji/docs/guide/jobs.md"
  git -C "$c9ji" add -A && git -C "$c9ji" commit -qm nested-iframe-raw-text
  check "an iframe ends at its first close too" pass "$c9ji"

  # An anchor cannot contain an anchor: the parser closes the outer one when
  # the inner opens, so the outer has no content at all (Chromium reports an
  # empty innerHTML). The first `</a>` was being read as the outer's close.
  local c9jb="$tmp/c9jb"; make_corpus "$c9jb"
  printf '# Jobs\n\n<a href="mail.md"><a href="https://example.com">Inner</a></a>\n' \
    > "$c9jb/docs/guide/jobs.md"
  git -C "$c9jb" add -A && git -C "$c9jb" commit -qm nested-anchor
  check "a nested anchor is not the outer's label" fail "$c9jb"

  # ...but the bound is whichever comes FIRST. An anchor that closes before
  # the next one opens keeps its own content, and text after it belongs to
  # nobody: taking the next opener unconditionally would read `text` as the
  # empty anchor's label.
  local c9jc="$tmp/c9jc"; make_corpus "$c9jc"
  printf '# Jobs\n\n<a href="mail.md"></a>text <a href="https://example.com">X</a>\n' \
    > "$c9jc/docs/guide/jobs.md"
  git -C "$c9jc" add -A && git -C "$c9jc" commit -qm empty-anchor-then-text
  check "text after an empty anchor is not its label" fail "$c9jc"

  # ...and a commented-out opener is not an opener, which is why the search
  # runs over the masked view rather than the raw source.
  local c9jd="$tmp/c9jd"; make_corpus "$c9jd"
  printf '# Jobs\n\n<a href="mail.md"><!-- <a href="x"> -->Mail</a>\n' \
    > "$c9jd/docs/guide/jobs.md"
  git -C "$c9jd" add -A && git -C "$c9jd" commit -qm commented-anchor-opener
  check "a commented-out opener does not cut a link short" pass "$c9jd"

  # A tag spelled inside raw text is not a tag. `</span>` here is textarea
  # CONTENT, so the hidden span never closes and the anchor renders nothing.
  local c9je="$tmp/c9je"; make_corpus "$c9je"
  printf '# Jobs\n\n<a href="mail.md"><span hidden><textarea></span>Secret</textarea></span></a>\n' \
    > "$c9je/docs/guide/jobs.md"
  git -C "$c9je" add -A && git -C "$c9je" commit -qm rcdata-close-spelling
  check "a close spelled inside raw text is not a close" fail "$c9je"

  # ...while a hidden raw-text element ends at its own first close, so the
  # label after it is still content.
  local c9jf="$tmp/c9jf"; make_corpus "$c9jf"
  printf '# Jobs\n\n<a href="mail.md"><textarea hidden></textarea>Mail</a>\n' \
    > "$c9jf/docs/guide/jobs.md"
  git -C "$c9jf" add -A && git -C "$c9jf" commit -qm hidden-raw-text-element
  check "a hidden raw-text element ends at its close" pass "$c9jf"

  # A reference WITHOUT its semicolon is visible text, in a raw anchor too.
  # The HTML tokenizer would decode `&#32` to a space, but it never sees this:
  # Markdown renders first and escapes the bare `&`, so the browser is handed
  # `&amp;#32` and paints the four characters `&#32` — 36px wide and clickable
  # (markdown-it into Chromium, end to end). Applying HTML decoding rules here
  # would discard a live link and invent an orphan, so CommonMark's stricter
  # requirement is the correct one for this pipeline, not a mismatch to fix.
  local c9ja="$tmp/c9ja"; make_corpus "$c9ja"
  printf '# Jobs\n\n<a href="mail.md">&#32</a>\n' > "$c9ja/docs/guide/jobs.md"
  git -C "$c9ja" add -A && git -C "$c9ja" commit -qm semicolonless-entity
  check "a reference without its semicolon is visible text" pass "$c9ja"

  # The anchor's OWN `hidden` renders no link at all, however solid its label.
  # `has_content` is handed the content slice and never sees the opening tag,
  # so this is checked where the tag is.
  local c9iy="$tmp/c9iy"; make_corpus "$c9iy"
  printf '# Jobs\n\n<a hidden href="mail.md">Mail</a>\n' > "$c9iy/docs/guide/jobs.md"
  git -C "$c9iy" add -A && git -C "$c9iy" commit -qm hidden-anchor
  check "a hidden anchor is not a route" fail "$c9iy"

  # ...but the check must read the ATTRIBUTE, not the word. A page whose own
  # name contains "hidden" is linked by an ordinary anchor, and a substring
  # test on the tag would strand it.
  local c9iz="$tmp/c9iz"; make_corpus "$c9iz"
  printf '# H\n\ntext\n' > "$c9iz/docs/guide/hidden.md"
  printf '# Jobs\n\n[Mail](mail.md)\n\n<a href="hidden.md">Docs</a>\n' \
    > "$c9iz/docs/guide/jobs.md"
  git -C "$c9iz" add -A && git -C "$c9iz" commit -qm href-path-named-hidden
  check "a path named hidden is still a route" pass "$c9iz"

  # A code span binds tighter than the link brackets, so a `]` inside one is
  # code-span content and the label runs past it. Counting that bracket ended
  # the label early; with a SIBLING-relative destination the bare-path fallback
  # has nothing to recover, so the page the reader can click was an orphan.
  local c9jg="$tmp/c9jg"; make_corpus "$c9jg"
  printf '# Jobs\n\n[foo `]` Mail](mail.md)\n' > "$c9jg/docs/guide/jobs.md"
  git -C "$c9jg" add -A && git -C "$c9jg" commit -qm bracket-in-code-span
  check "a bracket inside a code span does not end a label" pass "$c9jg"

  # ...and the harm ran the OTHER way too, which is the direction that let an
  # orphan through. Ending the label early hands the destination scanner the
  # parenthesis INSIDE the span, so `mail.md` was recorded as an edge even
  # though the reader sees it as code. The real destination here is `other.md`,
  # which no guide page is, so the corrected reading yields no edge at all and
  # mail.md is the orphan it always was.
  local c9jh="$tmp/c9jh"; make_corpus "$c9jh"
  printf '# Jobs\n\n[a `](mail.md)` b](other.md)\n' > "$c9jh/docs/guide/jobs.md"
  git -C "$c9jh" add -A && git -C "$c9jh" commit -qm span-swallows-destination
  check "a destination inside a code span is not an edge" fail "$c9jh"

  # The backtick must open a REAL span. `\`` is an escaped literal backtick and
  # opens nothing, so the `]` after it does end the label and this renders as
  # plain text — measured in cmark-gfm and markdown-it-py alike. This is the
  # case that catches the tempting fix below: blanking code spans first eats
  # the escaped backticks AND the `]` between them, inventing a link.
  local c9ji="$tmp/c9ji"; make_corpus "$c9ji"
  printf '# Jobs\n\n[foo \\`]\\` Mail](mail.md)\n' > "$c9ji/docs/guide/jobs.md"
  git -C "$c9ji" add -A && git -C "$c9ji" commit -qm escaped-backticks-in-label
  check "an escaped backtick opens no span" fail "$c9ji"

  # (An UNCLOSED run — `[foo `] Mail](mail.md)` — was written as a case here
  # and removed: the page holds one `]` and one destination, so EVERY reading,
  # right or wrong, reports the orphan. It verified nothing.)

  # A label that is ONLY a code span is visible content, not an empty link.
  # The tempting fix — blank code spans before scanning, as the reference pass
  # does — would strand this page, since the label would read as whitespace.
  local c9jk="$tmp/c9jk"; make_corpus "$c9jk"
  printf '# Jobs\n\n[`Mail`](mail.md)\n' > "$c9jk/docs/guide/jobs.md"
  git -C "$c9jk" add -A && git -C "$c9jk" commit -qm code-span-only-label
  check "a label that is only a code span is content" pass "$c9jk"

  # A code span cannot cross a blank line: the blank ends the paragraph, so
  # these are two literal backticks in two paragraphs, not a span over the
  # reference between them. `.*?` under `re.S` matched anyway and blanked the
  # only route to mail.md.
  local c9jl="$tmp/c9jl"; make_corpus "$c9jl"
  printf '# Jobs\n\nAn opening ` backtick.\n\n[Mail][m]\n\nA closing ` backtick.\n\n[m]: mail.md\n' \
    > "$c9jl/docs/guide/jobs.md"
  git -C "$c9jl" add -A && git -C "$c9jl" commit -qm backticks-across-blank-line
  check "backticks either side of a blank line are not a span" pass "$c9jl"

  # A LINK MAY NOT CONTAIN A LINK: the inner one renders and the outer opener
  # is deactivated, so the reader's route is `mail.md` and not the outer
  # destination. Balancing straight through the label yielded the outer and
  # skipped the inner — the page the reader can click, reported an orphan.
  local c9jm="$tmp/c9jm"; make_corpus "$c9jm"
  printf '# Jobs\n\n[outer [Mail](mail.md)](https://example.test)\n' \
    > "$c9jm/docs/guide/jobs.md"
  git -C "$c9jm" add -A && git -C "$c9jm" commit -qm inner-link-wins
  check "an inner link deactivates the opener above it" pass "$c9jm"

  # ...and the deactivated outer `](…)` is then LITERAL TEXT, so a path in it
  # is on screen and is a route by this gate's own rule. Blanking it as though
  # it were still a link destination stranded the page it names.
  local c9jn="$tmp/c9jn"; make_corpus "$c9jn"
  printf '# O\n\ntext\n' > "$c9jn/docs/guide/other.md"
  printf '# Jobs\n\n[outer [Mail](mail.md)](docs/guide/other.md)\n' \
    > "$c9jn/docs/guide/jobs.md"
  git -C "$c9jn" add -A && git -C "$c9jn" commit -qm deactivated-dest-visible
  check "a deactivated destination is visible text" pass "$c9jn"

  # An UNRESOLVED reference is not a link and deactivates nothing, so the outer
  # link still renders and its destination is still the reader's route. Reading
  # every bracket pair as an inner link would have stranded this page.
  local c9jo="$tmp/c9jo"; make_corpus "$c9jo"
  printf '# Jobs\n\n[outer [nosuch]](mail.md)\n' > "$c9jo/docs/guide/jobs.md"
  git -C "$c9jo" add -A && git -C "$c9jo" commit -qm undefined-ref-inside-label
  check "an undefined reference deactivates nothing" pass "$c9jo"

  # (An inner IMAGE deactivates nothing either — a link may not contain a link,
  # but it may contain an image — and that case was written here and removed.
  # It cannot fail: deactivating the outer opener turns its `](…)` into visible
  # text, and a destination named in visible text is a route by this gate's own
  # rule, so the page is reachable under either reading. Measured with the
  # destination bare and angle-wrapped alike. The asymmetry is real and stays
  # pinned where it is derived, in `inline_links`.)

  # A link inside a CODE SPAN deactivates nothing, since it renders as code.
  local c9jq="$tmp/c9jq"; make_corpus "$c9jq"
  printf '# Jobs\n\n[outer `[x](other.md)`](mail.md)\n' \
    > "$c9jq/docs/guide/jobs.md"
  git -C "$c9jq" add -A && git -C "$c9jq" commit -qm link-sample-inside-label
  check "a link sample in a code span deactivates nothing" pass "$c9jq"

  # An image's alt text is a link label and nests like one. The bounded pattern
  # took one level, so a deeper alt left the image unmasked and its destination
  # — a resource the page loads, never text on screen — reached the bare-path
  # scan as if the reader could read it there.
  local c9jr="$tmp/c9jr"; make_corpus "$c9jr"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n![outer [middle [alt]]](docs/guide/mail.md)\n' \
    > "$c9jr/README.md"
  git -C "$c9jr" add -A && git -C "$c9jr" commit -qm deep-nested-image-alt
  check "a deeply nested image alt still masks its destination" fail "$c9jr"

  # The same bound on the link-destination pattern let a repository path be
  # read out of an EXTERNAL URL's query string: unblanked, `scan` still held
  # `docs/guide/mail.md`, and clicking the link leaves the repository entirely.
  local c9js="$tmp/c9js"; make_corpus "$c9js"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n[outer [middle [label]]](https://example.test/?q=docs/guide/mail.md)\n' \
    > "$c9js/README.md"
  git -C "$c9js" add -A && git -C "$c9js" commit -qm deep-nested-external-dest
  check "a deeply nested label still blanks its destination" fail "$c9js"

  # A BACKSLASH IS A PATH SEPARATOR to the URL parser, so a raw anchor written
  # this way navigates to `docs/guide/mail.md` and is a route. Measured in
  # Chromium under an `https:` and a `file:` base alike.
  local c9jt="$tmp/c9jt"; make_corpus "$c9jt"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<a href="docs\\guide\\mail.md">Mail</a>\n' \
    > "$c9jt/README.md"
  git -C "$c9jt" add -A && git -C "$c9jt" commit -qm raw-href-backslash
  check "a raw href backslash is a path separator" pass "$c9jt"

  # ...but `%5C` is a LITERAL backslash in the path and reaches the tracked
  # file not at all. Converting after percent-decoding folds the two spellings
  # together and invents this route.
  local c9ju="$tmp/c9ju"; make_corpus "$c9ju"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n<a href="docs%%5Cguide%%5Cmail.md">Mail</a>\n' \
    > "$c9ju/README.md"
  git -C "$c9ju" add -A && git -C "$c9ju" commit -qm raw-href-percent-5c
  check "a percent-encoded backslash is not a separator" fail "$c9ju"

  # ...and MARKDOWN is the other way round: cmark-gfm percent-encodes the
  # backslash, emitting `href="docs%5Cguide%5Cmail.md"`, so the same source
  # characters render a dead link. Applying the raw rule to both branches
  # would invent a route out of it.
  local c9jv="$tmp/c9jv"; make_corpus "$c9jv"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n[Mail](docs\\guide\\mail.md)\n' \
    > "$c9jv/README.md"
  git -C "$c9jv" add -A && git -C "$c9jv" commit -qm markdown-dest-backslash
  check "a Markdown destination backslash is not a separator" fail "$c9jv"

  # A reference SPELLED IN CODE is not a link and deactivates nothing. The
  # inner-link rule reads the label with a scanner, which steps over code
  # spans, but the reference scans below it are patterns and could not — so a
  # code sample suppressed the outer link and stranded the page it names.
  local c9jw="$tmp/c9jw"; make_corpus "$c9jw"
  printf '# Jobs\n\n[outer `[fake][m]`](mail.md)\n\n[m]: https://example.test\n' \
    > "$c9jw/docs/guide/jobs.md"
  git -C "$c9jw" add -A && git -C "$c9jw" commit -qm ref-sample-inside-label
  check "a reference spelled in code deactivates nothing" pass "$c9jw"

  # `AGENTS.md` is loaded by name on entering the repository. Nothing links it,
  # which is what makes it an entry surface rather than a waypoint — as a
  # waypoint it is inert, and a guide indexed only from there was an orphan.
  local c9jx="$tmp/c9jx"; make_corpus "$c9jx"
  printf '# Agents\n\n- [Mail](docs/guide/mail.md)\n' > "$c9jx/AGENTS.md"
  git -C "$c9jx" add -A && git -C "$c9jx" commit -qm agents-md-is-a-root
  check "the root AGENTS.md is an entry surface" pass "$c9jx"

  # An anchor wrapping only an `<hr>` is a wide, thin, wholly clickable target
  # — 1264x2 in Chromium, hit-testing inside the anchor — so it is a route.
  # `has_content` strips tags, so a rule with no text read as an empty link.
  local c9jy="$tmp/c9jy"; make_corpus "$c9jy"
  printf '# Jobs\n\n<a href="mail.md"><hr></a>\n' > "$c9jy/docs/guide/jobs.md"
  git -C "$c9jy" add -A && git -C "$c9jy" commit -qm anchor-wrapping-a-rule
  check "an anchor wrapping a rule is a route" pass "$c9jy"

  # ...but `<br>` is NOT. It is full line height and ZERO width, so there is
  # nothing to click; adding the whole "elements that paint" family without
  # measuring each one would have made this empty anchor a route.
  local c9jz="$tmp/c9jz"; make_corpus "$c9jz"
  printf '# Jobs\n\n<a href="mail.md"><br></a>\n' > "$c9jz/docs/guide/jobs.md"
  git -C "$c9jz" add -A && git -C "$c9jz" commit -qm anchor-wrapping-a-break
  check "an anchor wrapping a line break is not a route" fail "$c9jz"

  # `agents/<name>.md` is the agent convention, and it holds under `.claude/`
  # too. Crossing the prefixes with the SKILL.md/AGENT.md basenames instead
  # left an ordinary agent file out and reported its guide as an orphan.
  local c9ka="$tmp/c9ka"; make_corpus "$c9ka"
  mkdir -p "$c9ka/.claude/agents"
  printf '# R\n\n- [Mail](docs/guide/mail.md)\n' > "$c9ka/.claude/agents/reviewer.md"
  git -C "$c9ka" add -A && git -C "$c9ka" commit -qm claude-agents-entry-file
  check "a .claude agent file is an entry surface" pass "$c9ka"

  # ...and the same cross-product seeded a SUPPORTING file as a root, because
  # the basename matched at any depth. A page nothing links any more must not
  # confer reachability — that is an orphan passing.
  local c9kb="$tmp/c9kb"; make_corpus "$c9kb"
  mkdir -p "$c9kb/skills/x/references"
  printf '# S\n\ntext\n' > "$c9kb/skills/x/SKILL.md"
  printf '# N\n\n- [Mail](docs/guide/mail.md)\n' > "$c9kb/skills/x/references/AGENT.md"
  git -C "$c9kb" add -A && git -C "$c9kb" commit -qm supporting-file-is-not-a-root
  check "a supporting file below a skill is not a root" fail "$c9kb"

  # ...but it is still a WAYPOINT: linked from its `SKILL.md`, it carries its
  # edges. Dropping such files from traversal instead of demoting them would
  # strand this guide.
  local c9kc="$tmp/c9kc"; make_corpus "$c9kc"
  mkdir -p "$c9kc/skills/x/references"
  printf '# S\n\n- [Notes](references/AGENT.md)\n' > "$c9kc/skills/x/SKILL.md"
  printf '# N\n\n- [Mail](docs/guide/mail.md)\n' > "$c9kc/skills/x/references/AGENT.md"
  git -C "$c9kc" add -A && git -C "$c9kc" commit -qm supporting-file-is-a-waypoint
  check "a linked supporting file still carries edges" pass "$c9kc"

  # A style attribute is a list of declarations, and `display` counts only when
  # it IS one. Searching the whole attribute read it out of another
  # declaration's VALUE and called a hidden link visible — an orphan passing.
  # All three spellings compute to `display: none` with a 0x0 box in Chromium.
  local c9kd="$tmp/c9kd"; make_corpus "$c9kd"
  printf '# Jobs\n\n<a style='"'"'display:none; --x:"display:block;"'"'"' href="mail.md">Mail</a>\n' \
    > "$c9kd/docs/guide/jobs.md"
  git -C "$c9kd" add -A && git -C "$c9kd" commit -qm decl-inside-custom-property
  check "a declaration inside a custom property is not one" fail "$c9kd"

  # ...and a SEMICOLON inside a string does not end a declaration, so the text
  # after it does not start one. This is the case that needs the split to know
  # about quotes rather than just anchoring the property name.
  local c9ke="$tmp/c9ke"; make_corpus "$c9ke"
  printf '# Jobs\n\n<a style='"'"'display:none; content:"a;display:block"'"'"' href="mail.md">Mail</a>\n' \
    > "$c9ke/docs/guide/jobs.md"
  git -C "$c9ke" add -A && git -C "$c9ke" commit -qm decl-after-string-semicolon
  check "a semicolon inside a string ends no declaration" fail "$c9ke"

  # ...nor does one inside `url()`, which is the parenthesis half of the same
  # rule.
  local c9kf="$tmp/c9kf"; make_corpus "$c9kf"
  printf '# Jobs\n\n<a style='"'"'display:none; background:url(a;display:block)'"'"' href="mail.md">Mail</a>\n' \
    > "$c9kf/docs/guide/jobs.md"
  git -C "$c9kf" add -A && git -C "$c9kf" commit -qm decl-after-url-semicolon
  check "a semicolon inside url() ends no declaration" fail "$c9kf"

  # `CLAUDE.md` is an instruction file loaded by name, exactly as `AGENTS.md`
  # is. Adding one of the pair and not the other is the omit-one-of-a-family
  # mistake, so both are matched by basename.
  local c9kg="$tmp/c9kg"; make_corpus "$c9kg"
  printf '# C\n\n- [Mail](docs/guide/mail.md)\n' > "$c9kg/CLAUDE.md"
  git -C "$c9kg" add -A && git -C "$c9kg" commit -qm claude-md-is-a-root
  check "the root CLAUDE.md is an entry surface" pass "$c9kg"

  # ...at ANY depth, because the convention is per-directory: tooling reads the
  # instruction file beside the code it is working on, so a nested one is
  # loaded exactly as the top-level one is and nothing links either.
  local c9kh="$tmp/c9kh"; make_corpus "$c9kh"
  mkdir -p "$c9kh/sub"
  printf '# C\n\n- [Mail](docs/guide/mail.md)\n' > "$c9kh/sub/CLAUDE.md"
  git -C "$c9kh" add -A && git -C "$c9kh" commit -qm nested-claude-md-is-a-root
  check "a nested instruction file is an entry surface" pass "$c9kh"

  # A reference image's alt text nests like any other label. The bounded
  # pattern took one level, so a deeper alt left the image unmasked and the
  # bare-path scan read a path out of alt text the reader never sees.
  local c9ki="$tmp/c9ki"; make_corpus "$c9ki"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n![outer [middle [docs/guide/mail.md]]][logo]\n\n[logo]: image.png\n' \
    > "$c9ki/README.md"
  git -C "$c9ki" add -A && git -C "$c9ki" commit -qm deep-nested-image-reference
  check "a deeply nested image reference is masked" fail "$c9ki"

  # ...but only when it RESOLVES. An unresolved reference is not an image at
  # all — it renders as literal text, so its label IS on screen, and masking it
  # would invent an orphan out of a path the reader can read.
  local c9kj="$tmp/c9kj"; make_corpus "$c9kj"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n\n![outer [middle [docs/guide/mail.md]]][nosuch]\n' \
    > "$c9kj/README.md"
  git -C "$c9kj" add -A && git -C "$c9kj" commit -qm unresolved-image-reference
  check "an unresolved image reference is visible text" pass "$c9kj"

  # The HTML tokenizer decodes character references in an attribute BEFORE CSS
  # sees it, so this reaches the engine as `display:none` and hides the anchor.
  # Handing the encoded source to the cascade recorded a live route.
  local c9kk="$tmp/c9kk"; make_corpus "$c9kk"
  printf '# Jobs\n\n<a style="display&#58;none" href="mail.md">Mail</a>\n' \
    > "$c9kk/docs/guide/jobs.md"
  git -C "$c9kk" add -A && git -C "$c9kk" commit -qm style-entity-colon
  check "a character reference in a style is decoded" fail "$c9kk"

  # ...by the TOKENIZER's rules, not CommonMark's: a numeric reference without
  # its semicolon is decoded in an attribute, and this anchor is hidden too.
  # The strict decoder used for Markdown destinations would have missed it.
  local c9kl="$tmp/c9kl"; make_corpus "$c9kl"
  printf '# Jobs\n\n<a style="display&#58none" href="mail.md">Mail</a>\n' \
    > "$c9kl/docs/guide/jobs.md"
  git -C "$c9kl" add -A && git -C "$c9kl" commit -qm style-entity-no-semicolon
  check "a semicolonless reference in a style is decoded" fail "$c9kl"

  # ...and decoding comes BEFORE the declaration split, so an encoded quote
  # becomes a real one and the semicolon inside it ends no declaration. Decoded
  # after the split, `display:block"` reads as a declaration and unhides this.
  local c9km="$tmp/c9km"; make_corpus "$c9km"
  printf '# Jobs\n\n<a style="display:none;content:&quot;a;display:block&quot;" href="mail.md">Mail</a>\n' \
    > "$c9km/docs/guide/jobs.md"
  git -C "$c9km" add -A && git -C "$c9km" commit -qm style-entity-quote
  check "an encoded quote is decoded before the split" fail "$c9km"

  # `<input type="hidden">` is 0x0 with nothing to click, so an anchor holding
  # only one is empty. `input` was added to the painted-element list by tag
  # NAME, which called this a route and let an orphan through.
  local c9kn="$tmp/c9kn"; make_corpus "$c9kn"
  printf '# Jobs\n\n<a href="mail.md"><input type="hidden"></a>\n' \
    > "$c9kn/docs/guide/jobs.md"
  git -C "$c9kn" add -A && git -C "$c9kn" commit -qm anchor-wrapping-hidden-input
  check "an anchor wrapping a hidden input is not a route" fail "$c9kn"

  # ...but every OTHER type paints — text 185x17, checkbox 20x17, submit
  # 57x17, and a bare `<input>` 185x17 — so dropping the element outright
  # strands a real link. That is the obvious over-correction to the case above.
  local c9ko="$tmp/c9ko"; make_corpus "$c9ko"
  printf '# Jobs\n\n<a href="mail.md"><input type="text"></a>\n' \
    > "$c9ko/docs/guide/jobs.md"
  git -C "$c9ko" add -A && git -C "$c9ko" commit -qm anchor-wrapping-text-input
  check "an anchor wrapping a text input is a route" pass "$c9ko"

  # The value must be EXACTLY `hidden`: `type=" hidden "` is not the hidden
  # type at all, it falls back to `text` and paints. Stripping the value before
  # comparing calls this empty and invents an orphan.
  local c9kp="$tmp/c9kp"; make_corpus "$c9kp"
  printf '# Jobs\n\n<a href="mail.md"><input type=" hidden "></a>\n' \
    > "$c9kp/docs/guide/jobs.md"
  git -C "$c9kp" add -A && git -C "$c9kp" commit -qm input-type-padded-hidden
  check "a padded type value is not the hidden type" pass "$c9kp"

  # ...matched case-insensitively, name and value, and the value may be
  # unquoted.
  local c9kq="$tmp/c9kq"; make_corpus "$c9kq"
  printf '# Jobs\n\n<a href="mail.md"><input TYPE=HIDDEN></a>\n' \
    > "$c9kq/docs/guide/jobs.md"
  git -C "$c9kq" add -A && git -C "$c9kq" commit -qm input-type-unquoted-upper
  check "an unquoted uppercase hidden type still hides" fail "$c9kq"

  # ...and the FIRST `type` wins when an author repeats it, so this is hidden
  # while the reverse order paints.
  local c9kr="$tmp/c9kr"; make_corpus "$c9kr"
  printf '# Jobs\n\n<a href="mail.md"><input type="hidden" type="text"></a>\n' \
    > "$c9kr/docs/guide/jobs.md"
  git -C "$c9kr" add -A && git -C "$c9kr" commit -qm input-duplicate-type
  check "the first type attribute decides" fail "$c9kr"

  # The type is read from RAW because values are blanked upstream — but the
  # TAGS are found in the masked view, so one inside a comment cannot count.
  # Scanning raw for both made this commented-out input content.
  local c9ks="$tmp/c9ks"; make_corpus "$c9ks"
  printf '# Jobs\n\n<a href="mail.md"><!-- <input type="text"> --></a>\n' \
    > "$c9ks/docs/guide/jobs.md"
  git -C "$c9ks" add -A && git -C "$c9ks" commit -qm commented-out-input
  check "a commented-out input is not content" fail "$c9ks"

  # ...and neither is one inside a hidden subtree.
  local c9kt="$tmp/c9kt"; make_corpus "$c9kt"
  printf '# Jobs\n\n<a href="mail.md"><span hidden><input type="text"></span></a>\n' \
    > "$c9kt/docs/guide/jobs.md"
  git -C "$c9kt" add -A && git -C "$c9kt" commit -qm hidden-subtree-input
  check "an input in a hidden subtree is not content" fail "$c9kt"

  # The tokenizer decodes an attribute value before anything sees it, so this
  # IS the hidden type and paints nothing. Comparing the encoded source called
  # it a text input and made an empty anchor a route.
  local c9ku="$tmp/c9ku"; make_corpus "$c9ku"
  printf '# Jobs\n\n<a href="mail.md"><input type="hidd&#101;n"></a>\n' \
    > "$c9ku/docs/guide/jobs.md"
  git -C "$c9ku" add -A && git -C "$c9ku" commit -qm input-type-entity
  check "a character reference in a type is decoded" fail "$c9ku"

  # ...by the TOKENIZER's rules: a numeric reference without its semicolon is
  # decoded in an attribute, where a Markdown destination would keep it.
  local c9kv="$tmp/c9kv"; make_corpus "$c9kv"
  printf '# Jobs\n\n<a href="mail.md"><input type="hidd&#101n"></a>\n' \
    > "$c9kv/docs/guide/jobs.md"
  git -C "$c9kv" add -A && git -C "$c9kv" commit -qm input-type-entity-no-semi
  check "a semicolonless reference in a type is decoded" fail "$c9kv"

  # ...and decoding can INTRODUCE the whitespace that decides the answer, so
  # the comparison stays exact afterwards: this decodes to ` hidden`, which is
  # not the hidden type and paints. Stripping after decoding strands the page.
  local c9kw="$tmp/c9kw"; make_corpus "$c9kw"
  printf '# Jobs\n\n<a href="mail.md"><input type="&#32;hidden"></a>\n' \
    > "$c9kw/docs/guide/jobs.md"
  git -C "$c9kw" add -A && git -C "$c9kw" commit -qm input-type-entity-space
  check "a decoded leading space is not the hidden type" pass "$c9kw"

  # An attribute value may be UNQUOTED, and this anchor is hidden — 0x0 in
  # Chromium. Accepting only quoted values read it as having no inline style
  # at all, so an empty anchor counted as a route.
  # (The `style=display: none` spelling was written here as a companion and
  # removed: an unquoted value ends at the first space, so it really does set
  # only `display:` and really does paint — but every variant reaches that
  # same answer by a different route, so the case pinned nothing. The measured
  # fact lives on `STYLE_ATTR_OPEN` instead.)
  local c9kx="$tmp/c9kx"; make_corpus "$c9kx"
  printf '# Jobs\n\n<a style=display:none href=mail.md>Mail</a>\n' \
    > "$c9kx/docs/guide/jobs.md"
  git -C "$c9kx" add -A && git -C "$c9kx" commit -qm unquoted-style-attribute
  check "an unquoted inline style still hides" fail "$c9kx"

  # `AGENT.md` is not an entry filename: the skills convention is `SKILL.md`,
  # and seeding a root on a basename no convention documents only lets an
  # ordinary supporting page confer reachability.
  local c9ky="$tmp/c9ky"; make_corpus "$c9ky"
  mkdir -p "$c9ky/skills/x"
  printf '# S\n\ntext\n' > "$c9ky/skills/x/SKILL.md"
  printf '# A\n\n- [Mail](docs/guide/mail.md)\n' > "$c9ky/skills/x/AGENT.md"
  git -C "$c9ky" add -A && git -C "$c9ky" commit -qm agent-md-is-not-a-skill-root
  check "an AGENT.md beside a skill is not a root" fail "$c9ky"

  # `visibility:hidden` keeps the BOX — the anchor is still 30x17 — but paints
  # nothing and hit-tests through to the body behind it, so there is nothing
  # to click. Only `display:none` was recognised, and this let an orphan pass.
  local c9kz="$tmp/c9kz"; make_corpus "$c9kz"
  printf '# Jobs\n\n<a style="visibility:hidden" href="mail.md">Mail</a>\n' \
    > "$c9kz/docs/guide/jobs.md"
  git -C "$c9kz" add -A && git -C "$c9kz" commit -qm visibility-hidden-anchor
  check "a visibility:hidden anchor is not a route" fail "$c9kz"

  # ...and `collapse` hides the same way outside a table.
  local c9la="$tmp/c9la"; make_corpus "$c9la"
  printf '# Jobs\n\n<a style="visibility:collapse" href="mail.md">Mail</a>\n' \
    > "$c9la/docs/guide/jobs.md"
  git -C "$c9la" add -A && git -C "$c9la" commit -qm visibility-collapse-anchor
  check "a visibility:collapse anchor is not a route" fail "$c9la"

  # ...but UNLIKE `display:none` this one has an escape, which is why it is not
  # simply another hidden tag: `visibility` inherits, and a descendant can set
  # it back. This span paints and hit-tests INSIDE the anchor, so it is a
  # route, and rejecting the anchor on its own style alone strands the page.
  local c9lb="$tmp/c9lb"; make_corpus "$c9lb"
  printf '# Jobs\n\n<a style="visibility:hidden" href="mail.md"><span style="visibility:visible">Mail</span></a>\n' \
    > "$c9lb/docs/guide/jobs.md"
  git -C "$c9lb" add -A && git -C "$c9lb" commit -qm visibility-descendant-override
  check "a visible descendant restores the route" pass "$c9lb"

  # A root may sit INSIDE the guide tree, and is then both an entry surface and
  # a node. Processing a frontier item never adds that item itself, so such a
  # root reported ITSELF as unreachable — the gate contradicting its own root
  # list. Both pages here are reachable: one as a root, one through it.
  local c9lc="$tmp/c9lc"; make_corpus "$c9lc"
  mkdir -p "$c9lc/docs/guide/topic"
  printf '# T\n\ntext\n' > "$c9lc/docs/guide/topic/other.md"
  printf '# A\n\n- [Mail](../mail.md)\n' > "$c9lc/docs/guide/topic/AGENTS.md"
  printf '# App\n\n- [Jobs](docs/guide/jobs.md)\n- [Other](docs/guide/topic/other.md)\n' \
    > "$c9lc/README.md"
  git -C "$c9lc" add -A && git -C "$c9lc" commit -qm root-inside-the-guide-tree
  check "a root inside the guide tree is reachable" pass "$c9lc"

  # `visibility` INHERITS, so a descendant spelling `inherit` inherits the
  # anchor's `hidden` and paints nothing. `unset`, `revert` and `revert-layer`
  # all land the same way; testing that a value is merely NOT `hidden`
  # restored a route the reader does not have.
  local c9ld="$tmp/c9ld"; make_corpus "$c9ld"
  printf '# Jobs\n\n<a style="visibility:hidden" href="mail.md"><span style="visibility:inherit">Mail</span></a>\n' \
    > "$c9ld/docs/guide/jobs.md"
  git -C "$c9ld" add -A && git -C "$c9ld" commit -qm visibility-inherit-descendant
  check "an inheriting descendant stays hidden" fail "$c9ld"

  # ...and so does an INVALID one: the declaration is dropped, leaving the
  # inherited `hidden`. Only values that compute to visible may restore it,
  # which is why the test is an allowlist.
  local c9le="$tmp/c9le"; make_corpus "$c9le"
  printf '# Jobs\n\n<a style="visibility:hidden" href="mail.md"><span style="visibility:bogus">Mail</span></a>\n' \
    > "$c9le/docs/guide/jobs.md"
  git -C "$c9le" add -A && git -C "$c9le" commit -qm visibility-invalid-descendant
  check "an invalid visibility stays hidden" fail "$c9le"

  # ...but `initial` DOES restore it, because `visibility`'s initial value is
  # `visible`. That is the one place the CSS-wide keywords part company, and an
  # allowlist of `visible` alone would strand this page.
  local c9lf="$tmp/c9lf"; make_corpus "$c9lf"
  printf '# Jobs\n\n<a style="visibility:hidden" href="mail.md"><span style="visibility:initial">Mail</span></a>\n' \
    > "$c9lf/docs/guide/jobs.md"
  git -C "$c9lf" add -A && git -C "$c9lf" commit -qm visibility-initial-descendant
  check "an initial visibility restores the route" pass "$c9lf"

  # The mirror of the hidden ANCHOR: a visible anchor whose only content is
  # hidden. The anchor keeps a 30x17 box and is even hit-testable, but there
  # is no ink in it, and visible-vs-invisible is the rule this gate runs on.
  local c9lg="$tmp/c9lg"; make_corpus "$c9lg"
  printf '# Jobs\n\n<a href="mail.md"><span style="visibility:hidden">Mail</span></a>\n' \
    > "$c9lg/docs/guide/jobs.md"
  git -C "$c9lg" add -A && git -C "$c9lg" commit -qm hidden-descendant-only-content
  check "an anchor whose only content is hidden is not a route" fail "$c9lg"

  # ...but the mask stops at that element. Text beside it is still on screen.
  local c9lh="$tmp/c9lh"; make_corpus "$c9lh"
  printf '# Jobs\n\n<a href="mail.md"><span style="visibility:hidden">H</span>Vis</a>\n' \
    > "$c9lh/docs/guide/jobs.md"
  git -C "$c9lh" add -A && git -C "$c9lh" commit -qm hidden-descendant-plus-text
  check "visible text beside a hidden span is a label" pass "$c9lh"

  # ...and a nested `visibility:visible` re-shows its subtree, which is the one
  # thing that makes this different from `display:none` and why it cannot join
  # the blanket subtree masking.
  local c9li="$tmp/c9li"; make_corpus "$c9li"
  printf '# Jobs\n\n<a href="mail.md"><span style="visibility:hidden"><span style="visibility:visible">Mail</span></span></a>\n' \
    > "$c9li/docs/guide/jobs.md"
  git -C "$c9li" add -A && git -C "$c9li" commit -qm hidden-subtree-with-visible-inner
  check "a visible subtree inside a hidden one still shows" pass "$c9li"

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
