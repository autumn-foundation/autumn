#!/usr/bin/env bash
# Markdown link gate: every relative link and heading anchor in the tracked
# markdown corpus must resolve.
#
# WHY THIS EXISTS: `scripts/check-docs.sh` gates *rustdoc* intra-doc links
# (`cargo doc -D rustdoc::broken_intra_doc_links`), and
# `scripts/check-plugin-freshness.sh` gates the `docs/guide/*.md` paths named
# from `skills/` and `agents/`. Between them, nothing checked the ~385-file
# markdown corpus itself, so a guide could link to a page that was renamed,
# never written, or lives one directory up and nothing would notice. The
# baseline run of this script found 19 such links across 11 pages — including
# five in `docs/guide/aggregates.md` that were rustdoc paths
# (`[x](autumn_web::aggregate::GroupedAggregate)`) pasted into hand-written
# markdown, where they render as relative links to a directory that does not
# exist. A reader clicking one gets a 404, and unlike a wrong sentence a 404
# is a defect they cannot route around.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Relative file links resolve to a path that exists on disk.
#   2. Heading anchors resolve — both in-page (`#section`) and cross-page
#      (`other.md#section`) — against GitHub's slug algorithm.
#   3. Rustdoc-style paths (`crate::mod::Item`) used as a markdown link
#      target are reported under their own message, because the fix is to
#      link to docs.rs rather than to repair a path.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - External `http(s)://` links. Network-dependent, so they would make this
#     gate flaky and slow; a link that 404s on someone else's server is also
#     not something a PR author can fix in this repo.
#   - Link targets inside fenced or inline code. Those are samples, not links.
#   - `examples/*/content/`. That tree is seed content for the wiki example
#     app, rendered by that app's own `#[static_get("/docs/{slug}")]` route —
#     its `/docs/configuration` is a live app route, not a file path.
#
# ANCHOR SLUGS: GitHub lowercases the heading, strips everything that is not
# a word character, whitespace, or a hyphen, then maps each remaining space
# to its own hyphen. It does NOT collapse runs, so `## a — b` becomes `a--b`
# (the em dash vanishes and both of its spaces survive). Getting this wrong
# in either direction produces confident nonsense, so it is exercised by
# --self-test.
#
# USAGE:
#   scripts/check-docs-links.sh              # gate the corpus
#   scripts/check-docs-links.sh --self-test  # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# The checker reads a corpus root and prints one line per defect. Kept in
# Python because the anchor algorithm and the code-fence stripping are both
# regex work that bash would render unreadable; python3 is already a
# dependency of scripts/check-plugin-freshness.sh.
run_check() {
  local dir="$1"
  python3 - "$dir" <<'PYEOF'
import os, re, subprocess, sys, collections, urllib.parse

root = sys.argv[1]

# NUL-delimited: a path containing whitespace (`docs/setup guide.md`) would
# otherwise split into fragments, inflating the corpus count while silently
# skipping the real file and every broken link in it. `-z` also stops git
# from quoting and escaping unusual paths.
files = [
    f
    for f in subprocess.run(
        ["git", "ls-files", "-z", "*.md"], cwd=root, capture_output=True, text=True
    ).stdout.split("\0")
    # Seed content for the wiki example app, resolved by that app's routes.
    if f and "/content/" not in f
]

# CommonMark also allows an angle-bracket destination — `[x](<target.md>)` —
# which is the only way to write a path containing spaces. Both spellings are
# captured; exactly one of the two groups matches per link. Without this, a
# valid `(<target.md>)` is reported broken (the brackets end up in the path)
# and a broken `(<missing file.md>)` is missed entirely.
# The bare form also permits balanced parentheses — `[x](guide(v2).md)` — so
# stopping at the first `)` would check `guide(v2` and fail on a file that
# exists. One level of nesting covers any realistic path.
BARE = r'(?:[^()\s]|\([^()\s]*\))+'
# A link title may be delimited by ", ' or (). Accepting only double quotes
# meant a link like `[x](missing.md 'why')` failed to match at all, so it was
# skipped rather than checked — the gate reporting success on a broken link.
TITLE = r'''(?:\s+(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^()\\]|\\.)*\)))?'''
DEST = r'\(\s*(?:<([^<>]*)>|(' + BARE + r'))' + TITLE + r'\s*\)'
# Two label shapes, because a linked image nests one link inside another:
# `[![alt](img.png)](page.md)`. The flat label finds the inner image target,
# the nesting one finds the outer click target. Matching only the flat form
# checked the image and never the page it links to.
# `\.` keeps an escaped bracket inside the label instead of ending it, so
# `[closing \]](page.md)` still yields its destination.
FLAT = r'(?:[^\[\]\\]|\\.)'
# `\[sample](x.md)` renders literal text, so the opening bracket must be
# unescaped for this to be a link at all.
INLINE = re.compile(r'(?<!\\)\[' + FLAT + r'*\]' + DEST)
NESTED = re.compile(r'(?<!\\)\[(?:' + FLAT + r'|\[' + FLAT + r'*\])*\]' + DEST)
REFDEF = re.compile(r'^ {0,3}\[' + FLAT + r'+\]:\s*(?:<([^<>]*)>|(\S+))', re.M)


# Markdown drops the backslash from an escaped ASCII punctuation character in
# a destination, so `guide\(v2\).md` addresses the file `guide(v2).md`.
# Keeping the backslashes would reject a link that resolves for the reader.
ESCAPED_PUNCT = re.compile(r'\\([!-/:-@\[-`{-~])')


def link_targets(text):
    """Yield every link destination once, unescaped and angle brackets removed.

    Escaped backslashes are folded to a placeholder first: `\\\\[x](y.md)` has
    its bracket left UNescaped (the first backslash consumes the second), and
    a fixed-width lookbehind cannot count the run to tell that from `\\[`.
    """
    text = text.replace("\\\\", "\x00")
    seen = set()
    for pattern in (INLINE, NESTED, REFDEF):
        for bracketed, bare in pattern.findall(text):
            raw = (bracketed or bare).strip().replace("\x00", "\\\\")
            t = ESCAPED_PUNCT.sub(r'\1', raw)
            if t and t not in seen:
                seen.add(t)
                yield t
# A rustdoc intra-doc path: two or more `::`-joined idents, no slashes.
RUSTDOC = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+$')


FENCE = re.compile(r'^([ \t]*)(`{3,}|~{3,})(.*)$')
LIST_ITEM = re.compile(r'^([ \t]*)(?:[-*+]|\d{1,9}[.)])([ \t]+)(?=\S)')
INDENT = re.compile(r'^[ \t]*')
# A block-quote marker may carry up to three spaces of indentation. Allowing
# more would let `    > ``` ` — an *indented code* line that merely starts
# with a quote marker — have both its marker and its four spaces stripped,
# turning a non-fence into a column-zero fence and reopening the swallowed-
# paragraph bug the indent bound above exists to prevent.
BLOCKQUOTE = re.compile(r'^ {0,3}(?:>[ \t]?)+')


def quote_depth(line):
    """How many block-quote markers open this line (0 if it is not quoted)."""
    m = BLOCKQUOTE.match(line)
    return m.group(0).count(">") if m else 0
# A Setext underline, and the paragraph line it may promote to a heading.
SETEXT = re.compile(r'^ {0,3}(=+|-+)[ \t]*$')
NOT_PARAGRAPH = re.compile(r'^(?: {0,3}[#>|]| {0,3}(?:[-*+=]|\d{1,9}[.)])[ \t]|\s*$|\s{4,})')


def column(s):
    """Width of an indent string, counting tabs to CommonMark's 4-col stops."""
    w = 0
    for ch in s:
        w = w + 4 - (w % 4) if ch == "\t" else w + 1
    return w


def strip_fences(text):
    """Drop fenced blocks so samples inside them are not read as live links.

    Scanned line by line rather than matched with an anchored regex, because
    a fence may be indented up to three columns past the start of its
    containing block: nested in a list item it routinely sits at four or more
    columns, which no fixed `^ {0,3}` prefix can express. This corpus uses 2-
    and 3-space fences in 176 places today; treating a markdown sample inside
    one as a live link would fail this gate on text that renders as code.

    The bound matters as much as the allowance. At top level, a line indented
    four or more columns is an *indented code block*, not a fence — so
    accepting unlimited indentation would let a pair of such lines swallow the
    live paragraph between them, hiding any broken link in it. So the innermost
    open list item's content column is tracked, and a fence is recognised only
    within three columns of it.

    A fence closes on the same character at least as long as the opener, so
    a ```` ``` ```` sample nested inside a ```` ```` ```` block stays inside it.

    An unclosed fence runs to end of file, which is what CommonMark specifies
    and what GitHub renders — so a file with an unbalanced fence has its tail
    treated as code by this gate exactly as a reader sees it. That is correct
    but worth knowing: it means such a tail is not link-checked. One file is
    in that state today (`skills/autumn-web/SKILL.md`, whose ```` ```bash ````
    opener around line 2479 went missing); balancing it is a separate change,
    not this gate's business.

    Fence lines are blanked rather than dropped so line positions survive:
    Setext heading detection below needs to know what really preceded an
    underline, and deleting a fenced block would splice a paragraph onto a
    following `---` and invent a heading that is not there.
    """
    out, opener, stack, quoted = [], None, [], 0
    for line in text.splitlines():
        base = stack[-1] if stack else 0
        depth = quote_depth(line)
        # A fenced block inside a block quote ends when that quote ends, so an
        # unclosed quoted fence must not blank the prose that follows it all
        # the way to EOF. Depth, not a boolean: a fence opened in `> > ` ends
        # when the line dedents to `> `, even though both are still quoted.
        if opener is not None and quoted and line.strip() and depth < quoted:
            opener, quoted = None, 0
        # A fence inside a block quote (`> ```md`) is still a fence; the
        # corpus has 12 of them. Detection runs on the quote-stripped content
        # while the original line is what gets kept, so real links inside a
        # block quote stay in scope.
        content = BLOCKQUOTE.sub("", line)
        m = FENCE.match(content)
        indent = column(m.group(1)) if m else None
        if opener is None:
            # Track the innermost open list item: its content column is what
            # a fence's three-column allowance is measured against. A non-blank
            # line dedented past it closes the item.
            item = LIST_ITEM.match(content)
            if item:
                marker = column(item.group(1))
                while stack and stack[-1] > marker:
                    stack.pop()
                stack.append(column(item.group(0)))
            elif content.strip():
                # Dedenting leaves the innermost items but stays inside any
                # outer one, so pop back to the enclosing item's column rather
                # than resetting to zero — a fence indented for the parent list
                # is still a fence.
                dedent = column(INDENT.match(content).group(0))
                while stack and dedent < stack[-1]:
                    stack.pop()
            base = stack[-1] if stack else 0
            # A backtick fence's info string may not itself contain a
            # backtick, so ```md`invalid is not an opener — accepting it would
            # open scanner state and blank live links until EOF.
            if m and indent <= base + 3 and not (m.group(2)[0] == "`" and "`" in m.group(3)):
                opener, quoted = m.group(2), depth
                out.append("")
            else:
                out.append(line)
        else:
            out.append("")
            if (
                m
                and indent <= base + 3
                and m.group(2)[0] == opener[0]
                and len(m.group(2)) >= len(opener)
                # A closing fence carries nothing but trailing whitespace.
                # Accepting ```not-a-closer as the closer made the real closer
                # an opener, hiding live prose to the end of the file.
                and not m.group(3).strip()
            ):
                opener = None
    return "\n".join(out)


# A code span is delimited by a run of backticks and closed by an equal-length
# run, so ``a [link](x.md) b`` is one span. Matching single backticks only
# consumed the delimiters as two empty spans and left the contents exposed as
# a live link.
# Both delimiters must be *complete* runs: a code span opened with one
# backtick is not closed by the first tick of a ``-run. Without the boundary
# assertions, `` `[x](missing.md)`` `` was treated as a span and its live
# broken link stripped away.
CODE_SPAN = re.compile(r'(?<!`)(`+)(?!`)(.*?)(?<!`)\1(?!`)')
# Markdown parked in an HTML comment renders nowhere and is not a link, so a
# commented-out draft must not fail the gate.
HTML_COMMENT = re.compile(r'<!--.*?-->', re.S)


def blank_comments(text):
    """Replace comments with as many newlines as they spanned.

    Line positions must survive: Setext detection reads the line above an
    underline, so collapsing a comment would splice unrelated lines together.
    """
    return HTML_COMMENT.sub(lambda m: "\n" * m.group(0).count("\n"), text)


def strip_code(text):
    return CODE_SPAN.sub('', blank_comments(strip_fences(text)))


def slugify(heading):
    h = re.sub(r'^#+\s*', '', heading.strip()).replace('`', '')
    h = re.sub(r'\[([^\]]*)\]\([^)]*\)', r'\1', h)   # keep link text only
    h = re.sub(r'[^\w\s-]', '', h.lower())
    # Each space becomes its own hyphen; runs are NOT collapsed.
    return h.strip().replace(' ', '-')


def read(path):
    """Decode a corpus file, sniffing the BOM.

    RELEASE_NOTES.md is UTF-16LE (a stale generated artifact); decoding by
    BOM keeps its links inside the gate instead of crashing it or, worse,
    silently skipping the one file nobody is looking at.
    """
    try:
        raw = open(path, "rb").read()
    except OSError:
        return None
    for bom, enc in ((b"\xff\xfe", "utf-16"), (b"\xfe\xff", "utf-16"), (b"\xef\xbb\xbf", "utf-8-sig")):
        if raw.startswith(bom):
            try:
                return raw.decode(enc)
            except UnicodeDecodeError:
                return None
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return None


anchors = {}
undecodable = []
for f in files:
    text = read(os.path.join(root, f))
    if text is None:
        # Skipping in silence is the one outcome a gate must never have: the
        # file stays in the corpus count while nothing in it is checked, so a
        # broken link inside it reports success. Same failure as the
        # whitespace-split path bug. Report it instead.
        undecodable.append(f)
        continue
    # Comments are stripped here too: a `## Phantom` inside one renders no
    # anchor, and indexing it would make a broken link look resolved.
    body = blank_comments(strip_fences(text))
    lines = body.splitlines()

    # Skip YAML front matter. Its closing `---` sits directly under a content
    # line, which is exactly the shape of a Setext H2 — every `---` in this
    # corpus that looks like a Setext underline is in fact a front-matter
    # closer or lives inside a fence. Indexing those would mint anchors for
    # text that is not a heading, and a phantom anchor is worse than a missing
    # one: it makes a broken link look resolved.
    start = 0
    if lines and lines[0].strip() == "---":
        for i in range(1, len(lines)):
            if lines[i].strip() == "---":
                start = i + 1
                break

    seen, found = collections.Counter(), set()

    def add(title):
        """Record a heading's anchor, disambiguating exactly as GitHub does.

        The suffix must land on an id nothing else claimed: headings `Foo`,
        `Foo`, `Foo-1` become `foo`, `foo-1`, `foo-1-1`, because the third
        one's own slug is already taken. Handing out `foo-1` twice would
        report a valid link to the third heading as broken.
        """
        s = slugify(title)
        if not s:
            return
        if s not in found:
            found.add(s)
            return
        n = max(seen[s], 1)
        while f"{s}-{n}" in found:
            n += 1
        seen[s] = n + 1
        found.add(f"{s}-{n}")

    # A heading nested in a block quote still renders a heading and still
    # carries an anchor, so quote prefixes come off before any of this — for
    # the underline, the ATX marker, AND the paragraph lines a Setext
    # underline promotes. Stripping the underline but testing the raw line
    # above it let `>` trip NOT_PARAGRAPH and lose the heading.
    unquoted = [BLOCKQUOTE.sub("", l) for l in lines]

    for i in range(start, len(lines)):
        line = unquoted[i]
        atx = re.match(r'^ {0,3}(#{1,6})\s+(.*)$', line)
        if atx:
            add(atx.group(2))
        elif SETEXT.match(line) and i > start and not NOT_PARAGRAPH.match(unquoted[i - 1]):
            # A Setext underline promotes the WHOLE paragraph above it, not
            # just its last line: `First line / second line / ---` is one
            # heading slugged `first-line-second-line`. Taking only the last
            # line would both miss that anchor and mint `second-line`, which
            # exists nowhere — and a phantom anchor makes a broken link look
            # resolved.
            j = i - 1
            while j > start and not NOT_PARAGRAPH.match(unquoted[j - 1]):
                j -= 1
            add(" ".join(l.strip() for l in unquoted[j:i]))

    # `id=x`, `id='x'` and `id="x"` are all valid HTML.
    for m in re.finditer(
        r'''<a\s[^>]*?\b(?:id|name)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))''',
        body,
    ):
        found.add(next(g for g in m.groups() if g is not None))
    anchors[f] = found

defects = [
    f"{f}: cannot decode (not UTF-8, and no UTF-16/UTF-8 BOM) — nothing in "
    f"this file was checked"
    for f in undecodable
]
for f in files:
    text = read(os.path.join(root, f))
    if text is None:
        continue
    for t in link_targets(strip_code(text)):
        if not t or t.startswith(("http://", "https://", "mailto:", "tel:")) or "://" in t:
            continue
        if RUSTDOC.match(t):
            defects.append(
                f"{f}: rustdoc path used as a markdown link target: `{t}` "
                f"(link to https://docs.rs/... instead)"
            )
            continue
        if t.startswith("#"):
            # The fragment is URL-encoded too: `#caf%C3%A9` addresses `#café`.
            # Compared exactly: fragments are case-sensitive, and GitHub's
            # slugger always lowercases, so `#Section` never reaches the
            # `id="section"` it was written for.
            frag = urllib.parse.unquote(t[1:])
            if frag and frag not in anchors.get(f, ()):
                defects.append(
                    f"{f}: no such heading anchor in this page: `{t}`"
                    + (" (case: anchors are lowercase)"
                       if frag.lower() in anchors.get(f, ()) else "")
                )
            continue
        path, _, anchor = t.partition("#")
        path = path.split("?")[0]
        if not path:
            continue
        # A rendered link is a URL: `a%20b.md` addresses the file `a b.md`.
        path = urllib.parse.unquote(path)
        resolved = os.path.normpath(os.path.join(os.path.dirname(os.path.join(root, f)), path))
        # A destination that climbs out of the checkout resolves against the
        # runner's filesystem, so `../../../../etc/passwd` "exists" and passes
        # while the rendered link reaches nothing. Existence off-tree is not
        # evidence of anything.
        if resolved != root and not resolved.startswith(root + os.sep):
            defects.append(f"{f}: link target escapes the repository: `{t}`")
            continue
        if not os.path.exists(resolved):
            defects.append(f"{f}: link target does not exist: `{t}`")
            continue
        rel = os.path.relpath(resolved, root)
        anchor = urllib.parse.unquote(anchor)
        if anchor and rel in anchors and anchor not in anchors[rel]:
            defects.append(
                f"{f}: no such heading anchor in {rel}: `{t}`"
                + (" (case: anchors are lowercase)"
                   if anchor.lower() in anchors[rel] else "")
            )

print(f"corpus: {len(files)} tracked markdown files")
for d in sorted(defects):
    print(f"  {d}")
print(f"defects: {len(defects)}")
sys.exit(1 if defects else 0)
PYEOF
}

self_test() {
  local tmp pass=0 total=0
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064 -- expand now: $tmp is function-local.
  trap "rm -rf '$tmp'" EXIT

  make_corpus() {
    local dir="$1"
    mkdir -p "$dir/docs/guide"
    git init -q "$dir"
    git -C "$dir" config user.email test@test
    git -C "$dir" config user.name test
    # `## a — b` must slug to `a--b`: the em dash is stripped and BOTH of its
    # spaces survive as hyphens. This is the case a naive `\s+ -> -` gets
    # wrong, and getting it wrong would flag every correct link of this shape.
    printf '# Jobs\n\n## `after_commit` — post-commit callbacks\n\ntext\n' \
      > "$dir/docs/guide/jobs.md"
    printf '# Mail\n\nSee [jobs](jobs.md#after_commit--post-commit-callbacks).\n' \
      > "$dir/docs/guide/mail.md"
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
  check "clean corpus passes (em-dash anchor resolves)" pass "$c1"

  # A missing file target.
  local c2="$tmp/c2"; make_corpus "$c2"
  printf 'See [gone](does-not-exist.md).\n' >> "$c2/docs/guide/mail.md"
  git -C "$c2" commit -qam broken-file
  check "missing link target fails" fail "$c2"

  # A cross-page anchor that does not exist.
  local c3="$tmp/c3"; make_corpus "$c3"
  printf 'See [nope](jobs.md#no-such-heading).\n' >> "$c3/docs/guide/mail.md"
  git -C "$c3" commit -qam broken-anchor
  check "missing cross-page anchor fails" fail "$c3"

  # An in-page anchor that does not exist.
  local c4="$tmp/c4"; make_corpus "$c4"
  printf 'See [nope](#no-such-heading).\n' >> "$c4/docs/guide/mail.md"
  git -C "$c4" commit -qam broken-inpage
  check "missing in-page anchor fails" fail "$c4"

  # A rustdoc path pasted in as a link target.
  local c5="$tmp/c5"; make_corpus "$c5"
  printf 'See [`X`](autumn_web::aggregate::GroupedAggregate).\n' >> "$c5/docs/guide/mail.md"
  git -C "$c5" commit -qam rustdoc-path
  check "rustdoc path as link target fails" fail "$c5"

  # Links inside code samples are samples, not links.
  local c6="$tmp/c6"; make_corpus "$c6"
  printf '\n```md\n[sample](totally-made-up.md)\n```\n\nand `[inline](nope.md)`.\n' \
    >> "$c6/docs/guide/mail.md"
  git -C "$c6" commit -qam code-samples
  check "links inside code are ignored" pass "$c6"

  # External links are out of scope.
  local c7="$tmp/c7"; make_corpus "$c7"
  printf 'See [ext](https://example.invalid/nope).\n' >> "$c7/docs/guide/mail.md"
  git -C "$c7" commit -qam external
  check "external links are ignored" pass "$c7"

  # Duplicate headings get GitHub's `-1`, `-2` disambiguating suffixes.
  local c8="$tmp/c8"; make_corpus "$c8"
  printf '\n## Setup\n\ntext\n\n## Setup\n\ntext\n' >> "$c8/docs/guide/jobs.md"
  printf 'See [second](jobs.md#setup-1).\n' >> "$c8/docs/guide/mail.md"
  git -C "$c8" commit -qam dup-headings
  check "duplicate-heading suffix resolves" pass "$c8"

  # CommonMark angle-bracket destinations, both directions: the resolving one
  # must not be reported (the brackets are not part of the path), and the
  # broken one must be — it is the spelling used for paths with spaces, so
  # a regex that only matches bare targets misses it entirely.
  local c9="$tmp/c9"; make_corpus "$c9"
  printf 'See [ok](<jobs.md>) and [ok too][ref].\n\n[ref]: <jobs.md>\n' \
    >> "$c9/docs/guide/mail.md"
  git -C "$c9" commit -qam angle-ok
  check "angle-bracket destination resolves" pass "$c9"

  local c10="$tmp/c10"; make_corpus "$c10"
  printf 'See [gone](<missing file.md>).\n' >> "$c10/docs/guide/mail.md"
  git -C "$c10" commit -qam angle-broken
  check "broken angle-bracket destination with a space fails" fail "$c10"

  # A fence indented inside a list item is still a fence. CommonMark allows up
  # to three spaces; the corpus uses this form widely.
  local c11="$tmp/c11"; make_corpus "$c11"
  printf '\n- a nested item:\n\n    ```md\n    [example](totally-made-up.md)\n\n    ## Phantom Heading\n    ```\n' \
    >> "$c11/docs/guide/mail.md"
  git -C "$c11" commit -qam indented-fence
  check "links inside an indented fence are ignored" pass "$c11"

  # ...and the heading inside that indented fence must not become a real
  # anchor, or a link to it would silently "resolve" to nothing.
  local c12="$tmp/c12"; make_corpus "$c12"
  printf '\n- a nested item:\n\n    ```md\n    ## Phantom Heading\n    ```\n' \
    >> "$c12/docs/guide/jobs.md"
  printf 'See [phantom](jobs.md#phantom-heading).\n' >> "$c12/docs/guide/mail.md"
  git -C "$c12" commit -qam phantom-anchor
  check "heading inside an indented fence is not an anchor" fail "$c12"

  # A tracked path containing a space must stay one record. Split on
  # whitespace it becomes two nonexistent fragments: the corpus count is
  # inflated and the real file — with its broken link — is skipped in silence.
  local c13="$tmp/c13"; make_corpus "$c13"
  printf '# Setup\n\nSee [gone](does-not-exist.md).\n' > "$c13/docs/guide/setup guide.md"
  git -C "$c13" add -A && git -C "$c13" commit -qm spaced-path
  check "broken link in a path containing a space is caught" fail "$c13"

  # At top level, four or more columns is an indented CODE block, not a fence.
  # Treating those two lines as a fence pair would swallow the live paragraph
  # between them and hide the broken link it carries.
  local c14="$tmp/c14"; make_corpus "$c14"
  printf '\n    ```\n\nA live paragraph with [bad](missing.md) in it.\n\n    ```\n' \
    >> "$c14/docs/guide/mail.md"
  git -C "$c14" commit -qam top-level-indented-code
  check "top-level indented backticks do not hide a live link" fail "$c14"

  # CommonMark allows balanced parentheses in a bare destination; stopping at
  # the first `)` would check `guide(v2` and fail on a file that exists.
  local c15="$tmp/c15"; make_corpus "$c15"
  printf 'x\n' > "$c15/docs/guide/guide(v2).md"
  printf 'See [guide](guide(v2).md).\n' >> "$c15/docs/guide/mail.md"
  git -C "$c15" add -A && git -C "$c15" commit -qm balanced-parens
  check "balanced parentheses in a destination resolve" pass "$c15"

  # A fence inside a block quote is still a fence (the corpus has 12).
  local c16="$tmp/c16"; make_corpus "$c16"
  printf '\n> A quoted sample:\n>\n> ```md\n> [sample](totally-made-up.md)\n> ```\n' \
    >> "$c16/docs/guide/mail.md"
  git -C "$c16" commit -qam blockquoted-fence
  check "sample inside a blockquoted fence is ignored" pass "$c16"

  # ...but quoted prose is not code: a real link inside a block quote must
  # still be checked, or the fix above would blind the gate to whole sections.
  local c17="$tmp/c17"; make_corpus "$c17"
  printf '\n> A quoted broken link: [gone](no-such-file.md).\n' \
    >> "$c17/docs/guide/mail.md"
  git -C "$c17" commit -qam blockquoted-link
  check "broken link in block-quoted prose is still caught" fail "$c17"

  # Setext headings carry anchors just as ATX ones do.
  local c18="$tmp/c18"; make_corpus "$c18"
  printf '\nSetext title\n============\n\nSub heading\n-----------\n' >> "$c18/docs/guide/jobs.md"
  printf 'See [one](jobs.md#setext-title) and [two](jobs.md#sub-heading).\n' \
    >> "$c18/docs/guide/mail.md"
  git -C "$c18" commit -qam setext
  check "Setext heading anchors resolve" pass "$c18"

  # The guard that makes the above safe: YAML front matter's closing `---`
  # sits under a content line, which is exactly a Setext H2's shape. Indexing
  # it would mint an anchor for text that is not a heading — and a phantom
  # anchor is worse than a missing one, because it makes a broken link look
  # resolved. Every `---` in this corpus that looks like a Setext underline
  # is in fact a front-matter closer.
  local c19="$tmp/c19"; make_corpus "$c19"
  printf -- '---\ntitle: Jobs\ndescription: not a heading\n---\n\n# Jobs\n' \
    > "$c19/docs/guide/jobs.md"
  printf 'See [phantom](jobs.md#description-not-a-heading).\n' >> "$c19/docs/guide/mail.md"
  git -C "$c19" commit -qam frontmatter-closer
  check "front-matter closer is not a Setext heading" fail "$c19"

  # An indented-code line that merely begins with a quote marker is not a
  # fence. Stripping the marker together with its four spaces would turn it
  # into a column-zero fence and swallow the paragraph between two of them.
  local c20="$tmp/c20"; make_corpus "$c20"
  printf '\n    > ```\n\nLive paragraph with [bad](missing.md).\n\n    > ```\n' \
    >> "$c20/docs/guide/mail.md"
  git -C "$c20" commit -qam quoted-indented-code
  check "quote marker in indented code does not hide a live link" fail "$c20"

  # CommonMark titles come in three delimiters. A link whose title the pattern
  # cannot parse is not merely mis-parsed, it is skipped — so a broken target
  # passes and the gate reports success.
  local c21="$tmp/c21"; make_corpus "$c21"
  printf "\nSee [x](missing.md 'a title') and [y](gone.md (a title)).\n" \
    >> "$c21/docs/guide/mail.md"
  git -C "$c21" commit -qam link-titles
  check "broken link with a single-quoted title is caught" fail "$c21"

  # A Setext underline promotes the whole paragraph above it, not its last
  # line: taking only the last line misses `#first-line-second-line` AND mints
  # `#second-line`, an anchor that exists nowhere.
  local c22="$tmp/c22"; make_corpus "$c22"
  printf '\nFirst line\nsecond line\n---\n' >> "$c22/docs/guide/jobs.md"
  printf 'See [ok](jobs.md#first-line-second-line).\n' >> "$c22/docs/guide/mail.md"
  git -C "$c22" commit -qam multiline-setext
  check "multiline Setext heading resolves as one anchor" pass "$c22"

  local c23="$tmp/c23"; make_corpus "$c23"
  printf '\nFirst line\nsecond line\n---\n' >> "$c23/docs/guide/jobs.md"
  printf 'See [phantom](jobs.md#second-line).\n' >> "$c23/docs/guide/mail.md"
  git -C "$c23" commit -qam multiline-setext-phantom
  check "last line alone is not a Setext anchor" fail "$c23"

  # A linked image nests one link inside another; both destinations count.
  # Checking only the inner one meant the page a reader actually clicks
  # through to went unchecked — this is how README's `](LICENSE)` badge, whose
  # target does not exist, went unnoticed.
  local c24="$tmp/c24"; make_corpus "$c24"
  printf 'x\n' > "$c24/docs/guide/diagram.png"
  printf '\n[![diagram](diagram.png)](no-such-page.md)\n' >> "$c24/docs/guide/mail.md"
  git -C "$c24" add -A && git -C "$c24" commit -qm linked-image-outer
  check "outer target of a linked image is checked" fail "$c24"

  local c25="$tmp/c25"; make_corpus "$c25"
  printf '\n[![diagram](no-such-image.png)](jobs.md)\n' >> "$c25/docs/guide/mail.md"
  git -C "$c25" commit -qam linked-image-inner
  check "inner target of a linked image is still checked" fail "$c25"

  # Dedenting out of a nested item stays inside the parent item, so a fence
  # indented for the parent is still a fence.
  local c26="$tmp/c26"; make_corpus "$c26"
  printf '\n- outer\n\n  - inner item\n\n  back to outer content\n\n    ```md\n    [x](totally-made-up.md)\n    ```\n' \
    >> "$c26/docs/guide/mail.md"
  git -C "$c26" commit -qam parent-list-fence
  check "fence indented for a parent list item is stripped" pass "$c26"

  # A code span may be delimited by a run of backticks.
  local c27="$tmp/c27"; make_corpus "$c27"
  printf '\nSee ``a span with [x](totally-made-up.md) inside`` here.\n' \
    >> "$c27/docs/guide/mail.md"
  git -C "$c27" commit -qam multi-backtick-span
  check "multi-backtick code span is stripped" pass "$c27"

  # A closing fence carries only whitespace. Accepting ```text as a closer
  # turns the real closer into an opener, hiding everything after it.
  local c28="$tmp/c28"; make_corpus "$c28"
  printf '\n```\n```not-a-closer\n```\n\nLive [bad](missing.md) after the block.\n' \
    >> "$c28/docs/guide/mail.md"
  git -C "$c28" commit -qam fence-trailing-text
  check "text after a closing fence does not hide later links" fail "$c28"

  # An opener of one backtick is not closed by the first tick of a ``-run, so
  # no code span forms and the link inside is live.
  local c29="$tmp/c29"; make_corpus "$c29"
  printf '\nSee `[bad](missing.md)`` here.\n' >> "$c29/docs/guide/mail.md"
  git -C "$c29" commit -qam unmatched-backtick-run
  check "unmatched backtick run does not form a code span" fail "$c29"

  # An escaped bracket belongs to the label, not the end of it.
  local c30="$tmp/c30"; make_corpus "$c30"
  printf '\nSee [closing \\]](missing.md).\n' >> "$c30/docs/guide/mail.md"
  git -C "$c30" commit -qam escaped-bracket-label
  check "escaped bracket in a label keeps the link checked" fail "$c30"

  # Markdown parked in an HTML comment renders nowhere.
  local c31="$tmp/c31"; make_corpus "$c31"
  printf '\n<!-- [draft](totally-made-up.md) -->\n' >> "$c31/docs/guide/mail.md"
  git -C "$c31" commit -qam html-comment
  check "link inside an HTML comment is ignored" pass "$c31"

  # A backtick fence's info string may not contain a backtick, so this is not
  # an opener and must not blank everything after it.
  local c32="$tmp/c32"; make_corpus "$c32"
  printf '\n```md`invalid\n\nLive [bad](missing.md) after.\n' >> "$c32/docs/guide/mail.md"
  git -C "$c32" commit -qam backtick-in-info-string
  check "backtick in a fence info string does not open a fence" fail "$c32"

  # A fenced block inside a block quote ends when the quote does.
  local c33="$tmp/c33"; make_corpus "$c33"
  printf '\n> ```md\n> sample\n\nLive [bad](missing.md) after the quote.\n' \
    >> "$c33/docs/guide/mail.md"
  git -C "$c33" commit -qam quoted-fence-ends-with-quote
  check "unclosed quoted fence does not swallow later prose" fail "$c33"

  # Markdown drops the backslash from escaped punctuation in a destination.
  local c34="$tmp/c34"; make_corpus "$c34"
  printf 'x\n' > "$c34/docs/guide/guide(v2).md"
  printf '\nSee [guide](guide\\(v2\\).md).\n' >> "$c34/docs/guide/mail.md"
  git -C "$c34" add -A && git -C "$c34" commit -qm escaped-destination
  check "escaped punctuation in a destination resolves" pass "$c34"

  # GitHub's disambiguating suffix must land on an unclaimed id: headings
  # Foo / Foo / Foo-1 become foo / foo-1 / foo-1-1.
  local c35="$tmp/c35"; make_corpus "$c35"
  printf '\n## Foo\n\n## Foo\n\n## Foo-1\n' >> "$c35/docs/guide/jobs.md"
  printf 'See [third](jobs.md#foo-1-1).\n' >> "$c35/docs/guide/mail.md"
  git -C "$c35" commit -qam slug-collision
  check "duplicate-slug suffix does not collide with a real heading" pass "$c35"

  # A reference definition's label honours escapes like an inline one.
  local c36="$tmp/c36"; make_corpus "$c36"
  printf '\nSee [x][closing \\]].\n\n[closing \\]]: missing.md\n' >> "$c36/docs/guide/mail.md"
  git -C "$c36" commit -qam refdef-escaped-label
  check "escaped bracket in a reference definition is parsed" fail "$c36"

  # Existence off-tree is not evidence: a link climbing out of the checkout
  # resolves against the runner's filesystem and would otherwise "pass".
  local c37="$tmp/c37"; make_corpus "$c37"
  printf '\nSee [host](../../../../../../etc/passwd).\n' >> "$c37/docs/guide/mail.md"
  git -C "$c37" commit -qam escapes-repository
  check "link climbing out of the repository is rejected" fail "$c37"

  # A file that cannot be decoded must be reported, never skipped in silence:
  # it stays in the corpus count while nothing in it is checked.
  local c38="$tmp/c38"; make_corpus "$c38"
  printf '# Latin\n\nCaf\xe9 and [x](missing.md).\n' > "$c38/docs/guide/latin.md"
  git -C "$c38" add -A && git -C "$c38" commit -qm undecodable
  check "undecodable file is reported, not skipped" fail "$c38"

  # A heading inside an HTML comment renders no anchor.
  local c39="$tmp/c39"; make_corpus "$c39"
  printf '\n<!--\n## Phantom\n-->\n' >> "$c39/docs/guide/jobs.md"
  printf 'See [p](jobs.md#phantom).\n' >> "$c39/docs/guide/mail.md"
  git -C "$c39" commit -qam commented-heading
  check "heading inside an HTML comment is not an anchor" fail "$c39"

  # A rendered link is a URL: `a%20b.md` addresses the file `a b.md`.
  local c40="$tmp/c40"; make_corpus "$c40"
  printf 'x\n' > "$c40/docs/guide/a b.md"
  printf '\nSee [enc](a%%20b.md).\n' >> "$c40/docs/guide/mail.md"
  git -C "$c40" add -A && git -C "$c40" commit -qm percent-encoded
  check "percent-encoded destination resolves" pass "$c40"

  # A fragment is URL-encoded like the rest of the link: `#caf%C3%A9` is a
  # valid way to address `## Café`.
  local c41="$tmp/c41"; make_corpus "$c41"
  printf '\n## Caf\xc3\xa9\n' >> "$c41/docs/guide/jobs.md"
  printf 'See [a](jobs.md#caf%%C3%%A9) and [b](#caf%%C3%%A9).\n' >> "$c41/docs/guide/mail.md"
  printf '\n## Caf\xc3\xa9\n' >> "$c41/docs/guide/mail.md"
  git -C "$c41" commit -qam encoded-fragment
  check "percent-encoded heading fragment resolves" pass "$c41"

  # A fence opened in a doubly-quoted context ends when the line dedents to
  # the outer quote — both lines are quoted, so a boolean cannot see it.
  local c42="$tmp/c42"; make_corpus "$c42"
  printf '\n> > ~~~md\n> > sample\n>\n> Live [bad](missing.md) in the outer quote.\n' \
    >> "$c42/docs/guide/mail.md"
  git -C "$c42" commit -qam nested-quoted-fence
  check "nested quoted fence ends when its quote depth drops" fail "$c42"

  # Fragments are case-sensitive and GitHub's slugger always lowercases, so
  # `#Section` never reaches the `id="section"` it was written for.
  local c43="$tmp/c43"; make_corpus "$c43"
  printf '\n## Section\n' >> "$c43/docs/guide/jobs.md"
  printf 'See [wrong](jobs.md#Section).\n' >> "$c43/docs/guide/mail.md"
  git -C "$c43" commit -qam fragment-case
  check "mixed-case fragment for a generated heading fails" fail "$c43"

  # ...but an explicit HTML id keeps its own case, so an exact match resolves.
  local c44="$tmp/c44"; make_corpus "$c44"
  printf '\n<a id="MixedId"></a>\n' >> "$c44/docs/guide/jobs.md"
  printf 'See [ok](jobs.md#MixedId).\n' >> "$c44/docs/guide/mail.md"
  git -C "$c44" commit -qam html-anchor-case
  check "explicit HTML id matches its own case" pass "$c44"

  # A title may contain its own delimiter, escaped. Stopping at the escaped
  # quote made the whole link fail to match, so it was skipped, not checked.
  local c45="$tmp/c45"; make_corpus "$c45"
  printf '\nSee [broken](missing.md "title with \\" quote").\n' >> "$c45/docs/guide/mail.md"
  git -C "$c45" commit -qam escaped-title-delimiter
  check "broken link with an escaped title delimiter is caught" fail "$c45"

  # A heading nested in a block quote still carries an anchor.
  local c46="$tmp/c46"; make_corpus "$c46"
  printf '\n> ## Quoted Heading\n' >> "$c46/docs/guide/jobs.md"
  printf 'See [ok](jobs.md#quoted-heading).\n' >> "$c46/docs/guide/mail.md"
  git -C "$c46" commit -qam quoted-heading
  check "heading inside a block quote is indexed" pass "$c46"

  # An escaped opening bracket renders literal text, not a link.
  local c47="$tmp/c47"; make_corpus "$c47"
  printf '\nLiteral: \\[sample](totally-made-up.md) renders as text.\n' \
    >> "$c47/docs/guide/mail.md"
  git -C "$c47" commit -qam escaped-open-bracket
  check "escaped opening bracket is not a link" pass "$c47"

  # An even-length backslash run leaves the bracket unescaped: the first
  # backslash consumes the second, so the link is live.
  local c48="$tmp/c48"; make_corpus "$c48"
  printf '\nEven run: \\\\\\\\[x](missing.md) is a live link.\n' >> "$c48/docs/guide/mail.md"
  git -C "$c48" commit -qam even-backslash-run
  check "even backslash run leaves the link live" fail "$c48"

  # A Setext heading inside a block quote: the underline AND the paragraph it
  # promotes both need the quote prefix removed.
  local c49="$tmp/c49"; make_corpus "$c49"
  printf '\n> Quoted heading\n> ---\n' >> "$c49/docs/guide/jobs.md"
  printf 'See [ok](jobs.md#quoted-heading).\n' >> "$c49/docs/guide/mail.md"
  git -C "$c49" commit -qam quoted-setext
  check "Setext heading inside a block quote is indexed" pass "$c49"

  # `id=x`, `id='x'` and `id="x"` are all valid HTML.
  local c50="$tmp/c50"; make_corpus "$c50"
  printf "\n<a id='MixedId'></a>\n<a name=Bare></a>\n" >> "$c50/docs/guide/jobs.md"
  printf 'See [q](jobs.md#MixedId) and [b](jobs.md#Bare).\n' >> "$c50/docs/guide/mail.md"
  git -C "$c50" commit -qam anchor-quoting-forms
  check "single-quoted and unquoted anchor ids resolve" pass "$c50"

  echo "self-test: $pass/$total passed"
  [[ "$pass" -eq "$total" ]]
}

case "${1-}" in
  --self-test)
    self_test
    ;;
  *)
    echo "Checking markdown links and heading anchors..."
    if run_check "$root"; then
      echo "Markdown link gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the markdown corpus has unresolvable links (listed above).

Fix each one where it lives:
  - renamed or moved page  -> point at the current path
  - page that never existed -> drop the cross-reference, or write the page
  - wrong relative depth    -> `../x.md` vs `./x.md`
  - rustdoc path            -> link to https://docs.rs/autumn-web/latest/...
  - anchor                  -> match the heading's GitHub slug (each space
                               becomes its own hyphen; `—` is stripped, so
                               `## a — b` is `#a--b`)
EOF
      exit 1
    fi
    ;;
esac
