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
# The first run of this direction found 26 of 194 command paths (13.4%) absent
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
#      They are tracked by FULL PATH, resolved through the enclosing enum
#      (`RunService` in `enum ServeCommands` is `serve run-service`): matching
#      the last component alone would hand a later visible `deploy run-service`
#      this one's exemption, and the "more than one component" guard that came
#      with it meant a hidden TOP-LEVEL command was never exempted at all.
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
# AND ONE THING THAT LOOKS LIKE COVERAGE AND IS ITS OPPOSITE: a page that names
# a command only to say it DOES NOT EXIST. Three do — `autumn generate seed`
# ("out of scope", tracked in #493), `autumn generate island` ("there is no…")
# and `autumn system-test` ("planned but does not exist yet") — each carrying
# the sibling gate's `cli-surface-allow` waiver. None is in the surface today,
# so nothing is miscounted yet; the bug is what happens when one SHIPS. The
# denial would read as documentation, the gate would go green, and the page
# would keep telling readers a shipped feature is unavailable — wrong docs, with
# the gate certifying them. Worse, the waiver comment CONTAINS the command it
# waives, so it satisfied coverage on its own.
#
# So: HTML comments are stripped before extraction (a comment renders as
# nothing, so it documents nothing — the line `check-docs-orphans.sh` draws,
# for the same reason), and a page's waived commands do not count as mentioned
# ON THAT PAGE. When such a command ships, the gate fails and names the page
# still denying it, so the stale passage is deleted rather than the waiver
# widened.
#
# ONE RULE BEHIND ALL OF THAT: invisible text is never authoritative, in either
# language. Everything this gate reads, it reads comment-stripped — the docs
# corpus, the page a generic RULE is verified against, and the clap derive input
# behind `hide = true`. Each was a separate hole found in review, and each had
# the same shape: a claim that survives in a comment while the rendered page or
# the compiled code says otherwise. A rule moved into an HTML comment would keep
# exempting all 13 `destroy` paths; a command unhidden by commenting the
# attribute out (`// #[command(hide = true)]`, the usual way to unhide) would
# keep its exemption. Both leave the gate green over documentation no reader
# sees and behaviour the binary does not have.
#
# The stale-denial check runs over the whole SURFACE, not over the failing
# rows, and fails on its own. Coverage and staleness are independent questions:
# a command that ships WITH proper docs on a new page is classified
# `documented`, so a check that only looked at defects would go quiet on the
# old page's denial at exactly the moment the denial became false.
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


ENUM = re.compile(r'^(?:pub )?enum ([A-Za-z0-9_]+)', re.M)

# Commented-out Rust is not Rust. The usual way to UNHIDE a command is to
# comment the attribute out rather than delete it — `// #[command(hide = true)]`
# — and the bare `HIDE` pattern matches that just as happily, so the now-visible
# command would keep its exemption and the gate would stay green with no docs.
# Same rule as the HTML comments above, one language over.
#
# Only whole comment LINES are dropped, not `//` anywhere on a line: everything
# this reads (attributes, `enum` declarations, variant names) sits at the start
# of its own line, so there is nothing to gain from parsing `//` inside a string
# literal and a URL in a doc comment to get wrong.
RUST_BLOCK = re.compile(r'/\*.*?\*/', re.S)
RUST_LINE = re.compile(r'^[ \t]*//.*$', re.M)


def strip_rust_comments(text):
    return RUST_LINE.sub('', RUST_BLOCK.sub('', text))


def _kebab(name):
    return re.sub(r'(?<!^)(?=[A-Z])', '-', name).lower()


def hidden_paths(root, surface):
    """FULL command paths carrying `#[command(hide = true)]`.

    The first cut kept only the variant name and matched it against a command's
    LAST component, which is wrong in both directions: a later visible
    `deploy run-service` would inherit `serve run-service`'s exemption, and a
    hidden TOP-LEVEL command was never exempted at all (it was guarded by
    `len(path.split()) > 1`). Resolve the owning path instead.

    The owner is the enclosing enum: `RunService` inside `enum ServeCommands`
    is `serve run-service`, and a variant of the root `enum Commands` is
    top-level. Anything that does not resolve to a real command path is
    reported rather than dropped — a silent miss here re-exempts nothing and
    hides a command from the gate forever.
    """
    found, unplaced = set(), []
    known = set(surface)
    for src in sorted((root / 'autumn-cli' / 'src').rglob('*.rs')):
        text = strip_rust_comments(src.read_text(errors='replace'))
        enums = [(m.start(), m.group(1)) for m in ENUM.finditer(text)]
        for m in HIDE.finditer(text):
            nxt = re.search(r'\n\s*([A-Z][A-Za-z0-9_]*)\s*[{(,]', text[m.end():])
            if not nxt:
                continue
            variant = _kebab(nxt.group(1))
            owner = ''
            for pos, name in enums:
                if pos < m.start():
                    owner = name
                else:
                    break
            if owner == 'Commands':
                path = variant
            else:
                parent = re.sub(r'(?:Sub)?[Cc]ommands$', '', owner)
                path = f"{_kebab(parent)} {variant}".strip() if parent else variant
            if path in known:
                found.add(path)
            else:
                unplaced.append(f"{path!r} (variant of `enum {owner}`, "
                                f"{src.relative_to(root)})")
    return found, unplaced


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
            # Comment-stripped, for the same reason every other read here is:
            # a rule a reader cannot see is not a rule they can follow. Checking
            # the raw page would let the `autumn destroy` explanation be moved
            # into an HTML comment and still exempt all 13 `destroy` paths —
            # this gate contradicting its own coverage rule one function away.
            text = COMMENT.sub(
                '', (root / page).read_text(errors='replace'))
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

# An HTML comment renders as nothing, so it can document nothing. Strip before
# extraction — the same line `check-docs-orphans.sh` draws, and for the same
# reason: the rule is VISIBLE vs INVISIBLE, not clickable vs not. A fenced
# command still counts (the reader can see and paste it); a commented one
# cannot.
#
# This is load-bearing rather than tidy. The waiver below is itself an HTML
# comment CONTAINING the command it waives, so without this strip
# `<!-- cli-surface-allow: autumn generate seed — … does not exist -->` would
# satisfy coverage for `autumn generate seed` all by itself.
COMMENT = re.compile(r'<!--.*?-->', re.S)

# The sibling gate's waiver, same grammar (scripts/check-docs-cli.sh). It marks
# a command named on a page ONLY to say it does not exist — "planned", "out of
# scope", "no such command". For the drift gate that means "do not fail on this
# spelling"; here it means the opposite of coverage, and it must be read or the
# gate inverts on exactly the commands most likely to ship next.
#
# Matched against the command path EXACTLY, and an OPTION waiver waives nothing
# here: `cli-surface-allow: autumn build --release` says that flag spelling does
# not exist, while `build` itself ships and is documented. Reading it as a
# command waiver would delete a real command's coverage.
#
# The separator is read by splitting the body rather than by a non-greedy
# capture. `([a-z0-9 -]+?)\s*(?:—|--|:)` — the sibling's pattern, which answers
# a different question — stops at the `--` of `--release` and captures `build`,
# which is exactly the misread above. Found by this gate's own self-test.
WAIVER = re.compile(r'<!--\s*cli-surface-allow:\s*autumn\s+(.*?)-->', re.S)
REASON = re.compile(r'—|(?<=\s)--(?=\s)|:')


def waived_commands(raw):
    """Command paths a page names only to deny. Option waivers are not these."""
    out = set()
    for m in WAIVER.finditer(raw):
        spec = REASON.split(m.group(1), maxsplit=1)[0]
        toks = spec.split()
        if toks and not any(t.startswith('-') for t in toks):
            out.add(' '.join(toks))
    return out


def read_pages(root, pages):
    """(visible text, commands this page waives as nonexistent) per page.

    Waivers are read from the RAW page, before comments are stripped — they
    live in comments, which is the point.
    """
    out = []
    for p in pages:
        raw = (root / p).read_text(errors='replace')
        out.append((p, COMMENT.sub('', raw), waived_commands(raw)))
    return out


def mentioned(cmd, pages):
    """True when some page NAMES `cmd` as a command path a reader can run.

    The path must be a token prefix of what follows `autumn`, so `openapi
    export` matches `openapi export` and never bare `export`. A page that
    waives `cmd` does not count: it names the command to deny it.
    """
    want = cmd.split()
    for _path, text, waived in pages:
        if cmd in waived:
            continue
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
    paths = corpus_pages()
    pages = read_pages(root, paths)
    hidden, unplaced = hidden_paths(root, cmds)

    rows, rule_failures = [], []
    for c in cmds:
        if c in hidden:
            rows.append((c, 'hidden')); continue
        if mentioned(c, pages):
            rows.append((c, 'documented')); continue
        if rule_exempt(c, root, rule_failures):
            rows.append((c, 'rule')); continue
        if c in BACKLOG:
            rows.append((c, 'backlog')); continue
        rows.append((c, 'DEFECT'))

    # A command that SHIPPED while a page still denies it exists is the worst
    # state this gate can describe: the docs actively tell a reader a feature
    # they can run is unavailable. It is computed over the whole SURFACE, not
    # over the defect rows — a shipped command documented on some new page is
    # classified `documented`, and the old page's denial would go unreported
    # precisely when the command became real. Coverage and staleness are
    # independent questions, and this one fails on its own.
    surface = set(cmds)
    stale = sorted({(c, p) for p, _t, w in pages for c in w if c in surface})
    return rows, rule_failures, len(paths), stale, unplaced


def report(root):
    rows, rule_failures, npages, stale, unplaced = analyse(root)
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
    for u in unplaced:
        print(f"HIDDEN COMMAND NOT PLACED: {u} does not resolve to any command "
              f"path. Its exemption is not being applied — fix the owner "
              f"derivation in hidden_paths() rather than leaving it unmatched.")

    print(f"defects: {len(defects)}  stale denials: {len(stale)}")
    if not defects and not rule_failures and not stale and not unplaced:
        print("CLI coverage gate OK.")
        return 0

    for c in defects:
        print(f"\n  `autumn {c}` is documented on none of the "
              f"{npages} reader-facing pages.")
    for c, p in stale:
        print(f"\n  {p} still carries a `cli-surface-allow` waiver saying "
              f"`autumn {c}` does not exist. It SHIPPED. Delete that passage and "
              f"its waiver — the page is telling readers a feature they can run "
              f"is unavailable, which is worse than missing docs. This fails "
              f"even when another page documents the command: the denial is a "
              f"defect on its own.")
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
    rows, rule_failures, npages, _stale, _unplaced = analyse(root)
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

    # mention matching. `page(text)` is the one-page corpus `mentioned` reads,
    # comments stripped and waivers extracted, exactly as read_pages builds it.
    def page(text):
        return [('p.md', COMMENT.sub('', text), waived_commands(text))]

    check('bare command', mentioned('token revoke', page('run `autumn token revoke`')), True)
    check('trailing args ignored', mentioned('db reset', page('autumn db reset --force')), True)
    check('builtin global flag skipped',
          mentioned('db reset', page('autumn --help db reset')), True)
    check('absent', mentioned('token revoke', page('autumn token issue')), False)
    check('not a substring match',
          mentioned('db reset', page('autumn db resetting-is-not-a-command')), False)
    check('needs the exe', mentioned('token revoke', page('the token revoke flow')), False)

    # REGRESSION: a longer command must not satisfy a shorter one as a suffix.
    # `autumn openapi export` is in openapi.md and the top-level `export`
    # command is documented nowhere; the first matcher reported it covered.
    check('suffix collision', mentioned('export', page('autumn openapi export')), False)
    check('the real path still matches',
          mentioned('openapi export', page('autumn openapi export')), True)
    check('does not run past a backtick',
          mentioned('db reset', page('`autumn db` reset')), False)
    check('hyphenated exe is not the exe',
          mentioned('token issue', page('autumn-cli token issue')), False)

    # REGRESSION: a NEGATIVE mention is not coverage. These pages name a command
    # only to say it does not exist; when one ships, the gate must still demand
    # real docs rather than accept the denial as documentation.
    denial = ('- **`autumn generate seed`** — tracked in #493 follow-up work.\n'
              '<!-- cli-surface-allow: autumn generate seed — listed under "Out '
              'of scope" precisely because it does not exist -->\n')
    check('waiver comment alone is not coverage',
          mentioned('generate seed', page(
              '<!-- cli-surface-allow: autumn generate seed — does not exist -->')), False)
    check('waived prose is not coverage',
          mentioned('generate seed', page(denial)), False)
    check('an unwaived command on the same page still counts',
          mentioned('generate model', page(denial + '\nRun `autumn generate model Post`.')), True)
    check('an option waiver does not waive its command',
          mentioned('build', page('Run `autumn build`.\n'
                                  '<!-- cli-surface-allow: autumn build --release '
                                  '— release is the default -->')), True)
    check('a plain HTML comment documents nothing',
          mentioned('db reset', page('<!-- autumn db reset -->')), False)

    # the summary line must never be read as a command
    check('summary dropped', bool(SUMMARY.match('53 top-level commands, 194 command paths')), True)
    check('command kept', bool(SUMMARY.match('token revoke')), False)

    # hidden detection
    check('hide parsed', bool(HIDE.search('#[command(hide = true)]')), True)
    check('hide not over-matched', bool(HIDE.search('#[command(verbatim_doc_comment)]')), False)

    # REGRESSION: hidden commands are tracked by FULL PATH. Matching only the
    # last component let `serve run-service`'s exemption cover a hypothetical
    # `deploy run-service`, and the `len(path.split()) > 1` guard meant a hidden
    # TOP-LEVEL command was never exempted at all.
    fake = pathlib.Path(os.environ['SELFTEST_TMP']) / 'hid'
    (fake / 'autumn-cli' / 'src').mkdir(parents=True, exist_ok=True)
    (fake / 'autumn-cli' / 'src' / 'main.rs').write_text(
        'enum Commands {\n'
        '    /// doc\n'
        '    #[command(hide = true)]\n'
        '    SecretTop,\n'
        '    Serve(ServeCommands),\n'
        '}\n'
        'enum ServeCommands {\n'
        '    Status,\n'
        '    #[command(hide = true)]\n'
        '    RunService {\n'
        '        x: u8,\n'
        '    },\n'
        '}\n')
    surface = ['secret-top', 'serve status', 'serve run-service',
               'deploy run-service']
    got, unplaced = hidden_paths(fake, surface)
    check('hidden resolved to full path', got, {'secret-top', 'serve run-service'})
    check('a sibling sharing the last component is NOT hidden',
          'deploy run-service' in got, False)
    check('a hidden top-level command IS exempted', 'secret-top' in got, True)
    check('nothing left unplaced', unplaced, [])
    # an owner that resolves to no command path is reported, never dropped
    _got2, unplaced2 = hidden_paths(fake, ['serve status'])
    check('unresolvable hidden path is reported', len(unplaced2), 2)

    # REGRESSION: commented-out Rust is not Rust. Unhiding a command by
    # commenting the attribute out must return it to the gate, not keep its
    # exemption alive in a comment.
    check('line-commented attribute is not a hide',
          bool(HIDE.search(strip_rust_comments('    // #[command(hide = true)]'))), False)
    check('block-commented attribute is not a hide',
          bool(HIDE.search(strip_rust_comments('/* #[command(hide = true)] */'))), False)
    check('a real attribute still is',
          bool(HIDE.search(strip_rust_comments('    #[command(hide = true)]'))), True)
    (fake / 'autumn-cli' / 'src' / 'main.rs').write_text(
        'enum ServeCommands {\n'
        '    // #[command(hide = true)]\n'
        '    RunService,\n'
        '}\n')
    got3, _ = hidden_paths(fake, ['serve run-service'])
    check('unhidden command loses its exemption', got3, set())

    # a rule stops exempting once its page stops stating it
    missing = []
    fake = pathlib.Path(os.environ['SELFTEST_TMP'])
    (fake / 'docs' / 'guide').mkdir(parents=True, exist_ok=True)
    (fake / 'docs' / 'guide' / 'generators.md').write_text('no rule here\n')
    check('deleted rule stops exempting', rule_exempt('destroy job', fake, missing), False)
    check('deleted rule is reported', len(missing), 1)
    (fake / 'docs' / 'guide' / 'generators.md').write_text(RULES[0][2] + ' generate.\n')
    check('present rule exempts', rule_exempt('destroy job', fake, []), True)

    # REGRESSION: a rule the reader cannot see is not a rule. Moving the
    # sentence into an HTML comment must stop it exempting all 13 `destroy`
    # paths, or the gate contradicts its own coverage rule one function away.
    hidden_rule = []
    (fake / 'docs' / 'guide' / 'generators.md').write_text(
        f'<!-- {RULES[0][2]} generate. -->\n')
    check('a rule buried in a comment does not exempt',
          rule_exempt('destroy job', fake, hidden_rule), False)
    check('and it is reported', len(hidden_rule), 1)

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
