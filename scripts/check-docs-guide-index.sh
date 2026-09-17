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
#   1. Every tracked page under `docs/guide/` is listed in
#      `docs/guide/index.md` — exactly once. Twice is a defect too: a reader
#      who meets the same page under two headings cannot tell whether they are
#      the same page, and the second entry is the one that rots.
#   2. Every guide page the index links exists, and is a guide page. A link to
#      a page that moved is caught by `check-docs-links.sh` as a 404; this
#      catches the index pointing somewhere outside the corpus it indexes.
#   3. Every entry sits under a `## ` section heading, so a page appended to
#      the end of the file lands somewhere a reader is actually scanning.
#   4. `README.md` links `docs/guide/index.md`. An index nobody can reach from
#      the landing page is the very defect this gate exists to prevent, and it
#      would otherwise be the one page the gate could not see.
#
# DELEGATION TO A SUB-INDEX. A subdirectory of `docs/guide/` that carries its
# own `index.md` — `tutorial/` does — is represented in the top-level index by
# that `index.md` alone. The tutorial is 12 ordered chapters; listing them
# individually in a task-shaped index would spray twelve near-identical entries
# across it and tell a reader nothing about which one to open first. The
# sub-index is listed, and it owns its own ordering.
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
LINK = re.compile(r"\[[^\]]*\]\(\s*([^)\s#]+)")


def tracked(root):
    out = subprocess.run(
        ["git", "ls-files", "-z", GUIDE],
        cwd=root, capture_output=True, text=True, check=True,
    ).stdout
    return sorted(p for p in out.split("\0") if p.endswith(".md"))


def required(pages):
    """The pages the top-level index must list.

    Everything tracked under `docs/guide/`, minus the index itself, minus the
    pages of any subdirectory that carries its own `index.md` — that
    subdirectory is represented by its index, which IS required. See the
    DELEGATION note in this file's header.
    """
    sub_indexes = {p for p in pages
                   if p.endswith("/index.md") and p != INDEX}
    delegated = {p.rsplit("/", 1)[0] + "/" for p in sub_indexes}
    out = set()
    for p in pages:
        if p == INDEX:
            continue
        if any(p.startswith(d) for d in delegated) and p not in sub_indexes:
            continue
        out.add(p)
    return out


def normalise(target):
    """Resolve an index link target to a repo-relative guide path, or None.

    A target that resolves outside `docs/guide/` returns None and is simply not
    an entry: the index is allowed to link docs.rs, and pointing that out is
    not this gate's job. A target INSIDE the guide is returned whether or not
    the page exists, so a link to a page that was deleted is reported as a
    defect rather than quietly ignored.
    """
    target = target.strip().rstrip("/")
    if not target or target.startswith(("http://", "https://", "mailto:")):
        return None
    if target.startswith("./"):
        target = target[2:]
    # Written from the repo root.
    if target.startswith(GUIDE):
        return target
    # Written relative to the index, which lives in `docs/guide/`. A target
    # climbing out with `../` leaves the guide, so it is not an entry.
    if not target.startswith(".."):
        return GUIDE + target
    return None


def entries(text):
    """Every guide-page link in the index, with the `##` section it sits under.

    Links inside fenced code are not entries: a fence showing what an entry
    looks like is documentation about the index, not a row of it.
    """
    out = []
    section = None
    in_fence = False
    for lineno, line in enumerate(text.split("\n"), 1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        if line.startswith("## "):
            section = line[3:].strip()
            continue
        for m in LINK.finditer(line):
            path = normalise(m.group(1))
            if path is None or not path.startswith(GUIDE):
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
need = required(pages)

try:
    with open(f"{root}/{INDEX}", encoding="utf-8") as fh:
        index_text = fh.read()
except FileNotFoundError:
    print(f"corpus: {len(pages)} pages under {GUIDE}")
    print(f"defects: {len(need)}")
    sys.exit(
        f"FAIL: {INDEX} does not exist, so none of the {len(need)} guide "
        "pages is listed in a reader-facing index."
    )

found = entries(index_text)

defects = []

seen = {}
for path, lineno, section in found:
    seen.setdefault(path, []).append((lineno, section))

# 1. Listed, and exactly once.
for path in sorted(need - set(seen)):
    defects.append((path, "listed in no section of the index"))
for path, hits in sorted(seen.items()):
    if len(hits) > 1:
        where = ", ".join(f"line {n}" for n, _ in hits)
        defects.append((path, f"listed {len(hits)} times ({where})"))

# 2. Every link resolves to a page that exists.
for path, hits in sorted(seen.items()):
    if path not in page_set:
        defects.append((path, f"line {hits[0][0]}: no such guide page"))

# 3. Every entry sits under a `## ` heading.
for path, hits in sorted(seen.items()):
    for lineno, section in hits:
        if section is None:
            defects.append(
                (path, f"line {lineno}: not under any `## ` section heading")
            )

# 4. The index is reachable from the landing page.
with open(f"{root}/{README}", encoding="utf-8") as fh:
    readme = fh.read()
if INDEX not in readme:
    defects.append(
        (README, f"does not link {INDEX}; the index itself is unfindable")
    )

print(f"corpus: {len(pages)} pages under {GUIDE}")
print(f"index:  {len(need)} required entries, {len(seen)} linked")
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
  printf '# T\n' > "$tmp/ok/docs/guide/tutorial/index.md"
  printf '# T1\n' > "$tmp/ok/docs/guide/tutorial/01-x.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [B](beta.md)\n- [T](tutorial/index.md)\n' \
    > "$tmp/ok/docs/guide/index.md"
  printf 'see docs/guide/index.md\n' > "$tmp/ok/README.md"
  _commit ok
  _case "complete index passes" 0 ok

  # 2. A page missing from the index fails.
  _scaffold missing
  printf '# A\n' > "$tmp/missing/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/missing/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n' > "$tmp/missing/docs/guide/index.md"
  printf 'see docs/guide/index.md\n' > "$tmp/missing/README.md"
  _commit missing
  _case "unlisted page fails" 1 missing

  # 3. A page listed twice fails.
  _scaffold dup
  printf '# A\n' > "$tmp/dup/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n\n## T\n\n- [A again](alpha.md)\n' \
    > "$tmp/dup/docs/guide/index.md"
  printf 'see docs/guide/index.md\n' > "$tmp/dup/README.md"
  _commit dup
  _case "duplicate entry fails" 1 dup

  # 4. An entry above every `## ` heading fails.
  _scaffold nosection
  printf '# A\n' > "$tmp/nosection/docs/guide/alpha.md"
  printf '# Guide\n\n- [A](alpha.md)\n' > "$tmp/nosection/docs/guide/index.md"
  printf 'see docs/guide/index.md\n' > "$tmp/nosection/README.md"
  _commit nosection
  _case "entry outside a section fails" 1 nosection

  # 5. A link to a guide page that does not exist fails.
  _scaffold ghost
  printf '# A\n' > "$tmp/ghost/docs/guide/alpha.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [G](ghost.md)\n' \
    > "$tmp/ghost/docs/guide/index.md"
  printf 'see docs/guide/index.md\n' > "$tmp/ghost/README.md"
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
  printf 'see docs/guide/index.md\n' > "$tmp/noindex/README.md"
  _commit noindex
  _case "absent index fails" 1 noindex

  # 8. A fenced example of an entry is not counted as one.
  _scaffold fence
  printf '# A\n' > "$tmp/fence/docs/guide/alpha.md"
  printf '# B\n' > "$tmp/fence/docs/guide/beta.md"
  printf '# Guide\n\n## S\n\n- [A](alpha.md)\n- [B](beta.md)\n\n```\n- [A](alpha.md)\n```\n' \
    > "$tmp/fence/docs/guide/index.md"
  printf 'see docs/guide/index.md\n' > "$tmp/fence/README.md"
  _commit fence
  _case "fenced entry is not double-counted" 0 fence

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
