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
#   1. The first token after `autumn` names a real top-level command — including
#      when the line is prefixed with environment assignments, which is how the
#      guide writes most production commands (`AUTUMN_ENV=prod autumn db backup`).
#   2. Each following token, while the command still has subcommands, names a
#      real subcommand of the path resolved so far. Options are walked THROUGH,
#      not treated as the end of the command, because clap accepts them before a
#      subcommand (`autumn migrate --with-maintenance down`). Whether an option
#      also eats the next token is read from the field's type in the derive —
#      `--force` (bool) does not, `--shard NAME` (Option<String>) does — so a
#      value is never mistaken for a subcommand, or a subcommand for a value.
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
#   - Whether a flag EXISTS. Options are parsed — the walk has to know their
#     value arity to find the subcommands written after them — but an option a
#     command does not declare is not reported, only walked away from: clap's
#     `conflicts_with`/`value_enum` forms make a source-parsed flag set far
#     noisier than a command set, and a wrong flag at least fails against a
#     command that exists. Commands first.
#   - Tokens past the point where the resolved command has no subcommands.
#     `autumn db pull posts` is a positional table name, not a subcommand, and
#     the parser records which commands take positionals so those tokens are
#     not mistaken for drift.
#   - Prose mentions. Only fenced shell blocks and inline code spans are read —
#     the two places a reader copies from. "run autumn migrate to apply" in a
#     sentence is not a copyable line, and scanning prose drags in every
#     "autumn is", "autumn never", "the autumn crate".
#
# CHAINS: a line may hold several commands (`autumn migrate && autumn seed &&
# autumn dev` appears in the guide). Every command in the chain is resolved, not
# just the first — checking only the head would leave the tail of every chained
# line ungated, which is where a second command is most easily left behind by a
# rename.
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
# real question. Waive it with a marker directly below the passage that names it:
#
#     <!-- cli-surface-allow: autumn generate island — planned, see #493 -->
#
# The marker sits in the page beside the claim, so when the sentence is deleted
# or the command ships, the waiver goes with it. A central allowlist would
# outlive both. Every waiver must carry a reason after the command.
#
# A waiver is scoped two ways, because a broad one is worse than none — it
# silently re-admits the defect the gate exists to catch:
#   - It NEVER applies inside a fenced shell block. A page may *name* a command
#     that does not exist; it may never hand one to a reader to run. This is the
#     rule that keeps a waiver from undoing the `system-tests.md` fix, where a
#     planned command sat inside a runnable block.
#   - It covers only its own blank-line-separated block and the one directly
#     above it — the passage it was written for. The same command spelled wrong
#     further down the page is still reported.
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
import os, re, subprocess, sys, pathlib, collections, tempfile

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


# An option field: the `#[arg(…)]` attribute, the field name, and its type. The
# type is what says whether the option eats the next token — `--force` (bool)
# does not, `--shard NAME` (Option<String>) does — and getting that backwards in
# either direction is a false positive: skip too little and `json` in
# `autumn routes --format json` reads as a bad subcommand; skip too much and a
# real subcommand after a boolean flag goes unchecked.
_ARG_FIELD = re.compile(
    r'#\[arg\(([^\]]*)\)\]\s*(?:pub\s+)?([a-z_0-9]+)\s*:\s*([A-Za-z0-9_:<>, ]+?)\s*,')


def _options(payload):
    """Map every option spelling on a variant to whether it takes a value."""
    opts = {}
    for m in _ARG_FIELD.finditer(payload):
        attrs, field, ftype = m.group(1), m.group(2), m.group(3)
        if not re.search(r'\b(long|short)\b', attrs):
            continue                            # positional, not an option
        takes_value = ftype.strip() != 'bool'
        named = re.search(r'\blong\s*=\s*"([^"]+)"', attrs)
        if named:
            opts['--' + named.group(1)] = takes_value
        elif re.search(r'\blong\b', attrs):
            opts['--' + field.replace('_', '-')] = takes_value
        for alias in re.findall(r'\balias\s*=\s*"([^"]+)"', attrs):
            opts['--' + alias] = takes_value
        short = re.search(r"\bshort\s*=\s*'(.)'", attrs)
        if short:
            opts['-' + short.group(1)] = takes_value
        elif re.search(r'\bshort\b', attrs):
            opts['-' + field[0]] = takes_value
    return opts


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
                node = {'children': {}, 'positionals': False, 'options': {}}
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
                            node['options'] = _options(structs[it])
                elif kind == 'struct':
                    st = _subcommand_type(payload)
                    if st:
                        node['children'] = build(st, seen)
                    node['positionals'] = _has_positional(payload)
                    node['options'] = _options(payload)
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
            flat[key] = {'children': set(v['children']),
                         'positionals': v['positionals'],
                         'options': v['options']}
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

# A line may chain several commands — `autumn migrate && autumn seed && autumn
# dev` is real, and appears in the guide. Splitting on shell operators first and
# then matching each segment is what makes the later commands in a chain
# checkable; a single regex over the whole line yields only the first, and the
# rest ride in free. Splitting is safe even inside a quoted argument, because
# only segments that then START with `autumn` are read as invocations.
CHAIN = re.compile(r'&&|\|\||[;|&]')

# `autumn` as the head of a segment, after any prompt or grouping punctuation,
# and after any leading environment assignments — `AUTUMN_ENV=prod autumn db
# backup` is how the guide writes most production commands, and requiring
# `autumn` to head the segment skipped every one of them.
# The `\s` after the name rejects `autumn-cli`, `autumn-web` and `autumn/src/…`;
# anchoring to the segment start rejects `./autumn` and `cd autumn`.
INVOCATION = re.compile(
    r'^[\s(]*\$?\s*(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*autumn\s+(.+)$')

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
    """Yield (line_no, argv, in_fence) for every copyable `autumn …` command.

    `in_fence` separates the two populations that matter for waivers: a command
    inside a fenced shell block is something a reader COPIES, while one in an
    inline span may be a sentence naming a command in order to say it does not
    exist. Only the second is ever waivable.
    """
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
            candidates.append((line, True))
        candidates.extend((span, False) for span in re.findall(r'`([^`\n]+)`', line))
        for text_, in_fence in candidates:
            for segment in CHAIN.split(text_):
                hit = INVOCATION.match(segment.rstrip())
                if hit:
                    yield lineno, hit.group(1).strip(), in_fence


def blocks(text):
    """Map each 1-based line number to the index of its blank-line-separated block.

    Used to tie a waiver to the passage it sits with, rather than to the whole
    file. A marker waives its own block and the one directly above it — which is
    where the sentence being waived actually lives — so an unrelated occurrence
    further down the page is still reported.
    """
    index = {}
    block = 0
    prev_blank = True
    for lineno, line in enumerate(text.split('\n'), 1):
        blank = not line.strip()
        if blank:
            prev_blank = True
        else:
            if prev_blank:
                block += 1
            prev_blank = False
        index[lineno] = block
    return index


def resolve(argv, surface):
    """Return the drifted command path, or None when the line resolves.

    Walks token by token: each token is an option (skipped, along with its value
    when it takes one), a subcommand of the path so far, an argument (which ends
    the walk), or drift.

    Options are walked THROUGH rather than treated as the end of the command,
    because clap accepts them before a subcommand: `autumn migrate
    --with-maintenance down` is real and appears in the guide, and stopping at
    the first `-` left every subcommand written after an option unchecked.
    Whether an option eats the next token is read from the field's type in the
    derive, never guessed — guessing makes `json` in `autumn routes --format
    json` look like a bad subcommand.
    """
    tokens = argv.split()
    if not tokens or not TOKEN.match(tokens[0]):
        return None
    if tokens[0] not in surface:
        return 'autumn ' + tokens[0]
    path = tokens[0]
    i = 1
    while i < len(tokens):
        tok = tokens[i]
        node = surface[path]
        if tok == '--':                         # everything after is arguments
            return None
        if tok.startswith('-') and len(tok) > 1:
            name = tok.split('=', 1)[0]
            if '=' in tok:                      # --name=value, self-contained
                i += 1
                continue
            if name not in node['options']:
                # An option this command does not declare. Flags are out of
                # scope, so this is not reported — but it also cannot be walked
                # past safely, since whether it consumes the next token is
                # unknown. Stop rather than risk a false positive.
                return None
            i += 2 if node['options'][name] else 1
            continue
        if not TOKEN.match(tok):
            return None
        if not node['children']:                # leaf: the rest are arguments
            return None
        if tok in node['children']:
            path = path + ' ' + tok
            i += 1
            continue
        if node['positionals']:                 # unjudgeable: a value, not a name
            return None
        return 'autumn ' + path + ' ' + tok
    return None


def scan(root, surface, files):
    defects, waived = [], 0
    for f in files:
        text = (root / f).read_text(errors='replace')
        line_block = blocks(text)

        # command -> the block indices it is waived in. A waiver covers its own
        # block and the one immediately above it; anything else in the file is
        # still gated. File-wide waivers were the first version of this and were
        # wrong: the `autumn system-test` waiver added for one sentence in
        # system-tests.md also silenced `autumn system-test check` inside a
        # runnable block on the same page — re-admitting, unnoticed, the exact
        # defect this gate was written to remove.
        allowed = collections.defaultdict(set)
        for m in WAIVER.finditer(text):
            marker_line = text.count('\n', 0, m.start()) + 1
            marker_block = line_block[marker_line]
            allowed[m.group(1).strip()].update({marker_block, marker_block - 1})

        for lineno, argv, in_fence in invocations(text):
            bad = resolve(argv, surface)
            if bad is None:
                continue
            command = bad[len('autumn '):]
            # A fenced shell block is copyable, so nothing waives it: a page may
            # NAME a command that does not exist, never hand one over to be run.
            if not in_fence and line_block[lineno] in allowed.get(command, ()):
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
            #[arg(long, value_name = "NAME")]
            shard: Option<String>,
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

    # --- options are walked through, not treated as the end of the command.
    # Regression test for a version that stopped at the first `-` and left every
    # subcommand written after an option unchecked.
    expect(surface['migrate']['options'] == {'--with-maintenance': False, '--shard': True},
           f"option value-taking must come from the field type, got {surface['migrate']['options']}")
    expect(resolve('migrate --with-maintenance status', surface) is None,
           'a boolean option must not hide the subcommand after it')
    expect(resolve('migrate --with-maintenance nope', surface) == 'autumn migrate nope',
           'drift after a boolean option must still be reported')
    expect(resolve('migrate --shard eu status', surface) is None,
           "a value-taking option's value must not be read as a subcommand")
    expect(resolve('migrate --shard=eu status', surface) is None,
           '--name=value is self-contained and consumes no extra token')
    expect(resolve('migrate --unknown-option nope', surface) is None,
           'an undeclared option stops the walk rather than risking a false positive')
    expect(resolve('migrate -- nope', surface) is None,
           'everything after `--` is arguments')

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
    expect(all(a == 'migrate run' for _, a, _ in found), f'bad argv extraction: {found}')
    expect([fenced for _, _, fenced in found] == [True, False],
           'a fenced command and an inline one must be told apart')

    # --- chains: every command on the line, not just the head. Regression test
    # for a version that matched the line once and let the tail ride in free.
    chained = list(invocations('```bash\nautumn migrate && autumn nope ; autumn c\n```'))
    expect([a for _, a, _ in chained] == ['migrate', 'nope', 'c'],
           f'every command in a chain must be extracted, got {chained}')
    expect(list(invocations('```bash\nautumn routes | grep GET\n```'))[0][1] == 'routes',
           'a pipe into a non-autumn command must still yield the autumn one')
    expect(len(list(invocations("```bash\nautumn generate scaffold Post 'a:String{x;y}'\n```"))) == 1,
           'splitting must not manufacture invocations out of a quoted argument')

    # --- environment-prefixed invocations. `AUTUMN_ENV=prod autumn db backup`
    # is how the guide writes production commands; requiring `autumn` to head
    # the segment skipped all 15 of them.
    env_doc = '```bash\nAUTUMN_ENV=prod DATABASE_URL="postgres://x" autumn migrate run\n```'
    expect([a for _, a, _ in invocations(env_doc)] == ['migrate run'],
           f'env assignments must not hide the command: {list(invocations(env_doc))}')
    expect(list(invocations('```bash\ncd autumn && ./autumn migrate\n```')) == [],
           '`cd autumn` and `./autumn` are not invocations of the CLI')

    # --- waivers
    expect(WAIVER.search('<!-- cli-surface-allow: autumn generate island — planned #493 -->'),
           'waiver marker with an em dash must parse')
    expect(not WAIVER.search('<!-- cli-surface-allow: autumn generate island -->'),
           'a waiver without a reason must not parse')

    # A waiver must not reach into a runnable block, and must not reach across
    # the page. Both were true of the first version of this script, and the
    # first one silently re-admitted the defect the gate was written to remove.
    waived_page = '\n'.join([
        'There is no `autumn nope`.',
        '',
        '<!-- cli-surface-allow: autumn nope — named only to say it does not exist -->',
        '',
        '```bash',
        'autumn nope',
        '```',
        '',
        'Unrelated later paragraph mentioning `autumn nope` by mistake.',
    ])
    # A real temp dir, not a fixed path under /tmp: the fixed name collided
    # between concurrent runs and leaked the file whenever scan() raised.
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = pathlib.Path(tmpdir) / 'waiver-selftest.md'
        tmp.write_text(waived_page)
        found_defects, n_waived = scan(tmp.parent, surface, [tmp.name])
    reported = sorted(lineno for _, lineno, _, _ in found_defects)
    expect(n_waived == 1, f'the sentence above the marker must be waived, got {n_waived}')
    expect(reported == [6, 9],
           f'a fenced block and a distant paragraph must both still report, got {reported}')

    for f in failures:
        print('SELF-TEST FAILURE: ' + f, file=sys.stderr)
    print(f"self-test: {13 + 16 + 8 + 4 - len(failures)} passed, {len(failures)} failed")
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
