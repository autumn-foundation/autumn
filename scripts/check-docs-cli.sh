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
#   3. A command whose subcommand clap REQUIRES is not left bare in a runnable
#      line. `autumn db` errors, and so does a bare `autumn` (the root takes a
#      required subcommand: `Usage: autumn <COMMAND>`, exit 2), while `autumn
#      migrate` — an `Option<>` subcommand — is fine. Checked only inside a
#      fence: in prose `autumn deploy` is how English names the command family
#      (49 times in this corpus) and `autumn` alone is just the binary's name,
#      so reporting those would bury the gate in false positives on correct
#      pages — and a gate people learn to ignore has stopped working.
#   4. The same, for a required POSITIONAL: `autumn replay` exits with "the
#      following required arguments were not provided: <CAPSULE>". Required
#      means only what clap cannot supply itself — a positional carrying
#      `default_value` is not required, and reading it as required reported 28
#      correct pages as broken (`autumn upgrade` defaults its `path` to "." and
#      the guide runs it bare 20 times) before that was caught by measuring.
#
# Both `requires_sub` and `requires_arg` were validated against the built
# binary's `--help` usage strings across all 173 command paths: exact agreement,
# no mismatch in either direction.
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
# WHAT IS NOT EXTRACTED, ON PURPOSE. The corpus was swept for every line in a
# copyable context naming `autumn <word>` that this script yields nothing for.
# 38 lines remain, and every one of them was resolved by hand against the parsed
# surface: none names a command that does not exist. So the list below is a
# latent gap, not a live defect, and widening the pattern to reach it would add
# regex surface and false-positive risk for zero defects found today:
#   - Program output quoted back at the reader: banner lines (`🍂 autumn
#     doctor`), log records (`[autumn routes] warning: …`), `--help` transcripts
#     (`Scaffold one with:   autumn new <name> --starter …`), tree diagrams
#     (`└─ autumn build --embed`). These are what the tool PRINTED, not what the
#     reader types, and gating them would make every log line in the guide a
#     hostage to the CLI's argument spelling.
#   - Shell comments inside a block. `shlex` strips them, so `# Run migrations
#     first (autumn seed will error …)` is prose behind a `#`, not a command.
#   - A command split across a prose line wrap OUTSIDE a code span (`… roll ONE
#     back with `autumn deploy` / `rollback --only <host>``, where the wrap
#     falls between two separate spans). A span that wraps INSIDE its backticks
#     is read — markdown renders it as one span, and missing that hid a live
#     `autumn migrate run` in migrations.md behind a `\n> ` break.
#   - Instructional prefixes in `skills/` (`2. Run: autumn migrate`). Reading
#     after an arbitrary prose prefix is the heuristic most likely to start
#     reporting sentences; the same skills name commands in code spans
#     elsewhere, and those ARE gated.
# Wrapper forms that ARE extracted, because they are unambiguous command
# positions rather than prose: after a `--` separator (`kubectl exec deploy/app
# -- autumn …`), inside a quoted wrapper argument (`fly ssh console -C "autumn
# …"`), and after env assignments whose value is a command substitution
# (`AUTUMN_MASTER_KEY=$(cat config/master.key) autumn …`).
#
# HOW A LINE IS READ. The line is TOKENIZED as shell (`shlex`), then reasoned
# about as tokens. It got there the hard way: the extraction started as one
# anchored regex and was widened five times — chains, environment prefixes,
# `--` separators, quoted wrappers, continuations — with each widening having to
# re-state the ones before it, and the fifth silently breaking the fourth. Every
# remaining defect after that was the same root cause, ad-hoc shell parsing:
# an operator inside a quoted value cut a command in half, a space inside one
# ended validation early, a closing quote stayed glued to the last word. `shlex`
# is in the standard library and settles the class, so quoting is no longer a
# case this file reasons about at all.
#
# On the token stream:
#   - operators (`&&`, `||`, `;`, `|`, `&`) separate commands — and cannot do so
#     from inside a quoted value, which is the point;
#   - a quoted option value is ONE token however many spaces it contains, so a
#     subcommand written after it is still judged;
#   - `#` starts a comment, so a shell comment naming a command needs no special
#     case;
#   - a command is recognised at the head of a segment (after any prompt and any
#     environment assignments, whose values may be quoted or `$(…)`), after a
#     `--` separator, or as a single token that is itself a command line — which
#     is how a quoted wrapper argument (`-C "autumn …"`) tokenizes.
# Before tokenizing, backslash continuations inside a fence are folded into one
# logical line, so a command path broken across a line break is still read (the
# defect is reported at the line the command starts on). A line that does not
# tokenize — an apostrophe in prose, an unbalanced quote in a transcript — falls
# back to whitespace splitting rather than being dropped: less accurate, never
# worse than before.
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
import os, re, shlex, subprocess, sys, pathlib, collections, tempfile

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
    """(type, required) for a variant's `#[command(subcommand)]` field.

    `required` is what says whether the bare group is runnable: clap rejects
    `autumn db` outright (the field is `DbCommands`) but accepts `autumn
    migrate` (the field is `Option<MigrateCommands>`), and the CLI's own tests
    assert both.
    """
    m = re.search(
        r'#\[command\([^\]]*\bsubcommand\b[^\]]*\)\]\s*(?:pub\s+)?\w+\s*:\s*'
        r'(Option\s*<\s*)?([A-Za-z0-9_:]+)',
        payload)
    if not m:
        return None, False
    return _last(m.group(2)), not m.group(1)


def _positionals(payload):
    """(takes_any, requires_one) for a variant's positional arguments.

    `takes_any` matters because a positional makes the next token in `autumn db
    pull posts` unjudgeable — it is a table name, not a subcommand — so the walk
    stops there rather than reporting drift it cannot prove.

    `requires_one` matters because clap rejects the command outright without it:
    `autumn replay` exits with "the following required arguments were not
    provided: <CAPSULE>". Required-ness is the field's type — a bare `String` is
    required, `Option<T>` and `Vec<T>` are not — and the field carries no
    `#[arg]` attribute at all in that case, which is why this walks fields
    rather than matching attributes.
    """
    takes_any = requires_one = False
    attrs = []
    pending = ''
    for raw in payload.split('\n'):
        line = raw.strip()
        if pending:                             # an attribute spanning lines
            pending += ' ' + line
            if pending.count('[') <= pending.count(']'):
                attrs.append(pending)
                pending = ''
            continue
        if not line or line.startswith('//') or line in '{}':
            continue
        if line.startswith('#['):
            if line.count('[') > line.count(']'):
                pending = line
            else:
                attrs.append(line)
            continue
        m = re.match(r'(?:pub\s+)?([a-z_][a-z_0-9]*)\s*:\s*(.+?),?\s*$', line)
        attr_text = ' '.join(attrs)
        attrs = []
        if not m:
            continue
        if 'command(' in attr_text:             # the subcommand field, not an arg
            continue
        if re.search(r'\b(long|short)\b', attr_text):
            continue                            # a named option, not a positional
        takes_any = True
        ftype = m.group(2).strip()
        # Not required when clap can supply it: an explicit `default_value`
        # (`autumn upgrade`'s `path: String` defaults to "." and the guide runs
        # it bare 20 times), an `Option<>`, or a `Vec<>` that accepts none.
        # `bool` is a flag, never a required positional. Getting this wrong is
        # expensive in one direction only: a required-ness false positive fires
        # on pages that are correct, which is how a gate loses its readers.
        if (ftype in ('bool',)
                or ftype.startswith(('Option<', 'Vec<'))
                or re.search(r'\bdefault_value', attr_text)):
            continue
        requires_one = True
    return takes_any, requires_one


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
                node = {'children': {}, 'positionals': False, 'options': {},
                        'requires_sub': False, 'requires_arg': False}
                if kind == 'tuple':
                    inner = re.search(r'\(\s*(?:pub\s+)?([A-Za-z0-9_:]+)', payload)
                    if inner:
                        it = _last(inner.group(1))
                        if 'subcommand' in attrs:
                            # `#[command(subcommand)] Variant(SomeCommands)` —
                            # a tuple payload is never Option, so the group
                            # cannot be run bare.
                            node['children'] = build(it, seen)
                            node['requires_sub'] = bool(node['children'])
                        elif it in structs:
                            # `Variant(SomeArgs)` — a flattened args struct that
                            # may itself carry the subcommand (`Upgrade(UpgradeArgs)`).
                            st, required = _subcommand_type(structs[it])
                            if st:
                                node['children'] = build(st, seen)
                                node['requires_sub'] = required and bool(node['children'])
                            node['positionals'], node['requires_arg'] = _positionals(structs[it])
                            node['options'] = _options(structs[it])
                elif kind == 'struct':
                    st, required = _subcommand_type(payload)
                    if st:
                        node['children'] = build(st, seen)
                        node['requires_sub'] = required and bool(node['children'])
                    node['positionals'], node['requires_arg'] = _positionals(payload)
                    node['options'] = _options(payload)
                for spelling in spellings:
                    tree[spelling] = node
            return tree
        if tname in structs:
            st, _ = _subcommand_type(structs[tname])
            return build(st, seen) if st else {}
        return {}

    tree = build('Commands')

    def flatten(t, prefix=''):
        flat = {}
        for k, v in t.items():
            key = (prefix + ' ' + k).strip()
            flat[key] = {'children': set(v['children']),
                         'positionals': v['positionals'],
                         'options': v['options'],
                         'requires_sub': v['requires_sub'],
                         'requires_arg': v['requires_arg']}
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
# EVERY fenced block is read, whatever language it is tagged with. A fence tag
# says what SYNTAX the block is, not whether the block contains commands a
# reader runs, and the corpus makes that distinction constantly: a GitHub
# Actions `run: |` step in a `yaml` fence (`data-scrubbing.md`), a nightly
# backup in a `cron` fence (`daemon.md`), a starter's post-scaffold notes
# inside a `toml` string (`starters.md`). Those are copyable recipes, and an
# allowlist of "shell-ish" tags skipped all of them.
#
# What discriminates is COMMAND POSITION, not the tag — `commands()` requires
# `autumn` to head a command, and that rejects the code and prose that sharing a
# fence with real commands would otherwise drag in. Measured on this corpus:
# reading every fence rather than an allowlist adds zero false positives, and a
# `rust` fence's `"…in-process autumn server"` string is skipped because
# `autumn` there is not in command position.
#
# The residual risk is a quoted string in a code fence that begins with
# `autumn ` and reads as a real command. Nothing in the corpus does, and a
# waiver deliberately cannot silence a fence (see WAIVERS), so the remedy would
# be to fix the page — which is the right outcome for a line that looks exactly
# like a command a reader could run.

# HOW A LINE IS TURNED INTO COMMANDS
#
# The line is TOKENIZED as shell, not split with regexes. Three review rounds
# of this gate were spent on the consequences of not doing that — operators and
# spaces inside a quoted value, a closing quote glued to the last token — and
# each ad-hoc fix left the next one waiting. `shlex` is in the standard library
# and settles the whole class: quotes bind, operators outside them separate
# commands, and `#` starts a comment (which is why a shell comment naming a
# command is no longer a special case in this file).
#
# Tokens, not text, are what the rest of the script reasons about, so a quoted
# option value is ONE token however many spaces or `&&`s are inside it.
_OPERATORS = {'&&', '||', ';', '|', '&', '\n'}

# An environment assignment standing before the command name.
_ENV_TOKEN = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*=')

# Shell metacharacters that separate tokens. Deliberately NOT shlex's default
# set, which also includes `<` and `>`: in a shell those are redirections, but
# in documentation `<name>` is overwhelmingly a placeholder, and splitting it
# turned `autumn migrate --shard <new>` into an option value of `<` followed by
# a bare `new` that read as a phantom subcommand. A redirection target is not a
# command position, so nothing is lost by leaving them inside the token.
_PUNCTUATION = '();&|'

# Prompts and grouping that may precede a command in a transcript. `#` is not
# here because shlex treats it as starting a comment, which is also why a shell
# comment naming a command needs no special case any more.
_PROMPT = {'$', '('}


def tokenize(text):
    """Shell tokens for `text`, or None when it does not tokenize.

    Returns None for unbalanced quotes — common in prose spans and in a
    `--help` transcript quoted into a fence. The caller falls back to plain
    whitespace splitting there, which is what this script did everywhere
    before: strictly less accurate, but never worse than the old behaviour.
    """
    lex = shlex.shlex(text, posix=True, punctuation_chars=_PUNCTUATION)
    lex.whitespace_split = True
    try:
        return list(lex)
    except ValueError:
        return None


def _segments(tokens):
    """Split a token list on shell operators into one list per command."""
    current, out = [], []
    for tok in tokens:
        if tok in _OPERATORS:
            out.append(current)
            current = []
        else:
            current.append(tok)
    out.append(current)
    return out


def commands(text):
    """Yield (display, argv_tokens) for every `autumn …` in command position.

    The three admissible positions are independent of one another, which is
    what keeps adding a fourth from breaking the other three:
      - the head of a command, after any prompt and any environment
        assignments;
      - immediately after a `--` separator (`kubectl exec deploy/app --
        autumn …`);
      - a single token that is itself a command line, which is how a quoted
        wrapper argument tokenizes (`fly ssh console -C "autumn migrate"`).
        Re-tokenizing that token gets the argv exactly, with no quote left
        glued to the last word.
    Anything else — prose, a path, `./autumn`, `cd autumn` — is not a command
    position and is skipped.
    """
    tokens = tokenize(text)
    if tokens is None:
        tokens = text.split()
    for segment in _segments(tokens):
        i = 0
        while i < len(segment) and (segment[i] in _PROMPT or _ENV_TOKEN.match(segment[i])):
            # `NAME=$(cat file)` tokenizes as `NAME=$` `(` `cat` `file` `)`, so
            # a command substitution in the value has to be stepped over as a
            # unit — otherwise the scan stops on `cat` and never reaches the
            # command the assignment was standing in front of.
            if segment[i].endswith('$') and i + 1 < len(segment) and segment[i + 1] == '(':
                depth = 0
                i += 1
                while i < len(segment):
                    if segment[i] == '(':
                        depth += 1
                    elif segment[i] == ')':
                        depth -= 1
                        if depth == 0:
                            i += 1
                            break
                    i += 1
                continue
            i += 1
        if i < len(segment) and segment[i] == 'autumn':
            yield ' '.join(segment[i + 1:]), segment[i + 1:]
        for j, tok in enumerate(segment):
            if tok == '--' and j + 1 < len(segment) and segment[j + 1] == 'autumn':
                yield ' '.join(segment[j + 2:]), segment[j + 2:]
            elif tok.startswith('autumn ') and (inner := tokenize(tok)):
                # The inner command line goes through the SAME segment split as
                # the outer one: `-C "autumn migrate && autumn nope"` is a chain
                # too, and yielding it as one argv left everything after the
                # first operator unjudged.
                for part in _segments(inner):
                    if part and part[0] == 'autumn' and len(part) > 1:
                        yield ' '.join(part[1:]), part[1:]


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
    held = []          # parts of a backslash-continued line, oldest first
    held_at = None     # the line the continued command STARTED on

    def flush(line, lineno):
        """Fold a backslash continuation into one logical line."""
        nonlocal held, held_at
        if line.rstrip().endswith('\\'):
            held.append(line.rstrip()[:-1])
            if held_at is None:
                held_at = lineno
            return None, None
        if held:
            joined = ' '.join(held) + ' ' + line
            at = held_at
            held, held_at = [], None
            return joined, at
        return line, lineno

    # Prose accumulates into paragraphs before its code spans are read, because
    # a markdown inline span may WRAP across source lines and still render as
    # one span. Reading prose line by line missed every wrapped one — 72 of
    # them in this corpus — including `autumn migrate\n> run` in
    # `migrations.md`, which rendered as `autumn migrate run`: the same phantom
    # subcommand this gate was written to remove, left behind by a text
    # substitution that could not match across the break either.
    para = []          # (offset_in_joined, lineno, text) for the current paragraph
    para_len = 0

    def take_paragraph():
        """Read the code spans out of the accumulated prose, then clear it."""
        nonlocal para, para_len
        if not para:
            return
        joined = ' '.join(part for _, _, part in para)
        for m in re.finditer(r'`([^`]+)`', joined):
            at = para[0][1]
            for offset, ln, _ in para:          # map the span back to its line
                if offset <= m.start():
                    at = ln
            yield at, m.group(1)
        para, para_len = [], 0

    def add_prose(line, lineno):
        """Accumulate one prose line, with any blockquote marker stripped."""
        nonlocal para_len
        stripped = re.sub(r'^\s*>+\s?', '', line)
        para.append((para_len, lineno, stripped))
        para_len += len(stripped) + 1

    for lineno, line in enumerate(text.split('\n'), 1):
        m = re.match(r'^\s*(`{3,}|~{3,})\s*([A-Za-z0-9_+-]*)', line)
        if m:
            held, held_at = [], None            # a fence boundary ends any hold
            yield from _spans(take_paragraph(), commands)
            if fence is None:
                fence, lang = m.group(1)[0], m.group(2).lower()
            elif line.strip().startswith(fence * 3):
                fence, lang = None, None
            continue

        if fence is None:
            if not line.strip():                # a blank line ends a paragraph
                yield from _spans(take_paragraph(), commands)
            else:
                add_prose(line, lineno)
            continue

        # Inside a fence, whatever the tag. Continuations are joined before
        # scanning — `autumn maintenance on \` + `--reason "…"` is one command,
        # and the guide writes five of them on that page alone. Scanning the
        # physical lines instead yields the argv `\`, and the command path split
        # across the break goes unchecked.
        logical, at = flush(line, lineno)
        if logical is None:
            continue
        # Operator splitting happens inside `commands()`, on tokens rather than
        # on the raw text, so an operator inside a quoted value cannot cut a
        # command in half.
        for display, argv in commands(logical):
            yield at, display, argv, True
        # A span inside a fence (a hint line quoting a command back) is still a
        # span, and cannot wrap: the fence preserves line breaks.
        for span in re.findall(r'`([^`\n]+)`', line):
            for display, argv in commands(span):
                yield lineno, display, argv, False

    yield from _spans(take_paragraph(), commands)


def _spans(pairs, commands):
    """Turn (lineno, span_text) pairs into invocation tuples."""
    for lineno, span in pairs:
        for display, argv in commands(span):
            yield lineno, display, argv, False


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


def resolve(tokens, surface, runnable=False):
    """Return the drifted command path, or None when the command resolves.

    Takes SHELL TOKENS, not a string to be split on whitespace: a quoted option
    value is one token however many spaces are inside it. Splitting on
    whitespace made `--shard "eu west" nope` consume `"eu` as the value and
    then stop at `west"`, so the phantom subcommand behind it was never judged.

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
    if not tokens:
        # `autumn` with nothing after it. The root takes a required subcommand
        # (`Usage: autumn <COMMAND>`, exit 2), so in a runnable line this is the
        # same defect as a bare `autumn db` — and in prose it is just the name
        # of the binary, which the corpus writes constantly.
        return 'autumn' if runnable else None
    if not TOKEN.match(tokens[0]):
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

    # Tokens exhausted on a group whose subcommand clap requires: `autumn db`
    # exits with an error, so a fenced block containing it hands the reader a
    # line that cannot run.
    #
    # ONLY inside a fence. In prose, `autumn deploy` is how English names the
    # command family — "every host `autumn deploy` manages" — and the corpus
    # does that 49 times for `deploy` alone and 15 for `generate`. Reporting
    # those would bury the gate in false positives on correct pages, which is a
    # worse outcome than the gap: a page that names a command group is not
    # telling anyone to run it bare.
    if runnable and surface[path]['requires_sub']:
        return 'autumn ' + path

    # Same shape one level down: `autumn replay` exits with "the following
    # required arguments were not provided: <CAPSULE>". The walk only reaches
    # here having consumed no positional — supplying one ends the walk earlier,
    # since a value cannot be told from a subcommand name.
    if runnable and surface[path]['requires_arg']:
        return 'autumn ' + path
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

        for lineno, display, argv, in_fence in invocations(text):
            bad = resolve(argv, surface, runnable=in_fence)
            if bad is None:
                continue
            command = bad[len('autumn '):]
            # A fenced shell block is copyable, so nothing waives it: a page may
            # NAME a command that does not exist, never hand one over to be run.
            if not in_fence and line_block[lineno] in allowed.get(command, ()):
                waived += 1
                continue
            defects.append((f, lineno, bad, display))
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
        Replay {
            /// Path to the capsule to replay.
            capsule: String,
            #[arg(short, long)]
            package: Option<String>,
        },
        New {
            name: Option<String>,
        },
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
        #[arg(value_name = "PATH", default_value = ".")]
        path: String,
    }
    enum UpgradeCommands { Apply }
    '''
    surface = build_surface([fake])
    failures = []

    def tk(argv):
        """Shell-tokenize a bare argv the way `commands()` would."""
        return tokenize(argv) or argv.split()

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
    expect(resolve(tk('migrate run'), surface) == 'autumn migrate run',
           'a phantom subcommand must be reported')
    expect(resolve(tk('migrate'), surface) is None,
           'a command with an OPTIONAL subcommand is valid bare')
    expect(resolve(tk('migrate --with-maintenance'), surface) is None,
           'a flag must not be read as a subcommand')
    expect(resolve(tk('db pull posts'), surface) is None,
           'a positional value must not be read as a subcommand')
    expect(resolve(tk('db pull posts --dry-run'), surface) is None,
           'positional plus flag must resolve')
    expect(resolve(tk('c'), surface) is None, 'an alias must resolve')
    expect(resolve(tk('nope'), surface) == 'autumn nope',
           'an unknown top-level command must be reported')
    expect(resolve(tk('console extra'), surface) is None,
           'a leaf command consumes its remaining tokens as arguments')
    expect(resolve(tk('$SOMETHING'), surface) is None,
           'a non-command token must be ignored')

    # --- options are walked through, not treated as the end of the command.
    # Regression test for a version that stopped at the first `-` and left every
    # subcommand written after an option unchecked.
    expect(surface['migrate']['options'] == {'--with-maintenance': False, '--shard': True},
           f"option value-taking must come from the field type, got {surface['migrate']['options']}")
    expect(resolve(tk('migrate --with-maintenance status'), surface) is None,
           'a boolean option must not hide the subcommand after it')
    expect(resolve(tk('migrate --with-maintenance nope'), surface) == 'autumn migrate nope',
           'drift after a boolean option must still be reported')
    expect(resolve(tk('migrate --shard eu status'), surface) is None,
           "a value-taking option's value must not be read as a subcommand")
    expect(resolve(tk('migrate --shard=eu status'), surface) is None,
           '--name=value is self-contained and consumes no extra token')
    expect(resolve(tk('migrate --unknown-option nope'), surface) is None,
           'an undeclared option stops the walk rather than risking a false positive')
    expect(resolve(tk('migrate -- nope'), surface) is None,
           'everything after `--` is arguments')

    # --- quoting. These are what shell-aware tokenization buys: splitting on
    # whitespace consumed `"eu` as the option value and then stopped at `west"`,
    # so the phantom subcommand behind a quoted value was never judged.
    expect(resolve(tk('migrate --shard "eu west" nope'), surface) == 'autumn migrate nope',
           'a quoted option value is ONE token, so drift behind it is still judged')
    expect(resolve(tk('migrate --shard "eu west" status'), surface) is None,
           'a real subcommand behind a quoted value must still resolve')
    expect([a for _, a in commands('autumn migrate --shard "eu&&us" nope')]
           == [['migrate', '--shard', 'eu&&us', 'nope']],
           'an operator INSIDE a quoted value must not split the command')
    expect([a for _, a in commands('autumn migrate --shard <new>')]
           == [['migrate', '--shard', '<new>']],
           'a `<placeholder>` must stay one token — splitting it made the '
           'placeholder name read as a phantom subcommand')

    # --- tokenization falls back rather than dropping the line.
    expect(tokenize("don't") is None, 'an unbalanced quote must report as untokenizable')
    expect([a for _, a in commands("autumn migrate don't")][0][:2] == ['migrate', "don't"],
           'an untokenizable line falls back to whitespace splitting, not silence')

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
    expect(all(d == 'migrate run' for _, d, _, _ in found), f'bad argv extraction: {found}')
    expect([fenced for _, _, _, fenced in found] == [True, False],
           'a fenced command and an inline one must be told apart')

    # --- chains: every command on the line, not just the head. Regression test
    # for a version that matched the line once and let the tail ride in free.
    chained = list(invocations('```bash\nautumn migrate && autumn nope ; autumn c\n```'))
    expect([d for _, d, _, _ in chained] == ['migrate', 'nope', 'c'],
           f'every command in a chain must be extracted, got {chained}')
    expect(list(invocations('```bash\nautumn routes | grep GET\n```'))[0][1] == 'routes',
           'a pipe into a non-autumn command must still yield the autumn one')
    expect(len(list(invocations("```bash\nautumn generate scaffold Post 'a:String{x;y}'\n```"))) == 1,
           'splitting must not manufacture invocations out of a quoted argument')

    # --- environment-prefixed invocations. `AUTUMN_ENV=prod autumn db backup`
    # is how the guide writes production commands; requiring `autumn` to head
    # the segment skipped all 15 of them.
    env_doc = '```bash\nAUTUMN_ENV=prod DATABASE_URL="postgres://x" autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(env_doc)] == ['migrate run'],
           f'env assignments must not hide the command: {list(invocations(env_doc))}')
    expect(list(invocations('```bash\ncd autumn && ./autumn migrate\n```')) == [],
           '`cd autumn` and `./autumn` are not invocations of the CLI')
    subst = '```bash\nAUTUMN_MASTER_KEY=$(cat config/master.key) autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(subst)] == ['migrate run'],
           'an env value may be a command substitution containing spaces')

    # --- wrappers handing the command to a remote shell.
    wrapped = '```bash\nkubectl exec deploy/app -- autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(wrapped)] == ['migrate run'],
           f'a command after a `--` separator must be read: {list(invocations(wrapped))}')
    quoted = '```bash\nfly ssh console -C "autumn migrate run"\n```'
    expect([d for _, d, _, _ in invocations(quoted)] == ['migrate run'],
           f'a quoted wrapper argv must END at the closing quote, not swallow it — '
           f'otherwise the last token stops looking like a command name and drift '
           f'there is silently accepted: {list(invocations(quoted))}')
    qenv = '```bash\nRUSTFLAGS="-C opt-level=3" autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(qenv)] == ['migrate run'],
           f'a quoted env value containing spaces must not swallow the command: '
           f'{list(invocations(qenv))}')

    # --- backslash continuations are shell syntax and are folded into one
    # logical line; the defect is reported where the command STARTS.
    cont = '```bash\nautumn migrate \\\n    run\n```'
    expect([(n, d) for n, d, _, _ in invocations(cont)] == [(2, 'migrate run')],
           f'a continued command must be joined and reported at its first line: '
           f'{list(invocations(cont))}')
    expect(list(invocations('```bash\nautumn migrate \\\n```')) == [],
           'a hold left open at the fence boundary must not leak into the next block')

    # --- an inline span may WRAP across source lines and still render as one
    # span. Missing that hid a live `autumn migrate run` in migrations.md — the
    # very phantom this gate was built to remove — behind a `\n> ` break.
    wrapped = 'Enforcement is `autumn migrate\nrun` in CI.'
    expect([d for _, d, _, _ in invocations(wrapped)] == ['migrate run'],
           f'a wrapped inline span must be read as one span: {list(invocations(wrapped))}')
    quoted_bq = '> Recording happens via `autumn migrate\n> run` and nothing else.'
    expect([d for _, d, _, _ in invocations(quoted_bq)] == ['migrate run'],
           f'a blockquote marker must not break a wrapped span: {list(invocations(quoted_bq))}')
    expect([n for n, _, _, _ in invocations(wrapped)] == [1],
           'a wrapped span is reported at the line it starts on')
    expect(list(invocations('One paragraph with `autumn\n\nmigrate` split by a blank line.')) == [],
           'a blank line ends a paragraph, so a span cannot span two of them')

    # --- a chain inside a quoted wrapper is still a chain.
    inner_chain = '```bash\nfly ssh -C "autumn migrate && autumn nope"\n```'
    expect([d for _, d, _, _ in invocations(inner_chain)] == ['migrate', 'nope'],
           f'the inner command line must be segmented too: {list(invocations(inner_chain))}')

    # --- groups whose subcommand clap requires. Only a RUNNABLE line is judged:
    # in prose `autumn db` is how English names the command family.
    expect(surface['db']['requires_sub'], 'a tuple subcommand payload is required')
    expect(not surface['migrate']['requires_sub'], 'an Option<> subcommand is not required')
    expect(not surface['upgrade']['requires_sub'],
           'an Option<> subcommand inside an args struct is not required')
    expect(resolve(tk('db'), surface, runnable=True) == 'autumn db',
           'a fenced bare required group must be reported')
    expect(resolve(tk('db'), surface, runnable=False) is None,
           'the same group named in prose must NOT be reported')
    expect(resolve(tk('migrate'), surface, runnable=True) is None,
           'a bare group with an optional subcommand runs fine')
    expect(resolve([], surface, runnable=True) == 'autumn',
           'a runnable line that is only `autumn` must be reported — the root '
           'takes a required subcommand and exits 2')
    expect(resolve([], surface, runnable=False) is None,
           'prose naming the binary `autumn` must NOT be reported')

    # --- required POSITIONALS, the same shape one level down. The first
    # version of this reported 28 false positives on correct pages, because it
    # read a positional carrying `default_value` as required — `autumn upgrade`
    # takes `path: String` defaulting to "." and the guide runs it bare 20
    # times. Required-ness is only what clap cannot supply itself.
    expect(surface['replay']['requires_arg'],
           'a bare `capsule: String` field is a required positional')
    expect(not surface['upgrade']['requires_arg'],
           'a positional with default_value is NOT required')
    expect(not surface['db pull']['requires_arg'], 'a Vec<> positional is not required')
    expect(not surface['new']['requires_arg'], 'an Option<> positional is not required')
    expect(resolve(tk('replay'), surface, runnable=True) == 'autumn replay',
           'a runnable line missing a required positional must be reported')
    expect(resolve(tk('replay capsule.json'), surface, runnable=True) is None,
           'supplying the positional resolves')
    expect(resolve(tk('replay'), surface, runnable=False) is None,
           'prose naming the command must NOT be reported')
    expect(resolve(tk('upgrade'), surface, runnable=True) is None,
           'a defaulted positional means the bare command runs')

    # --- every fence is read, whatever its tag: a YAML `run: |` step and a
    # cron line are copyable recipes, and command position keeps a code fence
    # from becoming noise.
    yaml_run = '```yaml\n- name: check\n  run: |\n    autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(yaml_run)] == ['migrate run'],
           f'a command in a YAML run: block must be read: {list(invocations(yaml_run))}')
    cron = '```cron\n0 2 * * *  cd /srv && AUTUMN_ENV=prod autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(cron)] == ['migrate run'],
           f'a cron command line must be read: {list(invocations(cron))}')
    rust = '```rust\nlet m = "failed to build the in-process autumn server";\n```'
    expect(list(invocations(rust)) == [],
           'a code fence naming autumn outside command position stays quiet')
    # The quote rule must require `autumn` immediately inside the quote, or
    # every apostrophe in prose becomes a command position.
    expect(list(invocations("```bash\n# don't run autumn migrate here\n```")) == [],
           'an apostrophe elsewhere in the line is not a command position')
    expect(list(invocations('```bash\n# set the mode to "autumn" first\n```')) == [],
           'a quoted bare word that is not followed by a command is not an invocation')

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
    print(f"self-test: {13 + 39 + 29 + 4 - len(failures)} passed, {len(failures)} failed")
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
            path = bad[len('autumn '):]
            if bad != 'autumn' and path not in surface:
                note = 'is not a command'
            elif bad != 'autumn' and surface[path]['requires_arg'] \
                    and not surface[path]['requires_sub']:
                note = 'needs an argument'
            else:
                note = 'needs a subcommand'
            line = ('autumn ' + argv).strip()
            print(f'{f}:{lineno}: `{bad}` {note}  (line: {line})')
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
