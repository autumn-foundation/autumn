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
# THE INVARIANT, in two halves:
#
#   1. `check-docs-cli.sh`, `check-docs-config.sh` and `check-docs-symbols.sh`
#      declare the SAME corpus. These three ask about three different things a
#      reader copies off one page; a page that is reader-facing for one of them
#      is reader-facing for all three. Their `INCLUDE_DIRS`, `INCLUDE_FILES` and
#      `INCLUDE_README_DIRS` must match exactly.
#
#   2. Every way `check-docs-routes.sh` differs from those three is DECLARED
#      below, in `DECLARED_DIFFERENCES`, with the reason.
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
# This gate does NOT read the corpus. It reads the four scripts' own scope
# declarations, so it fails at the moment a scope is edited rather than when a
# page that scope stopped covering finally rots.
#
# SCOPE CONSTANTS ARE PARSED, NOT IMPORTED: the gates are bash wrappers around
# embedded Python, so there is nothing to import. The three tuple literals are
# read out of each file's source with a parser that tolerates the inline
# comments `check-docs-routes.sh` interleaves between its entries (it is the one
# file that explains each addition in place). A file whose constants cannot be
# parsed is a FAILURE, not a skip: a gate that silently stops being checked is
# the defect this gate exists to catch, one level up.
#
# Run locally with:
#
#     scripts/check-docs-scope.sh              # gate the declarations
#     scripts/check-docs-scope.sh --self-test  # synthetic-source tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

read -r -d '' PYSRC <<'PYEOF' || true
import ast
import pathlib
import sys

# The three gates that must agree, and the one that must contain them.
SIBLINGS = (
    'scripts/check-docs-cli.sh',
    'scripts/check-docs-config.sh',
    'scripts/check-docs-symbols.sh',
)
SUPERSET = 'scripts/check-docs-routes.sh'
SELF = 'scripts/check-docs-scope.sh'

# The tuples that spell a corpus. `INCLUDE_README_DIRS` is absent from
# `check-docs-routes.sh` on purpose — it takes those directories WHOLE via
# `INCLUDE_DIRS`, which is a wider claim, not a missing one — so a gate that
# does not declare it reads as the empty tuple rather than as an error.
NAMES = ('INCLUDE_DIRS', 'INCLUDE_FILES', 'INCLUDE_README_DIRS')

# Every entry the routes gate has that its siblings do not, or vice versa, must
# appear here with the reason it does not propagate. Keyed by the path prefix or
# filename that differs; the value is why. An undeclared difference is a defect:
# see the note on half 2 in this file's header.
DECLARED_DIFFERENCES = {
    'examples/': (
        "the routes gate takes all of `examples/` while its siblings take only "
        "the `README.md` under it. `examples/wiki/content/` is embedded and "
        "SERVED, so a stale `/actuator/…` URL there is a page the running "
        "example shows a reader rather than one they might open. That argument "
        "is about URLs and does not carry to `autumn …` commands, `AUTUMN_*` "
        "variables or `autumn_web::…` paths, none of which those files carry."
    ),
}


def constants(path):
    """Read the scope tuples out of one gate's source.

    The assignment is found by name at the start of a line, and its right-hand
    side is handed to `ast.literal_eval` — which parses a tuple spanning several
    lines and interleaved with `#` comments exactly as Python would, so the
    parser never has to model the comment style of a file that explains each
    entry in place.
    """
    text = pathlib.Path(path).read_text(encoding='utf-8')
    out = {}
    for name in NAMES:
        # Anchored at a line start so a mention inside a comment or a docstring
        # is not mistaken for the declaration. The first line of a file is a
        # line start too: searching only for `\n` + name would miss a constant
        # declared at offset 0, which is how the self-test's synthetic gates are
        # written and, one day, how a real one might be.
        marker = name + ' = '
        start = 0 if text.startswith(marker) else text.find('\n' + marker)
        if start < 0:
            out[name] = ()
            continue
        # Walk forward from the `=` until the parenthesis that opened the tuple
        # closes. Counting depth rather than searching for `)` keeps a nested
        # parenthesis inside a comment from ending the literal early.
        i = text.index('(', start)
        depth, j = 0, i
        while j < len(text):
            if text[j] == '#':                       # a comment runs to the
                j = text.find('\n', j)               # end of its line and can
                if j < 0:                            # hold any parenthesis
                    break
            elif text[j] == '(':
                depth += 1
            elif text[j] == ')':
                depth -= 1
                if depth == 0:
                    break
            j += 1
        else:
            raise ValueError(f'{path}: {name} tuple never closes')
        try:
            value = ast.literal_eval(text[i:j + 1])
        except (SyntaxError, ValueError) as exc:
            raise ValueError(f'{path}: {name} is not a literal tuple: {exc}')
        out[name] = tuple(value) if isinstance(value, tuple) else (value,)
    if not out['INCLUDE_DIRS']:
        # An empty `INCLUDE_DIRS` means the parse found nothing, not that the
        # gate reads nothing. Refuse to pass on it.
        raise ValueError(
            f'{path}: parsed an empty INCLUDE_DIRS — the scope declaration '
            f'moved or changed shape, and this gate cannot tell agreement '
            f'from a parser failure. Fix the parser.')
    return out


# How wide a claim each tuple makes over the paths it names.
BREADTH = {'INCLUDE_DIRS': 'whole tree',
           'INCLUDE_FILES': 'this file',
           'INCLUDE_README_DIRS': 'README.md only'}


def flatten(scope):
    """One set of `(entry, breadth)` pairs across all three tuples.

    Breadth is part of the comparison, not erased by it: `examples/` under
    `INCLUDE_DIRS` and `examples/` under `INCLUDE_README_DIRS` name the same
    directory but claim different amounts of it, and that difference is
    precisely the kind worth declaring out loud. Erasing it would make the one
    difference this corpus actually has invisible, and leave the declaration
    mechanism untested against anything real.
    """
    return {(e, BREADTH[name]) for name in NAMES for e in scope[name]}


def main():
    try:
        scopes = {p: constants(p) for p in SIBLINGS + (SUPERSET,)}
    except (ValueError, OSError) as exc:
        print(f'ERROR: {exc}')
        return 1

    defects = []

    # Half 1: the three siblings are identical, compared against the first as
    # the reference so a divergence is reported once per file, not once per pair.
    reference = scopes[SIBLINGS[0]]
    for path in SIBLINGS[1:]:
        for name in NAMES:
            mine, theirs = scopes[path][name], reference[name]
            if set(mine) != set(theirs):
                only_mine = sorted(set(mine) - set(theirs))
                only_ref = sorted(set(theirs) - set(mine))
                if only_mine:
                    defects.append(
                        f'{path}: {name} has {only_mine} which '
                        f'{SIBLINGS[0]} does not')
                if only_ref:
                    defects.append(
                        f'{path}: {name} is missing {only_ref}, which '
                        f'{SIBLINGS[0]} has')

    # Half 2: every difference between the routes gate and the siblings is
    # declared. Both directions are checked — an entry the routes gate gained
    # alone is the drift this gate was built for, and one it lost alone would
    # leave a page gated for commands and ungated for URLs.
    wide = flatten(scopes[SUPERSET])
    narrow = flatten(reference)
    declared = set()
    for entry, breadth in sorted(wide ^ narrow):
        if entry in DECLARED_DIFFERENCES:
            declared.add(entry)
            continue
        if (entry, breadth) in wide:
            defects.append(
                f'{SUPERSET} gates {entry!r} ({breadth}) and {SIBLINGS[0]} '
                f'does not. Either add it to all three siblings, or record why '
                f'the argument for it does not carry to them, in '
                f'DECLARED_DIFFERENCES in {SELF}.')
        else:
            defects.append(
                f'{SIBLINGS[0]} gates {entry!r} ({breadth}) and {SUPERSET} '
                f'does not. Either add it there, or record why in '
                f'DECLARED_DIFFERENCES in {SELF}.')

    # A declaration that no longer describes a real difference is stale: the
    # difference was resolved and the note outlived it. Report it, so the table
    # cannot quietly accumulate reasons for differences that are gone.
    for entry in DECLARED_DIFFERENCES:
        if entry not in declared:
            defects.append(
                f'DECLARED_DIFFERENCES in {SELF} records {entry!r}, but the '
                f'gates no longer differ over it. Remove the entry.')

    print(f'gates compared: {len(SIBLINGS)} siblings + {SUPERSET}')
    print(f'scope entries per sibling: {len(narrow)}')
    print(f'declared differences: {len(declared)}/{len(DECLARED_DIFFERENCES)}')
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
    # The gate reads source files, so a synthetic test is a directory of
    # synthetic sources: four files carrying only the tuples that matter.
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    fails=0

    make_gates() {
      local dir="$1" claude_in_siblings="$2"
      mkdir -p "$dir/scripts"
      local extra=""
      [ "$claude_in_siblings" = "yes" ] && extra=", '.claude/skills/'"
      for g in cli config symbols; do
        cat > "$dir/scripts/check-docs-$g.sh" <<EOF
INCLUDE_DIRS = ('docs/guide/', 'skills/'$extra)
INCLUDE_FILES = ('README.md',)
INCLUDE_README_DIRS = ('examples/',)
EOF
      done
      cat > "$dir/scripts/check-docs-routes.sh" <<'EOF'
INCLUDE_DIRS = ("docs/guide/", "skills/",
                # a comment holding a stray ( parenthesis
                ".claude/skills/",
                "examples/")
INCLUDE_FILES = ("README.md",)
EOF
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

    c1="$tmp/c1"; make_gates "$c1" yes
    check "agreeing gates pass (comments and mixed quotes parsed)" pass "$c1"

    # The real defect: one sibling loses a directory the others keep.
    c2="$tmp/c2"; make_gates "$c2" yes
    sed -i.bak "s/, '.claude\/skills\/'//" "$c2/scripts/check-docs-config.sh"
    check "a sibling that drops a directory fails" fail "$c2"

    # The reverse: a sibling gains one the others lack.
    c3="$tmp/c3"; make_gates "$c3" no
    sed -i.bak "s/'skills\/')/'skills\/', 'docs\/perf\/')/" \
      "$c3/scripts/check-docs-cli.sh"
    check "a sibling that adds a directory alone fails" fail "$c3"

    # The declared-difference half, and the regression this gate exists for:
    # `.claude/skills/` in the routes gate ALONE. The three siblings still agree
    # with each other, which is exactly why a superset rule passed this state.
    c4="$tmp/c4"; make_gates "$c4" no
    check "a directory the routes gate gained alone fails" fail "$c4"

    # The reverse direction: routes narrower than its siblings.
    c5="$tmp/c5"; make_gates "$c5" yes
    sed -i.bak 's/"docs\/guide\/", //' "$c5/scripts/check-docs-routes.sh"
    check "a routes corpus narrower than its siblings fails" fail "$c5"

    # `examples/` differs in BREADTH — whole tree vs README only — and is the
    # one difference declared in this script, so it must not be reported.
    c8="$tmp/c8"; make_gates "$c8" yes
    check "the declared examples/ breadth difference is not a defect" pass "$c8"

    # A scope that cannot be parsed must fail, never silently pass.
    c6="$tmp/c6"; make_gates "$c6" yes
    cat > "$c6/scripts/check-docs-cli.sh" <<'EOF'
# the constant was renamed and this gate can no longer see it
CORPUS_DIRS = ('docs/guide/',)
EOF
    check "an unparseable scope fails rather than passing" fail "$c6"

    # `INCLUDE_README_DIRS` absent from routes is not a defect: it takes those
    # directories whole instead, which is the wider claim.
    c7="$tmp/c7"; make_gates "$c7" yes
    check "routes omitting INCLUDE_README_DIRS is not a defect" pass "$c7"

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
