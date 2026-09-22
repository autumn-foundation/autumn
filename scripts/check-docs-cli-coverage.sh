#!/usr/bin/env bash
# CLI coverage gate: every command `autumn-cli` ships must be written down
# somewhere a reader can find it.
#
# WHY THIS EXISTS: the corpus carries twelve docs gates and every one of them
# runs in the same direction — docs -> code. `check-docs-cli.sh` asks whether
# the commands the pages name still exist, `check-docs-symbols.sh` whether the
# `autumn_web::…` paths still resolve, `check-docs-config.sh` whether the
# `AUTUMN_*` variables are still read, `check-docs-links.sh` whether the links
# still land. All twelve ask "is what we wrote still true?", which is DRIFT.
# None asks "is what we shipped written down anywhere?", which is COVERAGE.
#
# That asymmetry has a specific consequence: a command can ship, work, carry
# good `--help` text and good rustdoc, and be documented NOWHERE, and the whole
# gate tree stays green. It is invisible by construction — a gate that only
# reads the docs can never notice a command the docs never mention. The reader
# concludes the feature does not exist and goes and builds it themselves, which
# is the same outcome as an unreachable page (`check-docs-orphans.sh`) reached
# by a different route.
#
# The defect that prompted this ran exactly that way. All four `autumn token`
# subcommands shipped — `issue` since 0.5.x, `list` / `rotate` since 0.6.0 —
# and searching all 160 guide pages for "revoke api token" returned ZERO
# results. A reader holding a leaked credential had no path from the page that
# raised the question to the command that answers it. The prose half of that
# was fixed in #2821; the gate to hold the line was deliberately deferred there
# ("a correct one has to reuse `check-docs-cli.sh`'s `resolve()` rather than
# re-implement it, and that is its own change"). This is that change.
#
# WHAT IT CHECKS: every command path in the surface is NAMED by at least one
# reader-facing page, unless it is exempt under a rule below.
#
# IT REUSES THE SIBLING GATE RATHER THAN RESPELLING IT. Surface, corpus and
# "which command does this line name?" all come from `check-docs-cli.sh`, via
# its `--list`, `--corpus`, `--resolved` and `--list-hidden` modes. This is not
# tidiness. A coverage checker that matches command paths against page text
# with its own regex gets the shallow cases right and the deep ones wrong, and
# every one of those wrong answers is a question `resolve()` already answers
# off the clap derive input with 866 self-tests behind it:
#
#   - `autumn openapi export`, documented in openapi.md, must NOT satisfy the
#     top-level `export` — a different command (an offline diagnostic
#     snapshot). A substring or suffix match says it does, and the top-level
#     `export` then reads as documented while appearing on no page at all.
#     That exact false negative was caught in review on the first attempt at
#     this gate.
#   - `autumn db pull posts` names `db pull`, not a subcommand `posts`.
#   - `autumn c` is a declared alias of `console`, and satisfies it.
#   - `autumn migrate --with-maintenance down` names `migrate down`; a matcher
#     that stops at the first `-` records only `migrate`.
#
# Asking the sibling cannot drift from its answer; modelling it can, which is
# the lesson `check-docs-scope.sh` already exists to enforce over the corpus
# definitions.
#
# ALIASES COLLAPSE ONTO THE CANONICAL COMMAND FIRST. `#[command(visible_alias
# = "c")]` makes `autumn c` another way to TYPE `autumn console`, not another
# command, and the sibling's surface lists both spellings as paths. Comparing
# spellings therefore demands that BOTH be documented independently, so a page
# that documents `autumn console` properly leaves `autumn c` looking
# undocumented — and the gate's advice would be to write the alias into the
# docs, which is the opposite of what a reader needs. Both sides are mapped
# through `--list-aliases` before anything is compared, segment by segment, so
# an alias at any level of a path collapses.
#
# This was latent rather than live when the gate landed: the corpus happens to
# write both `autumn c` and `autumn console`, which masked it. Rewriting the
# two `autumn c` lines to the canonical spelling reproduced it exactly —
# `defects: 1, autumn c`.
#
# WHAT COUNTS AS COVERAGE. A command path is covered when some reader-facing
# page names it or names a DESCENDANT of it. A descendant covers its ancestors
# because a page writing `autumn token issue` has by definition written
# `autumn token` — the reader sees the group on the way past. The reverse does
# NOT hold: `autumn token` on a page says nothing about where `rotate` is
# documented, and treating a group as covering its children is how the four
# `token` subcommands stayed invisible while `token` itself looked fine.
#
# THE THREE EXEMPTIONS, each verified rather than trusted:
#
#   1. HIDDEN. `#[command(hide = true)]` keeps a command runnable but out of
#      `--help` — the author saying it is not a reader's to find. Read out of
#      the derive input (`--list-hidden`), so hiding a command exempts it with
#      no edit to this file, and un-hiding one re-gates it the same way. Today:
#      `serve run-service`, which `install-service` registers as the Windows
#      service command line and which does nothing useful run by hand.
#
#   2. THE `destroy` FAMILY RULE. `generators.md` states the reversal over the
#      whole family — "`autumn destroy <thing> <the same arguments>` reverses a
#      matching `generate`" — so a documented `generate X` documents
#      `destroy X` too, and repeating thirteen subcommands would be the
#      duplication that makes two copies drift apart.
#
#      The exemption is CONDITIONAL on both halves, and both are checked:
#      the rule sentence must still be on the page (delete it and all thirteen
#      report), AND `generate X` must itself be covered. That second half is
#      what keeps this from being a blanket waiver on the word `destroy`: today
#      `destroy inbound-mail` and `destroy policy` are NOT exempt, because
#      `generate inbound-mail` and `generate policy` are undocumented too, and
#      a rule that says "the same as generate" covers nothing when generate is
#      covered nowhere.
#
#   3. THE TRIAGED BACKLOG. The commands undocumented on the day this gate
#      landed, each carrying a reason. A backlog is how a gate lands on a
#      corpus that does not yet pass it; it is not a place to put new work. A
#      newly shipped undocumented command is NOT on the list and fails.
#
#      The list is exact in both directions. An entry earns its place only by
#      being the ONLY thing keeping its command out of the defect list, and it
#      fails the moment that stops being true — otherwise the list rots into a
#      set of waivers nobody can tell from live ones, which is how a baseline
#      file stops meaning anything.
#
#      "Documented" is not the only way it stops being true, and checking only
#      that was wrong. An entry is equally spent once ANOTHER exemption
#      accounts for its command, and leaving it there then defeats that other
#      exemption's own conditionality. Worked example, reproduced before it was
#      fixed: `destroy inbound-mail` is backlogged because `generate
#      inbound-mail` is undocumented. Document `generate inbound-mail`, and the
#      family rule takes over — the backlog entry is now dead weight, and
#      nothing said so. Delete the reversal sentence from `generators.md`
#      afterwards and the other eleven `destroy` subcommands re-gate while
#      `destroy inbound-mail` alone stays silently waived, by a line whose
#      stated reason ("stranded by `generate inbound-mail`") is no longer even
#      true.
#
#      So staleness is decided by asking what the gate would do WITHOUT the
#      backlog: whatever does not then land in `defects` is accounted for by
#      something else, whether that is documentation, `hide = true`, or the
#      family rule. That question cannot drift from the exemption rules,
#      because it is those rules, run again.
#
# WHAT IT DELIBERATELY DOES NOT CHECK: whether the page that names a command
# explains it WELL, or whether a reader searching their own words would land
# there. Those are real and they are not mechanically decidable; this gate
# answers the one question that is — is it written down at all? — and leaves
# the rest to the retrieval test.
#
# USAGE:
#   scripts/check-docs-cli-coverage.sh              # gate the corpus
#   scripts/check-docs-cli-coverage.sh --list       # print the coverage matrix
#   scripts/check-docs-cli-coverage.sh --self-test  # synthetic tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
sibling="$root/scripts/check-docs-cli.sh"

if [ ! -x "$sibling" ]; then
  echo "ERROR: $sibling is missing or not executable — this gate reads its" >&2
  echo "surface, corpus and resolved invocations from it and cannot run alone." >&2
  exit 2
fi

run_py() {
  python3 - "$@" <<'PYEOF'
import re, subprocess, sys, pathlib

MODE = sys.argv[1]
ROOT = pathlib.Path(sys.argv[2])
SIBLING = str(ROOT / 'scripts' / 'check-docs-cli.sh')

# ------------------------------------------------------------- the backlog

# Command paths undocumented when this gate landed. Each carries the reason it
# is not being fixed in the same change as the gate, so the list can be worked
# down by someone who did not write it.
#
# Adding to this list is not how a new command gets shipped. It is how the
# corpus that existed before the gate is allowed to keep passing while the
# backlog is worked down.
BACKLOG = {
    'assets add':
        'the `autumn assets` family (pin/vendor/integrity-verify JS '
        'dependencies) is named once corpus-wide, in a parenthetical in a '
        'table cell in upgrading.md, and none of its four subcommands appear '
        'anywhere. Needs a guide page of its own — coverage, not findability.',
    'assets list': 'see `assets add`.',
    'assets update': 'see `assets add`.',
    'assets verify': 'see `assets add`.',
    'export':
        'top-level `autumn export` — an offline diagnostic snapshot of the '
        'app — appears on NO reader-facing page. The likeliest reader is '
        'someone assembling a support ticket, which is the worst moment to '
        'find nothing. Distinct from the documented `autumn openapi export` '
        'and `autumn data export`, which is why a text matcher reported it '
        'covered and `resolve()` does not.',
    'generate inbound-mail':
        'generators.md documents the generator family but omits this one; it '
        'also strands `destroy inbound-mail`, since the reversal rule covers '
        'a subcommand only once its `generate` half is documented.',
    'destroy inbound-mail': 'stranded by `generate inbound-mail`; see above.',
    'schema parse':
        '`autumn schema` is marked experimental in its own doc comment '
        '("Slices 2-3 ship `parse` and `snapshot`; `diff`/… arrive in later '
        'slices"). Documenting a surface that is still moving mints a page '
        'that has to be kept true through the rest of the slices; revisit '
        'when the subcommand set settles.',
}

# The sentence in generators.md that states the reversal over the whole
# `destroy` family. The exemption in rule 2 is only as good as this rule being
# on the page, so the page is read rather than trusted.
DESTROY_RULE = re.compile(
    r'`autumn destroy <thing> <the same arguments>`\s*reverses\s*a\s*matching',
    re.S)
DESTROY_RULE_PAGE = 'docs/guide/generators.md'


def sibling(*args):
    out = subprocess.run([SIBLING, *args], capture_output=True, text=True)
    if out.returncode != 0:
        print(f'ERROR: {SIBLING} {" ".join(args)} exited {out.returncode}',
              file=sys.stderr)
        print(out.stderr, file=sys.stderr)
        sys.exit(2)
    return out.stdout


def _parse_list(text):
    """Command paths out of `check-docs-cli.sh --list`.

    `--list` prints one path per line, then a BLANK LINE, then a one-line
    human summary. Split on that blank line rather than filtering the
    summary's wording out of the path list: a command PATH is space-joined
    segments, so a future `autumn command paths` is a perfectly constructible
    path that a `'command paths' not in line` filter would drop. It would
    then leave the gate entirely — out of the denominator AND out of
    classification — free to be undocumented without ever failing this
    check, which is the exact defect this gate exists to prevent, arriving
    through its own parser.

    Structure cannot collide with content, and a parser that cannot find the
    structure it expects raises rather than guessing: a gate that cannot
    compute its own question is a gate that fails.
    """
    head, sep, tail = text.partition('\n\n')
    if not sep:
        raise ValueError(
            'check-docs-cli.sh --list did not print the expected '
            '"<paths>, blank line, summary" shape. Its output format moved, '
            'and guessing which lines are paths is how a real command goes '
            'missing from this gate silently.')
    summary = tail.strip()
    if not re.match(r'^\d+ top-level commands, \d+ command paths,', summary):
        raise ValueError(
            f'check-docs-cli.sh --list printed an unrecognised summary line: '
            f'{summary!r}. Refusing to guess where the paths end.')
    return [l.strip() for l in head.splitlines() if l.strip()]


def surface():
    try:
        return _parse_list(sibling('--list'))
    except ValueError as exc:
        print(f'ERROR: {exc}', file=sys.stderr)
        sys.exit(2)


def aliases():
    """`{alias path: canonical path}` for every non-canonical spelling."""
    out = {}
    for line in sibling('--list-aliases').splitlines():
        if not line.strip():
            continue
        alias, canon = line.split('\t')
        out[alias] = canon
    return out


def documented():
    """The set of command paths the reader-facing corpus names."""
    paths = set()
    for line in sibling('--resolved').splitlines():
        if not line.strip():
            continue
        _f, _lineno, path = line.split('\t')
        paths.add(path)
    return paths


def covered(path, docd, alias_map=None):
    """Named outright, or named by a descendant (which writes the ancestor).

    Both sides are canonicalised first: an alias is another spelling of the
    same command, so documenting either spelling documents the command.
    """
    alias_map = alias_map or {}
    path = alias_map.get(path, path)
    return any(r == path or r.startswith(path + ' ')
               for r in (alias_map.get(d, d) for d in docd))


def destroy_rule_stated():
    page = ROOT / DESTROY_RULE_PAGE
    if not page.exists():
        return False
    return bool(DESTROY_RULE.search(page.read_text(errors='replace')))


def classify(paths, docd, hidden, rule_stated, backlog, alias_map=None):
    """Split the surface into covered / exempt / defect, with the reason.

    An alias spelling is never judged on its own: it is the same command as
    its canonical path, which is judged once under that name.
    """
    alias_map = alias_map or {}
    uncovered = [p for p in paths
                 if p not in alias_map and not covered(p, docd, alias_map)]
    exempt, defects = {}, []
    for p in uncovered:
        if p in hidden:
            exempt[p] = 'hidden (`#[command(hide = true)]`)'
            continue
        if rule_stated and p.startswith('destroy '):
            counterpart = 'generate ' + p[len('destroy '):]
            if covered(counterpart, docd, alias_map):
                exempt[p] = f'covered by the family rule: `{counterpart}` is documented'
                continue
        if p in backlog:
            exempt[p] = 'triaged backlog'
            continue
        defects.append(p)
    return uncovered, exempt, defects


def main():
    paths = surface()
    if not paths:
        print('ERROR: the sibling gate reported an empty command surface — '
              'the clap derive input moved or changed shape. Fix that gate '
              'first; this one cannot tell a coverage gap from a parser '
              'failure.', file=sys.stderr)
        return 2

    docd = documented()
    alias_map = aliases()
    hidden = set(l.strip() for l in sibling('--list-hidden').splitlines() if l.strip())
    rule_stated = destroy_rule_stated()
    corpus_size = len([l for l in sibling('--corpus').splitlines() if l.strip()])

    uncovered, exempt, defects = classify(paths, docd, hidden, rule_stated,
                                          BACKLOG, alias_map)
    # An alias is not a command to document, so it is not counted as one on
    # either side of the ratio.
    commands = [p for p in paths if p not in alias_map]
    covered_n = len(commands) - len(uncovered)

    if MODE == '--list':
        for p in paths:
            if p in alias_map:
                mark = f'alias of `{alias_map[p]}`'
            elif covered(p, docd, alias_map):
                mark = 'documented'
            else:
                mark = exempt.get(p, 'DEFECT')
            print(f'{p}\t{mark}')
        return 0

    print(f'corpus: {corpus_size} reader-facing markdown files')
    print(f'surface: {len(commands)} command paths parsed from autumn-cli/src'
          + (f' ({len(alias_map)} alias spelling'
             f'{"" if len(alias_map) == 1 else "s"} folded onto the canonical '
             f'command)' if alias_map else ''))
    print(f'documented: {covered_n}/{len(commands)} '
          f'({covered_n * 100 // len(commands)}%)')
    by_reason = {}
    for p, why in exempt.items():
        key = ('hidden' if why.startswith('hidden')
               else 'destroy family rule' if 'family rule' in why
               else 'triaged backlog')
        by_reason[key] = by_reason.get(key, 0) + 1
    if by_reason:
        print('exempt: ' + ', '.join(f'{n} {k}' for k, n in sorted(by_reason.items())))
    if not rule_stated:
        print(f'note: {DESTROY_RULE_PAGE} no longer states the `destroy` '
              f'reversal rule, so the family exemption has lapsed.')

    # A backlog entry that is no longer the thing holding its command back
    # has to leave the list, or the list stops distinguishing live waivers
    # from finished work. Answered by re-running the exemption rules with no
    # backlog at all: anything that does not land in `defects` that way is
    # already accounted for by documentation, `hide = true`, or the family
    # rule, and its entry is spent.
    unknown = sorted(p for p in BACKLOG if p not in paths)
    _u, without_backlog_exempt, without_backlog = classify(
        paths, docd, hidden, rule_stated, {}, alias_map)
    still_needed = set(without_backlog)
    stale = sorted(p for p in BACKLOG
                   if p not in unknown and p not in still_needed)

    print(f'defects: {len(defects)}'
          + (f' ({len(stale)} stale backlog entr'
             f'{"y" if len(stale) == 1 else "ies"})' if stale else '')
          + (f' ({len(unknown)} backlog entr'
             f'{"y" if len(unknown) == 1 else "ies"} not in the surface)'
             if unknown else ''))

    if defects:
        print()
        for p in sorted(defects):
            print(f'  autumn {p}')
        print()
        print('Each command above ships and is named on no reader-facing '
              'page, so nothing a reader can search will tell them it exists. '
              'Every existing docs gate stays green on this, because all of '
              'them read the docs and none of them read the CLI.')
        print()
        print('Document it on the page where the question arises — the page a '
              'reader is already on when they need it — rather than on a new '
              'page of its own. A command named nowhere is a COVERAGE defect; '
              'a command named on a page nobody reaches is a findability one, '
              'and a new page fixes the first and worsens the second.')
        print()
        print('If it is not a reader\'s to find, mark it in the derive input '
              'and this gate follows:')
        print('    #[command(hide = true)]')

    if stale:
        print()
        for p in stale:
            # The reason comes from the run WITHOUT the backlog, because in
            # the live classification the backlog won the tie and recorded
            # itself as the reason.
            why = ('now documented' if covered(p, docd, alias_map)
                   else without_backlog_exempt.get(
                       p, 'now covered by another exemption'))
            print(f'  autumn {p} — {why}; remove it from BACKLOG in '
                  f'scripts/check-docs-cli-coverage.sh')
        print()
        print('A backlog entry outliving the gap it describes turns the list '
              'into waivers nobody can audit — and a spent entry keeps '
              'waiving its command after the exemption that superseded it '
              'goes away. Delete the entry in the same change that accounts '
              'for the command.')

    if unknown:
        print()
        for p in unknown:
            print(f'  autumn {p} — in BACKLOG but not in the surface; the '
                  f'command was renamed or removed, so the entry is dead')

    if defects or stale or unknown:
        return 1
    print('CLI coverage gate OK.')
    return 0


# ------------------------------------------------------------------ tests

def self_test():
    passed = failed = 0

    def expect(cond, msg):
        nonlocal passed, failed
        if cond:
            passed += 1
        else:
            failed += 1
            print(f'FAIL: {msg}')

    docd = {'token issue', 'openapi export', 'db pull', 'generate model',
            'console'}

    # A descendant writes its ancestors; an ancestor says nothing about its
    # children. This is the asymmetry that hid the `token` subcommands.
    expect(covered('token', docd), 'a descendant covers its ancestor')
    expect(covered('token issue', docd), 'an exact match covers')
    expect(not covered('token rotate', docd),
           'an ancestor must NOT cover its children')

    # The review finding from the first attempt at this gate: a longer command
    # must not satisfy a shorter one as a suffix.
    expect(not covered('export', docd),
           '`openapi export` must not satisfy the top-level `export`')

    # A prefix that is not a path SEGMENT is not coverage.
    expect(not covered('db pu', docd), 'a partial segment is not coverage')
    expect(not covered('cons', docd), 'a partial top-level name is not coverage')

    # An alias is another way to TYPE a command, not a command of its own.
    # Documenting either spelling documents the one command, and the alias
    # spelling is never judged or counted on its own. Regression test for the
    # review finding on this gate: with the corpus writing only `autumn
    # console`, comparing spellings reported `autumn c` undocumented.
    amap = {'c': 'console', 'c seed': 'console seed'}
    expect(covered('c', {'console'}, amap),
           'the canonical spelling covers its alias')
    expect(covered('console', {'c'}, amap),
           'the alias spelling covers the canonical command')
    expect(covered('c seed', {'console seed'}, amap),
           'an alias collapses at every level of the path')
    expect(not covered('console', {'consoleee'}, amap),
           'canonicalising must not make unrelated spellings match')

    _u, _e, alias_defects = classify(['console', 'c'], {'console'}, set(),
                                     rule_stated=True, backlog={},
                                     alias_map=amap)
    expect(not alias_defects,
           'an alias of a documented command is not a defect')
    _u, _e, alias_defects2 = classify(['console', 'c'], set(), set(),
                                      rule_stated=True, backlog={},
                                      alias_map=amap)
    expect(alias_defects2 == ['console'],
           'an undocumented command is reported ONCE, under its canonical name')

    paths = ['token issue', 'token rotate', 'export', 'destroy model',
             'destroy policy', 'generate model', 'serve run-service']
    hidden = {'serve run-service'}

    _u, exempt, defects = classify(paths, docd, hidden, rule_stated=True,
                                   backlog={})
    expect('serve run-service' in exempt and exempt['serve run-service'].startswith('hidden'),
           'a hidden command is exempt off the derive input')
    expect('destroy model' in exempt and 'family rule' in exempt['destroy model'],
           '`destroy X` is exempt when `generate X` is documented')
    expect('destroy policy' in defects,
           '`destroy X` is NOT exempt when `generate X` is undocumented too')
    expect('token rotate' in defects and 'export' in defects,
           'an undocumented command is a defect')

    # The family exemption is conditional on the rule still being on the page.
    _u, exempt2, defects2 = classify(paths, docd, hidden, rule_stated=False,
                                     backlog={})
    expect('destroy model' in defects2,
           'deleting the reversal rule from generators.md re-gates the family')

    # A command on the backlog is exempt; one that is not is a defect. Checked
    # through `classify` with a stand-in so the real BACKLOG can change freely.
    _u, exempt3, defects3 = classify(paths, docd, hidden, rule_stated=True,
                                     backlog={'token rotate': 'stand-in'})
    expect(exempt3.get('token rotate') == 'triaged backlog',
           'a backlogged command is exempt')
    expect('export' in defects3,
           'a command NOT on the backlog still fails')

    # A backlog entry is spent once ANYTHING else accounts for its command,
    # not only once it is documented. Regression test for the second review
    # finding on this gate: leaving a superseded entry in place defeats the
    # conditionality of the exemption that superseded it.
    def spent(paths_, docd_, hidden_, rule_, backlog_, amap=None):
        _u, _e, without = classify(paths_, docd_, hidden_, rule_, {}, amap)
        return sorted(p for p in backlog_
                      if p in paths_ and p not in set(without))

    bl = {'destroy inbound-mail': 'x', 'generate inbound-mail': 'x',
          'token rotate': 'x'}
    ps = ['destroy inbound-mail', 'generate inbound-mail', 'token rotate']

    expect(spent(ps, set(), set(), True, bl) == [],
           'an entry whose command nothing else accounts for is NOT stale')
    expect(spent(ps, {'generate inbound-mail'}, set(), True, bl)
           == ['destroy inbound-mail', 'generate inbound-mail'],
           'documenting `generate X` spends BOTH its own entry and `destroy X`')
    expect(spent(ps, set(), {'token rotate'}, True, bl) == ['token rotate'],
           'hiding a command spends its backlog entry')
    expect(spent(ps, {'generate inbound-mail'}, set(), False, bl)
           == ['generate inbound-mail'],
           'with the family rule gone, `destroy X` needs its entry again')

    # The sequence that made this a defect rather than untidiness: once the
    # superseded entry is gone, removing the family rule must re-gate the
    # command instead of leaving it silently waived.
    _u, _e, after = classify(ps, {'generate inbound-mail'}, set(),
                             rule_stated=False,
                             backlog={'token rotate': 'x'})
    expect('destroy inbound-mail' in after,
           'a command re-gates once its spent entry is removed')

    # The summary line is found by STRUCTURE, not by its wording. Regression
    # test for the third review finding on this gate: a command path can
    # legitimately contain the summary's text, and filtering on that text
    # would drop the command out of the gate altogether.
    listing = ('console\ncommand\ncommand paths\nwebhook sim\n'
               '\n4 top-level commands, 4 command paths, 9 option spellings\n')
    expect(_parse_list(listing)
           == ['console', 'command', 'command paths', 'webhook sim'],
           'a command path spelled like the summary survives parsing')
    expect('4 top-level commands, 4 command paths, 9 option spellings'
           not in _parse_list(listing),
           'the summary line itself is not parsed as a command path')
    for bad, why in (('console\nwebhook\n', 'no blank-line separator'),
                     ('console\n\nsomething else entirely\n',
                      'an unrecognised summary line')):
        try:
            _parse_list(bad)
            expect(False, f'{why} must raise rather than guess')
        except ValueError:
            expect(True, why)

    # The real backlog must be honest: every entry names a real command path,
    # and none of them is already documented.
    real = surface()
    if real:
        real_docd = documented()
        real_aliases = aliases()
        expect(all(p in real for p in BACKLOG),
               'every BACKLOG entry names a real command path')
        expect(not any(p in real_aliases for p in BACKLOG),
               'no BACKLOG entry is an alias spelling rather than a command')
        real_hidden = set(l.strip() for l
                          in sibling('--list-hidden').splitlines() if l.strip())
        expect(not spent(real, real_docd, real_hidden, destroy_rule_stated(),
                         BACKLOG, real_aliases),
               'no BACKLOG entry is already accounted for by something else')

    print(f'self-test: {passed} passed, {failed} failed')
    return 1 if failed else 0


sys.exit(self_test() if MODE == '--self-test' else main())
PYEOF
}

mode="${1:-}"
case "$mode" in
  --self-test) run_py --self-test "$root" ;;
  --list)      run_py --list "$root" ;;
  "")          echo "Checking that every shipped CLI command is documented somewhere..."
               run_py --check "$root" ;;
  *)           echo "usage: $0 [--list|--self-test]" >&2; exit 2 ;;
esac
