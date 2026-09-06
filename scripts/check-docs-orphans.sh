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
#     - ROOTS are the surfaces a reader enters through: the root `README.md`,
#       `EXAMPLES.md`, `CONTRIBUTING.md`, `STABILITY.md`, `docs/plugins.md`,
#       every `skills/` and `agents/` page, and each `examples/*/README.md`.
#     - EDGES run from a root or another guide page to a guide page it names,
#       either as a markdown link or as a bare repo path.
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
# bare `docs/guide/x.md` count, including inside fenced blocks, and a guide page
# may link a sibling by relative filename (`](jobs.md)`, `](./jobs.md)`,
# `](tutorial/03-forms.md)`, `](../guide/jobs.md)`). A gate that argues about
# link syntax becomes a tax on writing docs; this one only ever asks whether
# some findable path exists.
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
import os, posixpath, re, subprocess, sys

root = sys.argv[1]

GUIDE = 'docs/guide/'
ROOT_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md',
              'docs/plugins.md')
ROOT_DIRS = ('skills/', 'agents/')
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


def is_root(f):
    if f in HISTORY or f.startswith(HISTORY):
        return False
    return (f in ROOT_FILES
            or (f.startswith(ROOT_DIRS) and f.endswith('.md'))
            or (f.startswith('examples/') and posixpath.basename(f) == 'README.md'))


roots = [f for f in tracked if is_root(f)]
node_set = set(nodes)


def read(f):
    try:
        with open(posixpath.join(root, f), encoding='utf-8', errors='ignore') as fh:
            return fh.read()
    except OSError:
        return ''


# `](target)` — the target stops at whitespace, `#` (anchor) or `)`.
MD_LINK = re.compile(r'\]\(\s*<?([^)\s<>#]+\.md)')
# A reference definition: `[mail]: mail.md`, optionally `<…>`-wrapped. Markdown
# allows up to three leading spaces. Reference-style links are a syntax
# check-docs-links.sh already parses and self-tests, so a page linked only that
# way is genuinely reachable; without this the gate would report it as an
# orphan and block a docs change written in a spelling the corpus supports.
REF_DEF = re.compile(r'^ {0,3}\[([^\]]+)\]:\s*(?:<([^<>]*)>|(\S+))', re.M)
# ...but a definition only becomes a link the reader can click when some label
# USES it. A leftover `[old]: mail.md` with no `[…][old]` anywhere renders as
# nothing at all, so counting it as an edge would let an obsolete line launder a
# genuinely orphaned page past this gate — the failure direction that matters,
# since it is the one the gate exists to catch. Collect the labels actually
# used, in all three CommonMark spellings, and resolve only those definitions.
#   full:      [text][label]
#   collapsed: [label][]
#   shortcut:  [label]        (not followed by `(`, `[` or `:`)
REF_USE_FULL = re.compile(r'\[[^\]]*\]\[([^\]]*)\]')
REF_USE_SHORTCUT = re.compile(r'\[([^\]]+)\](?![\(\[:])')


def ref_label(s):
    """CommonMark label matching: case-insensitive, internal whitespace collapsed."""
    return ' '.join(s.split()).lower()
# A bare repo path. The leading guard keeps `…/docs/guide/x.md` inside a longer
# path from matching at the wrong offset.
BARE = re.compile(r'(?<![\w/.-])(docs/guide/[A-Za-z0-9._/-]+\.md)')


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


def edges_from(f):
    """Guide pages this file gives a reader a way to reach."""
    txt = read(f)
    out = set()
    for m in BARE.finditer(txt):
        t = normalize(m.group(1))
        if t in node_set:
            out.add(t)
    base = posixpath.dirname(f)

    def add_relative(raw):
        raw = raw.split('#', 1)[0].strip()
        if not raw.endswith('.md'):
            return
        # An absolute-looking `/docs/guide/x.md` is a site path, not a file path.
        t = normalize(raw.lstrip('/') if raw.startswith('/') else posixpath.join(base, raw))
        if t in node_set:
            out.add(t)

    for m in MD_LINK.finditer(txt):
        add_relative(m.group(1))

    used = set()
    for m in REF_USE_FULL.finditer(txt):
        # `[label][]` (collapsed) leaves group 1 empty; the label is the text.
        inner = m.group(1)
        used.add(ref_label(inner) if inner.strip()
                 else ref_label(m.group(0)[1:m.group(0).index(']')]))
    for m in REF_USE_SHORTCUT.finditer(txt):
        used.add(ref_label(m.group(1)))
    for m in REF_DEF.finditer(txt):
        if ref_label(m.group(1)) not in used:
            continue
        add_relative(m.group(2) if m.group(2) is not None else m.group(3))
    return out


# Breadth-first from every root. Only roots and guide pages carry edges: that
# restriction is what keeps a security report or a `///` citation from standing
# in for a route a reader can walk.
seen = set()
frontier = list(roots)
while frontier:
    cur = frontier.pop()
    for nxt in edges_from(cur):
        if nxt not in seen:
            seen.add(nxt)
            frontier.append(nxt)

WAIVER = re.compile(r'<!--\s*orphan-allow:\s*(.+?)\s*-->', re.S)

defects, waived = [], 0
for n in sorted(node_set - seen):
    m = WAIVER.search(read(n))
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
