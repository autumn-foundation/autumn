#!/usr/bin/env bash
# CLI drift gate: every `autumn …` command the reader-facing docs tell someone
# to RUN must exist in the CLI.
#
# WHY THIS EXISTS: the corpus already gates its *links*
# (`scripts/check-docs-links.sh`) and its *rustdoc* intra-doc links
# (`scripts/check-docs.sh`), so a reader can no longer be sent to a page that
# does not exist. Nothing checked the other thing readers copy off a page: the
# command. `autumn-cli` carries 51 top-level commands and 174 command paths,
# the guide names them 2,400+ times, and a renamed or never-shipped subcommand
# leaves behind a line that looks exactly like a working one.
#
# The baseline run found `autumn migrate run` in three guide pages — a command
# that has never existed in `MigrateCommands` (`status`, `check`, `down`,
# `baseline`; the run action is the bare `autumn migrate`). One of the eight
# occurrences sat in a fenced `shell` block in `docs/guide/cloud-native.md`
# under "run the migration before deploying new workers", so the reader most
# likely to copy it was mid-production-upgrade. Clap answers with
# `unrecognized subcommand 'run'`, and — unlike a wrong sentence, which a
# reader can reason around — a command that does not parse is a dead end at
# the exact moment they cannot afford one.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. The first token after `autumn` names a real top-level command.
#   2. Each following token, while the command still has subcommands, names a
#      real subcommand of the path resolved so far.
#
# TRUTH SET: parsed from the clap derive input in `autumn-cli/src/**/*.rs`, not
# from a checked-in snapshot. A snapshot is one forgotten regeneration away
# from gating the docs against a CLI that no longer exists, which is the very
# failure this script is for. Parsing the source keeps the truth set correct by
# construction: rename a command and the gate moves with it in the same commit.
# The parser handles the four forms this CLI actually uses — `Variant`,
# `#[command(subcommand)] Variant(SomeCommands)`, `Variant { #[command(subcommand)]
# action: Option<SomeCommands> }`, and `Variant(SomeArgs)` where the args struct
# carries the subcommand — plus `#[command(name = "…")]` renames and
# `visible_alias`/`alias` (without which the real `autumn c` reads as drift).
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - Flags. `--profile`, `--force` and friends are worth gating, but clap's
#     `conflicts_with`/`value_enum`/global-arg forms make a source-parsed flag
#     set far noisier than a command set, and a wrong flag usually fails
#     loudly against a command that at least exists. Commands first.
#   - Tokens past the point where the resolved command has no subcommands.
#     `autumn db pull posts` is a positional table name, not a subcommand, and
#     the parser records which commands take positionals so those tokens are
#     not mistaken for drift.
#   - Prose mentions. Only fenced shell blocks and inline code spans are read —
#     the two places a reader copies from. "run autumn migrate to apply" in a
#     sentence is not a copyable line, and scanning prose drags in every
#     "autumn is", "autumn never", "the autumn crate".
#
# CORPUS SCOPE: reader-facing task docs only — `docs/guide/`, `docs/migrations/`,
# `skills/`, `agents/`, and the root `README.md` / `EXAMPLES.md` /
# `CONTRIBUTING.md` / `STABILITY.md`. Deliberately excluded:
#   - `CHANGELOG.md` and `docs/releases/` — a historical record. What was true
#     at 0.7.0 must stay written as it was; correcting history to satisfy a
#     gate is how a changelog stops being evidence.
#   - `docs/plans/`, `docs/stories/`, `docs/adr/`, `docs/reports/`,
#     `docs/design/` — planning artifacts whose job includes naming commands
#     that do not exist yet (`autumn harvest migrate`) or were rejected
#     (`autumn scaffold sync`). Gating them would make the gate a tax on
#     writing a proposal.
#
# WAIVERS: a reader-facing page sometimes has to name a command that does not
# exist — "`autumn generate island` does not exist yet" is a real answer to a
# real question. Waive it with a marker anywhere in the same file:
#
#     <!-- cli-surface-allow: autumn generate island — planned, see #493 -->
#
# The marker sits in the page beside the claim, so when the sentence is deleted
# or the command ships, the waiver goes with it. A central allowlist would
# outlive both. Every waiver must carry a reason after the command.
#
# USAGE:
#   scripts/check-docs-cli.sh              # gate the corpus
#   scripts/check-docs-cli.sh --list       # print the parsed command surface
#   scripts/check-docs-cli.sh --self-test  # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# Kept in Python for the same reason as scripts/check-docs-links.sh: brace
# matching over clap derive input and code-fence stripping are both work that
# bash renders unreadable, and python3 is already a dependency of
# scripts/check-plugin-freshness.sh and scripts/check-docs-links.sh.
run_py() {
  python3 - "$@" <<'PYEOF'
import os, re, subprocess, sys, pathlib, collections

MODE = sys.argv[1]
ROOT = pathlib.Path(sys.argv[2])

# ---------------------------------------------------------------- truth set

def kebab(name):
    """Clap's default rename_all for subcommands: CamelCase -> kebab-case."""
    return re.sub(r'(?<!^)(?=[A-Z])', '-', name).lower()


def _blocks(text, keyword):
    """Map `keyword <Name> { … }` to its brace-balanced body."""
    out = {}
    for m in re.finditer(r'\b' + keyword + r'\s+([A-Za-z0-9_]+)\s*\{', text):
        i = m.end()
        depth = 1
        start = i
        while i < len(text) and depth:
            if text[i] == '{':
                depth += 1
            elif text[i] == '}':
                depth -= 1
            i += 1
        # First definition wins: a `#[cfg]`-duplicated enum would otherwise
        # have its later copy silently replace the one that is compiled.
        out.setdefault(m.group(1), text[start:i - 1])
    return out


def _items(body):
    """Split an enum/struct body into (name, attrs, kind, payload).

    Walks the body rather than regexing whole variants, because doc comments
    on these enums run to dozens of lines and contain braces, brackets and
    example command lines of their own.
    """
    out = []
    attrs = []
    i, n = 0, len(body)
    while i < n:
        while i < n and body[i] in ' \t\r\n':
            i += 1
        if i >= n:
            break
        if body.startswith('//', i):
            j = body.find('\n', i)
            i = (j + 1) if j >= 0 else n
            continue
        if body[i] == '#':                      # attribute, possibly nested
            j, depth = i + 1, 0
            while j < n:
                if body[j] == '[':
                    depth += 1
                elif body[j] == ']':
                    depth -= 1
                    if depth == 0:
                        j += 1
                        break
                j += 1
            attrs.append(body[i:j])
            i = j
            continue
        m = re.match(r'(?:pub\s+)?([A-Za-z0-9_]+)', body[i:])
        if not m:
            i += 1
            continue
        name = m.group(1)
        i += m.end()
        while i < n and body[i] in ' \t':
            i += 1
        kind, payload = 'unit', ''
        if i < n and body[i] in '{(':
            op = body[i]
            cl = '}' if op == '{' else ')'
            depth, start = 0, i
            while i < n:
                if body[i] == op:
                    depth += 1
                elif body[i] == cl:
                    depth -= 1
                    if depth == 0:
                        i += 1
                        break
                i += 1
            kind = 'struct' if op == '{' else 'tuple'
            payload = body[start:i]
        while i < n and body[i] in ' ,\r\n\t':
            i += 1
        out.append((name, '\n'.join(attrs), kind, payload))
        attrs = []
    return out


def _last(path):
    """`schema::SchemaAction` -> `SchemaAction`."""
    return path.split('::')[-1]


def _subcommand_type(payload):
    m = re.search(
        r'#\[command\([^\]]*\bsubcommand\b[^\]]*\)\]\s*(?:pub\s+)?\w+\s*:\s*'
        r'(?:Option\s*<\s*)?([A-Za-z0-9_:]+)',
        payload)
    return _last(m.group(1)) if m else None


def _has_positional(payload):
    """True when a variant takes a bare value argument.

    Matters because a positional makes the next token in `autumn db pull posts`
    unjudgeable — it is a table name, not a subcommand — so the walk stops
    there rather than reporting drift it cannot prove.
    """
    for fm in re.finditer(r'#\[arg\(([^\]]*)\)\]\s*(?:pub\s+)?[a-z_0-9]+\s*:', payload):
        if not re.search(r'\b(long|short)\b', fm.group(1)):
            return True
    return False


def build_surface(sources):
    text = "\n".join(sources)
    enums = _blocks(text, 'enum')
    structs = _blocks(text, 'struct')

    def build(tname, seen=()):
        if tname in seen:                       # recursive subcommand type
            return {}
        seen = seen + (tname,)
        if tname in enums:
            tree = {}
            for name, attrs, kind, payload in _items(enums[tname]):
                if not re.match(r'^[A-Z]', name):
                    continue
                rename = re.search(r'\bname\s*=\s*"([^"]+)"', attrs)
                spellings = {rename.group(1) if rename else kebab(name)}
                for a in re.findall(r'\b(?:visible_)?alias\s*=\s*"([^"]+)"', attrs):
                    spellings.add(a)
                for group in re.findall(r'\b(?:visible_)?aliases\s*=\s*\[([^\]]*)\]', attrs):
                    spellings.update(re.findall(r'"([^"]+)"', group))
                node = {'children': {}, 'positionals': False}
                if kind == 'tuple':
                    inner = re.search(r'\(\s*(?:pub\s+)?([A-Za-z0-9_:]+)', payload)
                    if inner:
                        it = _last(inner.group(1))
                        if 'subcommand' in attrs:
                            node['children'] = build(it, seen)
                        elif it in structs:
                            # `Variant(SomeArgs)` — a flattened args struct that
                            # may itself carry the subcommand (`Upgrade(UpgradeArgs)`).
                            st = _subcommand_type(structs[it])
                            if st:
                                node['children'] = build(st, seen)
                            node['positionals'] = _has_positional(structs[it])
                elif kind == 'struct':
                    st = _subcommand_type(payload)
                    if st:
                        node['children'] = build(st, seen)
                    node['positionals'] = _has_positional(payload)
                for spelling in spellings:
                    tree[spelling] = node
            return tree
        if tname in structs:
            st = _subcommand_type(structs[tname])
            return build(st, seen) if st else {}
        return {}

    tree = build('Commands')

    def flatten(t, prefix=''):
        flat = {}
        for k, v in t.items():
            key = (prefix + ' ' + k).strip()
            flat[key] = {'children': set(v['children']), 'positionals': v['positionals']}
            flat.update(flatten(v['children'], key))
        return flat

    return flatten(tree)


def cli_sources(root):
    src = root / 'autumn-cli' / 'src'
    return [p.read_text(errors='replace') for p in sorted(src.rglob('*.rs'))]


# ------------------------------------------------------------ docs scanning

# Fences whose contents are shell input. `text` is included because a handful
# of pages fence terminal transcripts without a language; lines inside them
# still start with the command being demonstrated.
SHELL_LANGS = {'bash', 'sh', 'shell', 'console', 'zsh', 'terminal', 'text', ''}

# `autumn` as the head of a command: at line start, after a `$` prompt, or
# after a shell operator. The lookbehind rejects `autumn-cli`, `autumn-web`,
# `./autumn`, and `autumn/src/…`, none of which are CLI invocations.
INVOCATION = re.compile(r'(?:^|[;&|(]\s*|\$\s+)autumn\s+(.+)$')

# A token that could name a command. Anything else — a flag, a `<PLACEHOLDER>`,
# a TOML `= "0.1.0"`, a box-drawing character from a diagram — ends the walk.
TOKEN = re.compile(r'^[a-z][a-z0-9-]*$')

WAIVER = re.compile(r'<!--\s*cli-surface-allow:\s*autumn\s+([a-z0-9 -]+?)\s*(?:—|--|:)\s*(\S.*?)-->')

INCLUDE_DIRS = ('docs/guide/', 'docs/migrations/', 'skills/', 'agents/')
INCLUDE_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md')


def in_scope(path):
    return path.startswith(INCLUDE_DIRS) or path in INCLUDE_FILES


def corpus(root):
    # NUL-delimited so a path containing whitespace is not split into
    # fragments, and so git does not quote unusual paths.
    out = subprocess.run(['git', 'ls-files', '-z', '*.md'], cwd=root,
                         capture_output=True, text=True).stdout
    return [f for f in out.split('\0') if f and in_scope(f)]


def invocations(text):
    """Yield (line_no, argv) for every copyable `autumn …` line."""
    fence = None
    lang = None
    for lineno, line in enumerate(text.split('\n'), 1):
        m = re.match(r'^\s*(`{3,}|~{3,})\s*([A-Za-z0-9_+-]*)', line)
        if m:
            if fence is None:
                fence, lang = m.group(1)[0], m.group(2).lower()
            elif line.strip().startswith(fence * 3):
                fence, lang = None, None
            continue
        candidates = []
        if fence is not None and lang in SHELL_LANGS:
            candidates.append(line)
        candidates.extend(re.findall(r'`([^`\n]+)`', line))
        for c in candidates:
            hit = INVOCATION.search(c.strip())
            if hit:
                yield lineno, hit.group(1).strip()


def resolve(argv, surface):
    """Return the drifted command path, or None when the line resolves.

    Walks token by token: each token is a subcommand of the path so far, an
    argument (which ends the walk), or drift.
    """
    tokens = argv.split()
    if not tokens or not TOKEN.match(tokens[0]):
        return None
    if tokens[0] not in surface:
        return 'autumn ' + tokens[0]
    path = tokens[0]
    for tok in tokens[1:]:
        if not TOKEN.match(tok):
            return None
        node = surface[path]
        if not node['children']:                # leaf: the rest are arguments
            return None
        if tok in node['children']:
            path = path + ' ' + tok
            continue
        if node['positionals']:                 # unjudgeable: a value, not a name
            return None
        return 'autumn ' + path + ' ' + tok
    return None


def scan(root, surface, files):
    defects, waived = [], 0
    for f in files:
        text = (root / f).read_text(errors='replace')
        allowed = {m.group(1).strip() for m in WAIVER.finditer(text)}
        for lineno, argv in invocations(text):
            bad = resolve(argv, surface)
            if bad is None:
                continue
            if bad[len('autumn '):] in allowed:
                waived += 1
                continue
            defects.append((f, lineno, bad, argv))
    return defects, waived


# ------------------------------------------------------------------- modes

def self_test():
    """Exercise the parser and the walk against a synthetic CLI.

    The forms here are the ones that produced wrong answers while this script
    was written: an alias (without it the real `autumn c` reads as drift), a
    `#[command(name)]` rename, a cross-module subcommand type, an args-struct
    variant, and a positional that must not be mistaken for a subcommand.
    """
    fake = '''
    enum Commands {
        /// Run or inspect database migrations
        Migrate {
            #[command(subcommand)]
            action: Option<MigrateCommands>,
            #[arg(long)]
            with_maintenance: bool,
        },
        #[command(visible_alias = "c")]
        Console,
        #[command(subcommand, name = "db")]
        Db(DbCommands),
        #[command(name = "data-flow")]
        DataFlow,
        Schema {
            #[command(subcommand)]
            action: schema::SchemaAction,
        },
        Upgrade(UpgradeArgs),
    }
    enum MigrateCommands { Status, Check, Down }
    enum DbCommands {
        Create,
        Pull {
            #[arg(value_name = "TABLE")]
            tables: Vec<String>,
            #[arg(long)]
            dry_run: bool,
        },
    }
    enum SchemaAction { Parse, Diff }
    struct UpgradeArgs {
        #[command(subcommand)]
        action: Option<UpgradeCommands>,
    }
    enum UpgradeCommands { Apply }
    '''
    surface = build_surface([fake])
    failures = []

    def expect(cond, msg):
        if not cond:
            failures.append(msg)

    # --- surface parsing
    expect('migrate status' in surface, 'nested subcommand enum not resolved')
    expect('c' in surface and 'console' in surface, 'visible_alias not registered')
    expect('db create' in surface, '#[command(name)] rename on a tuple variant lost')
    expect('data-flow' in surface, '#[command(name)] rename on a unit variant lost')
    expect('schema parse' in surface, 'cross-module subcommand type not resolved')
    expect('upgrade apply' in surface, 'subcommand inside an args struct not resolved')
    expect(surface['db pull']['positionals'], 'positional value_name arg not detected')
    expect(not surface['migrate']['positionals'], 'a long-only field is not a positional')

    # --- the walk
    expect(resolve('migrate run', surface) == 'autumn migrate run',
           'a phantom subcommand must be reported')
    expect(resolve('migrate', surface) is None,
           'a command with an OPTIONAL subcommand is valid bare')
    expect(resolve('migrate --with-maintenance', surface) is None,
           'a flag must not be read as a subcommand')
    expect(resolve('db pull posts', surface) is None,
           'a positional value must not be read as a subcommand')
    expect(resolve('db pull posts --dry-run', surface) is None,
           'positional plus flag must resolve')
    expect(resolve('c', surface) is None, 'an alias must resolve')
    expect(resolve('nope', surface) == 'autumn nope',
           'an unknown top-level command must be reported')
    expect(resolve('console extra', surface) is None,
           'a leaf command consumes its remaining tokens as arguments')
    expect(resolve('$SOMETHING', surface) is None,
           'a non-command token must be ignored')

    # --- extraction
    doc = '\n'.join([
        '```shell',
        'autumn migrate run',
        '```',
        'Prose naming autumn migrate run without backticks is not copyable.',
        'But `autumn migrate run` in a code span is.',
        '```rust',
        '// autumn migrate run inside a rust block is a comment',
        '```',
    ])
    found = list(invocations(doc))
    expect(len(found) == 2, f'expected 2 copyable invocations, got {len(found)}: {found}')
    expect(all(a == 'migrate run' for _, a in found), f'bad argv extraction: {found}')

    # --- waivers
    expect(WAIVER.search('<!-- cli-surface-allow: autumn generate island — planned #493 -->'),
           'waiver marker with an em dash must parse')
    expect(not WAIVER.search('<!-- cli-surface-allow: autumn generate island -->'),
           'a waiver without a reason must not parse')

    for f in failures:
        print('SELF-TEST FAILURE: ' + f, file=sys.stderr)
    print(f'self-test: {13 + 9 + 2 + 2 - len(failures)} passed, {len(failures)} failed')
    return 1 if failures else 0


def main():
    surface = build_surface(cli_sources(ROOT))
    if not surface:
        print('ERROR: parsed an empty command surface from autumn-cli/src — '
              'the clap derive input moved or changed shape, and this gate '
              'cannot tell drift from a parser failure. Fix the parser.',
              file=sys.stderr)
        return 1

    if MODE == '--list':
        for path in sorted(surface):
            print(path)
        print(f'\n{len([p for p in surface if " " not in p])} top-level commands, '
              f'{len(surface)} command paths')
        return 0

    files = corpus(ROOT)
    defects, waived = scan(ROOT, surface, files)
    print(f'corpus: {len(files)} reader-facing markdown files')
    print(f'surface: {len(surface)} command paths parsed from autumn-cli/src')
    print(f'defects: {len(defects)}' + (f' ({waived} waived)' if waived else ''))
    if defects:
        print()
        for f, lineno, bad, argv in defects:
            print(f'{f}:{lineno}: `{bad}` is not a command  (line: autumn {argv})')
        print()
        print('Each line above tells a reader to run something the CLI will '
              'reject. Fix the page, or — if the page is deliberately naming a '
              'command that does not exist — add a waiver in that file, quoting '
              'the backticked command EXACTLY as reported above (that is the '
              'part that failed to resolve, which may be shorter than the line):')
        print('    <!-- cli-surface-allow: autumn <command> — why -->')
        print('Run `scripts/check-docs-cli.sh --list` to see the real surface.')
        return 1
    print('CLI drift gate OK.')
    return 0


sys.exit(self_test() if MODE == '--self-test' else main())
PYEOF
}

mode="${1:-}"
case "$mode" in
  --self-test) run_py --self-test "$root" ;;
  --list)      run_py --list "$root" ;;
  "")          echo "Checking CLI invocations across the reader-facing docs..."
               run_py --check "$root" ;;
  *)           echo "usage: $0 [--list|--self-test]" >&2; exit 2 ;;
esac
