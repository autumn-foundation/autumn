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

files = [
    f
    for f in subprocess.run(
        ["git", "ls-files", "*.md"], cwd=root, capture_output=True, text=True
    ).stdout.split()
    # Seed content for the wiki example app, resolved by that app's routes.
    if "/content/" not in f
]

INLINE = re.compile(r'\[(?:[^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)')
REFDEF = re.compile(r'^\s{0,3}\[[^\]]+\]:\s*(\S+)', re.M)
# A rustdoc intra-doc path: two or more `::`-joined idents, no slashes.
RUSTDOC = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+$')


def strip_fences(text):
    """Drop fenced blocks so headings inside samples don't become anchors."""
    text = re.sub(r'^```.*?^```', '', text, flags=re.S | re.M)
    return re.sub(r'^~~~.*?^~~~', '', text, flags=re.S | re.M)


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
    for target in INLINE.findall(strip_code(text)) + REFDEF.findall(strip_code(text)):
        t = target.strip()
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
