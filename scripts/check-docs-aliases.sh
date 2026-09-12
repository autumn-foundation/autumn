#!/usr/bin/env bash
# Reader-vocabulary gate: the page that documents a capability must carry the
# word a reader searches for it by.
#
# WHY THIS EXISTS: the corpus has eight docs gates and they all answer the same
# shape of question — is what this page says TRUE?
# `scripts/check-docs-links.sh` gates its *links* (a 404),
# `scripts/check-docs-cli.sh` its *commands* (`unrecognized subcommand`),
# `scripts/check-docs-config.sh` the `AUTUMN_*` variables they SET (a silent
# no-op), `scripts/check-docs-toml.sh` the `autumn.toml` keys they WRITE
# (dropped silently), `scripts/check-docs-symbols.sh` the `autumn_web::…` paths
# they IMPORT (E0432 against their own file), `scripts/check-docs-routes.sh` the
# `/actuator/…` URLs they REQUEST (a 404 that reads like a feature they failed
# to enable), `scripts/check-docs-macro-args.sh` the macro keywords they TYPE,
# and `scripts/check-docs-scope.sh` that those gates agree on which pages are
# reader-facing.
#
# Every one of them presupposes a reader who ALREADY REACHED the page.
# `check-docs-orphans.sh` is the closest thing to an exception, and it asks
# whether a page is reachable by CLICKING — whether a link path exists from an
# entry surface. That is not the same question as whether a reader FINDS it.
# Nobody clicks their way through a 159-page guide with no index; they type the
# word they already have into a search box.
#
# So the corpus has eight gates for accuracy and none for findability, and a
# findability defect is silent in a way even a wrong sentence is not. A wrong
# sentence is at least READ. A page the reader never lands on produces no 404,
# no exit code, no ignored override, and no support ticket — the reader
# concludes the framework does not have the feature and goes and builds it
# themselves, or picks a framework whose docs answered them. The page can be
# perfectly accurate, freshly verified, owned, and linked from four places, and
# still fail every reader who calls the thing by its common name instead of its
# protocol name.
#
# THE BASELINE DEFECT, and why it is the shape of the whole class:
# `autumn generate auth --totp` ships two-factor authentication — enrollment,
# login-verify, encrypted-at-rest secrets, single-use recovery codes. It is
# documented in `docs/guide/authentication.md`, accurately and in detail. That
# page called it "TOTP" and "Multi-factor" and never once called it "2FA" or
# "two-factor authentication", which is what essentially everyone types.
#
# A corpus-wide grep for the reader's words returned four hits and not one of
# them was an answer:
#
#   README.md                            `…license-MIT%2FApache…`  <- %2F
#   docs/guide/step-up-authentication.md `…return_to=%2Faccount…`  <- %2F
#   docs/guide/rate-limiting.md          `POST /login/2fa`         <- an example
#                                        URL in a page about rate limiting
#   skills/generate/SKILL.md             "add TOTP two-factor auth" <- an agent
#                                        skill file, not a reader-facing page
#
# Two of the four were URL-ENCODED SLASHES. That detail is why this gate strips
# `%XX` escapes before it searches, and it is the reason the defect survived: a
# naive grep reports the term as "present in the corpus" and moves on. The
# term was present as punctuation.
#
# WHAT THIS GATE CHECKS, and what it deliberately does not:
#
#   For each entry in READER_VOCABULARY below, the NAMED PAGE must contain the
#   reader's word in its own text. Presence anywhere else in the corpus does
#   not satisfy the entry — that is the point. `POST /login/2fa` on the
#   rate-limiting page is a hit for the corpus and not an answer for the
#   reader, and an entry that could be satisfied from another page would have
#   passed on the exact state this gate was written to catch.
#
#   The table is DECLARED, not discovered, for the same reason
#   `check-docs-scope.sh` declares its differences rather than deriving them:
#   there is no mechanical way to know that "2FA" is the reader's word for
#   `--totp` and "tsvector" is not a word any reader types. A human decides
#   that once, writes it down, and the gate holds it. So this gate cannot find
#   a vocabulary gap nobody has noticed yet — it locks in the ones that have
#   been noticed, and gives the next one a place to be recorded. Adding a row
#   when you name a capability is the cheap half; the expensive half is the
#   reader who never arrives to tell you they didn't find it.
#
# Run locally with:
#   ./scripts/check-docs-aliases.sh
#   ./scripts/check-docs-aliases.sh --self-test

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# Kept in Python for the same reason as its sibling gates: `%XX` unescaping and
# case-insensitive alternation over a declared table are both work that bash
# renders unreadable, and python3 is already a dependency of every other docs
# gate in this directory.
cd "$root"
exec python3 - "$@" <<'PYEOF'
import os
import re
import sys

MODE = sys.argv[1] if len(sys.argv) > 1 else ''

# (capability, page that documents it, TERMS — every one of which must appear)
#
# The terms are the READER's vocabulary, not the codebase's. A row earns its
# place when the two differ: `--totp` / "2FA" differ, and `rate-limiting.md` /
# "rate limit" do not. The rows where they agree are kept anyway as live
# tripwires — they cost one regex each and they fail loudly if a page is ever
# retitled into internal vocabulary.
#
# EVERY TERM IN A ROW IS REQUIRED, and that is the whole point rather than a
# detail. These started as one alternation per row, which meant any single
# alternative satisfied the entire row and the other terms were decorative: a
# row reading "2FA or two-factor" passed on "2FA" alone, so a reader typing
# "two-factor" was unprotected by a row that named their word. Readers do not
# search an alternation; they search one word, and the row has to hold for
# each word it claims.
#
# Consequently a LABEL MUST NAME EXACTLY WHAT THE TERMS ENFORCE. The earlier
# table failed this too, and more quietly: rows labelled "sign-up /
# registration" and "OAuth / social login" enforced only `\bsign[- ]?up\b` and
# `\boauth\b`. The label advertised a guarantee the pattern never made, which
# is the same defect as a docs page promising a feature it does not have. A
# term belongs in a row only once the page actually carries it.
#
# A single term MAY still spell one word several ways — `\bpassword reset\b|
# \breset password\b` is one reader term in two word orders, not two terms.
# The test is whether a reader would type one OR the other for the same idea;
# "2FA" and "two-factor" are the same idea and both get typed, so they are two
# required terms, not one alternation.
READER_VOCABULARY = (
    ('two-factor authentication (2FA)', 'docs/guide/authentication.md',
     (r'\b2fa\b', r'\btwo[- ]factor\b')),
    ('sign-up', 'docs/guide/authentication.md',
     (r'\bsign[- ]?up\b',)),
    ('password reset', 'docs/guide/authentication.md',
     (r'\bforgot[- ]password\b', r'\bpassword reset\b|\breset password\b')),
    ('OAuth sign-in', 'docs/guide/oauth.md', (r'\boauth\b',)),
    ('CSRF protection', 'docs/guide/middleware.md', (r'\bcsrf\b',)),
    ('CORS', 'docs/guide/middleware.md', (r'\bcors\b',)),
    ('rate limiting', 'docs/guide/rate-limiting.md', (r'\brate[- ]limit',)),
    ('background jobs', 'docs/guide/jobs.md', (r'\bbackground job',)),
    ('cron', 'docs/guide/jobs.md', (r'\bcron\b',)),
    ('websockets', 'docs/guide/websockets.md', (r'\bwebsocket',)),
    ('full-text search', 'docs/guide/full-text-search.md',
     (r'\bfull[- ]text search\b',)),
    ('file upload', 'docs/guide/forms.md', (r'\bfile upload\b',)),
    ('S3 storage', 'docs/guide/storage.md', (r'\bs3\b',)),
    ('database migrations', 'docs/guide/migrations.md', (r'\bmigration',)),
    ('testing', 'docs/guide/testing.md', (r'\btest',)),
    ('deployment', 'docs/guide/deployment.md', (r'\bdeploy',)),
)

PERCENT_ESCAPE = re.compile(r'%[0-9A-Fa-f]{2}')

# ONE SCANNER decides what each region is, in document order, because running
# these rules in sequence over the whole file makes the ANSWER DEPEND ON THE
# ORDER — and neither order is right:
#
#   strip comments first  -> deletes a comment a fence was DISPLAYING as code
#   segment code first    -> a backtick inside a comment splits that comment,
#                            and its text is carried through as "code"
#
# The second is not hypothetical; it was a live regression here. The real
# question is never "comments or code first", it is WHICH CONSTRUCT STARTS
# FIRST at this position, which is what a scanner answers and a pipeline of
# substitutions cannot.
NEXT_CONSTRUCT = re.compile(
    r'(?P<comment><!--)'
    r'|(?P<raw><(?P<rawname>script|style)\b)'
    r'|(?P<fence>^[ \t]{0,3}(?P<frun>`{3,}|~{3,})[^\n]*$)'
    r'|(?P<crun>`+)',
    re.M | re.I,
)

# Markup that carries text a reader never sees.
REF_DEFINITION = re.compile(
    r'^[ \t]*\[[^\]]+\]:[ \t]*\S+[^\n]*'                 # the definition
    r'(?:\n[ \t]+(?:"[^"\n]*"|\'[^\'\n]*\'|\([^)\n]*\))[ \t]*)?',  # its title
    re.M,
)
# An AUTOLINK is the one bracketed construct whose "destination" IS its label:
# `<https://example.com/x>` renders as the URL itself, which a reader sees and
# ctrl-F finds. So the brackets go and the URL STAYS. Stripping it outright —
# as this did, by filing autolinks with link destinations — could reject a page
# whose term was genuinely visible. The corpus carries 29 of them.
AUTOLINK = re.compile(r'<([a-zA-Z][a-zA-Z0-9+.-]*://[^>\s]*)>')

# A tag ends at the first `>` that is NOT inside a quoted attribute value, so
# `<span id=">x">` is one tag rather than a tag plus the stray text `x">`.
# `scripts/check-docs-orphans.sh` parses attributes the same way.
_ATTR_SOUP = r'(?:[^>"\']|"[^"]*"|\'[^\']*\')*'
HTML_TAG = re.compile(r'</?[A-Za-z][A-Za-z0-9:-]*' + _ATTR_SOUP + r'>')


# A destination can contain balanced parentheses, so it is scanned rather than
# matched. The corpus has one: `](javascript:alert(1))` in rich-text.md.
DEST_SCAN_LIMIT = 500


def _strip_link_destinations(text):
    """Remove `](…)` destinations, honouring nested parentheses.

    A regex cannot do this. `\]\([^)]*\)` stops at the FIRST `)`, so
    `[flow](./guide_(v1)/two-factor)` leaves `/two-factor)` behind and the term
    counts even though the reader sees only "flow" — the gate would stay green
    after the visible term was deleted from the page.

    Unbalanced input is deliberately left ALONE. A stray `](` in prose must not
    swallow the rest of the file, and malformed markdown renders literally, so
    its text really is on the page and really should count. The scan is bounded
    for the same reason: a destination is not a paragraph.
    """
    out = []
    i = 0
    n = len(text)
    while True:
        j = text.find('](', i)
        if j < 0:
            out.append(text[i:])
            return ''.join(out)
        out.append(text[i:j + 1])           # keep the link TEXT and its `]`
        k = j + 2
        depth = 1
        limit = min(n, k + DEST_SCAN_LIMIT)
        while k < limit and depth:
            c = text[k]
            if c == '\\':                   # an escaped paren is not a paren
                k += 2
                continue
            if c == '(':
                depth += 1
            elif c == ')':
                depth -= 1
            k += 1
        if depth:                           # unbalanced -> renders literally
            out.append('(')
            i = j + 2
        else:
            out.append(' ')
            i = k


def _strip_markup(chunk):
    chunk = REF_DEFINITION.sub(' ', chunk)
    chunk = AUTOLINK.sub(r' \1 ', chunk)    # keep the URL: it is the label
    chunk = _strip_link_destinations(chunk)  # before tags: `](…)` may hold `<…>`
    return HTML_TAG.sub(' ', chunk)


def _segments(text):
    """Walk the document once, yielding ('code'|'prose'|'drop', chunk).

    At each position the construct that STARTS FIRST wins, which is the only
    ordering that is not arbitrary. A comment opening before a fence swallows
    the fence's backticks; a fence opening before a comment displays the
    comment as code. Both are what a renderer does.
    """
    out = []
    i, n = 0, len(text)
    while i < n:
        m = NEXT_CONSTRUCT.search(text, i)
        if not m:
            out.append(('prose', text[i:]))
            break
        if m.start() > i:
            out.append(('prose', text[i:m.start()]))

        if m.group('comment'):
            end = text.find('-->', m.end())
            end = n if end < 0 else end + 3
            out.append(('drop', text[m.start():end]))
            i = end
            continue

        if m.group('raw'):
            # A `<script>`/`<style>` BODY is raw text: its backticks are not
            # code spans and its `<!--` is not a comment, so it has to be
            # consumed here rather than stripped afterwards. Stripping it after
            # segmentation let a backticked body split into "code" chunks and
            # survive — the same ordering mistake as the comment case.
            close = re.compile(r'</%s\b' % m.group('rawname'), re.I)
            cm = close.search(text, m.end())
            end = text.find('>', cm.end()) if cm else -1
            end = n if end < 0 else end + 1
            out.append(('drop', text[m.start():end]))
            i = end
            continue

        if m.group('fence'):
            run = m.group('frun')
            # A fence closes on a run of the SAME character at least as long as
            # the opener — so a ``` inside a ```` block is content, not a close.
            # `scripts/check-docs-cli.sh` applies the same rule for the same
            # reason: closing early makes the rest of the block read as prose.
            close = re.compile(
                r'^[ \t]{0,3}%s{%d,}[ \t]*$' % (re.escape(run[0]), len(run)),
                re.M,
            )
            cm = close.search(text, m.end())
            end = cm.end() if cm else n
            out.append(('code', text[m.start():end]))
            i = end
            continue

        run = m.group('crun')                       # inline code span
        cm = re.compile(r'(?<!`)%s(?!`)' % re.escape(run)).search(text, m.end())
        if cm is None or '\n\n' in text[m.end():cm.start()]:
            out.append(('prose', text[m.start():m.end()]))  # never closed
            i = m.end()
        else:
            out.append(('code', text[m.start():cm.end()]))
            i = cm.end()
    return out


def prose(text):
    """Reduce a page to what a reader actually sees, then search that.

    THE RULE, stated once rather than as a list of special cases: a term counts
    only if it survives into rendered text. Everything removed here is the same
    defect — a byte sequence that satisfies a grep without ever reaching the
    reader — and the gate exists because the baseline defect was exactly that.
    A gate that counts invisible hits reproduces the bug it was written to
    catch, so the rule is applied generally instead of one construct at a time.

    Two categories:

      - **Markup that is not text.** HTML comments (this repo waives gates in
        them — `route-surface-allow` and friends), reference definitions,
        autolinks, link destinations, and HTML tags *including their
        attributes*. `<span id="2fa">TOTP</span>` shows the reader "TOTP"; the
        `id` is not on the page. Inner text and link text are kept, because
        those are what renders — this drops the tag, never what it wraps.

      - **URL escapes.** `%2F` is a slash. Left as-is it reads as a literal
        "2F" and makes `\b2fa\b` match `…%2Faccount…` and `…MIT%2FApache…` —
        two of the four corpus-wide "2FA" hits were punctuation. Substituting a
        slash removes the false hit and still separates the surrounding words,
        so a real term beside an escape keeps matching. This one applies
        everywhere, including code: a `%2F` in a fence is a URL escape there
        too, and matching "2FA" inside it is the same false positive.

    Code regions are carried through UNTOUCHED, because code renders: a term in
    a fence or a `span` is on the page and ctrl-F finds it. That is why the
    markup rules run per-segment rather than over the whole file — stripping
    tags inside a fence would delete text the reader can see.

    `_segments` decides which regions those are in a single ordered pass, so
    that "comments first" versus "code first" never has to be answered: the
    construct that opens first wins, as it does in a renderer.
    """
    out = []
    for kind, chunk in _segments(text):
        if kind == 'code':
            out.append(chunk)
        elif kind == 'prose':
            out.append(_strip_markup(chunk))
        else:                                  # 'drop' — never rendered
            out.append(' ')
    return PERCENT_ESCAPE.sub('/', ''.join(out))


def check(rows, read=None, exists=None):
    """Return (defects, checked) for the given table.

    `read` and `exists` are injected by the self-test so it can run the real
    logic over synthetic pages without writing any files. Both are injected,
    not just `read`: with only `read` overridden every synthetic row fails the
    existence check first and returns a defect for the wrong reason, which
    makes the assertions pass while testing nothing.
    """
    if read is None:
        def read(path):
            with open(path, encoding='utf-8', errors='replace') as fh:
                return fh.read()
    if exists is None:
        exists = os.path.exists

    defects = []
    checked = 0
    for label, page, terms in rows:
        if not exists(page):
            defects.append((label, page, terms[0], 'page does not exist'))
            continue
        checked += 1
        visible = prose(read(page))
        # EVERY term is required. A row is only as strong as its weakest term,
        # because a reader searches one word rather than an alternation.
        for term in terms:
            if not re.findall(term, visible, re.I):
                defects.append((label, page, term, 'reader word absent'))
    return defects, checked


def self_test():
    """Assert the gate fails on the states it exists to catch.

    Three properties, each of which was true of the corpus at some point:

      1. A page that documents a capability without ever using the reader's
         word FAILS. This is the baseline defect verbatim.
      2. A `%XX` escape does NOT satisfy a row. `…path=%2FA then…` puts the
         letters 2, F, A between two non-word characters, so `\b2fa\b` matches
         it even with both boundaries anchored. That is a URL-encoded slash
         followed by a capital A, not the word a reader typed, and without
         `prose()` the gate passes on a corpus where the term appears nowhere.
      3. A page that does carry the word passes.
      4. A missing page is reported as a defect rather than crashing.

    Rows 1-3 inject both `read` and `exists`: a synthetic path does not exist
    on disk, so without an injected `exists` every one of them would return a
    "page does not exist" defect and the assertions would pass while proving
    nothing about the matching logic.
    """
    failures = []

    row = (('2FA', 'p.md', (r'\b2fa\b|\btwo[- ]factor\b',)),)
    here = lambda p: True

    def one(text):
        return check(row, read=lambda p: text, exists=here)

    # 1. absent -> exactly one defect, and it is the word defect
    d, _ = one('TOTP enrollment and multi-factor login.')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('a page missing the reader word should be a defect')

    # 2. a %XX escape must not count as a hit
    d, _ = one('see path=%2FA then stop')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('a %2F escape must not satisfy a reader-word row')

    # ... and the same bytes WITHOUT unescaping do match, which is the whole
    # reason prose() exists. Assert that too, so a future edit that drops
    # prose() fails here rather than silently passing the corpus.
    if not re.search(r'\b2fa\b', 'see path=%2FA then stop', re.I):
        failures.append('precondition: raw %2F text should match \\b2fa\\b')

    # 3. present -> no defect
    d, _ = one('Two-factor authentication (2FA) via TOTP.')
    if d:
        failures.append('a page carrying the reader word should pass')

    # 4. a missing page is a defect, not a crash
    d, _ = check((('x', 'does/not/exist.md', (r'x',)),))
    if len(d) != 1 or d[0][3] != 'page does not exist':
        failures.append('a missing page should be reported as a defect')

    # 4b. EVERY term in a row is required. This is the bug the table shape
    # replaced: as one alternation, "2FA or two-factor" passed on "2FA" alone,
    # leaving a reader who types "two-factor" unprotected by a row that named
    # their word. Two required terms, only one present -> a defect naming the
    # missing one.
    two = (('2FA', 'p.md', (r'\b2fa\b', r'\btwo[- ]factor\b')),)
    d, _ = check(two, read=lambda p: 'Enable 2FA with TOTP.', exists=here)
    if len(d) != 1 or d[0][2] != r'\btwo[- ]factor\b':
        failures.append('every declared term must be required, not just one')
    # both present -> clean
    d, _ = check(two, read=lambda p: 'Two-factor auth (2FA) via TOTP.',
                 exists=here)
    if d:
        failures.append('a row whose terms are all present should pass')

    # 5-8. a term that never renders does not satisfy a row. Each of these
    # passes a plain grep over the source and shows the reader nothing.
    invisible = (
        ('an HTML comment', '<!-- drift-allow: 2FA is covered -->\nTOTP setup.'),
        ('a multi-line HTML comment', '<!--\n2FA\n-->\nTOTP setup.'),
        ('a link destination', 'See [the flow](./two-factor-setup.md) for TOTP.'),
        ('a reference definition', 'See [flow][f].\n\n[f]: ./2fa-guide.md\n'),
    )
    for what, text in invisible:
        d, _ = one(text)
        if len(d) != 1 or d[0][3] != 'reader word absent':
            failures.append(f'{what} must not satisfy a reader-word row')

    # ... and each of them DOES match without prose(), which is why the
    # stripping exists. Assert the preconditions so a future edit that drops
    # one of these rules fails here rather than passing the corpus silently.
    for what, text in invisible:
        if not re.search(r'\b2fa\b|\btwo[- ]factor\b', text, re.I):
            failures.append(f'precondition: raw {what} should match the row')

    # 9-12. what the markup WRAPS is visible even when the markup is not, so
    # the stripping cannot degrade into "delete anything near a bracket or an
    # angle bracket". Each of these renders and ctrl-F finds it.
    visible = (
        ('link text', 'See [two-factor setup](./totp.md).'),
        ('HTML inner text', '<span class="x">two-factor</span> setup.'),
        ('a fenced code block', 'Setup:\n\n```sh\n# enable two-factor\n```\n'),
        ('an inline code span', 'Run `--totp` for `two-factor` login.'),
    )
    for what, text in visible:
        d, _ = one(text)
        if d:
            failures.append(f'{what} is reader-visible and should satisfy a row')

    # 13. an HTML ATTRIBUTE is not rendered text. `<span id="2fa">TOTP</span>`
    # shows the reader "TOTP" only. This is the third finding in this class,
    # which is why the rule above is general rather than another special case.
    d, _ = one('<span id="2fa">TOTP</span> enrollment.')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('an HTML attribute must not satisfy a reader-word row')
    if not re.search(r'\b2fa\b', '<span id="2fa">TOTP</span>', re.I):
        failures.append('precondition: raw HTML attribute should match the row')

    # 14. a destination with BALANCED parentheses is still a destination. The
    # regex this replaced stopped at the first `)` and leaked the tail, so the
    # gate stayed green with the visible term gone from the page.
    nested = 'See [flow](./guide_(v1)/two-factor) and TOTP.'
    d, _ = one(nested)
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('a balanced-paren destination must not satisfy a row')
    if not re.search(r'two[- ]factor', re.sub(r'\]\([^)]*\)', ' ', nested), re.I):
        failures.append('precondition: the old regex should have leaked the tail')

    # 15. an escaped paren does not close a destination.
    d, _ = one(r'See [flow](./a\)two-factor) and TOTP.')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('an escaped paren must not close a destination')

    # 16. UNBALANCED `](` is malformed markdown: it renders literally, so its
    # text is on the page and must still count — and must not swallow the file.
    d, _ = one('Stray ](./two-factor and more prose.')
    if d:
        failures.append('unbalanced `](` renders literally and should count')

    # 17. A COMMENT CONTAINING BACKTICKS is still a comment. Segmenting code
    # before removing comments let the inline span split the comment and carry
    # its text through as "code" — a regression introduced by that ordering,
    # and the reason segmentation is now one ordered pass.
    d, _ = one('<!-- `2FA` and `two-factor` wording -->\nTOTP enrollment.')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('a comment containing backticks must not satisfy a row')

    # 18. A FENCE CLOSES ON A LONGER RUN. Opening with ``` and closing with
    # ```` is valid markdown; failing to see the close made the block read as
    # prose, which stripped a destination the reader can see and could REJECT
    # a valid page. Same rule as scripts/check-docs-cli.sh.
    d, _ = one('Text.\n\n```\n[flow](./two-factor)\n````\n')
    if d:
        failures.append('a fence must close on a run at least as long')

    # ... and a shorter run inside a longer fence is content, not a close.
    d, _ = one('````\n```\ntwo-factor\n````\n')
    if d:
        failures.append('a shorter run inside a longer fence is content')

    # 19. A REFERENCE DEFINITION'S TITLE may sit on the next line, and is just
    # as unrendered as the destination.
    d, _ = one('See [f].\n\n[f]: /totp\n    "two-factor setup"\n')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('a ref-definition continuation title must not count')

    # 20. A `>` INSIDE A QUOTED ATTRIBUTE does not end the tag. Ending at the
    # first `>` left `two-factor">TOTP` searchable while the browser renders
    # only "TOTP".
    d, _ = one('<span id=">two-factor">TOTP</span> enrollment.')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('a quoted > must not end an HTML tag')

    # 21-24. `<script>` and `<style>` BODIES never render. Their visibility
    # differs from ordinary inner text, which does render and must still count
    # (property 10) — so this drops the body, not merely the tags.
    #
    # The BACKTICKED forms are the ones that matter: a raw-text body is raw, so
    # its backticks are not code spans. Stripping these after segmentation let
    # the body split into "code" chunks and survive — the same ordering mistake
    # as the comment case, which is why raw text is consumed by the scanner.
    for tag in ('style', 'script'):
        d, _ = one(f'<{tag}>.two-factor {{ color: red }}</{tag}>\nTOTP setup.')
        if len(d) != 1 or d[0][3] != 'reader word absent':
            failures.append(f'a <{tag}> body must not satisfy a reader-word row')
        d, _ = one(f'<{tag}>\nx = `two-factor`;\n</{tag}>\nTOTP setup.')
        if len(d) != 1 or d[0][3] != 'reader word absent':
            failures.append(f'a backticked <{tag}> body must not satisfy a row')

    # 25. ... but a raw-text element shown INSIDE A FENCE is visible code: the
    # fence opens first, so it wins. Same precedence rule as everything else.
    d, _ = one('Example:\n\n```html\n<style>.two-factor{}</style>\n```\n')
    if d:
        failures.append('a <style> inside a fence is visible code and counts')

    # 26. AN AUTOLINK'S URL IS ITS LABEL. `<https://…/two-factor>` renders as
    # the URL, which a reader sees and ctrl-F finds, so it must still count —
    # unlike a `[text](dest)` destination, which never reaches the page. Filing
    # the two together made this the false-FAILURE direction: rejecting a page
    # whose term was genuinely visible.
    d, _ = one('See <https://example.com/two-factor> for setup.')
    if d:
        failures.append("an autolink's URL is visible and should satisfy a row")

    # ... while an explicit destination is still not visible.
    d, _ = one('See [the guide](https://example.com/two-factor) for setup.')
    if len(d) != 1 or d[0][3] != 'reader word absent':
        failures.append('an explicit link destination must not satisfy a row')

    if failures:
        print('SELF-TEST FAILED:', file=sys.stderr)
        for f in failures:
            print(f'  - {f}', file=sys.stderr)
        return 1
    print('Self-test OK (31 properties).')
    return 0


if MODE == '--self-test':
    sys.exit(self_test())

print('Checking that capability pages carry the words readers search by...')
defects, checked = check(READER_VOCABULARY)
terms = sum(len(r[2]) for r in READER_VOCABULARY)
print(f'vocabulary rows: {len(READER_VOCABULARY)} ({terms} required terms, '
      f'{checked} pages read)')
print(f'defects: {len(defects)}')

if defects:
    print()
    print('A reader searching for this capability by its common name does not')
    print('land on the page that answers them:')
    for label, page, pattern, why in defects:
        print(f'  {page}: {why} -- {label}')
        print(f'    expected to match: {pattern}')
    print()
    print('Fix by putting the reader\'s word on that page (a retitle, a sentence')
    print('in the intro, or the capability named in its own table row) -- NOT by')
    print('writing a new page: that splits the search rank and the two copies')
    print('drift apart. If the capability genuinely moved, update the row.')
    sys.exit(1)

print('Reader-vocabulary gate OK.')
PYEOF
