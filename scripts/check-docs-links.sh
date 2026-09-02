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
import os, re, subprocess, sys, collections

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
INLINE = re.compile(
    r'\[(?:[^\]]*)\]\(\s*(?:<([^<>]*)>|(' + BARE + r'))(?:\s+"[^"]*")?\s*\)'
)
REFDEF = re.compile(r'^ {0,3}\[[^\]]+\]:\s*(?:<([^<>]*)>|(\S+))', re.M)


def link_targets(text):
    """Yield every link destination, angle brackets stripped."""
    for pattern in (INLINE, REFDEF):
        for bracketed, bare in pattern.findall(text):
            yield (bracketed or bare).strip()
# A rustdoc intra-doc path: two or more `::`-joined idents, no slashes.
RUSTDOC = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+$')


FENCE = re.compile(r'^([ \t]*)(`{3,}|~{3,})')
LIST_ITEM = re.compile(r'^([ \t]*)(?:[-*+]|\d{1,9}[.)])([ \t]+)(?=\S)')
INDENT = re.compile(r'^[ \t]*')


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
    """
    out, opener, base = [], None, 0
    for line in text.splitlines():
        m = FENCE.match(line)
        indent = column(m.group(1)) if m else None
        if opener is None:
            # Track the innermost open list item: its content column is what
            # a fence's three-column allowance is measured against. A non-blank
            # line dedented past it closes the item.
            item = LIST_ITEM.match(line)
            if item:
                base = column(item.group(0))
            elif line.strip() and column(INDENT.match(line).group(0)) < base:
                base = 0
            if m and indent <= base + 3:
                opener = m.group(2)
            else:
                out.append(line)
        elif (
            m
            and indent <= base + 3
            and m.group(2)[0] == opener[0]
            and len(m.group(2)) >= len(opener)
        ):
            opener = None
    return "\n".join(out)


def strip_code(text):
    return re.sub(r'`[^`\n]*`', '', strip_fences(text))


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
for f in files:
    text = read(os.path.join(root, f))
    if text is None:
        continue
    body = strip_fences(text)
    seen, found = collections.Counter(), set()
    for m in re.finditer(r'^(#{1,6})\s+(.*)$', body, re.M):
        s = slugify(m.group(2))
        if not s:
            continue
        n = seen[s]
        seen[s] += 1
        found.add(s if n == 0 else f"{s}-{n}")
    for m in re.finditer(r'<a\s+(?:id|name)="([^"]+)"', body):
        found.add(m.group(1))
    anchors[f] = found

defects = []
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
            if t[1:] and t[1:].lower() not in anchors.get(f, ()):
                defects.append(f"{f}: no such heading anchor in this page: `{t}`")
            continue
        path, _, anchor = t.partition("#")
        path = path.split("?")[0]
        if not path:
            continue
        resolved = os.path.normpath(os.path.join(os.path.dirname(os.path.join(root, f)), path))
        if not os.path.exists(resolved):
            defects.append(f"{f}: link target does not exist: `{t}`")
            continue
        rel = os.path.relpath(resolved, root)
        if anchor and rel in anchors and anchor.lower() not in anchors[rel]:
            defects.append(f"{f}: no such heading anchor in {rel}: `{t}`")

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
