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

# (capability, page that documents it, regex of the words a reader searches by)
#
# The regex is the READER's vocabulary, not the codebase's. A row earns its
# place when the two differ: `--totp` / "2FA" differ, and `rate-limiting.md` /
# "rate limit" do not. The rows where they agree are kept anyway as live
# tripwires — they cost one regex each and they fail loudly if a page is ever
# retitled into internal vocabulary.
READER_VOCABULARY = (
    ('two-factor authentication (2FA)', 'docs/guide/authentication.md',
     r'\b2fa\b|\btwo[- ]factor\b'),
    ('sign-up / registration', 'docs/guide/authentication.md',
     r'\bsign[- ]?up\b|\bregistration\b'),
    ('forgot-password / password reset', 'docs/guide/authentication.md',
     r'\bforgot[- ]password\b|\bpassword reset\b|\breset password\b'),
    ('OAuth / social login', 'docs/guide/oauth.md', r'\boauth\b'),
    ('CSRF protection', 'docs/guide/middleware.md', r'\bcsrf\b'),
    ('CORS', 'docs/guide/middleware.md', r'\bcors\b'),
    ('rate limiting', 'docs/guide/rate-limiting.md', r'\brate[- ]limit'),
    ('background jobs', 'docs/guide/jobs.md', r'\bbackground job'),
    ('cron / scheduled work', 'docs/guide/jobs.md', r'\bcron\b'),
    ('websockets', 'docs/guide/websockets.md', r'\bwebsocket'),
    ('full-text search', 'docs/guide/full-text-search.md',
     r'\bfull[- ]text search\b'),
    ('file upload', 'docs/guide/forms.md', r'\bfile upload\b|\bupload'),
    ('file storage / S3', 'docs/guide/storage.md', r'\bs3\b'),
    ('database migrations', 'docs/guide/migrations.md', r'\bmigration'),
    ('testing', 'docs/guide/testing.md', r'\btest'),
    ('deployment', 'docs/guide/deployment.md', r'\bdeploy'),
)

PERCENT_ESCAPE = re.compile(r'%[0-9A-Fa-f]{2}')

# Code renders verbatim, so these regions are carried through untouched and
# every markup rule below is applied only OUTSIDE them. Fenced blocks first, so
# a stray backtick inside a fence cannot start an inline span.
CODE_REGION = re.compile(
    r'^[ \t]*(?P<f>`{3,}|~{3,})[^\n]*\n.*?^[ \t]*(?P=f)[ \t]*$'  # fenced block
    r'|`+[^`\n]*`+',                                             # inline span
    re.M | re.S,
)

# Markup that carries text a reader never sees.
HTML_COMMENT = re.compile(r'<!--.*?-->', re.S)
REF_DEFINITION = re.compile(r'^[ \t]*\[[^\]]+\]:[ \t]*\S+.*$', re.M)
AUTOLINK = re.compile(r'<[a-zA-Z][a-zA-Z0-9+.-]*://[^>\s]*>')
HTML_TAG = re.compile(r'</?[A-Za-z][^>]*>')

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
    chunk = HTML_COMMENT.sub(' ', chunk)
    chunk = REF_DEFINITION.sub(' ', chunk)
    chunk = AUTOLINK.sub(' ', chunk)
    chunk = _strip_link_destinations(chunk)  # before tags: `](…)` may hold `<…>`
    return HTML_TAG.sub(' ', chunk)


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
    """
    out = []
    pos = 0
    for m in CODE_REGION.finditer(text):
        out.append(_strip_markup(text[pos:m.start()]))
        out.append(m.group(0))
        pos = m.end()
    out.append(_strip_markup(text[pos:]))
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
    for label, page, pattern in rows:
        if not exists(page):
            defects.append((label, page, pattern, 'page does not exist'))
            continue
        checked += 1
        hits = re.findall(pattern, prose(read(page)), re.I)
        if not hits:
            defects.append((label, page, pattern, 'reader word absent'))
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

    row = (('2FA', 'p.md', r'\b2fa\b|\btwo[- ]factor\b'),)
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
    d, _ = check((('x', 'does/not/exist.md', r'x'),))
    if len(d) != 1 or d[0][3] != 'page does not exist':
        failures.append('a missing page should be reported as a defect')

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

    if failures:
        print('SELF-TEST FAILED:', file=sys.stderr)
        for f in failures:
            print(f'  - {f}', file=sys.stderr)
        return 1
    print('Self-test OK (16 properties).')
    return 0


if MODE == '--self-test':
    sys.exit(self_test())

print('Checking that capability pages carry the words readers search by...')
defects, checked = check(READER_VOCABULARY)
print(f'vocabulary rows: {len(READER_VOCABULARY)} ({checked} pages read)')
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
