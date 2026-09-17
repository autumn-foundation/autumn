#!/usr/bin/env bash
# Guide-index completeness gate: every page in `docs/guide/` must be listed in
# `docs/guide/index.md`, and that index must be linked from `README.md`.
#
# WHY THIS EXISTS: the corpus gates every page a reader ALREADY REACHED — its
# links (`check-docs-links.sh`), its commands (`check-docs-cli.sh`), its
# `AUTUMN_*` variables (`check-docs-config.sh`), its `autumn.toml` keys
# (`check-docs-toml.sh`), its `autumn_web::…` paths (`check-docs-symbols.sh`),
# its `/actuator/…` URLs (`check-docs-routes.sh`), its macro arguments, its
# feature gates and its version pins. `check-docs-orphans.sh` goes one step
# further and proves the page can be reached AT ALL.
#
# Reachable is not findable, and the gap between them is where this corpus was
# losing readers. `check-docs-orphans.sh` asks "does any inbound link exist",
# and it passes when the only inbound link is a mid-page crosslink from another
# guide page or a mention inside an agent `SKILL.md` — a surface written for a
# language model, not for a person with a question. Its own header says where
# discovery actually happens:
#
#   "docs/guide/ has no index page of its own. The guide pages are discovered
#    through the hand-maintained `## Documentation` list in README.md and the
#    skill indexes — surfaces someone has to remember to update. Nothing
#    noticed when they forgot."
#
# Nothing noticed, and they forgot 92 times. The baseline run of this gate, on
# the commit that introduced it, found 92 of the 147 `docs/guide/` pages listed
# in NO reader-facing index — 63% of the guide. Among them were the pages a
# reader needs in their first week:
#
#   docs/guide/middleware.md      docs/guide/testing.md
#   docs/guide/migrations.md      docs/guide/repositories.md
#   docs/guide/jobs.md            docs/guide/authorization.md
#   docs/guide/pagination.md      docs/guide/websockets.md
#   docs/guide/rate-limiting.md   docs/guide/i18n.md
#   docs/guide/events.md          docs/guide/oauth.md
#
# A cold retrieval test over the README's `## Documentation` list — the repo's
# landing page, and the one surface an arriving reader is guaranteed to see —
# found the answering page absent for 11 of 15 ordinary question classes. The
# answers all existed and every drift gate was green over them. The reader
# simply had no way to land on them, which is indistinguishable, from where
# they sit, from the feature not existing.
#
# WHY AN INDEX RATHER THAN 92 MORE README BULLETS. `README.md` is the project's
# landing page, not its table of contents; its `## Documentation` list is a
# curated set of highlights with paragraph-length entries, and pasting the
# whole guide into it would bury the highlights without making the rest
# navigable. The index is a separate page whose entire job is navigation, which
# is also why it carries no answers of its own: an index that explains things
# is a page that drifts against the pages it indexes.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Every tracked page under `docs/guide/` is listed in the index that owns
#      it — exactly once, counted ACROSS ALL INDEXES. Twice is a defect: a
#      reader who meets the same page under two headings cannot tell whether
#      they are the same page, and the second entry is the one that rots. A
#      per-file tally cannot see the worst version of this — the top-level
#      index listing a tutorial chapter that `tutorial/index.md` also lists
#      shows one hit in each file — so entries are gathered from every index
#      before any of them is judged.
#   2. Every guide page an index links exists, and is a guide page. A link to
#      a page that moved is caught by `check-docs-links.sh` as a 404; this
#      catches an index pointing somewhere outside the corpus it indexes.
#   3. Every entry sits under a `## ` section heading, so a page appended to
#      the end of the file lands somewhere a reader is actually scanning.
#   4. `README.md` carries a markdown LINK whose target resolves to
#      `docs/guide/index.md`. An index nobody can reach from the landing page
#      is the very defect this gate exists to prevent, and it would otherwise
#      be the one page the gate could not see. Checking for the literal path as
#      a substring is not enough: a plain-text mention satisfies it while
#      getting the reader nowhere, so the README's link destinations are
#      parsed and resolved.
#
# ONLY LINKS A READER CAN FOLLOW COUNT, everywhere links are extracted — in an
# index and in `README.md` alike. `readable()` blanks fenced code (``` and ~~~),
# HTML comments, inline code spans and four-space indented code blocks before
# anything is matched. Each of those is a way an unlisted page was kept green
# while no reader could reach it: an example of what an entry looks like, an
# entry parked behind `<!-- -->`, a README whose only index link sat inside a
# fence. They arrived as four separate review findings against four separate
# ad-hoc filters, which is why the reduction now lives in ONE function that
# every caller goes through rather than in a filter per caller.
#
# AN ENTRY IS A LINK IN A LIST ITEM. That is what separates an index's rows
# from its prose, and it is load-bearing in both directions. `tutorial/index.md`
# carries four ordinary cross-references in paragraphs and blockquotes — "see
# the [i18n guide]", "if you have already read the [Getting Started guide]" —
# which claim to index nothing. Counting them made the gate report 165 links
# for 161 required pages, and under the cross-index rule above it would have
# flagged every one of them as a duplicate listing of a page another index
# owns. Writing about a page is not indexing it.
#
# DELEGATION TO A SUB-INDEX. A subdirectory of `docs/guide/` that carries its
# own `index.md` — `tutorial/` does — is represented in the TOP-LEVEL index by
# that `index.md` alone, and owns its own pages. The tutorial is 12 ordered
# chapters; listing them individually in a task-shaped index would spray twelve
# near-identical entries across it and tell a reader nothing about which one to
# open first. The sub-index is listed, and it owns its own ordering.
#
# Delegation is not an escape. Each page is checked against the NEAREST index
# above it, so an unlisted tutorial chapter fails against `tutorial/index.md`
# rather than vanishing from the required set — and the rule nests to any
# depth. Getting that wrong is how the first revision of this gate let a new
# tutorial chapter listed nowhere at all pass with zero defects.
#
# TRUTH SET: `git ls-files docs/guide`, read at run time. There is no snapshot
# to regenerate — a page added in a commit is required by this gate in the same
# commit, which is the only moment anyone has the context to write its line.
#
# Usage:
#   scripts/check-docs-guide-index.sh              # gate the real corpus
#   scripts/check-docs-guide-index.sh --self-test  # synthetic-corpus tests

set -euo pipefail

# The parsing is line-oriented markdown with enough link and heading regex work
# that bash would render it unreadable; python3 is already a hard dependency of
# every sibling docs gate in this directory.
run_check() {
  local dir="$1"
  python3 - "$dir" <<'PYEOF'
import re
import subprocess
import sys

root = sys.argv[1]

GUIDE = "docs/guide/"
INDEX = GUIDE + "index.md"
README = "README.md"

# A markdown link whose target is a guide page, written relative to the index
# (`middleware.md`, `tutorial/index.md`) or from the repo root
# (`docs/guide/middleware.md`). Both spellings resolve to the same page, and
# both are accepted so the gate never argues with `check-docs-links.sh` about
# relative depth — that is its sibling's job, not this one's.
#
# The `(?<!!)` rejects image syntax. `![alt](page.md)` shares every character of
# a link but renders a picture, so it navigates nowhere: an index row reading
# `- ![preview](beta.md)` indexes nothing, and `![Guide](docs/guide/index.md)`
# in README.md is not a way to reach the index.
LINK = re.compile(r"(?<!!)\[[^\]]*\]\(\s*([^)\s#]+)")

# An HTML comment, to the closing `-->` or to end of file if it never closes.
COMMENT = re.compile(r"<!--.*?(?:-->|\Z)", re.DOTALL)
# A fence opener or closer: three or more backticks or tildes, indented at most
# three spaces, with whatever info string follows.
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
# An inline code span on one line. Multi-line spans are not matched; see the
# note in `readable()`.
INLINE_CODE = re.compile(r"`+[^`\n]*`+")
# A list item marker — `-`, `*`, `+` or `1.` / `1)` — and everything up to the
# content after it. An index ENTRY is a link in a list item; see `entries()`.
LIST_ITEM = re.compile(r"^(\s*)(?:[-*+]|\d{1,9}[.)])\s+")


def _blank(s):
    """`s` with every character replaced by a space, newlines preserved."""
    return "".join("\n" if c == "\n" else " " for c in s)


def blank_fences(text):
    """Blank fenced code blocks, backtick- and tilde-delimited alike.

    An earlier revision toggled fence state on backticks only, so a valid
    `~~~markdown` example counted its links as real entries. The close must use
    the SAME character and be at least as long as the opener, which is what
    keeps a ``` inside a ~~~ block from ending it.
    """
    out = []
    fence = None
    for line in text.split("\n"):
        m = FENCE.match(line)
        if fence is None:
            # A backtick opener's info string may not itself contain a
            # backtick (CommonMark), which keeps an inline span from opening
            # a block.
            if m and not (m.group(1)[0] == "`" and "`" in m.group(2)):
                fence = (m.group(1)[0], len(m.group(1)))
                out.append(_blank(line))
                continue
            out.append(line)
        else:
            char, length = fence
            if (m and m.group(1)[0] == char and len(m.group(1)) >= length
                    and not m.group(2).strip()):
                fence = None
            out.append(_blank(line))
    return "\n".join(out)


def blank_indented_code(text):
    """Blank markdown's four-space indented code blocks.

    A block opens at an indent of four or more spaces after a blank line and
    runs until the indent drops, so it cannot interrupt a paragraph — which is
    what CommonMark says, and what keeps a wrapped line of prose readable.

    Indentation is RELATIVE TO THE ENCLOSING LIST ITEM, which is the whole
    subtlety. Under `- Example:` — content indent 2 — a code block starts at
    six spaces, not four, and four spaces there is a nested list. An earlier
    revision collapsed this to "no code blocks inside a list at all", which
    left a code block nested in a list counting as a real entry; the revision
    before that had no list awareness and blanked nested lists instead. Both
    directions are wrong and they fail differently: missing a code block lets
    an unlisted page pass, while blanking a nested list silently DELETES real
    entries. The second is the dangerous one, because it makes this gate
    quieter rather than louder, so the stack below is kept honest by
    self-tests pinning both.
    """
    out = []
    in_code = False
    # Content indents of the open list items, outermost first. `- x` pushes 2.
    lists = []
    prev_blank = True
    for line in text.split("\n"):
        if not line.strip():
            out.append(line)
            prev_blank = True
            continue
        indent = len(line) - len(line.lstrip(" "))
        if in_code:
            if indent >= (lists[-1] if lists else 0) + 4:
                out.append(_blank(line))
                prev_blank = False
                continue
            in_code = False
        # Dropping back to or past an enclosing item's content column closes
        # every list item indented deeper than this line.
        while lists and indent < lists[-1]:
            lists.pop()
        # The code-block test comes BEFORE the list-item test, because
        # `    - [B](b.md)` after a blank line and outside a list is a code
        # block that happens to contain a bullet, not a bullet that happens to
        # be indented. Testing for the list marker first made exactly that
        # example count as an entry.
        if indent >= (lists[-1] if lists else 0) + 4 and prev_blank:
            in_code = True
            out.append(_blank(line))
            prev_blank = False
            continue
        m = LIST_ITEM.match(line)
        if m:
            lists.append(len(m.group(0)))
        out.append(line)
        prev_blank = False
    return "\n".join(out)


def readable(text):
    """The part of a markdown document a reader can actually see and click.

    Everything blanked here is blanked SPACE FOR SPACE, so the line numbers in
    reported defects stay accurate.

    This function is the answer to a class of finding rather than to one
    instance of it. Review of this gate turned up three separate ways to
    satisfy the completeness check with a link no reader can follow — an entry
    inside an HTML comment, an entry inside a tilde fence, and a README whose
    only index link sat inside a fenced example. Each was a different hole in
    ad-hoc, per-caller extraction. One reduction, applied at every place links
    are extracted, closes all three and whatever the next spelling would have
    been:

      - fenced code, ``` or ~~~ — an example showing what an entry looks like
        is documentation about the index, not a row of it
      - HTML comments — parking an entry by commenting it out is an ordinary
        mid-edit move, and it must not keep an unlisted page green
      - inline code spans — `[A](a.md)` renders as literal text, not a link

    Fences are blanked first, so a comment delimiter inside a code sample
    cannot start a comment that swallows the rest of the file. Both remaining
    unterminated cases — a fence or a comment that never closes — blank to end
    of file, which makes entries below them vanish and the gate FAIL. That is
    the safe direction: malformed markup should make the gate loud, not blind.

    Known limit: an inline code span that wraps across lines is not blanked. A
    link inside one would still count. Multi-line spans do not occur in this
    corpus, and the conservative single-line pattern cannot run away on an
    unmatched backtick.
    """
    return blank_indented_code(
        INLINE_CODE.sub(
            lambda m: _blank(m.group(0)),
            COMMENT.sub(lambda m: _blank(m.group(0)), blank_fences(text)),
        )
    )


def tracked(root):
    out = subprocess.run(
        ["git", "ls-files", "-z", GUIDE],
        cwd=root, capture_output=True, text=True, check=True,
    ).stdout
    return sorted(p for p in out.split("\0") if p.endswith(".md"))


def index_plan(pages):
    """Map every index page under `docs/guide/` to the pages it must list.

    A page is owned by the NEAREST index above it: `tutorial/01-*.md` belongs
    to `tutorial/index.md`, and `tutorial/index.md` itself belongs to the
    top-level index, which is owned by nobody. Delegation is therefore not a
    hole — an unlisted tutorial chapter fails against its own sub-index — and
    it nests to any depth without this function knowing how deep the tree is.

    An earlier revision of this gate excluded a delegated subdirectory's pages
    from the required set and stopped there, never checking the sub-index that
    was supposed to have taken responsibility for them. A new tutorial chapter
    listed nowhere at all passed with zero defects, which is exactly the
    guarantee this gate exists to make. Caught in review on the PR that added
    it; the synthetic corpora below now pin both halves.
    """
    indexes = sorted(p for p in pages
                     if p == INDEX or p.endswith("/index.md"))
    plan = {i: set() for i in indexes}
    for p in pages:
        if p == INDEX:
            continue
        owner, owner_dir = None, None
        for idx in indexes:
            if idx == p:
                continue
            d = idx.rsplit("/", 1)[0] + "/"
            if p.startswith(d) and (owner_dir is None or len(d) > len(owner_dir)):
                owner, owner_dir = idx, d
        if owner is not None:
            plan[owner].add(p)
    return plan


def normalise(target, base):
    """Resolve a link target to a repo-relative guide path, or None.

    `base` is the directory of the file the link was written in ("" for a
    repo-root file such as README.md), so a sub-index's `01-project-setup.md`
    resolves against its own directory rather than against `docs/guide/`.

    A target that resolves outside `docs/guide/` returns None and is simply not
    an entry: an index is allowed to link docs.rs, and pointing that out is not
    this gate's job. A target INSIDE the guide is returned whether or not the
    page exists, so a link to a page that was deleted is reported as a defect
    rather than quietly ignored.
    """
    target = target.strip().rstrip("/")
    if not target or target.startswith(("http://", "https://", "mailto:")):
        return None
    if target.startswith("./"):
        target = target[2:]
    # Written from the repo root, or relative to `base`.
    path = target if target.startswith(GUIDE) else base + target
    parts = []
    for seg in path.split("/"):
        if seg == "..":
            if not parts:
                return None
            parts.pop()
        elif seg not in ("", "."):
            parts.append(seg)
    path = "/".join(parts)
    return path if path.startswith(GUIDE) else None


def entries(text, base):
    """Every index ENTRY in a file, with the `##` section it sits under.

    An entry is a link in a LIST ITEM. That is what separates the index's rows
    from its prose, and the distinction is load-bearing: `tutorial/index.md`
    carries four cross-references in paragraphs and blockquotes — "see the
    [i18n guide]", "if you have already read the [Getting Started guide]" —
    which are ordinary writing, not claims to index those pages. Counting them
    as entries made the gate report 165 links for 161 required pages, and would
    make the cross-index ownership rule below flag every one of them.

    Only links a reader can actually follow count; `readable()` says which
    those are, and blanking rather than skipping is what keeps the reported
    line numbers honest. A `## ` heading inside a fence does not open a
    section, for the same reason.
    """
    out = []
    section = None
    for lineno, line in enumerate(readable(text).split("\n"), 1):
        if line.startswith("## "):
            section = line[3:].strip()
            continue
        if not LIST_ITEM.match(line):
            continue
        for m in LINK.finditer(line):
            path = normalise(m.group(1), base)
            if path is None:
                continue
            out.append((path, lineno, section))
    return out


pages = tracked(root)

# A truth set that came back empty means `docs/guide/` was moved or renamed out
# from under this gate. Passing everything silently is the one outcome a drift
# gate must never have, so fail loudly instead.
if not pages:
    sys.exit(
        "FAIL: `git ls-files docs/guide` matched no markdown. The guide "
        "directory was moved or renamed; this gate has no truth set to read "
        "and would pass everything. Fix GUIDE in "
        "scripts/check-docs-guide-index.sh."
    )

page_set = set(pages)
plan = index_plan(pages)

if INDEX not in plan:
    print(f"corpus: {len(pages)} pages under {GUIDE}")
    print(f"defects: {len(pages)}")
    sys.exit(
        f"FAIL: {INDEX} does not exist, so none of the {len(pages)} guide "
        "pages is listed in a reader-facing index."
    )

defects = []
required_total = sum(len(n) for n in plan.values())

# Entries from EVERY index, gathered before anything is judged. Counting per
# index in isolation is what let a page be listed twice — once in the
# top-level index and once in the sub-index that owns it — while each file's
# own tally showed one. The duplicate is invisible from inside either file, so
# the check cannot live inside the per-file loop.
owner_of = {p: idx for idx, need in plan.items() for p in need}
seen = {}
for index_path in sorted(plan):
    base = index_path.rsplit("/", 1)[0] + "/"
    with open(f"{root}/{index_path}", encoding="utf-8") as fh:
        for path, lineno, section in entries(fh.read(), base):
            seen.setdefault(path, []).append((index_path, lineno, section))

linked_total = sum(len(h) for h in seen.values())

# 1. Every page is listed exactly once, across all indexes, by the index that
#    owns it.
for path, index_path in sorted(owner_of.items()):
    if path not in seen:
        defects.append((path, f"listed in no section of {index_path}"))
for path, hits in sorted(seen.items()):
    if len(hits) > 1:
        where = ", ".join(f"{i} line {n}" for i, n, _ in hits)
        defects.append((path, f"listed {len(hits)} times ({where})"))
    owner = owner_of.get(path)
    for index_path, lineno, _ in hits:
        if owner is not None and index_path != owner:
            defects.append(
                (path,
                 f"{index_path} line {lineno}: listed here, but {owner} owns "
                 "this page — an entry belongs to exactly one index")
            )

# 2. Every link resolves to a page that exists.
for path, hits in sorted(seen.items()):
    if path not in page_set:
        index_path, lineno, _ = hits[0]
        defects.append((path, f"{index_path} line {lineno}: no such guide page"))

# 3. Every entry sits under a `## ` heading.
for path, hits in sorted(seen.items()):
    for index_path, lineno, section in hits:
        if section is None:
            defects.append(
                (path,
                 f"{index_path} line {lineno}: not under any `## ` section "
                 "heading")
            )

# 4. The index is reachable from the landing page — by a LINK, not a mention.
#    Checking for the literal path as a substring passed on a plain-text or
#    inline-code mention, which is not clickable and does not get the reader
#    anywhere. Caught in review on the PR that added this gate.
with open(f"{root}/{README}", encoding="utf-8") as fh:
    readme = readable(fh.read())
if not any(normalise(m.group(1), "") == INDEX
           for m in LINK.finditer(readme)):
    defects.append(
        (README,
         f"has no markdown link whose target resolves to {INDEX}; a mention "
         "the reader cannot click — plain text, inline code, a fenced "
         "example, or a commented-out link — leaves the index unfindable")
    )

print(f"corpus: {len(pages)} pages under {GUIDE}")
print(f"indexes: {len(plan)} ({', '.join(sorted(plan))})")
print(f"entries: {required_total} required, {linked_total} linked")
print(f"defects: {len(defects)}")

if defects:
    print()
    for path, why in defects:
        print(f"  {path}: {why}")
    sys.exit(1)
PYEOF
}

# Synthetic corpora, each a throwaway git repo, so every rule above is proved
# to FAIL when violated rather than assumed to. A gate nobody has watched fail
# is a gate that might be passing for the wrong reason.
self_test() {
  local pass=0 total=0 tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  _case() {
    local name="$1" want="$2" dir="$tmp/$3"
    total=$((total + 1))
    local got=0
    run_check "$dir" >/dev/null 2>&1 || got=$?
    if [ "$got" -eq "$want" ]; then
      pass=$((pass + 1))
      echo "  ok   $name"
    else
      echo "  FAIL $name (wanted exit $want, got $got)"
    fi
  }

  _scaffold() {
    local dir="$tmp/$1"
    mkdir -p "$dir/docs/guide/tutorial"
    git -C "$dir" init -q
    git -C "$dir" config user.email t@t
    git -C "$dir" config user.name t
  }

  _commit() { git -C "$tmp/$1" add -A >/dev/null 2>&1; }

  # 1. A complete index passes.
  _scaffold ok
  printf '# A\n' > "$tmp/ok/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/ok/docs/guide/beta.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n' \
    > "$tmp/ok/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/ok/docs/guide/tutorial/01-x.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [B](beta.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/ok/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ok/README.md"
  _commit ok
  _case "complete index passes" 0 ok

  # 2. A page missing from the index fails.
  _scaffold missing
  printf '# A\n' > "$tmp/missing/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/missing/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' > "$tmp/missing/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/missing/README.md"
  _commit missing
  _case "unlisted page fails" 1 missing

  # 3. A page listed twice fails.
  _scaffold dup
  printf '# A\n' > "$tmp/dup/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n## T\n\n- [A again](alpha.md)\n' \
    > "$tmp/dup/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/dup/README.md"
  _commit dup
  _case "duplicate entry fails" 1 dup

  # 4. An entry above every `## ` heading fails.
  _scaffold nosection
  printf '# A\n' > "$tmp/nosection/docs/guide/alpha.md"
  printf '# Guide\n\n- [A](alpha.md)\n' > "$tmp/nosection/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/nosection/README.md"
  _commit nosection
  _case "entry outside a section fails" 1 nosection

  # 5. A link to a guide page that does not exist fails.
  _scaffold ghost
  printf '# A\n' > "$tmp/ghost/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [G](ghost.md)\n' \
    > "$tmp/ghost/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/ghost/README.md"
  _commit ghost
  _case "link to a missing guide page fails" 1 ghost

  # 6. A README that does not link the index fails — the index's own
  #    findability is the one thing the index cannot assert about itself.
  _scaffold unlinked
  printf '# A\n' > "$tmp/unlinked/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' > "$tmp/unlinked/docs/guide/index.md"
  printf 'no pointer here\n' > "$tmp/unlinked/README.md"
  _commit unlinked
  _case "README without the index link fails" 1 unlinked

  # 7. A missing index file fails rather than passing vacuously.
  _scaffold noindex
  printf '# A\n' > "$tmp/noindex/docs/guide/alpha.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/noindex/README.md"
  _commit noindex
  _case "absent index fails" 1 noindex

  # 8. A fenced example of an entry is not counted as one.
  _scaffold fence
  printf '# A\n' > "$tmp/fence/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/fence/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [B](beta.md)\n\n```\n- [A](alpha.md)\n```\n' \
    > "$tmp/fence/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/fence/README.md"
  _commit fence
  _case "fenced entry is not double-counted" 0 fence

  # 9. A page in a delegated subdirectory, listed in NEITHER index, fails.
  #    The first revision of this gate passed this corpus: it dropped the
  #    subdirectory's pages from the required set and never checked the
  #    sub-index that was meant to own them.
  _scaffold delegated
  printf '# A\n' > "$tmp/delegated/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n' \
    > "$tmp/delegated/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/delegated/docs/guide/tutorial/01-x.md"
  printf '# T2\n' > "$tmp/delegated/docs/guide/tutorial/02-unlisted.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/delegated/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/delegated/README.md"
  _commit delegated
  _case "unlisted page in a delegated subdir fails" 1 delegated

  # 10. The same corpus passes once the sub-index lists it — a sub-index link
  #     resolves against its OWN directory, not against docs/guide/.
  _scaffold delegated_ok
  printf '# A\n' > "$tmp/delegated_ok/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n2. [Two](02-listed.md)\n' \
    > "$tmp/delegated_ok/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/delegated_ok/docs/guide/tutorial/01-x.md"
  printf '# T2\n' > "$tmp/delegated_ok/docs/guide/tutorial/02-listed.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/delegated_ok/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/delegated_ok/README.md"
  _commit delegated_ok
  _case "delegated page listed in its sub-index passes" 0 delegated_ok

  # 11. A README that MENTIONS the index without linking it fails. The
  #     substring check this replaced passed on exactly this corpus.
  _scaffold mention
  printf '# A\n' > "$tmp/mention/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' > "$tmp/mention/docs/guide/index.md"
  printf 'The guide index lives at `docs/guide/index.md` somewhere.\n' \
    > "$tmp/mention/README.md"
  _commit mention
  _case "README mention without a link fails" 1 mention

  # 12. An entry commented out in HTML is not an entry — readers cannot see or
  #     follow it. Parking one this way used to keep an unlisted page green.
  _scaffold commented
  printf '# A\n' > "$tmp/commented/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/commented/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n<!-- - [B](beta.md) -->\n' \
    > "$tmp/commented/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/commented/README.md"
  _commit commented
  _case "commented-out entry does not count" 1 commented

  # 13. A multi-line comment blanks without shifting the line numbers the
  #     remaining defects are reported at.
  _scaffold multiline
  printf '# A\n' > "$tmp/multiline/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/multiline/docs/guide/beta.md"
  printf '# Guide\n\n<!--\nparked:\n- [B](beta.md)\n-->\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/multiline/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/multiline/README.md"
  _commit multiline
  _case "multi-line commented entry does not count" 1 multiline

  # 14. A tilde fence hides its links too. Fence state used to toggle on
  #     backticks only, so a valid `~~~markdown` example counted as entries.
  _scaffold tilde
  printf '# A\n' > "$tmp/tilde/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/tilde/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n~~~markdown\n- [B](beta.md)\n~~~\n' \
    > "$tmp/tilde/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/tilde/README.md"
  _commit tilde
  _case "tilde-fenced entry does not count" 1 tilde

  # 15. A backtick fence nested inside a tilde fence must not close it early.
  _scaffold nested_fence
  printf '# A\n' > "$tmp/nested_fence/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/nested_fence/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n~~~markdown\n```\n- [B](beta.md)\n```\n~~~\n' \
    > "$tmp/nested_fence/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/nested_fence/README.md"
  _commit nested_fence
  _case "backtick fence inside a tilde fence stays hidden" 1 nested_fence

  # 16. The README link check reads the same reduction: a fenced example is
  #     not a clickable link to the index.
  _scaffold readme_fence
  printf '# A\n' > "$tmp/readme_fence/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/readme_fence/docs/guide/index.md"
  printf 'Docs\n\n```markdown\n[Guide](docs/guide/index.md)\n```\n' \
    > "$tmp/readme_fence/README.md"
  _commit readme_fence
  _case "README link only inside a fence fails" 1 readme_fence

  # 17. An inline code span is not a link either.
  _scaffold inline
  printf '# A\n' > "$tmp/inline/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/inline/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- write it as `[B](beta.md)`\n' \
    > "$tmp/inline/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/inline/README.md"
  _commit inline
  _case "inline-code entry does not count" 1 inline

  # 18. A four-space indented code block hides its links too.
  _scaffold indented
  printf '# A\n' > "$tmp/indented/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/indented/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\nFor example:\n\n    - [B](beta.md)\n' \
    > "$tmp/indented/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/indented/README.md"
  _commit indented
  _case "indented-code entry does not count" 1 indented

  # 19. ...but four-space indentation UNDER A LIST ITEM is a nested list, not
  #     code. Over-blanking here would delete real entries, which is the one
  #     direction that makes this gate quieter instead of louder.
  _scaffold nested_list
  printf '# A\n' > "$tmp/nested_list/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/nested_list/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n    - [B](beta.md)\n' \
    > "$tmp/nested_list/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/nested_list/README.md"
  _commit nested_list
  _case "nested list item still counts as an entry" 0 nested_list

  # 20. A page listed in BOTH the top-level index and the sub-index that owns
  #     it. Each file's own tally shows one hit, so this is invisible from
  #     inside either of them.
  _scaffold cross_dup
  printf '# A\n' > "$tmp/cross_dup/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n' \
    > "$tmp/cross_dup/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/cross_dup/docs/guide/tutorial/01-x.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n- [One](tutorial/01-x.md)\n' \
    > "$tmp/cross_dup/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/cross_dup/README.md"
  _commit cross_dup
  _case "page listed in two indexes fails" 1 cross_dup

  # 21. A PROSE cross-reference is not an entry and must not trip rule 1. The
  #     real `tutorial/index.md` carries four of these; counting them would
  #     flag ordinary writing as a duplicate listing.
  _scaffold prose_xref
  printf '# A\n' > "$tmp/prose_xref/docs/guide/alpha.md"
  printf '# T\n\n## Chapters\n\n1. [One](01-x.md)\n\nSee the [A guide](../alpha.md) when you finish.\n' \
    > "$tmp/prose_xref/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/prose_xref/docs/guide/tutorial/01-x.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/prose_xref/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/prose_xref/README.md"
  _commit prose_xref
  _case "prose cross-reference is not an entry" 0 prose_xref

  # 22. A code block NESTED IN A LIST. Under `- Example:` (content indent 2) a
  #     block starts at six spaces. Treating "inside a list" as "no code blocks
  #     here" let this count as an entry.
  _scaffold list_code
  printf '# A\n' > "$tmp/list_code/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/list_code/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- Example:\n\n      - [B](beta.md)\n' \
    > "$tmp/list_code/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/list_code/README.md"
  _commit list_code
  _case "code block nested in a list does not count" 1 list_code

  # 23. ...and four spaces under that same item is a NESTED LIST, not code,
  #     because the block would start at six. This is the direction that
  #     silently deletes real entries, so it is pinned separately.
  _scaffold list_nested_deep
  printf '# A\n' > "$tmp/list_nested_deep/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/list_nested_deep/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n    - [B](beta.md)\n' \
    > "$tmp/list_nested_deep/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/list_nested_deep/README.md"
  _commit list_nested_deep
  _case "nested list after a blank line still counts" 0 list_nested_deep

  # 24. An image is not a navigable link, in an index...
  _scaffold image_entry
  printf '# A\n' > "$tmp/image_entry/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/image_entry/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- ![preview](beta.md)\n' \
    > "$tmp/image_entry/docs/guide/index.md"
  printf '[Guide index](docs/guide/index.md)\n' > "$tmp/image_entry/README.md"
  _commit image_entry
  _case "image entry does not count" 1 image_entry

  # 25. ...nor in README.md.
  _scaffold image_readme
  printf '# A\n' > "$tmp/image_readme/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' \
    > "$tmp/image_readme/docs/guide/index.md"
  printf '![Guide](docs/guide/index.md)\n' > "$tmp/image_readme/README.md"
  _commit image_readme
  _case "image in README is not an index link" 1 image_readme

  echo "self-test: $pass/$total passed"
  [ "$pass" -eq "$total" ]
}

root="$(cd "$(dirname "$0")/.." && pwd)"

case "${1-}" in
  --self-test)
    self_test
    ;;
  "")
    echo "Checking that every guide page is listed in the guide index..."
    if run_check "$root"; then
      echo "Guide index gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the guide index does not account for every page in docs/guide/.

Fix each one where it lives:
  - page listed nowhere   -> add one line to docs/guide/index.md, under the
                             `## ` section a reader with that question would
                             scan; write the line in the reader's words, not
                             the internal feature name
  - page listed twice     -> keep the entry under the section a reader would
                             look in first, and delete the other
  - link to nothing       -> the page moved or was deleted; point at the
                             current path, or drop the entry
  - entry above every `## ` heading -> move it under the section it belongs to
  - README link missing   -> restore the `docs/guide/index.md` link in
                             README.md's `## Documentation` list
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
