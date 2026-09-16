#!/usr/bin/env bash
# CLI coverage gate: every `autumn …` command a reader can RUN must be findable
# somewhere in the reader-facing docs.
#
# WHY THIS EXISTS: the corpus has eleven docs gates and every one of them runs
# in the same direction — `check-docs-cli.sh` asks whether the commands the docs
# NAME still exist, `check-docs-config.sh` the same for `AUTUMN_*` variables,
# `check-docs-symbols.sh` for `autumn_web::…` paths, `check-docs-routes.sh` for
# `/actuator/…` URLs. All four answer "is what we wrote still true?", which is
# drift. None answers the other half — "is what we SHIPPED written down
# anywhere?" — so a command could be added to the CLI and documented nowhere,
# and every gate in the tree would stay green. That is a coverage defect, and it
# is invisible by construction: a gate that only reads the docs can never notice
# a command the docs never mention.
#
# The first run of this direction found 25 of 195 command paths (12.8%) absent
# from all 212 reader-facing pages. Most were benign and are classified below;
# the one that mattered was the `autumn token` family. `issue` reached readers
# only through the agent skill tree, and `list` / `rotate` / `revoke` reached
# them nowhere at all — so a reader whose API token had leaked could search all
# 160 guide pages for "revoke api token" and get zero results, while
# `autumn token revoke` had shipped since 0.5.x. The answer existed, in clap's
# `--help` and in rustdoc, and no page a reader browses pointed at it.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   The command surface `check-docs-cli.sh` already parses, minus the commands a
#   reader cannot run, diffed against the corpus `check-docs-cli.sh` already
#   reads. A command path is a defect when no reader-facing page mentions it and
#   no rule below accounts for it. Both halves are borrowed on purpose: a
#   coverage gate that spelled its own surface or its own corpus would drift
#   from the drift gate, and `check-docs-scope.sh` exists because that already
#   happened once between four gates that each spelled their own.
#
# THREE THINGS ARE NOT DEFECTS, and the distinction is the whole gate:
#
#   1. HIDDEN COMMANDS. `serve run-service` is `#[command(hide = true)]` — the
#      entrypoint a service manager invokes, not something a reader types. It
#      does not appear in `--help`, so documenting it would describe a surface
#      the reader cannot discover. Hidden commands are read out of the clap
#      derive input rather than listed here, so hiding a command exempts it and
#      un-hiding one puts it back under the gate, with no edit to this file.
#
#   2. COMMANDS COVERED BY A GENERIC RULE. `docs/guide/generators.md` documents
#      `autumn destroy` as a rule over its argument — "`autumn destroy <thing>
#      <the same arguments>` reverses a matching `generate`" — and the CLI backs
#      that rule exactly: all 21 `generate` subcommands have an inverse and
#      there are no extras either way. A reader who ran `autumn generate job`
#      reads the rule and correctly infers `autumn destroy job`. Enumerating 21
#      near-identical sections would add pages that answer no question the rule
#      leaves open, and every one of them would be a page to keep true forever.
#      A RULE is declared as a prefix plus the page that states it, and the gate
#      VERIFIES the page still states it — a rule silently deleted from the page
#      stops exempting its commands, rather than exempting them forever on the
#      strength of a comment in this file.
#
#   3. TRIAGED BACKLOG. Genuinely undocumented commands that are not this
#      change's to write. They are listed one per line with a reason, so the
#      count is a number someone can work down rather than a silence. Adding a
#      NEW command without documenting it fails the gate; it does not get to
#      join the backlog by default.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - Whether the mention is any GOOD. A command named once in a table is
#     "covered" here. This gate answers whether a reader can find the command at
#     all, which is the question that was going unasked; whether the page then
#     serves them is a judgment no grep settles.
#   - Option spellings. `check-docs-cli.sh` parses 963 of them, and requiring
#     every flag to appear in prose would mint exactly the comprehensiveness
#     this corpus avoids. Commands are the unit a reader searches for.
#   - Anything outside the CLI. The same blind spot exists for config keys and
#     `autumn_web::…` symbols; this gate takes the surface where the reverse
#     direction is cheapest to read and most obviously reader-facing.
#
# USAGE:
#   scripts/check-docs-cli-coverage.sh              # gate the corpus
#   scripts/check-docs-cli-coverage.sh --list       # print the coverage matrix
#   scripts/check-docs-cli-coverage.sh --self-test  # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

run_py() {
  python3 - "$@" <<'PYEOF'
import os, re, subprocess, sys, pathlib

MODE = sys.argv[1]
ROOT = pathlib.Path(sys.argv[2])

SIBLING = ROOT / 'scripts' / 'check-docs-cli.sh'

# --------------------------------------------------------------- the surface

def sibling(flag):
    """Read the surface/corpus from the drift gate, so the two cannot disagree."""
    out = subprocess.run(['bash', str(SIBLING), flag], cwd=ROOT,
                         capture_output=True, text=True)
    if out.returncode != 0:
        print(f"check-docs-cli.sh {flag} failed:\n{out.stderr}", file=sys.stderr)
        sys.exit(2)
    return [l.rstrip('\n') for l in out.stdout.splitlines() if l.strip()]


# `--list` ends with a summary line ("53 top-level commands, 194 command
# paths, …"). It is prose, not a command path; drop it by shape rather than by
# position so a reworded summary cannot become a phantom command.
SUMMARY = re.compile(r'^\d+ top-level commands\b')


def command_paths():
    return [c for c in sibling('--list') if not SUMMARY.match(c)]


def corpus_pages():
    return sibling('--corpus')


# Commands a reader cannot discover are not commands a reader can miss. Read
# `hide = true` out of the clap derive input rather than listing the hidden
# commands here, so this file never has to be edited when one is added.
HIDE = re.compile(r'#\[command\([^)]*\bhide\s*=\s*true')


def hidden_variants(root):
    """Variant names carrying `#[command(hide = true)]`, kebab-cased."""
    out = set()
    for src in (root / 'autumn-cli' / 'src').rglob('*.rs'):
        text = src.read_text(errors='replace')
        for m in HIDE.finditer(text):
            tail = text[m.end():]
            # the next identifier at the start of a line is the variant name
            nxt = re.search(r'\n\s*([A-Z][A-Za-z0-9_]*)\s*[{(,]', tail)
            if nxt:
                name = nxt.group(1)
                out.add(re.sub(r'(?<!^)(?=[A-Z])', '-', name).lower())
    return out


# ----------------------------------------------------------------- the rules

# A generic rule documents a whole prefix by describing its argument. Each is
# (prefix, page, the sentence the page must still contain). The sentence is
# checked, not trusted: a rule deleted from the page stops exempting anything.
RULES = [
    ('destroy', 'docs/guide/generators.md',
     '`autumn destroy <thing> <the same arguments>` reverses a matching'),
]

# Undocumented, triaged, not this change's to write. One line, one reason.
BACKLOG = {
    'assets add':    'vendored-asset management; `autumn assets` is named in upgrading.md but the family is undocumented',
    'assets list':   'ditto',
    'assets update': 'ditto',
    'assets verify': 'ditto',
    'db reset':      'dev-only drop/create/migrate/seed; 13 of 14 `db` subcommands are documented',
    'export':        'offline diagnostic snapshot; a top-level command named on no page, and the one this gate found only once the matcher stopped letting `autumn openapi export` satisfy it',
    'schema parse':  'schema-tooling internal; the other 5 `schema` subcommands are documented',
    'generate inbound-mail': 'generator with no guide section; 19 of 21 are documented',
    'generate policy':       'generator with no guide section; 19 of 21 are documented',
}


def rule_exempt(cmd, root, failures):
    for prefix, page, sentence in RULES:
        if cmd == prefix or cmd.startswith(prefix + ' '):
            text = (root / page).read_text(errors='replace')
            if sentence in text:
                return True
            failures.append(
                f"{page} no longer states the `{prefix}` rule "
                f"({sentence!r}), so `autumn {cmd}` is no longer covered by it")
            return False
    return False


# Everything a reader may type between `autumn` and the command path. The `Cli`
# struct carries no global options — it is `#[command(subcommand)] command` and
# nothing else — so this is only clap's builtins, and on the current corpus
# skipping them changes nothing (23 undocumented either way). It is here so a
# global option added later does not start hiding commands.
#
# The first cut allowed ARBITRARY text here (`autumn[^\n`]{0,80}?\bcmd\b`) and
# that was wrong in the direction that matters: a longer command satisfied a
# shorter one as a suffix. `autumn openapi export`, documented in openapi.md,
# made the runnable TOP-LEVEL `export` command report as documented, so the gate
# was green while missing a real undocumented command — the exact failure it
# exists to catch. Match the path as a token PREFIX of what follows `autumn`.
GLOBAL_OPTS = ('--help', '-h', '--version', '-V')

# Stop at a newline or a backtick: a command does not span either, and running
# past a closing backtick is how a code span's neighbour gets read as arguments.
INVOCATION = re.compile(r'\bautumn[ \t]+([^\n`]{0,120})')


def mentioned(cmd, text):
    """True when some `autumn …` invocation names `cmd` as its command path.

    The path must be a token prefix of what follows `autumn`, so `openapi
    export` matches `openapi export` and never bare `export`.
    """
    want = cmd.split()
    for m in INVOCATION.finditer(text):
        toks = m.group(1).split()
        while toks and (toks[0] in GLOBAL_OPTS
                        or (toks[0].startswith('--') and '=' in toks[0])):
            toks.pop(0)
        if toks[:len(want)] == want:
            return True
    return False


def analyse(root):
    cmds = command_paths()
    pages = corpus_pages()
    text = "\n".join((root / p).read_text(errors='replace') for p in pages)
    hidden = hidden_variants(root)

    rows, rule_failures = [], []
    for c in cmds:
        last = c.split()[-1]
        if last in hidden and len(c.split()) > 1:
            rows.append((c, 'hidden')); continue
        if mentioned(c, text):
            rows.append((c, 'documented')); continue
        if rule_exempt(c, root, rule_failures):
            rows.append((c, 'rule')); continue
        if c in BACKLOG:
            rows.append((c, 'backlog')); continue
        rows.append((c, 'DEFECT'))
    return rows, rule_failures, len(pages)


def report(root):
    rows, rule_failures, npages = analyse(root)
    counts = {}
    for _, k in rows:
        counts[k] = counts.get(k, 0) + 1
    defects = [c for c, k in rows if k == 'DEFECT']

    print(f"corpus:  {npages} reader-facing markdown files")
    print(f"surface: {len(rows)} command paths "
          f"(from scripts/check-docs-cli.sh --list)")
    print(f"  documented: {counts.get('documented', 0)}")
    print(f"  hidden (#[command(hide = true)]): {counts.get('hidden', 0)}")
    print(f"  covered by a generic rule: {counts.get('rule', 0)}")
    print(f"  triaged backlog: {counts.get('backlog', 0)}")
    print()

    if counts.get('backlog'):
        print("Undocumented, triaged (tracked, not failing):")
        for c, k in rows:
            if k == 'backlog':
                print(f"  autumn {c}\n      {BACKLOG[c]}")
        print()

    for f in rule_failures:
        print(f"RULE BROKEN: {f}")

    print(f"defects: {len(defects)}")
    if not defects and not rule_failures:
        print("CLI coverage gate OK.")
        return 0

    for c in defects:
        print(f"\n  `autumn {c}` is documented on none of the "
              f"{npages} reader-facing pages.")
    print("""
A command a reader can run and cannot find is a coverage defect: the answer does
not exist where they look, so they conclude the feature does not exist. Fix it at
the lowest rung that works:

  - the command belongs to a family a page already covers
        -> name it on that page, beside its siblings
  - a rule already describes it (as generators.md describes `autumn destroy`)
        -> declare the rule in RULES above; the gate verifies the page states it
  - it is an internal entrypoint a reader should never type
        -> mark it `#[command(hide = true)]`; this gate then exempts it, and so
           does `--help`, which is the same claim made to the reader
  - it is real, reader-facing, and genuinely needs prose you are not writing now
        -> add it to BACKLOG above WITH A REASON, so it stays a number

Inspect what the gate read:  scripts/check-docs-cli-coverage.sh --list""")
    return 1


def show(root):
    rows, rule_failures, npages = analyse(root)
    for c, k in sorted(rows):
        print(f"{k:11} autumn {c}")
    for f in rule_failures:
        print(f"RULE BROKEN: {f}")


# ------------------------------------------------------------------ self-test

def self_test():
    """Assert the classifier's rules on synthetic inputs, plus a non-zero
    surface against the real tree — a matrix that silently went empty would
    otherwise pass this gate forever."""
    fails = []

    def check(name, got, want):
        if got != want:
            fails.append(f"{name}: got {got!r}, want {want!r}")

    # mention matching
    check('bare command', mentioned('token revoke', 'run `autumn token revoke`'), True)
    check('trailing args ignored', mentioned('db reset', 'autumn db reset --force'), True)
    check('builtin global flag skipped',
          mentioned('db reset', 'autumn --help db reset'), True)
    check('absent', mentioned('token revoke', 'autumn token issue'), False)
    check('not a substring match',
          mentioned('db reset', 'autumn db resetting-is-not-a-command'), False)
    check('needs the exe', mentioned('token revoke', 'the token revoke flow'), False)

    # REGRESSION: a longer command must not satisfy a shorter one as a suffix.
    # `autumn openapi export` is in openapi.md and the top-level `export`
    # command is documented nowhere; the first matcher reported it covered.
    check('suffix collision', mentioned('export', 'autumn openapi export'), False)
    check('the real path still matches',
          mentioned('openapi export', 'autumn openapi export'), True)
    check('does not run past a backtick',
          mentioned('db reset', '`autumn db` reset'), False)
    check('hyphenated exe is not the exe',
          mentioned('token issue', 'autumn-cli token issue'), False)

    # the summary line must never be read as a command
    check('summary dropped', bool(SUMMARY.match('53 top-level commands, 194 command paths')), True)
    check('command kept', bool(SUMMARY.match('token revoke')), False)

    # hidden detection
    check('hide parsed', bool(HIDE.search('#[command(hide = true)]')), True)
    check('hide not over-matched', bool(HIDE.search('#[command(verbatim_doc_comment)]')), False)

    # a rule stops exempting once its page stops stating it
    missing = []
    fake = pathlib.Path(os.environ['SELFTEST_TMP'])
    (fake / 'docs' / 'guide').mkdir(parents=True, exist_ok=True)
    (fake / 'docs' / 'guide' / 'generators.md').write_text('no rule here\n')
    check('deleted rule stops exempting', rule_exempt('destroy job', fake, missing), False)
    check('deleted rule is reported', len(missing), 1)
    (fake / 'docs' / 'guide' / 'generators.md').write_text(RULES[0][2] + ' generate.\n')
    check('present rule exempts', rule_exempt('destroy job', fake, []), True)

    # the real surface must not be empty
    real = command_paths()
    if len(real) < 50:
        fails.append(f"surface collapsed to {len(real)} command paths")
    if any(SUMMARY.match(c) for c in real):
        fails.append("summary line leaked into the command surface")

    if fails:
        for f in fails:
            print(f"SELF-TEST FAILURE: {f}")
        return 1
    print(f"Self-test OK ({len(real)} command paths parsed from the real tree).")
    return 0


if MODE == '--self-test':
    sys.exit(self_test())
elif MODE == '--list':
    show(ROOT); sys.exit(0)
else:
    sys.exit(report(ROOT))
PYEOF
}

mode="${1:-}"
case "$mode" in
  --self-test)
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    SELFTEST_TMP="$tmp" run_py --self-test "$root"
    ;;
  --list)  run_py --list "$root" ;;
  "")      echo "Checking that every runnable \`autumn …\` command is findable in the docs..."
           run_py --check "$root" ;;
  *)       echo "usage: $0 [--list|--self-test]" >&2; exit 2 ;;
esac
