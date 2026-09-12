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


def prose(text):
    """Undo URL escaping before searching for a reader's word.

    `%2F` is a slash. Left as-is it reads as a literal "2F" and makes
    `\b2fa\b` match `…%2Faccount…` and `…MIT%2FApache…`, which is exactly how
    the baseline defect hid from a plain grep: two of the four corpus-wide
    "2FA" hits were punctuation. Replacing every escape with a slash both
    removes the false hit and keeps the surrounding words separated, so a real
    term next to an escape still matches.
    """
    return PERCENT_ESCAPE.sub('/', text)


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

    if failures:
        print('SELF-TEST FAILED:', file=sys.stderr)
        for f in failures:
            print(f'  - {f}', file=sys.stderr)
        return 1
    print(f'Self-test OK ({4} properties).')
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
