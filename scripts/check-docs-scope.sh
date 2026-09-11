#!/usr/bin/env bash
# Corpus-scope agreement gate: the docs drift gates must not disagree about
# which pages are reader-facing.
#
# WHY THIS EXISTS: four gates each answer a different question about the same
# set of pages — `check-docs-cli.sh` (the `autumn …` commands a reader copies),
# `check-docs-config.sh` (the `AUTUMN_*` variables), `check-docs-symbols.sh`
# (the `autumn_web::…` paths) and `check-docs-routes.sh` (the `/actuator/…`
# URLs). Each spells its own corpus as a tuple of path prefixes, and three of
# them carry a comment saying the definitions "are kept identical on purpose,
# since a page covered by one gate and not the other is how a page ends up with
# no owner."
#
# Nothing enforced that. The definitions drifted:
#
#   `.claude/skills/` is a second skill tree — the agent machinery loads a
#   `SKILL.md` there by name, and `run-autumn` lives only there. It was added to
#   `check-docs-routes.sh` (with the argument that its SKILL.md "drives a real
#   server with `curl`, so its actuator paths are the most literally copy-and-run
#   text in the tree") and to `check-docs-orphans.sh`'s entry surfaces, and never
#   to the other three. `check-docs-toml.sh` and `check-docs-macro-args.sh` read
#   the whole tracked corpus, so they covered it all along. The result: five of
#   the eight docs gates treated the tree as reader-facing and three did not, so
#   that SKILL.md's `autumn seed --package`, `autumn routes --bin`,
#   `AUTUMN_SERVER__PORT` and `AUTUMN_DATABASE__URL` were ungated — while the
#   same file already carried `route-surface-allow` waivers for the gate that did
#   read it. A rename on either surface would have rotted there silently.
#
# The drift was invisible because it lives in four files that are never read
# side by side. This gate reads them side by side.
#
# Two more divergences turned up once this gate existed, and they are the reason
# it compares the file lists the gates REPORT rather than rules read out of their
# source. A corpus is widened in several places at once, so a checker that models
# some of them reports agreement over the rest:
#
#   - `check-docs-cli.sh` passed `*.md` to `git ls-files` where its three
#     siblings passed `*.md` AND `*.md.tmpl`, so
#     `autumn-cli/src/templates/README.md.tmpl` — the README `autumn new` writes
#     into every scaffolded project, carrying a reference table of `autumn dev`,
#     `autumn migrate`, `autumn doctor`, `autumn routes`, `autumn generate
#     scaffold` and `autumn release init` — sat outside the one gate that exists
#     to check `autumn …` commands. Identical tuples, different corpora.
#
#   - `check-docs-routes.sh` reads the `readme = "…"` page of every crate
#     manifest, because a crates.io landing page is reader-facing by PUBLICATION
#     rather than by where it sits in the tree. Its three siblings did not, so
#     the seven published plugin and subcrate READMEs were ungated for the
#     `autumn_web::…` paths and `AUTUMN_*` variables they carry — 18 symbol
#     occurrences and 3 variables, none of them drifted yet.
#
# Both are fixed in the same change as this gate: the siblings' corpus goes
# 200 -> 207, and all four gates stay green over it.
#
# THE INVARIANT, in two halves, over the set of files each gate actually reads:
#
#   1. `check-docs-cli.sh`, `check-docs-config.sh` and `check-docs-symbols.sh`
#      read the SAME PAGES. These three ask about three different things a
#      reader copies off one page; a page that is reader-facing for one of them
#      is reader-facing for all three.
#
#   2. Every page `check-docs-routes.sh` reads that those three do not, or the
#      reverse, is DECLARED below in `DECLARED_DIFFERENCES` — with the DIRECTION
#      the difference runs in, so a note cannot outlive the thing it describes
#      and waive its own opposite.
#
# Half 2 is the half that catches the drift described above, and it is why this
# gate does not simply require the routes corpus to be a superset. A superset
# rule permits exactly what happened: `.claude/skills/` was added to the wider
# gate alone, the three siblings still agreed with each other, and a
# superset check passes that state happily. The rule has to be that a
# difference is *written down*, not that a difference is allowed — because the
# useful moment is the one where someone widens one gate, and the question
# "does this argument carry to the other three?" is asked while they still have
# the answer in their head.
#
# So a new divergence fails until its author either propagates it or records why
# it does not propagate. Today exactly one difference is declared: the routes
# gate reads all of `examples/`, not just the `README.md` under it, because
# `examples/wiki/content/` is embedded and SERVED — a stale URL there is not a
# page a reader might open, it is a page the running example shows them. That
# argument is about URLs and does not carry to commands or config keys, so the
# difference is deliberate and stays.
#
# This gate reads no page's CONTENT. It runs each of the four gates with the
# `--corpus` mode this change adds to them, which prints that gate's own
# resolved corpus one path per line, and compares the lists. It therefore fails
# at the moment a scope is edited rather than when a page that scope stopped
# covering finally rots.
#
# THE GATES ARE ASKED, NOT MODELLED. The first version of this script re-derived
# each corpus by parsing the gate's source, and was wrong twice over in exactly
# the way a model is always eventually wrong: it compared the scope tuples and
# reported agreement while the `ls-files` globs differed, and once the globs
# were modelled too it still had no idea the routes gate reads crate manifests.
# Every rule a corpus is built from would have had to be mirrored here and kept
# mirrored — a second implementation with its own drift, gating the first. So a
# gate answers for itself.
#
# A gate whose `--corpus` fails, or prints nothing, is a FAILURE and never a
# skip: an empty corpus compares equal to another empty corpus, and a gate that
# silently stops being checked is the defect this gate exists to catch, one
# level up.
#
# Run locally with:
#
#     scripts/check-docs-scope.sh              # gate the corpora
#     scripts/check-docs-scope.sh --self-test  # synthetic-gate tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

read -r -d '' PYSRC <<'PYEOF' || true
import subprocess
import sys

# The three gates that must agree, and the one that must contain them.
SIBLINGS = (
    'scripts/check-docs-cli.sh',
    'scripts/check-docs-config.sh',
    'scripts/check-docs-symbols.sh',
)
SUPERSET = 'scripts/check-docs-routes.sh'
SELF = 'scripts/check-docs-scope.sh'

# Every page the routes gate reads that its siblings do not, or vice versa, must
# appear here with the DIRECTION the difference runs in, the CLAIM each side
# makes over that prefix, and the reason the difference does not propagate. An
# undeclared difference is a defect: see the note on half 2 in this file's
# header.
#
# `side` and the claims are what keep a declaration from outliving the thing it
# describes — twice now, a looser key let this note waive something it was never
# written about:
#
#   - Keyed by path alone, it waived its own OPPOSITE. Removing `examples/` from
#     the routes gate makes it narrower than its siblings, a real loss of
#     coverage and the reverse of what is written here, and the lookup still
#     matched. Hence `side`.
#
#   - Keyed by path and side, it waived any ONE file under the prefix and
#     counted itself used. The routes gate could stop reading
#     `examples/wiki/content/` — the served pages this note exists for — and
#     gain some unrelated `examples/todo/NOTES.md` instead, and the declaration
#     would sail through on the replacement while the coverage it claims was
#     gone. Hence the claims, which are CHECKED against the tracked tree rather
#     than trusted: a declaration has to keep being true, not merely keep
#     matching something.
CLAIMS = {
    'every page': lambda under: under,
    'the READMEs': lambda under: {f for f in under
                                  if f.rsplit('/', 1)[-1] == 'README.md'},
}

DECLARED_DIFFERENCES = (
    {
        'prefix': 'examples/',
        'side': 'routes-only',
        'routes': 'every page',
        'siblings': 'the READMEs',
        'why': (
            "the routes gate takes all of `examples/` while its siblings take "
            "only the `README.md` under it. `examples/wiki/content/` is "
            "embedded and SERVED, so a stale `/actuator/…` URL there is a page "
            "the running example shows a reader rather than one they might "
            "open. That argument is about URLs and does not carry to `autumn "
            "…` commands, `AUTUMN_*` variables or `autumn_web::…` paths, none "
            "of which those files carry."
        ),
    },
)


def tracked_markdown():
    """Every markdown-ish file in the tree, as the four gates' globs see it."""
    out = subprocess.run(
        ['git', 'ls-files', '-z', '*.md', '*.md.tmpl'],
        capture_output=True, text=True, check=True).stdout
    return {f for f in out.split('\0') if f}


def corpus(path):
    """Ask one gate for the pages it reads.

    Each gate answers `--corpus` by running its own `corpus()` and printing the
    result, so this comparison is over what the gates ACTUALLY read. The first
    version of this script re-derived each corpus from the gate's source
    instead, and that was wrong in the way a model is always eventually wrong:
    a corpus is widened in several places at once, and the model knew about
    some of them. It compared the three scope tuples and reported agreement
    while `check-docs-cli.sh` was passing a narrower `git ls-files` glob than
    its siblings; once the globs were modelled too, it still missed the crate
    `readme = "…"` manifests that `check-docs-routes.sh` reads. Asking cannot
    drift from the answer.
    """
    out = subprocess.run(['bash', path, '--corpus'],
                         capture_output=True, text=True)
    if out.returncode != 0:
        raise ValueError(
            f'{path} --corpus exited {out.returncode}: '
            f'{out.stderr.strip()[:200]}')
    files = {line for line in out.stdout.splitlines() if line.strip()}
    if not files:
        # An empty corpus means the mode broke, not that the gate reads
        # nothing. Refuse to pass on it: a gate that silently stops being
        # checked is the defect this gate exists to catch, one level up.
        raise ValueError(
            f'{path} --corpus printed nothing — the mode moved or changed '
            f'shape, and this gate cannot tell agreement from a broken call. '
            f'Fix it rather than letting an empty corpus compare equal.')
    return files


def main():
    try:
        corpora = {p: corpus(p) for p in SIBLINGS + (SUPERSET,)}
        tracked = tracked_markdown()
    except (ValueError, OSError, subprocess.CalledProcessError) as exc:
        print(f'ERROR: {exc}')
        return 1

    defects = []

    # Half 1: the three siblings read the same pages, compared against the first
    # as the reference so a divergence is reported once per file, not per pair.
    reference = corpora[SIBLINGS[0]]
    for path in SIBLINGS[1:]:
        only_mine = sorted(corpora[path] - reference)
        only_ref = sorted(reference - corpora[path])
        if only_mine:
            defects.append(f'{path} reads {only_mine} and {SIBLINGS[0]} '
                           f'does not.')
        if only_ref:
            defects.append(f'{SIBLINGS[0]} reads {only_ref} and {path} '
                           f'does not.')

    # Half 2: every difference between the routes gate and the siblings is
    # declared — with the direction it runs in, and with each side's claim over
    # that prefix verified against the tracked tree. A declaration is only
    # allowed to waive a difference while the thing it says is still true.
    wide, narrow = corpora[SUPERSET], reference
    under_prefix = {}
    verified = set()
    for d in DECLARED_DIFFERENCES:
        under = {f for f in tracked if f.startswith(d['prefix'])}
        under_prefix[d['prefix']] = under
        broken = False
        for label, seen, claim in (('routes', wide & under, d['routes']),
                                   ('the siblings', narrow & under, d['siblings'])):
            want = CLAIMS[claim](under)
            if seen == want:
                continue
            broken = True
            missing, extra = sorted(want - seen), sorted(seen - want)
            defects.append(
                f'DECLARED_DIFFERENCES in {SELF} says {label} read '
                f'{claim} under {d["prefix"]!r}, but that is no longer true'
                + (f' — not read: {missing}' if missing else '')
                + (f' — read anyway: {extra}' if extra else '')
                + '. Fix the gate, or rewrite the declaration to what is now '
                  'the case.')
        if not broken:
            verified.add(d['prefix'])

    used = set()
    for f in sorted(wide ^ narrow):
        side = 'routes-only' if f in wide else 'siblings-only'
        match = next((d for d in DECLARED_DIFFERENCES
                      if f.startswith(d['prefix']) and d['side'] == side), None)
        if match:
            # A declaration whose claim just failed has already been reported,
            # precisely. Stay quiet about the individual files under it rather
            # than burying that message in a list of consequences.
            used.add(match['prefix'])
            continue
        if side == 'routes-only':
            defects.append(
                f'{SUPERSET} reads {f!r} and {SIBLINGS[0]} does not. Either '
                f'add it to all three siblings, or record why the argument for '
                f'it does not carry to them, in DECLARED_DIFFERENCES in '
                f'{SELF}.')
        else:
            defects.append(
                f'{SIBLINGS[0]} reads {f!r} and {SUPERSET} does not. Either '
                f'add it there, or record why in DECLARED_DIFFERENCES in '
                f'{SELF}.')

    # A declaration that no longer describes a real difference is stale: the
    # difference was resolved and the note outlived it. Report it, so the table
    # cannot quietly accumulate reasons for differences that are gone.
    for d in DECLARED_DIFFERENCES:
        if d['prefix'] not in used:
            defects.append(
                f'DECLARED_DIFFERENCES in {SELF} records {d["prefix"]!r} as '
                f'{d["side"]}, but the gates no longer differ that way over '
                f'it. Remove the entry.')

    print(f'gates compared: {len(SIBLINGS)} siblings + {SUPERSET}')
    print(f'pages in the sibling corpus: {len(reference)}')
    print(f'declared differences: {len(verified)} verified, '
          f'{len(used)} matched, of {len(DECLARED_DIFFERENCES)}')
    print(f'defects: {len(defects)}')
    if defects:
        print()
        for d in defects:
            print(f'  {d}')
        print()
        print('The docs drift gates disagree about which pages are reader-')
        print('facing. A page covered by one gate and not another has no')
        print('owner: fix the scope that drifted, in every file that spells it.')
        return 1
    print('Corpus-scope agreement gate OK.')
    return 0


sys.exit(main())
PYEOF

case "${1-}" in
  --self-test)
    # The gate asks each gate for its corpus and checks each declaration's claim
    # against the tracked tree, so a synthetic test is a small git repo plus
    # four stub gates: scripts that answer `--corpus` with a page list. Stubbing
    # the ANSWER rather than the rules is the point — this script no longer
    # cares how a corpus is built, only that the four agree on it.
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    fails=0

    PAGES='docs/guide/a.md
skills/s.md
README.md
examples/x/README.md
examples/wiki/content/p.md
examples/todo/NOTES.md'

    # $1 dir, $2 newline-separated sibling corpus, $3 the same for routes.
    make_gates() {
      local dir="$1" sib="$2" rts="$3"
      mkdir -p "$dir/scripts"
      local f
      while IFS= read -r f; do
        [ -n "$f" ] || continue
        mkdir -p "$dir/$(dirname "$f")"
        echo page > "$dir/$f"
      done <<< "$PAGES"
      git -C "$dir" init -q
      git -C "$dir" add -A

      for g in cli config symbols; do
        { echo '#!/usr/bin/env bash'; echo "cat <<'CORPUS'"; echo "$sib"
          echo 'CORPUS'; } > "$dir/scripts/check-docs-$g.sh"
      done
      { echo '#!/usr/bin/env bash'; echo "cat <<'CORPUS'"; echo "$rts"
        echo 'CORPUS'; } > "$dir/scripts/check-docs-routes.sh"
    }

    check() {
      local label="$1" want="$2" dir="$3"
      local got=0
      (cd "$dir" && python3 -c "$PYSRC") >/dev/null 2>&1 || got=$?
      if { [ "$want" = pass ] && [ "$got" -eq 0 ]; } \
        || { [ "$want" = fail ] && [ "$got" -ne 0 ]; }; then
        echo "  ok   $label"
      else
        echo "  FAIL $label (wanted $want, exit $got)"
        fails=$((fails + 1))
      fi
    }

    # The clean shape: the siblings read the READMEs under `examples/`, the
    # routes gate reads every page under it, which is exactly what the one
    # declaration claims.
    BASE='docs/guide/a.md
skills/s.md
README.md'
    SIB_OK="$BASE
examples/x/README.md"
    RTS_OK="$SIB_OK
examples/wiki/content/p.md
examples/todo/NOTES.md"

    c1="$tmp/c1"; make_gates "$c1" "$SIB_OK" "$RTS_OK"
    check "four gates agreeing, with the declared difference, pass" pass "$c1"

    c2="$tmp/c2"; make_gates "$c2" "$SIB_OK" "$RTS_OK"
    echo 'echo docs/guide/extra.md' >> "$c2/scripts/check-docs-config.sh"
    check "a sibling reading a page the others do not fails" fail "$c2"

    c3="$tmp/c3"; make_gates "$c3" "$SIB_OK" "$RTS_OK"
    printf '#!/usr/bin/env bash\necho docs/guide/a.md\n' \
      > "$c3/scripts/check-docs-cli.sh"
    check "a sibling missing pages the others read fails" fail "$c3"

    # The regression this gate exists for: the routes gate gains a page alone,
    # outside any declared prefix, and the three siblings still agree with each
    # other — which is exactly why a superset rule passed this state.
    c4="$tmp/c4"; make_gates "$c4" "$SIB_OK" "$RTS_OK"
    echo 'echo .claude/skills/run/SKILL.md' \
      >> "$c4/scripts/check-docs-routes.sh"
    check "a page the routes gate gained alone fails" fail "$c4"

    # The reverse: the siblings read a page the routes gate does not.
    c5="$tmp/c5"; make_gates "$c5" "$SIB_OK
autumn-search/README.md" "$RTS_OK"
    check "a page the siblings gained alone fails" fail "$c5"

    # The declaration must not waive its own OPPOSITE: `examples/` moves to the
    # siblings, so the routes gate is the narrower one — a real loss of
    # coverage, and the reverse of what is declared.
    c6="$tmp/c6"; make_gates "$c6" "$RTS_OK" "$SIB_OK"
    check "the declared difference does not waive its opposite" fail "$c6"

    # Nor may it survive on a REPLACEMENT. The routes gate stops reading the
    # served wiki pages the note exists for and keeps an unrelated same-
    # direction file under the same prefix. Matching prefix and side alone,
    # the declaration sailed through on the substitute.
    c7="$tmp/c7"; make_gates "$c7" "$SIB_OK" "$SIB_OK
examples/todo/NOTES.md"
    check "the declared difference does not survive on a replacement" fail "$c7"

    # A declaration with nothing left to describe is stale and reported.
    c8="$tmp/c8"; make_gates "$c8" "$RTS_OK" "$RTS_OK"
    check "a declaration describing no live difference fails" fail "$c8"

    # A gate whose --corpus fails must fail the gate, never be skipped.
    c9="$tmp/c9"; make_gates "$c9" "$SIB_OK" "$RTS_OK"
    printf '#!/usr/bin/env bash\nexit 3\n' > "$c9/scripts/check-docs-symbols.sh"
    check "a gate whose --corpus exits non-zero fails" fail "$c9"

    # An EMPTY corpus compares equal to another empty one, so it must fail
    # rather than pass quietly.
    c10="$tmp/c10"; make_gates "$c10" "$SIB_OK" "$RTS_OK"
    printf '#!/usr/bin/env bash\nexit 0\n' > "$c10/scripts/check-docs-cli.sh"
    check "a gate whose --corpus prints nothing fails" fail "$c10"

    echo ""
    if [ "$fails" -eq 0 ]; then
      echo "Self-test OK."
    else
      echo "Self-test FAILED: $fails"
      exit 1
    fi
    ;;
  "")
    echo "Checking that the docs drift gates agree on the reader-facing corpus..."
    python3 -c "$PYSRC"
    ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
