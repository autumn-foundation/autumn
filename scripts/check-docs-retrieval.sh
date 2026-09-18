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

# Words a reader types that carry no retrieval signal. Kept short on purpose:
# every entry here is a word the matcher stops requiring, so a long list turns
# a failing question into a passing one without changing a page.
STOPWORDS = {
    'a', 'an', 'and', 'are', 'at', 'be', 'by', 'can', 'do', 'does', 'for',
    'from', 'how', 'i', 'in', 'is', 'it', 'me', 'my', 'of', 'on', 'or', 'the',
    'to', 'use', 'using', 'what', 'when', 'where', 'with', 'you', 'your',
}


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
        h1, headings = '', []
        fence = None
        for line in text.splitlines():
            # A `#` inside a fence is a shell comment, a TOML comment or a Rust
            # attribute, not a heading, and it must not be indexed: a `# Raise
            # the global level` comment in a curl fence would let a question
            # match the page that fence sits on, which is exactly the page a
            # fixture row names — so the false positive lands on the EXPECTED
            # page and the gate passes while the reader still finds nothing.
            marker = re.match(r'^\s{0,3}(`{3,}|~{3,})', line)
            if marker:
                run = marker.group(1)
                if fence is None:
                    # Keep the opener VERBATIM, character and length. A page
                    # documenting markdown opens with ```` so it can show a
                    # ``` block inside; recording that opener as three would
                    # let the inner line close it, and every heading after it
                    # would be indexed while still inside the outer fence —
                    # the same false positive this block exists to stop.
                    fence = run
                elif run[0] == fence[0] and len(run) >= len(fence):
                    # CommonMark: the closing fence is the same character and
                    # at least as long as the opener. A `~~~` never closes a
                    # ``` block, and a shorter run is content.
                    fence = None
                continue
            if fence is not None:
                continue
            m = re.match(r'^(#{1,6})\s+(.*\S)\s*$', line)
            if not m:
                continue
            level, title = len(m.group(1)), m.group(2)
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

  # 11. A comment line and a blank line in the fixture are skipped.
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
