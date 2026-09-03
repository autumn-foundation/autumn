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
#   2b. A command is recognised after a scalar key that means "a command
#      follows" (`command:`, `run:`, `Run:`), and the binary is recognised
#      however its path is spelled — `./autumn`, `/usr/local/bin/autumn`, or as
#      the value of a systemd `ExecStart=`.
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
# Both `requires_sub` and the required-positional COUNT were validated against
# the built binary's `--help` usage strings across all 173 command paths: exact
# agreement, no mismatch in either direction.
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
#   - An ARBITRARY prose prefix. A bounded set of scalar keys that mean "this is
#     a command" — `command:`, `run:`, `entrypoint:`, `cmd:`, `exec:`,
#     `script:` — IS read, since Compose writes `command: autumn migrate`, a
#     workflow step writes `run: autumn routes`, and the skills write `Run:
#     autumn migrate` as an instruction. An earlier version of this script
#     lumped those in with arbitrary prefixes and skipped all 14; the reasoning
#     was right about arbitrary prefixes and wrong about these, which are the
#     keys whose whole meaning is that a command follows.
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
    """(takes_any, required_count) for a variant's positional arguments.

    `takes_any` matters because a positional makes the next token in `autumn db
    pull posts` unjudgeable — it is a table name, not a subcommand — so the walk
    stops there rather than reporting drift it cannot prove.

    `required_count` matters because clap rejects the command outright without
    them, and it is a COUNT rather than a flag because a command can require
    more than one: `autumn generate controller pages` supplies `name` and stops,
    but `actions` is `required = true` as well, so clap still rejects it.
    `autumn replay` exits with "the following required arguments were not
    provided: <CAPSULE>". Required-ness is the field's type — a bare `String` is
    required, `Option<T>` and `Vec<T>` are not — and the field carries no
    `#[arg]` attribute at all in that case, which is why this walks fields
    rather than matching attributes.
    """
    takes_any, required = False, 0
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
        if ftype in ('bool',) or re.search(r'\bdefault_value', attr_text):
            continue
        if ftype.startswith('Option<'):
            continue
        if ftype.startswith('Vec<'):
            # A `Vec` positional is optional unless clap is told otherwise.
            # `generate controller` marks its `actions: Vec<String>` with
            # `required = true`, so it needs at least one on top of `name`.
            if re.search(r'\brequired\s*=\s*true', attr_text):
                required += 1
            continue
        required += 1
    return takes_any, required


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
                        'requires_sub': False, 'required_args': 0}
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
                            node['positionals'], node['required_args'] = _positionals(structs[it])
                            node['options'] = _options(structs[it])
                elif kind == 'struct':
                    st, required = _subcommand_type(payload)
                    if st:
                        node['children'] = build(st, seen)
                        node['requires_sub'] = required and bool(node['children'])
                    node['positionals'], node['required_args'] = _positionals(payload)
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
                         'required_args': v['required_args']}
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
# `;;` (and bash's `;&` / `;;&` fallthrough spellings) terminate a case ARM,
# so they separate commands exactly as `;` does — without them the arm's argv
# ran on into `;; esac`.
_OPERATORS = {'&&', '||', ';', '|', '&', '\n', ';;', ';&', ';;&'}

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
# Tokens that stand in front of a command without being one: a shell prompt,
# a subshell's `(`, and a brace group's `{`. `{ autumn migrate; }` runs the
# command inside the group — bash's own help calls it `{ COMMANDS ; }` — and
# leaving `{` as the segment head meant the scan never reached it.
_PROMPT = {'$', '(', '{'}

# Shell keywords that stand in front of a command without being one. `if autumn
# migrate; then …` runs `autumn migrate` as the condition, and leaving `if` as
# the segment head meant the scan never reached it.
_CONTROL = {'if', 'elif', 'while', 'until', 'then', 'else', 'do', 'done', 'fi',
            # `case … in` heads a construct whose ARMS hold the commands; the
            # words between it and the arm are patterns, not a program.
            'case', 'in', 'esac',
            'time', '!', 'exec', 'command', 'nohup', 'sudo', 'env', 'xargs',
            'timeout', 'nice', 'ssh',
            # `eval` combines its arguments into shell input and runs the
            # result, so an UNQUOTED `eval autumn …` is that command with a
            # prefix; the quoted form is handled as a command string below.
            'eval',
            # …and the launchers that run a DIRECT command operand, not only
            # one after a `--`. Each was listed as a program whose separator
            # introduces a command, which covered `systemd-run -- autumn …`
            # and nothing else — so the ordinary spelling went unread.
            'systemd-run', 'flock', 'chroot', 'nsenter', 'doas'}

# Options a wrapper takes that consume a SEPARATE value token. `env` and
# `sudo` both put flags between the keyword and the command they launch —
# `env [OPTION]... [NAME=VALUE]... COMMAND`, `sudo [OPTION]... COMMAND` — and
# the scan stopped on the first flag, so the command behind it went unread.
#
# Only the value-taking spellings need listing: anything else starting with
# `-` consumes just itself, and `--opt=value` is self-contained either way. An
# option this table does not know is treated as a flag, and when that is wrong
# the head lands on something that is not the binary — so the line degrades to
# silence rather than to a false report.
_WRAPPER_OPTS = {
    'env': {'-u', '--unset', '-C', '--chdir', '-S', '--split-string'},
    'sudo': {'-u', '--user', '-g', '--group', '-p', '--prompt',
             '-D', '--chdir', '-R', '--chroot', '-T', '--command-timeout'},
    # The shell builtins in `_CONTROL` take options too, and were being
    # stepped over without them: `command -p autumn …` left `-p` as the
    # prospective executable. `command [-pVv]` and `time [-p]` are flags
    # only; `exec [-cl] [-a name]` has one that takes a value.
    'command': set(),
    'time': set(),
    'nohup': set(),
    'exec': {'-a'},
    # `xargs [OPTION]... COMMAND [INITIAL-ARGS]...` runs COMMAND directly, not
    # only after a `--`. It was recognised for the separator form alone, so
    # the ordinary `… | xargs autumn migrate` went unread entirely.
    # Only the spellings whose value is a SEPARATE token. `-e/--eof`,
    # `-i/--replace` and `-l` take an OPTIONAL attached value (`--eof[=END]`),
    # so bare they are flags — listing them here made the walk swallow the
    # command as their value and `xargs -i autumn …` went unread.
    'xargs': {'-a', '--arg-file', '-E', '-I', '-L', '--max-lines',
              '-n', '--max-args', '-P', '--max-procs', '-s', '--max-chars',
              '-d', '--delimiter', '--process-slot-var'},
    'timeout': {'-k', '--kill-after', '-s', '--signal'},
    'nice': {'-n', '--adjustment'},
    # `systemd-run [OPTIONS...] COMMAND` and `flock [options] <file> <command>`
    # — the usage lines the installed binaries print. Only the SEPARATED
    # value-taking spellings need listing; the attached `--unit=x` form is
    # self-contained, and an option missing from this table is read as a flag,
    # which degrades to silence rather than to a false report.
    'systemd-run': {'-H', '--host', '-M', '--machine', '-u', '--unit',
                    '-p', '--property', '-E', '--setenv', '--description',
                    '--slice', '--expand-environment', '--service-type',
                    '--uid', '--gid', '--nice', '--working-directory',
                    '--path-property', '--socket-property', '--on-active',
                    '--on-boot', '--on-startup', '--on-unit-active',
                    '--on-unit-inactive', '--on-calendar', '--timer-property'},
    # `flock -c '…'` hands its string to a shell, but the option's owner is
    # the wrapper rather than the word before it, which this file's `-c`
    # ownership rule does not yet express — so that spelling stays unread
    # (silence, not a false report). The direct operand form is what the
    # corpus and the finding are about.
    'flock': {'-w', '--timeout', '-E', '--conflict-exit-code',
              '-c', '--command'},
    'chroot': {'--userspec', '--groups'},
    # `ssh [options] destination [command [argument ...]]`. Unlike every other
    # entry in this table, this one is NOT read off a locally installed
    # binary — ssh is not present in this environment — so it comes from the
    # documented option set instead. Its `-c` is a CIPHER, which is why ssh is
    # deliberately absent from `_SHELL_C_OPTS`.
    'ssh': {'-b', '-c', '-D', '-E', '-e', '-F', '-I', '-i', '-J', '-L', '-l',
            '-m', '-O', '-o', '-p', '-Q', '-R', '-S', '-W', '-w'},
    # nsenter spells almost everything with an OPTIONAL attached value
    # (`--setuid[=<uid>]`, `--root[=<dir>]`, `--wd[=<dir>]`), so those are
    # flags when written bare; only `--target` and `--wdns` take a separate
    # one. Listing the optional ones ate the command after them.
    'nsenter': {'-t', '--target', '-W', '--wdns'},
    'doas': {'-u', '-C'},
}

# Wrappers that take POSITIONAL operands of their own before the command:
# `timeout [OPTION] DURATION COMMAND`. Stepping over the options alone left
# the duration looking like the executable.
_WRAPPER_OPERANDS = {'timeout': 1, 'flock': 1, 'chroot': 1, 'ssh': 1}


# Options that turn a wrapper into an INSPECTION: it describes the names that
# follow instead of running them. `command -v autumn migrate` performs two
# lookups and executes nothing, so reading the tail as a command reported drift
# on a line that runs no autumn at all.
_INSPECT_OPTS = {'command': 'vV'}


def _inspects(segment, i, name):
    """True when this wrapper is being asked to DESCRIBE rather than run."""
    letters = _INSPECT_OPTS.get(name)
    if not letters:
        return False
    while i < len(segment) and segment[i].startswith('-') and len(segment[i]) > 1:
        tok = segment[i]
        if not tok.startswith('--') and any(ch in letters for ch in tok[1:]):
            return True
        i += 1
    return False


def _skip_wrapper_options(segment, i, value_opts):
    """Step past a wrapper's own options so its COMMAND becomes the head."""
    while i < len(segment):
        tok = segment[i]
        if tok == '-':                          # `env - COMMAND`: empty env
            i += 1
            continue
        if not tok.startswith('-') or len(tok) < 2:
            break
        i += 1
        if tok.startswith('--'):
            if '=' not in tok and tok in value_opts:
                i += 1                          # separated value
        else:
            # A short CLUSTER: `ssh -vp 22` is `-v` (flag) then `-p` (value).
            # Testing the whole `-vp` against the value set found nothing and
            # treated it as one flag, so `22` was read as the destination and
            # the command behind it was lost. Only the LAST letter can take a
            # SEPARATE token; a value-taking letter before the end carries its
            # value attached and ends the cluster. A letter the table does not
            # list is a flag, the same assumption the whole-token test made.
            for pos in range(1, len(tok)):
                if ('-' + tok[pos]) in value_opts:
                    if pos == len(tok) - 1:
                        i += 1                  # value is the next token
                    break                       # attached value ends the group
    return i

# A crontab line puts the command after a five-field schedule, so `autumn` is
# not the segment head and the scan stopped on the minute field. The corpus
# only ever writes the `0 2 * * *  cd /srv && … autumn …` form, where the `&&`
# creates a fresh segment and the command was reached by accident — a reader
# scheduling `autumn db backup` directly, which is the ordinary crontab line,
# went ungated.
#
# A field is `*`, a number, a list, a range, a step, or a month/day name.
# `@daily` and friends replace the whole schedule with one token.
_CRON_FIELD = re.compile(r'^(?:\*|[0-9]{1,2}|[A-Za-z]{3})'
                         r'(?:[-/,][0-9A-Za-z*]{1,3})*$')
_CRON_SHORTCUT = re.compile(r'^@(?:reboot|yearly|annually|monthly|weekly|'
                            r'daily|midnight|hourly)$')
_CRON_USER = re.compile(r'^[a-z_][a-z0-9_-]{0,31}$')


def _cron_prefix(segment):
    """Index past a leading crontab schedule, or 0 when there is none.

    Deliberately narrow. Five schedule fields in a row is a shape no shell
    command line has, so consuming them cannot move the head of a real
    command; and the head that follows still has to look like the binary
    before anything is reported, so a misread here degrades to silence.

    The system-crontab user field (`/etc/cron.d`: `0 2 * * * root autumn …`)
    is indistinguishable from a command name in general, so it is stepped over
    only when the token after it is something that LAUNCHES: the binary
    itself, or a wrapper that reaches it (`root flock /var/lock/x autumn …`,
    which the narrower "must be the binary" test could not see past). That
    cannot manufacture a false positive: the schedule has already matched, and
    the invocation is still read from the executable itself either way.
    """
    if segment and _CRON_SHORTCUT.match(segment[0]):
        i = 1
    elif len(segment) > 5 and all(_CRON_FIELD.match(t) for t in segment[:5]):
        i = 5
    else:
        return 0
    if i < len(segment) and not _autumn_exe(segment[i]) \
            and _CRON_USER.match(segment[i]) and i + 1 < len(segment) \
            and (_autumn_exe(segment[i + 1])
                 or segment[i + 1] in _CONTROL):
        i += 1
    return i


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


def _escaped(text, i):
    """True when the character at `i` is escaped by a backslash.

    PARITY, not presence: a backslash escapes the one after it, so a run of
    them pairs off and only an ODD run leaves the next character escaped.
    `"\\\\$(autumn …)"` is a literal backslash followed by a REAL substitution,
    and testing only the immediate predecessor suppressed it. The same question
    decides whether a trailing backslash continues a line and whether a `<<`
    opens a heredoc, so all three ask it here.
    """
    run = 0
    while i - run - 1 >= 0 and text[i - run - 1] == '\\':
        run += 1
    return run % 2 == 1


def _open_quote(text, quote=None):
    """The shell quote left OPEN at the end of `text`, or None.

    A quote spans physical lines — `printf '%s\\n' '` opens a string that the
    next line continues — so the lines inside one are string DATA, not commands
    the reader runs. Reading each physical line on its own left the opener
    untokenizable, fell back to whitespace splitting, and reported the text
    inside the string as a runnable command on a correct page.
    """
    i, n = 0, len(text)
    while i < n:
        ch = text[i]
        if quote == "'":
            # Single quotes are literal all the way to the next one; a
            # backslash inside them escapes nothing.
            if ch == "'":
                quote = None
        elif quote == '"':
            if ch == '\\':
                i += 2
                continue
            if ch == '"':
                quote = None
        else:
            if ch == '\\':
                i += 2
                continue
            # An unquoted `#` starts a comment, and the rest of the line —
            # apostrophes in English included — is text.
            if ch == '#' and (i == 0 or text[i - 1] in _COMMENT_BEFORE):
                return quote
            if ch in '"\'':
                quote = ch
        i += 1
    return quote


def _paren_delta(tok):
    """Net parenthesis depth contributed by a token.

    `shlex` COALESCES a run of punctuation into one token, so the end of a
    nested substitution arrives as a single `))`. Testing for an exact `)` never
    returned to depth zero there, and everything after it — including the
    command — was swallowed into the assignment.

    Only a token made entirely of shell punctuation is counted, so parentheses
    inside an ordinary word or a quoted string (which posix `shlex` hands back
    with its quotes removed) do not move the depth.
    """
    if tok and all(c in _PUNCTUATION for c in tok):
        return tok.count('(') - tok.count(')')
    return 0


def _segments(tokens, classify=None):
    """Split a token list on shell operators into one list per command.

    An operator inside a COMMAND SUBSTITUTION does not separate commands:
    `KEY=$(cat secret | tr -d '\n') autumn migrate` is one command whose
    assignment happens to contain a pipe, and splitting there threw away the
    `autumn migrate` that followed. Depth is tracked only for `$(`, not for a
    plain subshell — `(autumn migrate && autumn seed)` really does hold two
    commands, and both should still be judged.
    """
    # Which token DECIDES an operator or a paren may differ from the token
    # that is kept: posix shlex strips quotes, so a quoted `';'` arrives
    # looking exactly like a separator and started a new command — reporting
    # the words after it as an invocation on a page that only prints them.
    # `classify` is the same line with every quoted run blanked, tokenized in
    # parallel, so the decision is positional while the argv stays real.
    if classify is None or len(classify) != len(tokens):
        classify = tokens
    current, out, depth, prev = [], [], 0, ''
    for tok, cls in zip(tokens, classify):
        nested = depth > 0
        # `<(` and `>(` are PROCESS substitutions and bracket their contents
        # the same way `$(` does. Tracking only `$(` left their closing paren
        # at depth zero, where the case-arm rule below took it for a boundary
        # and reported the command inside them twice — once from the
        # substitution walk and once as a segment head.
        if cls.startswith('(') and prev.endswith(('$', '<', '>')):
            depth += _paren_delta(cls)
            nested = True
        elif depth:
            depth = max(0, depth + _paren_delta(cls))
        # `2>&1` is ONE redirection, but `&` is in the punctuation set so shlex
        # hands back `2>` `&` `1`. Splitting there made the command after a
        # leading `2>&1` start a new segment headed by `1`, and the whole line
        # went unread. The three are rejoined rather than merely not split, so
        # the result is a single self-contained redirection token.
        if current and cls == '&' and current[-1].endswith(('>', '<')):
            current[-1] += tok
            prev = tok
            continue
        # `2>&-` CLOSES a descriptor, so the duplication target is `-` as well
        # as a number. Accepting only digits left the `-` behind as its own
        # token, and a leading `2>&-` made it the segment head with the real
        # command behind it unread.
        if current and cls in ('-',) and current[-1].endswith(('>&', '<&')):
            current[-1] += tok
            prev = tok
            continue
        if current and cls.isdigit() and current[-1].endswith(('>&', '<&')):
            current[-1] += tok
            prev = tok
            continue
        # shlex COALESCES a run of punctuation, so a substitution's closer and
        # the operator after it arrive as one token: `echo $(date); autumn …`
        # hands back `);`. Testing the whole token against the operator set
        # matched neither half, the line never split, and the command after the
        # separator went unread. The parens are consumed for depth (already
        # done above) and what REMAINS is tested as the operator it is.
        rest = cls.lstrip('()') if cls and all(c in _PUNCTUATION for c in cls) else cls
        # A `)` at depth zero ends a case ARM'S PATTERN — `case x in x) autumn
        # …` runs the command after it — and it also closes a subshell, which
        # is a boundary just the same. Inside a substitution the depth is
        # non-zero, so its own parens never reach here.
        # `name() { … }` coalesces to a single `()` token, so the test is that
        # the punctuation CONTAINS a closer, not that it starts with one —
        # otherwise the declaration stayed in one segment headed by the
        # function's name and its body was never reached.
        if not depth and not nested and rest == '' and ')' in cls:
            out.append(current)
            current = []
            prev = tok
            continue
        if not depth and (cls in _OPERATORS or rest in _OPERATORS):
            if cls != rest:
                current.append(tok[:len(tok) - len(rest)])
            out.append(current)
            current = []
        else:
            current.append(tok)
        prev = cls
    out.append(current)
    # Grouping parens bracket a segment, they are not part of the command:
    # `(autumn migrate && autumn dev)` ends with a `)` that would otherwise ride
    # along in the argv and be echoed into the defect report. Only the ENDS are
    # stripped, so a `$( … )` substitution sitting mid-segment is untouched.
    return [_ungrouped(seg) for seg in out]


def _ungrouped(segment):
    start, end = 0, len(segment)
    while start < end and segment[start] in '()':
        start += 1
    while end > start and segment[end - 1] in '()':
        end -= 1
    return segment[start:end]


# A scalar key whose value is a command line. Compose writes `command: autumn
# migrate`, a GitHub Actions step writes `run: autumn routes`, and the skills
# write `Run: autumn migrate` as an instruction — all of them commands someone
# or something will execute.
#
# A BOUNDED set, not "any word before a colon". The earlier version of this
# script excluded these as "instructional prefixes" on the grounds that reading
# after an arbitrary prose prefix starts reporting sentences. That reasoning was
# right about arbitrary prefixes and wrong to lump these in with them: `command`
# and `run` are not arbitrary, they are the keys that mean "this is a command".
# Two families, because a list under one means something different from a list
# under the other, and the KEY is what says which — not the contents.
#
#   exec:   `command: ["autumn", "migrate", "--shard", "eu west"]`
#           one argv, whose elements may legitimately contain spaces.
#   script: `script: [- autumn migrate, - cargo test]`
#           a list of whole shell LINES (GitLab), each scanned on its own.
#
# Telling them apart by whether any element held a space was wrong in both
# directions: an exec array with a spaced argument (`--shard "eu west"`) was
# read as shell lines, leaving a lone `autumn` that reported as a bare root on
# a valid manifest.
#
# The exec family is itself three SLOTS, because a container's argv is
# assembled from up to three keys and every runtime spells them differently:
#
#   Docker/Compose   ENTRYPOINT + CMD          `entrypoint:` + `command:`
#   Kubernetes       command    + args         `command:`    + `args:`
#
# Concatenated in that order, both spellings fall out of one rule. Reading any
# single key as the whole argv reported `entrypoint: ["autumn"]` with
# `command: ["serve"]` as a bare root — a valid Compose file.
_ENTRY_KEYS = r'entrypoint'
_CMD_KEYS = r'command|cmd|exec'
_EXEC_KEYS = _ENTRY_KEYS + r'|' + _CMD_KEYS
_SCRIPT_KEYS = r'script|run'
_COMMAND_KEYS = _EXEC_KEYS + r'|' + _SCRIPT_KEYS
# A qualifier may precede the key word: Fly writes `release_command = "autumn
# migrate"`, which is executed on every deploy. Anchoring the key to the whole
# token missed both live uses of it (deployment.md, maintenance-mode.md) — and
# a synthetic `command = "…"` test passed while the real `release_command`
# lines went ungated, which is how the gap survived a round of review.
_QUALIFIED = r'(?:[A-Za-z0-9]+[_-])*'
# A YAML COMPACT SEQUENCE writes the first key of a mapping on the same line
# as its `- ` marker: `- command: ["autumn"]` with `args:` under it is the
# ordinary way to spell a Kubernetes container. The key patterns were anchored
# at the indent, so the marker hid the key, the pair never assembled, and the
# generic scan reported a bare root on a correct manifest. The marker is
# counted as part of the INDENT, which is where YAML puts the key's column.
_COMMAND_KEY = re.compile(r'^' + _QUALIFIED + r'(' + _COMMAND_KEYS + r'):$', re.I)
# The same keys in TOML, where the value is quoted after an `=`.
_COMMAND_KEY_BARE = re.compile(r'^' + _QUALIFIED + r'(' + _COMMAND_KEYS + r')$', re.I)


def _autumn_exe(tok):
    """True when the token names the autumn binary, however it is spelled.

    `autumn`, `./autumn`, `/usr/local/bin/autumn` (how `daemon.md`'s systemd
    units write it), `target/debug/autumn`. NOT `autumn-cli`, `autumn-web` or
    `autumn/src/…`, whose basenames differ — and NOT `cd autumn`, which is a
    directory and is excluded by command position rather than by spelling.

    Two spellings are excluded that a plain basename test accepts, both of which
    reported correct pages as broken when this was first written: a URL whose
    last path segment happens to be `autumn`
    (`AUTUMN_ALERTS__WEBHOOK_URL=https://…/hooks/autumn`), and an assignment
    token, which the caller handles separately so that
    `AUTUMN_CLUSTER__CLUSTER_NAME=autumn` stays a cluster's name.
    """
    if '://' in tok or '=' in tok:
        return False
    if tok == 'autumn':
        return True
    return '/' in tok and tok.rsplit('/', 1)[-1] == 'autumn'


def _exe_path(value):
    """True when an assignment's VALUE is a PATH to the autumn binary.

    Stricter than `_autumn_exe` on purpose: `ExecStart=/usr/local/bin/autumn` is
    a command, but `AUTUMN_CLUSTER__CLUSTER_NAME=autumn` is a cluster's name and
    reading the bare word as an executable reported a correct page as broken.
    A value has to look like a path before it can look like a program.
    """
    return '/' in value and _autumn_exe(value)


# A service directive whose VALUE systemd runs as a command line. The KEY has
# to say so: an ordinary shell assignment does not execute its value, and
# treating every `NAME=/path/to/autumn` as a command reported
# `BIN=/usr/local/bin/autumn` — documenting a reusable binary path, which is a
# correct and ordinary thing for a page to do — as a bare root needing a
# subcommand. Failing a correct page is the direction that teaches readers to
# ignore a gate, so the key is now matched rather than assumed.
#
# Only the two `ExecStart=` lines in `daemon.md` reach this branch in the whole
# corpus, so narrowing it to the directives systemd actually executes costs no
# coverage at all.
_EXEC_DIRECTIVE = re.compile(
    r'^Exec(?:Start(?:Pre|Post)?|Stop(?:Post)?|Reload|Condition)?=', re.I)


def _exec_command(tok):
    """True when `tok` is a service directive whose value is the binary."""
    m = _EXEC_DIRECTIVE.match(tok)
    return bool(m) and _exe_path(tok[m.end():])


# The flag whose value is a whole command line, PER PROGRAM — because the
# letter is not shared. `bash -C` is noclobber, not a command string, and
# accepting both spellings for every shell read a flag as a recipe.
#
# A short option may also be BUNDLED: `bash -lc 'autumn …'` runs the string,
# and bash takes `c`'s value from the next word wherever `c` sits in the
# cluster (`-lc` and `-cl` both work). Requiring the standalone spelling missed
# the ordinary login-shell form.
# Programs whose `-c` argument is a shell COMMAND STRING. Any other program's
# `-c` means something else entirely — `echo -c 'autumn db'` prints its
# arguments — and recursing into the value reported a page for a command that
# never runs. The same presence-versus-position mistake as the `--` separator
# had: the option is only a command string when its OWNER makes it one.
# Only the programs whose `-c` really is a shell command string. `ssh -c`
# names a CIPHER and `kubectl -c` a container, so listing every program that
# can reach a shell was the same over-broad reading one level down: what
# matters is whose option it is AND what that option means to them.
_SHELL_C_OPTS = {'sh': 'c', 'bash': 'c', 'zsh': 'c', 'ksh': 'c', 'dash': 'c',
                 'ash': 'c', 'busybox': 'c', 'su': 'c', 'runuser': 'c',
                 # Fly spells it `-C`, which is why the letter is per-program.
                 'fly': 'C', 'flyctl': 'C',
                 # `flock [options] <file> -c <command>` runs its string
                 # through a shell, which is why the option is here as well as
                 # in the wrapper table: one says how to WALK past it, this
                 # says what it MEANS.
                 'flock': 'c'}
_SHELL_RUNNERS = set(_SHELL_C_OPTS)
# …and the runtimes that take `--entrypoint`, for the same reason.
_CONTAINER_RUNNERS = {'docker', 'podman', 'nerdctl'}


def _is_runner(segment, cmd, names):
    """True when the command at `cmd` is one of `names`."""
    return (cmd < len(segment)
            and segment[cmd].rsplit('/', 1)[-1] in names)


def _shell_c(tok, owner):
    """True when `tok` is `owner`'s command-string option.

    Bundled or standalone, but only the letter this program actually spells it
    with — see `_SHELL_C_OPTS`.
    """
    letter = _SHELL_C_OPTS.get(owner.rsplit('/', 1)[-1])
    if letter is None:
        return False
    if tok == '--command':
        return True
    return (len(tok) > 1 and tok[0] == '-' and tok[1] != '-'
            and tok[1:].isalpha() and letter in tok[1:])

# A command key that carries no value on its own line, and the YAML list items
# that follow it. A deployment recipe writes the container command either
# inline (`command: ["autumn", "migrate"]`, which tokenizes) or as a block
# list, where the argv arrives on the lines below. Read one at a time, no item
# is a command, so the whole recipe was invisible.
#
# Items may be indented deeper than the key or sit at the same column — YAML
# allows both, and Kubernetes manifests in the wild use both.
_KEY_ONLY = re.compile(r'^(\s*(?:-\s+)?)' + _QUALIFIED + r'(?:' + _COMMAND_KEYS + r'):\s*$',
                       re.I)
_LIST_ITEM = re.compile(r'^(\s*)-\s+(\S.*?)\s*$')
# A markdown list item, matched for its CONTENT COLUMN — the column a fence
# inside the item is measured against.
_CONTAINER_ITEM = re.compile(r'^ *(?:[-*+]|\d+[.)])\s+')

# The same key with its list written inline, and the sibling `args:` key.
#
# Kubernetes splits one argv across two keys: `command:` holds the executable
# and `args:` the rest. Judged apart, each half is wrong in a different
# direction — the command half is a lone `autumn` that reads as a bare root on
# a perfectly correct manifest, and the args half carries the subcommand that
# would actually drift, attached to no executable at all. Joined, both become
# the argv the container really runs.
_KEY_INLINE = re.compile(r'^(\s*(?:-\s+)?)' + _QUALIFIED + r'(?:' + _COMMAND_KEYS +
                         r'):\s*\[(.*)\]\s*$', re.I)
_ARGS_ONLY = re.compile(r'^(\s*(?:-\s+)?)' + _QUALIFIED + r'args:\s*$', re.I)
_ARGS_INLINE = re.compile(r'^(\s*(?:-\s+)?)' + _QUALIFIED + r'args:\s*\[(.*)\]\s*$', re.I)
_EXEC_KEY_LINE = re.compile(r'^\s*(?:-\s+)?' + _QUALIFIED + r'(?:' + _EXEC_KEYS + r'):', re.I)
# A block scalar, folded (`>`) or literal (`|`). A FOLDED one joins its lines
# with spaces, so a command written across two of them is one command;
# scanning the physical lines let the first resolve on its own and dropped the
# rest, which is where the drift was. A LITERAL one keeps its lines separate
# and each is its own command — but it is still that key's VALUE, so under an
# exec-family key it fills the container's slot exactly as every other form
# does. Excluding it left `entrypoint: ["autumn"]` beside `command: |` with an
# empty slot, and the pair emitted a bare root on a correct recipe.
#
# The style is captured, because it decides whether the lines join.
#
# `args` belongs here too, even though it is not a command key: it carries the
# other HALF of a Kubernetes argv, and its inline, block-list and plain-scalar
# forms all fill their slot already. Leaving the block-scalar form out was the
# same omission as the literal one above, one key over — `command: ["autumn"]`
# beside `args: >` reported a bare root on a valid manifest.
_FOLDED_KEY = re.compile(r'^(\s*(?:-\s+)?)' + _QUALIFIED + r'(?:' + _COMMAND_KEYS +
                         r'|args):\s*([>|])[-+]?([0-9]?)[-+]?\s*$', re.I)
# Any mapping key, used only to spot where one object ends and its sibling
# begins — Compose names its services this way rather than with list items.
_MAPPING_KEY = re.compile(r'^(\s*)[A-Za-z_][\w.-]*:(\s|$)')
# A markdown block-quote marker, which prefixes every line of a fence nested
# inside one.
_BLOCKQUOTE = re.compile(r'^\s*(?:>\s?)+')
# A heredoc's body is DATA the shell writes, not commands it runs. A page
# showing `cat > file <<'EOF'` around some example text was having that text
# scanned as if it were runnable, so an illustrative `autumn db` inside one
# failed the gate.
#
# The lookbehind excludes bash's HERE-STRING, `<<<EOF`, which feeds one word
# to stdin and consumes no following lines at all — reading it as a heredoc
# swallowed the command underneath it.
# The delimiter is a shell WORD, not an identifier: `cat <<END.JSON` and
# `cat <<END}` are both valid and both end on their own spelling. Accepting
# only `[A-Za-z_][\w-]*` captured the `END` prefix of the first, so the
# scanner waited for a terminator that never came and ate the rest of the
# fence — the real command after `END.JSON` included. A word ends at
# whitespace or at a shell operator, which is what the class says.
# The operator only; the delimiter after it is read by `_heredoc_delim`,
# because a shell word is more than any one alternation can describe.
_HEREDOC = re.compile(r'(?<!<)<<-?[ \t]*')


def _heredoc_delim(text, i):
    """The heredoc delimiter word starting at `i`, with its quoting removed.

    A delimiter is a shell WORD, and a word may be spelled in PIECES:
    `<<'END'.JSON` quotes the first half only, and quote removal joins the two
    into `END.JSON`. Matching one quoted run or one bare run captured half of
    it, so the terminator never matched and the rest of the fence — the real
    command after it included — was eaten as body.

    Returns None when there is no word, which is what makes the here-string
    `<<<EOF` fall out: the character after the operator is `<`, and an
    operator ends a word rather than joining it.
    """
    out, n, quoted = [], len(text), False
    while i < n:
        ch = text[i]
        if ch in ' \t' or ch in ';&|<>()':
            break
        if ch == '\\' and i + 1 < n:
            out.append(text[i + 1])
            quoted = True
            i += 2
            continue
        if ch in '"\'':
            # Inside DOUBLE quotes a backslash escapes the next character, so
            # searching for the next quote mistook an escaped one for the
            # terminator and rejected the whole delimiter — the heredoc then
            # opened nothing and its data was scanned as commands. Inside
            # SINGLE quotes a backslash is literal, which is why the walk
            # differs by quote kind, exactly as `_mask_quoted` does.
            quoted = True
            j = i + 1
            while j < n:
                if ch == '"' and text[j] == '\\' and j + 1 < n:
                    # Inside double quotes a backslash quotes only a SPECIAL
                    # character; before anything else it is a literal
                    # backslash and stays in the word. `<<"END\q"` terminates
                    # on `END\q`, so dropping the backslash recorded the wrong
                    # delimiter, the terminator never matched, and the body was
                    # swallowed. `\"` still escapes the closing quote.
                    if text[j + 1] in '$`"\\':
                        out.append(text[j + 1])
                    else:
                        out.append(text[j])
                        out.append(text[j + 1])
                    j += 2
                    continue
                if text[j] == ch:
                    break
                out.append(text[j])
                j += 1
            if j >= n:                      # unterminated: not a delimiter
                return None
            i = j + 1
            continue
        out.append(ch)
        i += 1
    word = ''.join(out)
    return (word, quoted) if word else None
# A slot key whose value is a plain scalar on the same line. Compose mixes the
# forms freely — `entrypoint: ["autumn"]` with `command: migrate` — and reading
# only the bracketed form left the pair half-assembled, so a valid recipe
# reported a bare root. Block indicators (`>`, `|`) and anchors are excluded:
# those are handled before this, or are not values at all.
_KEY_SCALAR = re.compile(r'^(\s*(?:-\s+)?)' + _QUALIFIED + r'(?:' + _COMMAND_KEYS +
                         r'|args):\s*(?![>|&*\[])(\S.*)$', re.I)
_ENTRY_KEY_LINE = re.compile(r'^\s*(?:-\s+)?' + _QUALIFIED + r'(?:' + _ENTRY_KEYS + r'):', re.I)
_ARGS_KEY_LINE = re.compile(r'^\s*(?:-\s+)?' + _QUALIFIED + r'args:', re.I)


def _backticks(line):
    """Yield (executes, text) for every backtick run on the line.

    `executes` is True when the run OPENS outside single quotes, which is
    exactly when a shell would run it: single quotes make a backtick literal,
    double quotes do NOT, and a backslash escapes the next character except
    inside single quotes. Treating every run alike marked
    `printf '%s\\n' '`autumn db`'` as a command and reported a correct
    snippet as broken; a literal run is still yielded, as a mention.
    """
    literal, in_single, i = [], False, 0
    while i < len(line):
        if line[i] == '\\' and not in_single:
            literal.extend([in_single] * 2)
            i += 2
            continue
        literal.append(in_single)
        if line[i] == "'":
            in_single = not in_single
        i += 1
    literal.extend([in_single] * (len(line) - len(literal)))
    for m in re.finditer(r'`([^`\n]+)`', line):
        yield (not literal[m.start()]), m.group(1)


# What may stand immediately before a `#` for it to open a COMMENT. A `#`
# begins a comment only at the start of a WORD, which is why `echo ok#x` is
# not one — but an operator ends the word before it, so `echo ok;# note` is,
# and bash proves it by running the line after. Requiring whitespace missed
# that: the comment's own trailing backslash was then read as a continuation
# and the next line was joined INTO the comment and discarded.
_COMMENT_BEFORE = ' \t;&|('


def _strip_comment(text):
    """Drop a trailing YAML comment, which begins at an unquoted ` #`.

    An annotated array is ordinary — `- definitely-not-a-command # typo` — and
    keeping the annotation in the argv made the element resolve to nothing at
    all, so the drift it was annotating went unreported. Quoting is tracked so
    a `#` inside a value survives, and YAML's own rule (a comment needs
    whitespace before it, or the start of the line) keeps `--tag=#1` intact.
    """
    quote, i, n = None, 0, len(text)
    while i < n:
        ch = text[i]
        # A backslash escapes the next character, except inside single
        # quotes where it is literal. Without this the walker closed a
        # string at an ESCAPED quote and truncated the line at a `#` that
        # was still quoted — discarding, in one reproduction, the heredoc
        # opener that kept the lines below it from being read as commands.
        if ch == '\\' and quote != "'":
            i += 2
            continue
        if quote:
            if ch == quote:
                quote = None
        elif ch in '"\'':
            quote = ch
        elif ch == '#' and (i == 0 or text[i - 1] in _COMMENT_BEFORE):
            return text[:i]
        i += 1
    return text


def _list_value(text):
    """One list item, stripped of the quoting and separators YAML/JSON add."""
    return _strip_comment(text).strip().rstrip(',').strip().strip('\'"')


def _inline_items(body):
    """The items of an inline `[a, "b, c", 'd']` list.

    Splitting on every comma corrupted a quoted element: an argv containing
    `--shard "eu,west"` became two items, and the gate reported the second
    half as a phantom subcommand on a page that was correct.
    """
    items, buf, quote = [], '', None
    for ch in body:
        if quote:
            if ch == quote:
                quote = None
            else:
                buf += ch
        elif ch in '"\'':
            quote = ch
        elif ch == ',':
            items.append(buf.strip())
            buf = ''
        else:
            buf += ch
    items.append(buf.strip())
    return [i for i in items if i]


def _nested_command(segment, j, tok, cmd=0):
    """True when this token is itself a command line rather than an argument.

    Two shapes: a quoted token carrying arguments (`-C "autumn migrate run"`,
    which tokenizes as one token containing spaces), and a token standing alone
    as a shell's `-c` value (`sh -c "autumn"`), which has no space to give it
    away and would otherwise slip past the bare-root check.

    The `-c` test is what keeps the second shape from over-matching: without it,
    a bare `autumn` used as an option VALUE — `docker run --entrypoint autumn
    img sbom` — would read as an empty command line and be reported as needing a
    subcommand, on a page that is correct.
    """
    # What marks a quoted token as a command line is the thing in front of it,
    # never its contents. A shell's `-c`/`-C`/`--command`, a scalar key
    # (`command: "…"`), or that key's TOML spelling (`command = "…"`).
    #
    # Judging by contents instead — "it starts with `autumn `" — was the earlier
    # rule and it is wrong in both directions: it misses
    # `sh -c "AUTUMN_ENV=prod autumn …"`, whose first word is a prefix, and it
    # reads `autumn maintenance on --reason "autumn migrate failed"` as an
    # invocation of `autumn migrate failed`, reporting a correct page.
    if j == 0:
        return False
    prev = segment[j - 1]
    # The option's owner is the command word it is attached to, which is not
    # always the segment head: in `kubectl exec pod -- sh -c '…'` the `-c` is
    # sh's. Either the token before the option or the head may own it —
    # `fly ssh console -C '…'` is the second shape, where the option sits
    # several words after the program that defines it.
    # …and the SEGMENT HEAD owns it too. `flock <file> -c '…'` puts the option
    # after flock's operand, so neither the preceding word nor the command
    # position names the program whose option it is — the head does.
    after_c = any(_shell_c(prev, segment[k]) for k in (j - 2, cmd, 0)
                  if 0 <= k < len(segment))
    # `eval 'autumn …'` joins its arguments into shell input and executes
    # them, so a quoted token after it is a command line exactly as a shell's
    # `-c` string is. It carries no option to hang the rule on, so the KEYWORD
    # itself owns what follows — but only a MULTI-WORD token: an unquoted
    # `eval autumn migrate` is already reached by the prefix walk, and marking
    # its bare `autumn` as a nested line too reported the page twice, once for
    # the real command and once for an empty argv.
    eval_owns = (prev == 'eval' or (cmd < j and segment[cmd] == 'eval')
                 or (j > 0 and segment[0] == 'eval'))
    # `env -S 'autumn …'` / `--split-string` splits the value into an argv and
    # runs its first word, so the value is a command line — owned by env, the
    # same ownership question as a shell's `-c`. (The attached spellings
    # `-S'…'` and `--split-string=…` carry the value inside the token and are
    # handled at the call site, where the part after the option can be split
    # off before recursing.)
    env_split = (prev in ('-S', '--split-string')
                 and any(0 <= k < len(segment)
                         and segment[k].rsplit('/', 1)[-1] == 'env'
                         for k in (j - 2, cmd, 0)))
    if ' ' in tok:
        marked = (after_c or eval_owns or env_split
                  or bool(_COMMAND_KEY.match(prev)))
        if not marked and prev == '=' and j > 1:
            marked = bool(_COMMAND_KEY_BARE.match(segment[j - 2]))
        return marked
    # A SINGLE token is a command line after `-c`, or as the value of a TOML
    # `key = "autumn"` — where `release_command = "autumn"` really is a broken
    # release command, exiting with clap's missing-subcommand error.
    #
    # NOT after a colon key: the key branch already reads `command: autumn
    # migrate` inline, and treating its bare `autumn` as a nested line too
    # yielded a second, empty invocation that reported the page as broken.
    if not _autumn_exe(tok):
        return False
    if after_c or env_split:
        return True
    return prev == '=' and j > 1 and bool(_COMMAND_KEY_BARE.match(segment[j - 2]))


# Programs that execute the argv after their `--` separator. Everything else
# receives those words as data: `echo -- autumn db` prints them.
#
# An allowlist rather than a rule, because there is no way to tell from the
# text whether an unknown program runs its arguments — and the failure
# directions are not symmetric. Omitting a runner loses one latent invocation;
# admitting a printer fails a correct page. The corpus contains no `-- autumn`
# at all today, so this costs nothing measurable either way.
_RUNNERS = {'kubectl', 'oc', 'docker', 'podman', 'nerdctl', 'ssh', 'sudo',
            'env', 'xargs', 'timeout', 'nice', 'flock', 'su', 'runuser',
            'doas', 'nsenter', 'chroot', 'systemd-run', 'fly', 'heroku'}


def _runs_argv(segment, cmd):
    """True when the command at `cmd` executes the argv after its `--`.

    The COMMAND POSITION decides, not mere presence: `echo docker -- autumn db`
    prints five words, and searching every preceding token found `docker` there
    and read the tail as a command — the same over-broad reading that treating
    every non-autumn segment as a wrapper had, one step narrower.
    """
    return (cmd < len(segment)
            and segment[cmd].rsplit('/', 1)[-1] in _RUNNERS)


def _after_image(segment, k):
    """Yield the argv following a container IMAGE.

    `docker run --rm --entrypoint autumn my-app:latest sbom --binary …` runs an
    autumn command, but `autumn` there is the ENTRYPOINT — an option's value —
    and the command is what follows the image. Docker's own grammar is
    `docker run [OPTIONS] IMAGE [COMMAND] [ARG…]`, so the options run out, one
    token is the image, and the rest is ours.

    Deliberately no model of docker's own option arity: if an option before the
    image takes a value (`-v /host:/ctr`), that value is mistaken for the image
    and the real image lands first in the argv, where it fails to look like a
    command name and the walk stops. That degrades to silence, never to a false
    positive on a page that is correct.
    """
    while k < len(segment) and segment[k].startswith('-'):
        k += 1                                  # further options before IMAGE
    k += 1                                      # the IMAGE itself
    if k < len(segment):
        yield ' '.join(segment[k:]), segment[k:]


def _unbracket(tokens):
    """Strip list punctuation from an exec-form command value.

    A deployment recipe may write the command as a list —
    `command: ["autumn", "migrate"]` — which shlex hands back as `[autumn,` and
    `migrate]` once the quotes are stripped. The brackets and separating commas
    are the list's syntax, not the command's, and leaving them attached meant
    the executable never matched. A value written the plain way passes through
    unchanged.
    """
    out = []
    for tok in tokens:
        cleaned = tok.strip('[],')
        if cleaned:
            out.append(cleaned)
    return out


def _unwrap(tokens):
    """Drop the substitution's own outer parens, keeping everything between.

    Only ONE parenthesis is removed from each end, and only from a token that
    is pure punctuation. Filtering every punctuation token out instead — the
    first version of this — deleted the inner `;` from
    `OUT=$(printf x; autumn migrate)`, collapsing two commands into one headed
    by `printf` and losing the autumn invocation entirely.
    """
    inner = list(tokens)
    if inner and all(c in _PUNCTUATION for c in inner[0]):
        inner[0] = inner[0][1:]
        if not inner[0]:
            inner.pop(0)
    if inner and all(c in _PUNCTUATION for c in inner[-1]):
        inner[-1] = inner[-1][:-1]
        if not inner[-1]:
            inner.pop()
    return inner


def _embedded_spans(tok):
    """Yield the text inside each `$( … )` written within a single token.

    The depth count is quote-aware: a parenthesis inside a quoted argument is
    data, not nesting. `OUT="$(printf '('; autumn migrate)"` opened a depth the
    real `)` then only closed halfway, so the substitution was never terminated
    and the command inside it was dropped.
    """
    i = 0
    while True:
        start = tok.find('$(', i)
        if start < 0:
            return
        # `$((` is ARITHMETIC, not a command. Reporting it rejected a page
        # that runs no autumn at all. Whether the `$` was ESCAPED is decided
        # by the masked copy instead — see `_mask_single_quoted` — because
        # tokenization destroys the backslash parity this would need.
        if tok[start:start + 3] == '$((':
            i = start + 2
            continue
        depth, j, quote = 0, start + 1, None
        while j < len(tok):
            ch = tok[j]
            if quote:
                if ch == quote:
                    quote = None
            elif ch in '"\'':
                quote = ch
            elif ch == '\\':
                j += 1
            elif ch == '(':
                depth += 1
            elif ch == ')':
                depth -= 1
                if depth == 0:
                    break
            j += 1
        if j >= len(tok):
            return
        yield start, tok[start + 2:j]
        i = j + 1


def _mask_quoted(text, quotes="'\""):
    """`text` with QUOTED content replaced by filler of equal length.

    Which quotes count depends on the question being asked, and the two uses
    here differ:

      - a command SUBSTITUTION runs inside double quotes, so only single ones
        are masked (`_mask_single_quoted`);
      - a heredoc operator is inert inside EITHER, so both are, and
        `printf '%s' "<<EOF"` opens nothing.

    Length is preserved so an offset in the masked copy still refers to the
    same character, which is what makes the decision positional.
    """
    out, i, n = [], 0, len(text)
    while i < n:
        ch = text[i]
        out.append(ch)
        i += 1
        if ch not in quotes:
            continue
        # Finding the terminator with a plain search mistook an ESCAPED quote
        # for it — `"\" <<EOF"` ended the string early, leaving the operator
        # unmasked. A backslash escapes the next character inside double
        # quotes; inside single quotes it is literal, so it is only honoured
        # for the double-quoted case, which is what the shell does.
        end = i
        while end < n:
            if ch == '"' and text[end] == '\\':
                end += 2
                continue
            if text[end] == ch:
                break
            end += 1
        if end >= n:                            # unbalanced: mask the rest
            out.append('x' * (n - i))
            break
        out.append('x' * (end - i))
        out.append(ch)
        i = end + 1
    return ''.join(out)


def _mask_arithmetic(text):
    """`text` with every `$(( … ))` blanked, length preserved.

    A `<<` inside an arithmetic expression is a LEFT SHIFT, not a heredoc
    operator: `echo $((1 << 2))` opens nothing at all. Reading it as an opener
    made the scanner wait for a `2` terminator and eat the rest of the fence,
    so the command below it was never judged.
    """
    out, i, n = list(text), 0, len(text)
    while True:
        start = text.find('$((', i)
        if start < 0:
            return ''.join(out)
        depth, j = 0, start + 1
        while j < n:
            if text[j] == '(':
                depth += 1
            elif text[j] == ')':
                depth -= 1
                if depth == 0:
                    break
            j += 1
        end = min(j, n - 1)
        for k in range(start, end + 1):
            out[k] = 'x'
        i = end + 1


def _heredoc_openers(text):
    """Yield the heredoc (delimiter, allows_tabs) pairs a line opens.

    Quoted operators are inert and so are arithmetic ones, an ESCAPED `<<` is
    a literal `<`, and the delimiter is a shell word — each of those was a
    separate round of this review, so they are answered in one place now.
    """
    masked = _mask_arithmetic(_mask_quoted(text))
    for opener in _HEREDOC.finditer(text):
        if masked[opener.start():opener.start() + 2] != '<<':
            continue
        if _escaped(text, opener.start()):
            continue
        found = _heredoc_delim(text, opener.end())
        if found is None:
            continue
        delim, quoted = found
        # An UNQUOTED delimiter leaves the body subject to expansion — bash's
        # manual lists parameter expansion, command substitution and
        # arithmetic — so `cat <<EOF` with `$(autumn …)` under it really does
        # run that command, while `cat <<'EOF'` prints it. Skipping every body
        # line alike let the first bypass the gate.
        yield (delim, text[opener.start():opener.start() + 3] == '<<-',
               not quoted)


def _body_expansions(text):
    """Yield the command text of every LIVE expansion in a heredoc body.

    Quoting is not special in a body — `'$(autumn …)'` still expands — but a
    BACKSLASH is: bash's here-document rules let it quote `$` and a backtick,
    so `\\$(…)` and `` \\` `` are printed rather than run. And a body expands
    legacy backtick substitutions too, which the `$(` scan alone missed.

    Parity is read straight off the raw line here. That works only because
    nothing has been tokenized: on a command line shlex resolves `\\\\$(` and
    `\\$(` to the same characters, which is why THAT path has to decide it from
    a masked copy instead.
    """
    for start, inner in _embedded_spans(text):
        if not _escaped(text, start):
            yield inner
    i, n = 0, len(text)
    while i < n:
        if text[i] != '`' or _escaped(text, i):
            i += 1
            continue
        end = i + 1
        while end < n and (text[end] != '`' or _escaped(text, end)):
            end += 1
        if end >= n:
            return                          # unterminated: not a substitution
        yield text[i + 1:end]
        i = end + 1


def _body_balanced(text):
    """True when a heredoc-body fragment leaves no `$(` or backtick open.

    A command substitution in a body may span newlines, so a fragment ending
    mid-`$( … )` has to be held and joined with the lines below it before it is
    scanned. A backslash quotes `$` and a backtick in a body, so an escaped
    opener does not count toward the balance.
    """
    depth, bt, i, n = 0, 0, 0, len(text)
    while i < n:
        if text[i] == '\\':
            i += 2
            continue
        if text[i] == '`':
            bt ^= 1
        elif bt:
            pass                            # inside a backtick run, ` decides
        elif text[i:i + 2] == '$(':
            depth += 1
            i += 2
            continue
        elif text[i] == ')' and depth:
            depth -= 1
        i += 1
    return depth == 0 and bt == 0


def _body_commands(buf, lineno, text):
    """Accumulate one heredoc-body line; yield (lineno, inner) when it settles.

    `buf` is a list of (lineno, text) carried across calls while a substitution
    is still open. Nothing is yielded until the fragment balances, at which
    point every live expansion in the whole fragment is read and the buffer
    clears. An unbalanced tail left at the terminator is a shell syntax error,
    so the caller drops it.
    """
    buf.append((lineno, text))
    joined = '\n'.join(t for _, t in buf)
    if _body_balanced(joined):
        at = buf[0][0]
        for inner in _body_expansions(joined):
            yield at, inner
        buf.clear()


def _script_lines(lines):
    """Yield the (lineno, text) of `lines` that are COMMANDS, not data.

    A literal `run: |` block is a shell script, so the same two rules a fence
    obeys apply inside it: a heredoc body is data the shell writes, and a
    quote spans physical lines. Emitting each line on its own reported
    `cat <<EOF` / `autumn db` / `EOF` as if the body were runnable, which
    fails a correct workflow.

    An unterminated quote hands its lines back one at a time rather than
    swallowing them, exactly as the fence path does.
    """
    heredocs, quoted, quoted_at, quote = [], [], None, None
    heredoc_buf = []
    for lineno, text in lines:
        if heredocs:
            delim, tabs, expands = heredocs[0]
            candidate = text.lstrip('\t') if tabs else text
            if candidate.rstrip('\r') == delim:
                heredocs.pop(0)
                heredoc_buf = []            # drop any unclosed fragment
            elif expands:
                for at, inner in _body_commands(heredoc_buf, lineno, text):
                    yield at, inner
            continue
        if quote:
            quoted.append(text)
            quote = _open_quote(text, quote)
            if quote:
                continue
            yield quoted_at, '\n'.join(quoted)
            quoted, quoted_at = [], None
            continue
        opening = _open_quote(text)
        if opening:
            quote, quoted, quoted_at = opening, [text], lineno
            continue
        yield lineno, text
        heredocs.extend(_heredoc_openers(_strip_comment(text)))
    for offset, held in enumerate(quoted):
        yield (quoted_at or 0) + offset, held


def _mask_single_quoted(text):
    """The substitution reading: only single quotes stop `$( … )`.

    An ESCAPED `$(` is stopped too, and the decision has to be made HERE, on
    the raw text, rather than on the token. shlex resolves `"\\\\$("` (a literal
    backslash in front of a LIVE substitution) and `"\\$("` (an escaped dollar
    the shell prints) to the same characters, so after tokenization the two are
    indistinguishable — a parity test on the token suppressed the real one.
    Blanking the dollar keeps the existing positional check as the single place
    that decides.
    """
    masked = _mask_quoted(text, "'")
    out = list(masked)
    for m in re.finditer(r'\$\(', masked):
        if _escaped(text, m.start()):
            out[m.start()] = 'x'
    return ''.join(out)


def _in_substitutions(segment, masked=()):
    """Yield the commands written INSIDE `$( … )` or `<( … )` in a segment.

    Re-enters the same parser on the substitution's own tokens, so a chain or
    an environment prefix inside one is handled exactly as it is outside.
    """
    i = 0
    while i < len(segment):
        # `<(` and `>(` are PROCESS SUBSTITUTION: bash runs the list inside
        # them and hands the caller a file. Only `$(` was recursed into, so a
        # command written in one — `cat <(autumn migrate)` — went unread.
        if segment[i].endswith(('$', '<', '>')) and i + 1 < len(segment) \
                and segment[i + 1].startswith('('):
            depth, start = 0, i + 1
            i += 1
            while i < len(segment):
                depth += _paren_delta(segment[i])
                i += 1
                if depth <= 0:
                    break
            yield from _from_tokens(_unwrap(segment[start:i]))
            continue
        # `OUT="$(autumn migrate)"` is quoted, so shlex keeps the whole
        # substitution inside ONE token and the `$` / `(` never sit adjacent.
        # The text between `$(` and its matching `)` is still a command line.
        for start, text in _embedded_spans(segment[i]):
            # …unless THIS substitution was written inside single quotes, where
            # the shell prints it rather than running it. Masking preserves
            # length, so the same offset in the masked token still reads `$(`
            # only when the shell would have executed it. Comparing the body
            # TEXT instead was wrong twice over: it suppressed a real
            # substitution that repeated a quoted one, and it suppressed a real
            # one whose own body contained a quoted character.
            mask_tok = masked[i] if i < len(masked) else ''
            if mask_tok[start:start + 2] != '$(':
                continue
            yield from commands(text)
        i += 1


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
    The binary is recognised however it is spelled — `autumn`, `./autumn`,
    `/usr/local/bin/autumn` — because a path-qualified executable is still an
    invocation. `cd autumn` is excluded by POSITION (it is not a command head),
    not by spelling, and `autumn-cli` / `autumn/src/…` by basename.
    """
    tokens = tokenize(text)
    if tokens is None:
        tokens = text.split()
    masked = tokenize(_mask_single_quoted(text))
    # A SECOND mask, for a second question. Substitutions run inside double
    # quotes, so only single ones are blanked above; an OPERATOR is data
    # inside either, so this one blanks both.
    yield from _from_tokens(tokens, masked, tokenize(_mask_quoted(text)))


def _from_tokens(tokens, masked=None, classify=None):
    """The token half of `commands()`, so nested contexts can re-enter it."""
    segments = _segments(tokens, classify)
    # The masked copy is tokenized and segmented in parallel. Masking replaces
    # only characters INSIDE single quotes, leaving every delimiter and
    # operator in place, so the two shapes agree — and when they somehow do
    # not, the masked view is dropped rather than trusted.
    masked_segments = _segments(masked, classify) if masked is not None else None
    if masked_segments is None or len(masked_segments) != len(segments) \
            or any(len(a) != len(b) for a, b in zip(segments, masked_segments)):
        masked_segments = segments
    for segment, masked_segment in zip(segments, masked_segments):
        inspecting = False
        i = _cron_prefix(segment)
        while i < len(segment) and (segment[i] in _PROMPT or segment[i] in _CONTROL
                                    or _ENV_TOKEN.match(segment[i])):
            # A service directive whose VALUE is the binary is itself the
            # command head, not something to step over: a systemd unit writes
            # `ExecStart=/usr/local/bin/autumn db backup …`, and skipping it as
            # an environment prefix left the whole recipe ungated. An ordinary
            # assignment stays a prefix — it does not run its value.
            if _exec_command(segment[i]):
                break
            # `NAME=$(cat file)` tokenizes as `NAME=$` `(` `cat` `file` `)`, so
            # a command substitution in the value has to be stepped over as a
            # unit — otherwise the scan stops on `cat` and never reaches the
            # command the assignment was standing in front of.
            if segment[i].endswith('$') and i + 1 < len(segment) \
                    and segment[i + 1].startswith('('):
                depth = 0
                i += 1
                while i < len(segment):
                    depth += _paren_delta(segment[i])
                    i += 1
                    if depth <= 0:              # a coalesced `))` closes both
                        break
                continue
            wrapper_name = segment[i]
            wrapper = _WRAPPER_OPTS.get(wrapper_name)
            i += 1
            if wrapper is not None:
                if _inspects(segment, i, wrapper_name):
                    inspecting = True       # describes its arguments, runs none
                    break
                i = _skip_wrapper_options(segment, i, wrapper)
                i += _WRAPPER_OPERANDS.get(wrapper_name, 0)
                # A wrapper's own `--` ends ITS options; the command follows.
                # For a wrapper with no operands the option walk already ate
                # it, but for one with them the separator arrives afterwards
                # and stopped the walk — `timeout 5 -- autumn migrate` and
                # `flock /tmp/lock -- autumn migrate` went unread, while the
                # spelling without the separator was fine.
                if i < len(segment) and segment[i] == '--':
                    i += 1
        # A redirection may also come BEFORE the command — `>/tmp/out autumn
        # migrate` is valid and runs autumn — and the prefix walk stopped on
        # it, so the whole line went unread.
        while i < len(segment):
            eaten = _redirect(segment[i])
            if not eaten:
                break
            i += eaten
        if inspecting:
            continue
        cmd_at = i              # the command position, for `--` grammar below
        head = None
        if i < len(segment):
            if _autumn_exe(segment[i]) or _exec_command(segment[i]):
                head = i
        if head is not None:
            i = head
        if i < len(segment) and head is not None:
            yield ' '.join(segment[i + 1:]), segment[i + 1:]
        for j, tok in enumerate(segment):
            if _COMMAND_KEY.match(tok) and j + 1 < len(segment):
                rest = _unbracket(segment[j + 1:])
                if rest and _autumn_exe(rest[0]):
                    yield ' '.join(rest[1:]), rest[1:]
            elif tok == '--' and j + 1 < len(segment) \
                    and _autumn_exe(segment[j + 1]) and head is None \
                    and _runs_argv(segment, cmd_at):
                # Only a command that RUNS what follows its separator
                # introduces a nested command. Two things had to be excluded,
                # and `head is None` alone covered just the first:
                #   - the segment is itself an autumn command, so the `--` is
                #     its own — `autumn test -- autumn` forwards to the
                #     harness (`Test::cargo_args` is `trailing_var_arg`);
                #   - the segment is some OTHER program that merely prints its
                #     arguments — `echo -- autumn db` runs nothing, and
                #     reading `db` there failed a correct page.
                yield ' '.join(segment[j + 2:]), segment[j + 2:]
            elif tok == '--entrypoint' and j + 2 < len(segment) \
                    and _autumn_exe(segment[j + 1]) \
                    and _is_runner(segment, cmd_at, _CONTAINER_RUNNERS):
                yield from _after_image(segment, j + 2)
            elif tok.startswith('--entrypoint=') \
                    and _autumn_exe(tok[len('--entrypoint='):]) \
                    and _is_runner(segment, cmd_at, _CONTAINER_RUNNERS):
                # The attached form takes a path just as the separated one
                # does — `--entrypoint=/usr/local/bin/autumn` — and comparing
                # the whole token against one spelling accepted only the bare
                # word, so the path-qualified recipe went unread.
                yield from _after_image(segment, j + 1)
            elif (tok.startswith('--split-string=')
                  or (tok.startswith('-S') and not tok.startswith('--')
                      and len(tok) > 2)) \
                    and any(segment[k].rsplit('/', 1)[-1] == 'env'
                            for k in range(j)):
                # The ATTACHED split-string spellings carry the command inside
                # the token — `env --split-string='autumn …'` and `env
                # -S'autumn …'` — so the part after the option is split off and
                # re-entered, the same way the separated form is marked in
                # `_nested_command`. env owns it wherever it stands in the
                # prefix: the wrapper walk has already stepped `cmd_at` past
                # this option by the time the branch runs, so the head is not
                # where env is.
                value = tok.split('=', 1)[1] if tok.startswith('--') else tok[2:]
                yield from commands(value)
            elif _nested_command(segment, j, tok, cmd_at):
                # A nested command line is RE-ENTERED, not re-implemented. It
                # gets environment prefixes, chains, path-qualified spellings
                # and everything else for free, because it is the same parser:
                # `sh -c "AUTUMN_ENV=prod autumn migrate"` had none of that when
                # this branch matched only a literal `autumn ` head of its own.
                yield from commands(tok)

        # A command substitution is a command line too. Stepping over it to
        # reach what follows the assignment is only half the job —
        # `OUT=$(autumn migrate)` runs a command that nothing was reading.
        yield from _in_substitutions(segment, masked_segment)


# A token that could name a command. Anything else — a flag, a `<PLACEHOLDER>`,
# a TOML `= "0.1.0"`, a box-drawing character from a diagram — ends the walk.
TOKEN = re.compile(r'^[a-z][a-z0-9-]*$')

# A redirection is shell plumbing, not part of the command. `<` and `>` are
# deliberately NOT in `_PUNCTUATION` — the docs write `--shard <new>` and
# splitting on the angle brackets invented a phantom subcommand — so a
# redirection arrives as one token and stopped the walk before it could judge
# whether the command was complete: `autumn db >/tmp/out` passed while the
# identical `autumn db` was reported.
#
# A placeholder is told apart by its shape rather than by position: `<new>`
# closes its own bracket, and no redirection target does.
_PLACEHOLDER = re.compile(r'^<[^<>\s]*>$')
_REDIRECT = re.compile(r'^(?:[0-9]+|&)?(?:>>?|<<?<?)(.*)$')


def _redirect(tok):
    """Tokens a redirection consumes, or 0 when this is not one.

    1 when the target is glued on (`>/tmp/out`, `2>&1`), 2 when it is the next
    token (`> /tmp/out`).
    """
    if _PLACEHOLDER.match(tok):
        return 0
    m = _REDIRECT.match(tok)
    if not m:
        return 0
    return 1 if m.group(1) else 2

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
    fence_indent = 0   # how far the OPENING marker was indented
    container = 0      # content column of the innermost open list item
    lang = None
    held = []          # parts of a backslash-continued line, oldest first
    held_at = None     # the line the continued command STARTED on
    quoted = []        # physical lines inside a quote that spans them
    quoted_at = None   # the line the open quote STARTED on
    quote = None       # the quote character still open, or None

    def flush(line, lineno):
        """Fold a backslash continuation and a multi-line quote into one line."""
        nonlocal held, held_at, quoted, quoted_at, quote
        # A quote that is still open runs on into the next physical line, so
        # everything until it closes is string DATA. Only a SHELL fence gets
        # this reading: an apostrophe in a `rust` fence is a lifetime and in a
        # `toml` one a plain character, and folding on those would swallow the
        # rest of the block — 173 spans across the corpus, against 4 here.
        if quote:
            quoted.append(line)
            quote = _open_quote(line, quote)
            if quote:
                return None, None
            line, lineno = '\n'.join(quoted), quoted_at
            quoted, quoted_at = [], None
            return line, lineno
        # A trailing backslash continues the line only when it is itself
        # UNESCAPED. `printf '%s' \\\\` prints one literal backslash and
        # continues nothing, and joining the next line onto it hid the command
        # there inside the printf's argv. Parity decides: an odd run escapes
        # the newline, an even one is literal backslashes.
        # …and only OUTSIDE a comment. `echo ok # trailing \\` ends the
        # command; the backslash is comment text. Joining there swallowed the
        # next line into the comment, where shlex discarded it — a WRONG join,
        # not the missed one I had reasoned it would be.
        body = _strip_comment(line).rstrip()
        if _escaped(body + ' ', len(body)):
            held.append(body[:-1])
            if held_at is None:
                held_at = lineno
            return None, None
        if held:
            # Bash removes the backslash-newline pair and joins what is left
            # with NOTHING between: `autumn mig\` + `rate` runs `autumn
            # migrate`. Inserting a space made that `autumn mig rate` and
            # failed a correct page. Whitespace written before the backslash
            # is already in the held fragment, and whitespace at the start of
            # the next line is still there to separate the words — verified
            # against the installed bash in both directions.
            line = ''.join(held) + line
            lineno = held_at
            held, held_at = [], None
        if lang in _SHELL_LANGS:
            quote = _open_quote(line)
            if quote:
                quoted, quoted_at = [line], lineno
                return None, None
        return line, lineno

    def close_quote():
        """Read an unterminated quote's lines the way they were read before.

        A fence ends the string whatever the shell would do, and a block that
        opens a quote it never closes is a FRAGMENT of a larger file. Dropping
        its lines would lose coverage silently, so each is scanned on its own —
        exactly what this script did before the fold existed. No shell fence in
        this corpus reaches here; it is the safety net for the one that does.
        """
        nonlocal quoted, quoted_at, quote
        pending_lines, at = quoted, quoted_at
        quoted, quoted_at, quote = [], None, None
        for offset, held_line in enumerate(pending_lines):
            for display, argv in commands(held_line):
                yield (at or 0) + offset, display, argv, FENCED_COMMAND

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

    # Exec arrays, inline or block, with a `command:` list held back until the
    # line after it has been seen — because a sibling `args:` may carry the
    # rest of the same argv.
    pending = None          # items being collected right now
    pending_kind = None     # 'entry', 'cmd', 'args' or 'script'
    pending_at = None       # the line the key sits on
    pending_indent = 0
    # One container's argv, in three slots. A YAML mapping is unordered, so any
    # of them may arrive first and none can be judged until the block ends.
    slots = {}               # slot -> items
    slots_at = None
    slots_indent = 0

    def emit_slots():
        """Join whichever slots were filled and read them as one argv."""
        nonlocal slots, slots_at
        filled, at = slots, slots_at
        slots, slots_at = {}, None
        # ENTRYPOINT + CMD + args, which is Docker's rule and Kubernetes'
        # alike once each runtime's key names are mapped onto the slots.
        items = (filled.get('entry') or []) + (filled.get('cmd') or []) \
            + (filled.get('args') or [])
        if not items:
            return
        for display, argv in _from_tokens(items):
            yield at, display, argv, FENCED_COMMAND

    fence_len = 0           # how long the opening run was: a closer matches it
    quoted_fence = False    # the open fence sits inside a markdown block quote
    heredocs = []           # terminators still to consume, in order
    heredoc_buf = []        # body lines held while a `$( … )` spans them
    folded = None           # paragraphs of a folded (`>`) block scalar
    folded_at = None
    folded_indent = 0
    folded_base = None      # the column its content sits at
    folded_slot = None      # which container slot it fills, if any
    folded_literal = False  # `|` keeps its lines apart; `>` joins them
    folded_cont = False     # …unless the line before ended in a backslash

    def close_folded():
        """Read a folded scalar: one command per paragraph, lines joined."""
        nonlocal folded, folded_at, folded_base, folded_slot
        nonlocal folded_literal, folded_cont
        nonlocal pending, pending_kind, pending_at, pending_indent
        paras, at, slot = folded, folded_at, folded_slot
        folded_literal_here = folded_literal
        folded, folded_at, folded_base, folded_slot = None, None, None, None
        folded_literal, folded_cont = False, False
        if not paras:
            return
        # A folded value under an exec-family key is that container's argv,
        # exactly as the inline, block-list and plain-scalar forms are, so it
        # fills the slot rather than standing alone. Opening a separate
        # accumulator for it left `entrypoint: ["autumn"]` with a folded
        # `command:` reporting a bare root on a correct recipe.
        #
        # Only a SINGLE paragraph can be one argv; more than one is more than
        # one command, and those are read on their own as before.
        filled = [para for para in paras if para[1]]
        if slot and len(filled) == 1:
            value = ' '.join(filled[0][1])
            pending_kind, pending_at = slot, filled[0][0] or at
            pending_indent = folded_indent
            pending = tokenize(value) or value.split()
            yield from close_pending()
            return
        # Folding joins lines with spaces, but a BLANK line survives as a real
        # newline — so it separates two commands. Joining across it built
        # `autumn migrate autumn routes` out of two valid lines and reported
        # the second `autumn` as a phantom subcommand, failing a correct file.
        #
        # Each paragraph reports its OWN first line, not the key's. Reporting
        # the key sent a reader to the `run: |` three lines above the command
        # that actually fails, and `file:line:` is the whole of what the gate
        # hands them.
        # A LITERAL scalar is a shell script, so its lines carry heredoc and
        # quote state across one another. A folded one is a single logical
        # value per paragraph and has no such state to keep.
        logical = [(para_at or at, ' '.join(para))
                   for para_at, para in paras if para]
        if folded_literal_here:
            logical = list(_script_lines(logical))
        for para_at, text in logical:
            for display, argv in commands(text):
                yield para_at, display, argv, FENCED_COMMAND

    def close_pending():
        """Finish the list being collected: fill a slot, or run script lines."""
        nonlocal pending, pending_kind, pending_at
        nonlocal slots, slots_at, slots_indent
        items, kind, at = pending, pending_kind, pending_at
        indent = pending_indent
        pending, pending_kind, pending_at = None, None, None
        if items is None:
            return
        if kind == 'script':
            # Whole shell lines, each its own command; nothing to assemble.
            for it in items:
                for display, argv in commands(it):
                    yield at, display, argv, FENCED_COMMAND
            return
        # Refilling a slot, or a key at a different depth, means a new
        # container: judge what was collected before starting on it.
        if kind in slots or (slots and indent != slots_indent):
            yield from emit_slots()
        slots[kind] = items
        if slots_at is None:
            slots_at, slots_indent = at, indent

    def add_prose(line, lineno):
        """Accumulate one prose line, with any blockquote marker stripped."""
        nonlocal para_len
        stripped = re.sub(r'^\s*>+\s?', '', line)
        para.append((para_len, lineno, stripped))
        para_len += len(stripped) + 1

    for lineno, line in enumerate(text.split('\n'), 1):
        # A fence nested in a BLOCK QUOTE carries a `>` on every line, so the
        # marker is not at the start and the whole block went unread. The
        # prefix is stripped only while such a fence is open, because inside an
        # ordinary fence a leading `>` is a redirection, not a quote marker.
        if fence is not None and quoted_fence:
            line = _BLOCKQUOTE.sub('', line, count=1)
        # A fence may open on a LIST ITEM's own line — `- ```bash` — with its
        # body and closer indented under the item. The marker is counted as
        # indent, so the closer's relative bound still measures from the
        # column the fence really starts at.
        m = re.match(r'^( *)(?:>\s?)*( *(?:[-*+]\s+|\d+[.)]\s+)?)'
                     r'(`{3,}|~{3,})\s*(.*)$', line)
        # A line that LOOKS like a fence marker is only a boundary if it can
        # actually open or close one. Deciding that first matters: the state
        # reset below is what a boundary does, and running it on a marker that
        # turns out to be CONTENT threw away the heredoc queue, so an indented
        # ``` inside a heredoc made the data under it read as commands and
        # failed a correct page.
        closes = (m and fence is not None
                  and m.group(3)[0] == fence and len(m.group(3)) >= fence_len
                  and not m.group(4).strip()
                  # Markdown allows a closing fence at most three spaces
                  # further in than the block it closes; beyond that the line
                  # is content. The bound is RELATIVE to the opener rather
                  # than absolute, because a fence nested in a list item is
                  # legitimately indented — this corpus has none past three
                  # spaces, but capping absolutely would stop reading the
                  # first one that appears.
                  and len(m.group(1)) + len(m.group(2)) <= fence_indent + 3)
        # A fence OPENS only within three spaces of its container's content
        # column — at the top level that is zero, so a four-space line is an
        # indented code block and the prose after it is prose. The bound has
        # to be the same one the closer uses; accepting any indent on the way
        # in meant an indented block was entered as a fence and a correct page
        # was gated. Verified against a CommonMark implementation, which also
        # showed the six-space-under-a-list case this file used to assert is
        # indented code rather than a fence.
        opens = m is not None and fence is None \
            and len(m.group(1)) + len(m.group(2)) <= container + 3
        if m and (opens or closes):
            held, held_at, heredocs = [], None, []   # a fence boundary ends any hold
            heredoc_buf = []
            yield from close_quote()            # …an unterminated quote
            yield from close_folded()           # …a folded scalar
            yield from close_pending()          # …and any list
            yield from emit_slots()
            yield from _spans(take_paragraph(), commands)
            if fence is None:
                # The opening run's LENGTH is remembered, not just its
                # character. A markdown fence closes only on a run at least as
                # long as the one that opened it, so a ``` inside a ````
                # block is content — closing on it ended the fence early and
                # everything after read as prose, where a runnable command is
                # not judged.
                fence, fence_len = m.group(3)[0], len(m.group(3))
                fence_indent = len(m.group(1)) + len(m.group(2))
                # The info string is the first word of the rest.
                info = m.group(4).strip()
                lang = re.split(r'[\s,]', info)[0].lower() if info else ''
                quoted_fence = bool(_BLOCKQUOTE.match(line))
            else:
                fence, lang, quoted_fence = None, None, False
            continue

        if fence is None:
            if not line.strip():                # a blank line ends a paragraph
                yield from _spans(take_paragraph(), commands)
            else:
                # A list item opens a CONTAINER whose content column is where
                # its text starts; anything less indented has left it again.
                item = _CONTAINER_ITEM.match(line)
                if item:
                    container = len(item.group(0))
                elif len(line) - len(line.lstrip()) < container:
                    container = 0
                add_prose(line, lineno)
            continue

        # Inside a fence, whatever the tag. Continuations are joined before
        # scanning — `autumn maintenance on \` + `--reason "…"` is one command,
        # and the guide writes five of them on that page alone. Scanning the
        # physical lines instead yields the argv `\`, and the command path split
        # across the break goes unchecked.
        # Inside a heredoc the shell is writing data, not running it.
        # One command line may open SEVERAL heredocs — `cat <<ONE <<TWO` —
        # and bash consumes their bodies in order. Keeping only the first
        # delimiter resumed scanning inside the second body, so its data read
        # as commands.
        if heredocs:
            delim, tabs, expands = heredocs[0]
            candidate = line.lstrip('\t') if tabs else line
            if candidate.rstrip('\r') == delim:
                heredocs.pop(0)
                heredoc_buf = []            # drop any unclosed fragment
            elif expands:
                # The body is DATA, but an unquoted delimiter still lets an
                # expansion inside it run. Quoting is not special in a heredoc
                # body — only a backslash is — so the spans are read by the
                # rules `_body_expansions` states, and a `$( … )` that spans
                # body lines is held by `_body_commands` until it closes.
                for at, inner in _body_commands(heredoc_buf, lineno, line):
                    for display, argv in commands(inner):
                        yield at, display, argv, FENCED_COMMAND
            continue

        # A trailing comment is valid on a key line, and these patterns are
        # end-anchored, so the key is matched against the line without it.
        # `run: > # folded for readability` is an ordinary thing to write.
        keyline = _strip_comment(line).rstrip()

        # A folded scalar swallows every more-indented line below its key; a
        # blank line inside it is a paragraph break, not nothing.
        if folded is not None:
            if not line.strip():
                # A blank line ENDS a pending continuation: bash joins the
                # backslash to the empty next line, so `autumn migrate \` + a
                # blank line is the finished command `autumn migrate`. Leaving
                # `folded_cont` set here appended onto an empty paragraph on
                # the next content line and raised IndexError, crashing the
                # whole docs job on a valid script.
                folded.append([None, []])
                folded_cont = False
                continue
            depth = len(line) - len(line.lstrip())
            if depth > folded_indent:
                # Folding joins lines at the scalar's own column. A MORE
                # indented line is not folded into the one above it — YAML
                # keeps the newline around such a block — so a change of
                # column breaks the paragraph. Joining across it built
                # `autumn migrate autumn routes` from two valid lines.
                if folded_base is None:
                    folded_base = depth
                # A LITERAL (`|`) scalar keeps every newline it is written
                # with, so each of its lines is its own command and none of
                # them join — which is also true of a more-indented block
                # inside a FOLDED one, for the same reason. A more-indented
                # block keeps EVERY newline, not just the ones at its edges:
                # breaking only when the depth changed still joined two such
                # lines, and because the first accepts arguments the second
                # vanished into it.
                if folded_literal or depth > folded_base:
                    # A trailing unescaped backslash continues the line here
                    # exactly as it does in a shell fence — these lines ARE
                    # shell. Sending each to the parser on its own stopped the
                    # first at the backslash and left the second with no
                    # executable, so `autumn \` / `definitely-not-a-command`
                    # resolved to nothing and the drift went unreported.
                    # A continuation keeps the next line's RELATIVE
                    # indentation, because that whitespace is what separates
                    # the words once bash removes the backslash-newline:
                    # `autumn\` + `    nope` is `autumn    nope`, two words.
                    # Stripping it concatenated them into one unreadable
                    # token and the drift went unreported. The block's own
                    # column comes off; anything past it is content.
                    text = (line[folded_base:].rstrip()
                            if folded_cont and folded_base is not None
                            and not line[:folded_base].strip()
                            else line.strip())
                    body = _strip_comment(text).rstrip()
                    cont = _escaped(body + ' ', len(body))
                    if cont:
                        text = body[:-1]
                    if folded_cont:
                        # …joined with nothing, as in a fence. (A literal
                        # scalar's lines are stripped of the block indent, so
                        # a continuation whose next line is indented FURTHER
                        # loses that separation — the direction is a missed
                        # reading, never a false report, and this corpus
                        # writes none.)
                        folded[-1][1][-1] += text
                    else:
                        folded.append([lineno, [text]])
                    folded_cont = cont
                    if not cont:
                        folded.append([None, []])
                    continue
                if depth < folded_base:
                    folded_base = depth
                    folded.append([None, []])
                if folded[-1][0] is None:
                    folded[-1][0] = lineno
                folded[-1][1].append(line.strip())
                continue
            yield from close_folded()
        fold = _FOLDED_KEY.match(keyline)
        if fold:
            folded, folded_at = [[None, []]], lineno
            folded_indent = len(fold.group(1))
            # An explicit indentation indicator SETS the content column, so a
            # line further right than it is more-indented and keeps its
            # newline. Deriving the column from the first content line instead
            # made that line the base and folded the block into one argv,
            # hiding whatever followed. Only when there is no indicator does
            # the first content line decide.
            folded_literal, folded_cont = fold.group(2) == '|', False
            folded_base = (folded_indent + int(fold.group(3))
                           if fold.group(3) else None)
            folded_slot = ('args' if _ARGS_KEY_LINE.match(keyline)
                           else 'entry' if _ENTRY_KEY_LINE.match(keyline)
                           else 'cmd' if _EXEC_KEY_LINE.match(keyline)
                           else None)      # `script:`/`run:` pair with nothing
            continue

        # A command key opens a list — inline on its own line, or as the block
        # of items below it. Anything else closes it.
        item = _LIST_ITEM.match(line)
        if pending is not None and item and len(item.group(1)) >= pending_indent:
            pending.append(_list_value(item.group(2)))
            continue
        # YAML allows a blank line or a standalone comment between sequence
        # entries. Closing the list on one reduced a valid `command:` to its
        # first element and reported a bare root on a correct manifest.
        if pending is not None and not _strip_comment(line).strip():
            continue
        yield from close_pending()
        inline = _KEY_INLINE.match(keyline) or _ARGS_INLINE.match(keyline)
        key = inline or _KEY_ONLY.match(keyline) or _ARGS_ONLY.match(keyline)
        if key:
            if _ARGS_ONLY.match(keyline) or _ARGS_INLINE.match(keyline):
                kind = 'args'
            elif _ENTRY_KEY_LINE.match(keyline):
                kind = 'entry'
            elif _EXEC_KEY_LINE.match(keyline):
                kind = 'cmd'
            else:
                kind = 'script'
            pending_kind, pending_at = kind, lineno
            pending_indent = len(key.group(1))
            pending = _inline_items(inline.group(2)) if inline else []
            if inline:
                yield from close_pending()
            continue
        # The same keys with a plain scalar value. A scalar fills its slot like
        # a list does, so `entrypoint: ["autumn"]` + `command: migrate` is one
        # argv — and routing it here rather than leaving it to the line scan is
        # what keeps a single report per recipe instead of two readings of it.
        scalar = _KEY_SCALAR.match(keyline)
        if scalar:
            if _ARGS_KEY_LINE.match(keyline):
                kind = 'args'
            elif _ENTRY_KEY_LINE.match(keyline):
                kind = 'entry'
            elif _EXEC_KEY_LINE.match(keyline):
                kind = 'cmd'
            else:
                kind = None         # `script:`/`run:` — one whole shell line,
            if kind:                # which the ordinary line scan reads.
                # YAML's own quoting comes off FIRST. Left on, the whole
                # command became one shell token — `command: "autumn migrate"`
                # tokenized to a single `autumn migrate` that matched no
                # executable — so quoting a scalar silently disabled the check
                # while the identical unquoted line was read.
                value = scalar.group(2)
                if len(value) > 1 and value[0] == value[-1] and value[0] in '"\'':
                    value = value[1:-1]
                pending_kind, pending_at = kind, lineno
                pending_indent = len(scalar.group(1))
                pending = tokenize(value) or value.split()
                yield from close_pending()
                continue
        # A held half waits for its sibling across intervening keys, because a
        # YAML mapping is unordered — `image:` sits between the two often
        # enough, and `args:` may be written first.
        #
        # What ends the wait is the start of a SIBLING object: a list entry at
        # or left of the pair's column, or a mapping key strictly left of it.
        # Compose names its services with mapping keys rather than list items,
        # so watching only for list entries let two services' slots run
        # together into `worker autumn definitely-not-a-command`, where autumn
        # is no longer the head and the drift went unseen. The `<` is what
        # keeps a sibling key of the SAME object — `image:` between `command:`
        # and `args:` — from ending it.
        if item and len(item.group(1)) <= slots_indent:
            yield from emit_slots()
        elif slots and _MAPPING_KEY.match(keyline) \
                and len(_MAPPING_KEY.match(keyline).group(1)) < slots_indent:
            yield from emit_slots()

        logical, at = flush(line, lineno)
        if logical is None:
            continue
        # Operator splitting happens inside `commands()`, on tokens rather than
        # on the raw text, so an operator inside a quoted value cannot cut a
        # command in half.
        for display, argv in commands(logical):
            yield at, display, argv, FENCED_COMMAND
        # …and a line that OPENS a heredoc is itself a command, but everything
        # after it until the terminator is the data it writes.
        # The DELIMITER is read from the real line — masking would replace the
        # name inside `<<'EOF'` with filler — while the masked copy decides
        # whether the operator itself was written inside single quotes. A
        # `<<EOF` mentioned in a comment or quoted as an argument opens
        # nothing, and treating it as an opener silenced every line after it.
        # `<<-` allows the terminator to be indented with TABS; a plain `<<`
        # requires it alone on the line — that pair is carried with each
        # opener, and every rule about which operators are real lives in
        # `_heredoc_openers`.
        heredocs.extend(_heredoc_openers(_strip_comment(logical)))
        # Backticks inside a fence are not markdown spans. In a shell block
        # they are legacy command substitution — `` OUT=`autumn migrate` `` is
        # a command the reader runs — and elsewhere they are a line quoting a
        # command back. Either way the text is inside a fence, so it is
        # RUNNABLE and a waiver must not reach it. Marking these `False` let a
        # nearby marker suppress a fenced command, contradicting the invariant
        # stated at the top of this file: a page may name a nonexistent
        # command, never hand one over to run.
        # In a SHELL-tagged fence, backticks on a real command line are legacy
        # command substitution — `` OUT=`autumn db` `` runs `autumn db`, which
        # clap rejects — so those are full command lines. On a COMMENT line
        # they are prose quoting a command back, which is what
        # `maintenance-mode.md:131` does (`# on every host `autumn deploy`
        # manages`), and in a non-shell fence they are prose too: untagged
        # fences in `skills/autumn-web/SKILL.md` hold paragraphs of English,
        # and `generators.md:514` is a rust `//!` doc comment. Those name a
        # command group the ordinary way and must not read as bare-group
        # defects — but they are still fenced, so no waiver reaches them.
        shell_line = lang in _SHELL_LANGS and not line.lstrip().startswith('#')
        for executes, span in _backticks(line):
            # Single quotes make a backtick literal — `printf '%s\n' '`autumn
            # db`'` prints the text and runs nothing — so a run opening inside
            # them is data, not a command. It is still read as a mention, so
            # drift in it is reported; it just is not a runnable command line.
            runnable = shell_line and executes
            for display, argv in commands(span):
                yield lineno, display, argv, (FENCED_COMMAND if runnable
                                              else FENCED_SPAN)

    yield from close_folded()
    yield from close_pending()
    yield from emit_slots()
    yield from _spans(take_paragraph(), commands)


# Where an invocation was found. These decide two DIFFERENT things, and one
# boolean was conflating them:
#
#   * RUNNABLE — is this a line the reader copies and runs? Only then is a bare
#     command group (`autumn deploy`) a defect, because English names the
#     family that way constantly.
#   * FENCED — is this inside a code fence? Then no waiver may reach it: a page
#     may NAME a command that does not exist, never hand one over to run.
#
# A backticked span inside a fence is the case that separates them. It is not
# a command line — untagged fences in `skills/autumn-web/SKILL.md` hold whole
# paragraphs of prose, and a `rust` fence's `//! Generated by \`autumn
# generate\`.` is a doc comment — so bare groups there are not defects. But it
# IS inside a fence, so a nearby marker must not silence real drift in it.
# Treating it as neither left `` OUT=`autumn nope` `` waivable; treating it as
# both reported seven correct pages.
# Fences whose contents are shell input, where a backtick run is command
# substitution rather than a quoted mention. Deliberately does NOT include the
# untagged fence: `skills/autumn-web/SKILL.md` opens two of those around whole
# paragraphs of documentation prose.
_SHELL_LANGS = {'bash', 'sh', 'shell', 'zsh', 'ksh'}

FENCED_COMMAND = 'fenced-command'       # runnable, not waivable
FENCED_SPAN = 'fenced-span'             # not runnable, not waivable
PROSE_SPAN = 'prose-span'               # not runnable, waivable


def _spans(pairs, commands):
    """Turn (lineno, span_text) pairs into invocation tuples."""
    for lineno, span in pairs:
        for display, argv in commands(span):
            yield lineno, display, argv, PROSE_SPAN


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


def _short_cluster(tok, options):
    """Tokens consumed by a compact short-option group, or None if unknown.

    Returns 1 when the group is self-contained — every letter is a boolean
    (`-rd`), or the letter that takes a value carries it attached (`-pfoo`) —
    and 2 when the group ends on a value-taking option whose value is the next
    token (`-rp foo`).

    Returns None the moment a letter is not declared, so an unrecognised group
    is still not walked past: whether it eats the following token is exactly
    what is unknown there, and guessing is how a gate invents a defect on a
    correct page.
    """
    for pos, ch in enumerate(tok[1:], start=1):
        name = '-' + ch
        if name not in options:
            return None
        if options[name]:                       # this letter takes a value
            return 1 if pos < len(tok) - 1 else 2
    return 1


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
            if not tok.startswith('--') and len(tok) > 2:
                # A compact short-option group. POSIX lets shorts bundle and
                # lets the last one carry its value attached, so `-pfoo` is
                # `--package foo` and `-rd` is two booleans — both of which
                # read as one unrecognised token and stopped the walk, leaving
                # every subcommand written after them unchecked.
                eaten = _short_cluster(tok, node['options'])
                if eaten is not None:
                    i += eaten
                    continue
            if name not in node['options']:
                # An option this command does not declare. Flags are out of
                # scope, so this is not reported — but it also cannot be walked
                # past safely, since whether it consumes the next token is
                # unknown. Stop rather than risk a false positive.
                return None
            i += 2 if node['options'][name] else 1
            continue
        if not node['children']:
            # A leaf: everything left is arguments, and here the ambiguity
            # between "value" and "subcommand name" is gone, so the remaining
            # tokens can be COUNTED against the required positionals rather
            # than abandoned. `autumn generate controller pages` supplies
            # `name` and stops, but `actions` is `required = true` too.
            supplied = 0
            while i < len(tokens):
                t2 = tokens[i]
                if t2 == '--':
                    i += 1
                    continue
                if t2.startswith('-') and len(t2) > 1:
                    o = t2.split('=', 1)[0]
                    if '=' in t2:
                        i += 1
                        continue
                    if o not in node['options']:
                        return None             # unknown arity: cannot count on
                    i += 2 if node['options'][o] else 1
                    continue
                eaten = _redirect(t2)
                if eaten:                       # a redirection is not an argument
                    i += eaten
                    continue
                supplied += 1
                i += 1
            if runnable and supplied < node['required_args']:
                return 'autumn ' + path
            return None
        eaten = _redirect(tok)
        if eaten:                               # shell plumbing, not the command
            i += eaten
            continue
        if not TOKEN.match(tok):
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

    # Same shape one level down, for a command reached with no tokens left:
    # `autumn replay` exits with "the following required arguments were not
    # provided: <CAPSULE>".
    if runnable and surface[path]['required_args']:
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

        for lineno, display, argv, where in invocations(text):
            bad = resolve(argv, surface, runnable=where == FENCED_COMMAND)
            if bad is None:
                continue
            command = bad[len('autumn '):]
            # A fenced shell block is copyable, so nothing waives it: a page may
            # NAME a command that does not exist, never hand one over to be run.
            # That covers a backticked span inside a fence too, which is not a
            # command line but is still fenced.
            if where == PROSE_SPAN and line_block[lineno] in allowed.get(command, ()):
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
            // Shorts, for the compact-group walk. The real CLI declares no
            // BOOLEAN short today — every short it has takes a value — so
            // that branch of `_short_cluster` is reachable only here, which
            // is what a synthetic CLI is for.
            #[arg(short, long)]
            package: Option<String>,
            #[arg(short, long)]
            verbose: bool,
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
        Controller {
            name: String,
            #[arg(required = true)]
            actions: Vec<String>,
            #[arg(long)]
            api: bool,
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
    checked = []

    def tk(argv):
        """Shell-tokenize a bare argv the way `commands()` would."""
        return tokenize(argv) or argv.split()

    def expect(cond, msg):
        # The total is counted here rather than written down as a sum of
        # section sizes. A hand-maintained count is a second source of truth
        # that no test can fail on: assertions were added without it moving,
        # so it under-reported the suite it was there to describe — the same
        # drift this whole script exists to gate, in the gate's own output.
        checked.append(msg)
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
    expect(surface['migrate']['options'] == {'--with-maintenance': False, '--shard': True,
                                             '-p': True, '--package': True,
                                             '-v': False, '--verbose': False},
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

    # --- a redirection is shell plumbing, not part of the command. `<` and `>`
    # are deliberately not split on (the docs write `--shard <new>`), so a
    # redirection arrives whole and stopped the walk before it could judge
    # whether the command was complete.
    for form in ('>/tmp/out', '> /tmp/out', '2>/dev/null', '>>log'):
        expect(resolve(tk('db ' + form), surface, runnable=True) == 'autumn db',
               f'a bare group is still incomplete before {form!r}')
    # `&>all` and `2>&1` carry an `&`, which is an operator: they are split by
    # SEGMENTATION before `resolve` ever sees them, so they have to be checked
    # through the whole pipeline rather than as a token list.
    for form in ('&>all', '2>&1', '>out'):
        doc = f'```bash\nautumn db {form}\n```'
        got = [resolve(a, surface, runnable=w == FENCED_COMMAND)
               for _, _, a, w in invocations(doc)]
        expect(got == ['autumn db'],
               f'a bare group is still incomplete before {form!r}: {got}')
    expect(resolve(tk('replay >out.json'), surface, runnable=True) == 'autumn replay',
           'a redirection is not the required positional')
    expect(resolve(tk('replay capsule.json >out'), surface, runnable=True) is None,
           'a real positional plus a redirection is complete')
    expect(resolve(tk('migrate --shard <new>'), surface, runnable=True) is None,
           'a <placeholder> is not a redirection')
    expect(resolve(tk('migrate --shard <new> status'), surface, runnable=True) is None,
           'a placeholder does not hide the subcommand behind it')
    expect(resolve(tk('nope >/tmp/out'), surface) == 'autumn nope',
           'drift before a redirection is still reported')

    # --- compact short-option groups. `-pfoo` and `-rd` are one token each,
    # and both read as an unrecognised option, which stopped the walk and left
    # whatever followed unchecked. Every short the real CLI declares takes a
    # value, so the boolean branches below are reachable only through the
    # synthetic `-v` above — which is the point of having a synthetic CLI.
    expect(resolve(tk('migrate -pfoo nope'), surface) == 'autumn migrate nope',
           'an attached short-option value must not hide the drift behind it')
    expect(resolve(tk('migrate -pfoo status'), surface) is None,
           'a real subcommand behind an attached value must still resolve')
    expect(resolve(tk('migrate -p foo nope'), surface) == 'autumn migrate nope',
           'the spaced form of the same option must keep working')
    expect(resolve(tk('migrate -v nope'), surface) == 'autumn migrate nope',
           'a boolean short consumes no value')
    expect(resolve(tk('migrate -vpfoo nope'), surface) == 'autumn migrate nope',
           'a bundled boolean then an attached value is one self-contained token')
    expect(resolve(tk('migrate -vp foo nope'), surface) == 'autumn migrate nope',
           'a bundle ending on a value-taking short eats the NEXT token')
    expect(resolve(tk('migrate -vp foo status'), surface) is None,
           'that bundle must not eat the subcommand as well')
    expect(resolve(tk('migrate -pv nope'), surface) == 'autumn migrate nope',
           "a value-taking short swallows the rest of its group, so `v` is its value")
    expect(resolve(tk('migrate -Zfoo nope'), surface) is None,
           'an unknown short group still stops the walk rather than guessing')
    expect(resolve(tk('migrate -vZ nope'), surface) is None,
           'one unknown letter makes the whole group unknown')

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
    expect([where for _, _, _, where in found] == [FENCED_COMMAND, PROSE_SPAN],
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
    # `cd autumn` is a directory; `./autumn` is the binary. Conflating the two
    # is how this assertion was originally written, and it encoded the wrong
    # belief: a path-qualified executable IS an invocation.
    expect([d for _, d, _, _ in invocations('```bash\ncd autumn && ./autumn migrate\n```')]
           == ['migrate'],
           '`cd autumn` is a directory, but `./autumn` is the binary')
    expect(_autumn_exe('autumn') and _autumn_exe('./autumn')
           and _autumn_exe('/usr/local/bin/autumn'),
           'the binary is recognised however its path is spelled')
    expect(not _autumn_exe('autumn-cli') and not _autumn_exe('autumn/src/lib.rs'),
           'a different basename is not the binary')
    expect(not _autumn_exe('https://alerts.example.com/hooks/autumn'),
           'a URL ending in /autumn is not an executable')
    expect(not _autumn_exe('AUTUMN_CLUSTER__CLUSTER_NAME=autumn'),
           'an assignment token is not itself the binary')
    expect(_exe_path('/usr/local/bin/autumn') and not _exe_path('autumn'),
           "an assignment's VALUE must be a PATH before it is a program")

    # --- a `--` separator introduces a nested command only for a program that
    # RUNS what follows it. `head is None` established only that the segment is
    # not an autumn command itself, which admitted every other program too —
    # and `echo -- autumn db` prints those words rather than running them.
    for runner in ('kubectl exec --', 'kubectl exec deploy/app --',
                   'docker exec ctr --', 'sudo --', 'xargs --',
                   '/usr/bin/kubectl exec --'):
        doc = f'```bash\n{runner} autumn migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'{runner!r} runs what follows: {got}')
    # The shell BUILTINS take options too, and stepping over the keyword alone
    # left the first of them looking like the executable.
    for builtin in ('command -p', 'time -p', 'exec -a name',
                    'command', 'exec', 'nohup'):
        doc = f'```bash\n{builtin} autumn migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'{builtin!r} must reach the command: {got}')
    # `xargs [OPTION]... COMMAND` runs COMMAND directly, not only after a
    # `--`. It was listed as a runner for the separator form alone, so the
    # ordinary pipeline spelling went unread entirely.
    for form in ('printf x | xargs', 'xargs', 'xargs -n1', 'xargs -I{}',
                 'xargs -I {}', 'xargs --max-args 1', 'xargs --'):
        doc = f'```bash\n{form} autumn migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'{form!r} runs its command: {got}')
    expect(list(invocations('```bash\ncat list | xargs rm\n```')) == [],
           'xargs running something else stays quiet')

    # …but `-v`/`-V` make `command` DESCRIBE its arguments rather than run
    # them, so nothing there is an invocation. An earlier round asserted the
    # opposite, which encoded the bug rather than the rule.
    for inspect in ('command -v', 'command -V', 'command -pv', 'command -vp'):
        doc = f'```bash\n{inspect} autumn db\n```'
        expect(list(invocations(doc)) == [],
               f'{inspect!r} inspects, it does not run: {list(invocations(doc))}')

    for printer in ('echo', 'printf', 'ls'):
        doc = f'```bash\n{printer} -- autumn db\n```'
        expect(list(invocations(doc)) == [],
               f'{printer} -- prints its arguments, it does not run them: '
               f'{list(invocations(doc))}')
    # …and a runner NAME is not a runner: the command position decides.
    for data in ('echo docker -- autumn db', 'echo kubectl -- autumn db',
                 'ls docker -- autumn db'):
        doc = f'```bash\n{data}\n```'
        expect(list(invocations(doc)) == [],
               f'a runner named as data does not run anything: {data} -> '
               f'{list(invocations(doc))}')

    # --- a redirection may also come BEFORE the command, and the prefix walk
    # stopped on it, so the whole line went unread.
    for lead in ('>/tmp/out', '> /tmp/out', '2>/dev/null', '<in.txt'):
        doc = f'```bash\n{lead} autumn migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'a leading {lead!r} must be stepped over: {got}')
    lead_bare = '```bash\n>/tmp/out autumn db\n```'
    got = [resolve(a, surface, runnable=w == FENCED_COMMAND)
           for _, _, a, w in invocations(lead_bare)]
    expect(got == ['autumn db'], f'…and the command is still judged complete: {got}')
    # `2>&1` is ONE redirection, but `&` is punctuation, so shlex hands back
    # three tokens. They are rejoined — merely declining to SPLIT there left
    # `1` as the segment head and the command after it went unread.
    for dup in ('2>&1', '1>&2', '>&2'):
        doc = f'```bash\n{dup} autumn migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'a leading {dup} must be one token: {got}')
        trail = f'```bash\nautumn db {dup}\n```'
        got = [resolve(a, surface, runnable=w == FENCED_COMMAND)
               for _, _, a, w in invocations(trail)]
        expect(got == ['autumn db'], f'…and a trailing {dup} is not an argument: {got}')
    # …while a real `&` still separates commands.
    amp = '```bash\nautumn migrate & autumn db\n```'
    got = [resolve(a, surface, runnable=w == FENCED_COMMAND)
           for _, _, a, w in invocations(amp)]
    expect(got == [None, 'autumn db'], f'`&` still separates two commands: {got}')

    # --- a heredoc body is DATA the shell writes, not commands it runs.
    # A backslash quotes the delimiter too, exactly as the quote marks do.
    for opener in ("<<'EOF'", '<<"EOF"', '<<EOF', '<<-EOF', '<<\\EOF', '<<-\\EOF'):
        doc = f'```bash\ncat >/tmp/x {opener}\nautumn db\nEOF\nautumn migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'],
               f'a heredoc body ({opener}) is not scanned, and the line after it is: {got}')
    # `<<<EOF` is a HERE-STRING: it feeds one word to stdin and consumes no
    # following lines. Reading it as a heredoc swallowed the command under it.
    hstring = '```bash\ncat <<<EOF\nautumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(hstring)] == ['migrate run'],
           f'a here-string opens no heredoc: {list(invocations(hstring))}')
    # …and a `<<EOF` that is only mentioned opens nothing either. BOTH quote
    # kinds make the operator inert, unlike `$( )`, where only single ones do.
    # The last of these is why the terminator search honours backslash
    # escapes: a plain search mistook the escaped quote for the end of the
    # string, left the operator unmasked, and opened a heredoc on data.
    for mention in ('# see <<EOF below', "echo '<<EOF'", 'printf \'%s\' "<<EOF"',
                    'printf \'%s\' "\\" <<EOF"'):
        doc = f'```bash\n{mention}\nautumn migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'],
               f'a mentioned <<EOF ({mention}) opens nothing: {got}')
    # The terminator's indentation is part of the grammar: a plain `<<` ends
    # only on the delimiter alone, while `<<-` allows leading TABS and not
    # spaces. Accepting any indentation closed a heredoc early and reported
    # its data as a runnable command.
    indented_term = '```bash\ncat <<EOF\n EOF\nautumn db\n```'
    expect(list(invocations(indented_term)) == [],
           f'an indented terminator does not end a plain heredoc: '
           f'{list(invocations(indented_term))}')
    tab_term = '```bash\ncat <<-EOF\nautumn db\n\tEOF\nautumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(tab_term)] == ['migrate run'],
           f'`<<-` ends on a tab-indented terminator: {list(invocations(tab_term))}')
    space_term = '```bash\ncat <<-EOF\nautumn db\n  EOF\n```'
    expect(list(invocations(space_term)) == [],
           f'…but not a space-indented one: {list(invocations(space_term))}')
    # The first UNMASKED match opens the heredoc, not the first textual one.
    after_quoted = ('```bash\nprintf \'%s\' "<<NO"; cat <<EOF\nautumn db\nEOF\n'
                    'autumn migrate run\n```')
    expect([d for _, d, _, _ in invocations(after_quoted)] == ['migrate run'],
           f'a quoted candidate must not hide a real opener after it: '
           f'{list(invocations(after_quoted))}')
    # One command line may open SEVERAL heredocs, and bash consumes their
    # bodies in order. Keeping only the first delimiter resumed scanning
    # inside the second body, so its data read as commands.
    two_docs = ('```bash\ncat <<ONE <<TWO\nautumn db\nONE\nautumn db\nTWO\n'
                'autumn migrate run\n```')
    expect([d for _, d, _, _ in invocations(two_docs)] == ['migrate run'],
           f'both heredoc bodies are data: {list(invocations(two_docs))}')
    # An unterminated heredoc must not swallow the rest of the FILE.
    unterminated = ('```bash\ncat <<EOF\nautumn db\n```\n\n'
                    '```bash\nautumn migrate run\n```')
    expect([d for _, d, _, _ in invocations(unterminated)] == ['migrate run'],
           f'a fence boundary ends a heredoc: {list(invocations(unterminated))}')

    # --- an option is a command string only when its OWNER makes it one. Any
    # program may take a `-c` or an `--entrypoint`; only some run the value.
    for runner in ("sh -c", "bash -c", "su -c", "fly ssh console -C"):
        doc = f"```bash\n{runner} 'autumn migrate run'\n```"
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'{runner!r} runs its string: {got}')
    for other in ("echo -c", "printf -c", "ls -c",
                  # `ssh -c` names a CIPHER and `kubectl -c` a container.
                  "ssh -c", "kubectl logs -c"):
        doc = f"```bash\n{other} 'autumn db'\n```"
        expect(list(invocations(doc)) == [],
               f'{other!r} does not run its argument: {list(invocations(doc))}')
    # …and the owner is the word the option is attached to, which is not
    # always the segment head: a shell reached THROUGH another program still
    # owns its own `-c`.
    for nested in ('kubectl exec pod -- sh -c', 'docker exec c sh -c',
                   'env FOO=1 sh -c', 'ssh host sh -c'):
        doc = f"```bash\n{nested} 'autumn migrate run'\n```"
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'{nested!r} owns its -c: {got}')
    for runner in ('docker run', 'podman run', 'nerdctl run'):
        doc = f'```bash\n{runner} --entrypoint autumn img migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'{runner!r} takes an entrypoint: {got}')
    for other in ('echo', 'ls'):
        doc = f'```bash\n{other} --entrypoint autumn img db\n```'
        expect(list(invocations(doc)) == [],
               f'{other!r} has no entrypoint to override: {list(invocations(doc))}')

    # --- a trailing backslash continues a line only when itself unescaped.
    escaped = "```bash\nprintf '%s' \\\\\\\\\nautumn migrate run\n```"
    expect([d for _, d, _, _ in invocations(escaped)] == ['migrate run'],
           f'a doubled backslash is literal, not a continuation: '
           f'{list(invocations(escaped))}')
    real = '```bash\nautumn maintenance on \\\n  --reason x\n```'
    expect([d for _, d, _, _ in invocations(real)] == ['maintenance on --reason x'],
           f'a single backslash still continues: {list(invocations(real))}')
    # …and a backslash inside a COMMENT continues nothing. Joining there
    # swallowed the next line into the comment, where shlex discarded it.
    commented = '```bash\necho ok # no continuation \\\nautumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(commented)] == ['migrate run'],
           f'a backslash in a comment is comment text: {list(invocations(commented))}')

    # --- a folded scalar may carry an explicit indentation indicator, in
    # either order with the chomping one.
    for indicator in ('>', '>-', '>+', '>2', '>2-', '>-2'):
        doc = f'```yaml\nrun: {indicator}\n  autumn migrate\n  run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'`{indicator}` folds: {got}')
    # An explicit indicator SETS the content column, so a line further right
    # than it is more-indented and keeps its newline. Taking the column from
    # the first content line instead folded the block into one argv and hid
    # whatever followed. Both lines here sit at 4, with the indicator at 2.
    explicit = '```yaml\nrun: >2-\n    autumn routes\n    autumn run\n```'
    expect([d for _, d, _, _ in invocations(explicit)] == ['routes', 'run'],
           f'an explicit indicator decides the base column: {list(invocations(explicit))}')
    # …and with no indicator the first content line still decides.
    implicit = '```yaml\nrun: >\n    autumn migrate\n    run\n```'
    expect([d for _, d, _, _ in invocations(implicit)] == ['migrate run'],
           f'without one, the first content line decides: {list(invocations(implicit))}')

    # --- a brace group runs the commands inside it, so `{` stands in front of
    # a command the way a subshell's `(` does.
    for group in ('{ autumn migrate run; }', '( autumn migrate run )',
                  '{ autumn migrate run; } >log'):
        doc = f'```bash\n{group}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'a group must be entered: {group} -> {got}')
    expect(list(invocations('```bash\n{ echo hi; }\n```')) == [],
           'a group running something else stays quiet')

    # --- wrappers that put their OWN options between the keyword and the
    # command they launch. The scan stopped on the first flag, so everything
    # after it went unread — `env` entirely, since it was not even a keyword.
    for launcher in ('env AUTUMN_ENV=prod autumn migrate run',
                     'env autumn migrate run',
                     'env -i autumn migrate run',
                     'env -u FOO autumn migrate run',
                     'env --chdir=/srv autumn migrate run',
                     'env - autumn migrate run',
                     'env -u FOO -i AUTUMN_ENV=prod autumn migrate run',
                     'sudo autumn migrate run',
                     'sudo -u postgres autumn migrate run',
                     'sudo -u postgres env AUTUMN_ENV=prod autumn migrate run'):
        doc = f'```bash\n{launcher}\n```'
        expect([d for _, d, _, _ in invocations(doc)] == ['migrate run'],
               f'a command launched through a wrapper must be read: {launcher} -> '
               f'{list(invocations(doc))}')
    # A value-taking option must eat its value and no more, so a real
    # subcommand behind it still resolves…
    ok = '```bash\nsudo -u postgres autumn migrate\n```'
    expect([d for _, d, _, _ in invocations(ok)] == ['migrate'],
           f'a correct command behind a wrapper option must resolve: {list(invocations(ok))}')
    # …including when the option's VALUE is itself the word `autumn`.
    named = '```bash\nsudo -u autumn autumn migrate\n```'
    expect([d for _, d, _, _ in invocations(named)] == ['migrate'],
           f"a user named `autumn` is not the command: {list(invocations(named))}")
    expect(list(invocations('```bash\nenv | grep AUTUMN\n```')) == [],
           'env printing the environment launches nothing')

    # A systemd unit writes the command as an assignment to a path.
    unit = '```ini\n[Service]\nExecStart=/usr/local/bin/autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(unit)] == ['migrate run'],
           f'a systemd ExecStart= recipe must be read: {list(invocations(unit))}')
    for directive in ('ExecStartPre', 'ExecStop', 'ExecReload'):
        doc = f'```ini\n{directive}=/usr/local/bin/autumn migrate run\n```'
        expect([d for _, d, _, _ in invocations(doc)] == ['migrate run'],
               f'{directive}= is executed too: {list(invocations(doc))}')
    # …but an ordinary assignment does NOT run its value. Documenting a
    # reusable binary path is a correct and ordinary thing for a page to do,
    # and reading it as a command reported it as a bare root — failing a
    # correct page, which is the direction that teaches readers to ignore a
    # gate. The KEY has to say the value is executed.
    for assignment in ('BIN=/usr/local/bin/autumn',
                       'AUTUMN=./autumn',
                       'BIN="/usr/local/bin/autumn"',
                       'EXECUTABLE=/usr/local/bin/autumn'):
        doc = f'```bash\n{assignment}\n```'
        expect(list(invocations(doc)) == [],
               f'a plain assignment is not an invocation: {assignment} -> '
               f'{list(invocations(doc))}')
    # The variable being USED is still a command, and still not `autumn`:
    # nothing resolves `$BIN`, so this degrades to silence rather than drift.
    used = '```bash\nBIN=/usr/local/bin/autumn\n$BIN migrate run\n```'
    expect(list(invocations(used)) == [],
           f'a command run through a variable stays quiet: {list(invocations(used))}')
    # --- operators inside a command substitution do not separate commands,
    # but a plain subshell's really do.
    subst = ('```bash\nKEY=$(cat secret | tr -d \'x\') autumn migrate run\n```')
    expect([d for _, d, _, _ in invocations(subst)] == ['migrate run'],
           f'a pipe inside $( ) must not split the command: {list(invocations(subst))}')
    # shlex coalesces a run of punctuation, so a nested substitution ends in a
    # single `))` token; testing for an exact `)` never reached depth zero and
    # swallowed the command that followed.
    nested_sub = '```bash\nTAG=$(echo $(date +%s)) autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(nested_sub)] == ['migrate run'],
           f'a coalesced `))` must close both substitutions: {list(invocations(nested_sub))}')
    # A parenthesis inside a quoted argument is DATA, not nesting. Counting it
    # opened a depth the real `)` only closed halfway, so the substitution was
    # never terminated and the command inside it was dropped.
    # Single quotes only: a double quote inside a double-quoted substitution
    # re-opens rather than nests, and shlex splits the token three ways there.
    # That is genuinely ambiguous shell which the corpus does not write, so it
    # is left unasserted rather than pinned to a guess.
    for label, quoted in [("single", "'('"), ("closer", "')'"),
                          ("both", "'()'")]:
        doc = f'```bash\nOUT="$(printf {quoted}; autumn migrate run)"\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'],
               f'a {label} quoted paren must not affect nesting: {got}')
    deep_sub = '```bash\nA=$(f $(g $(h))) autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(deep_sub)] == ['migrate run'],
           f'three levels close on one `)))`: {list(invocations(deep_sub))}')
    expect(_paren_delta('))') == -2 and _paren_delta('(') == 1,
           'a punctuation run counts every parenthesis in it')
    expect(_paren_delta('title:String(x)') == 0,
           'parens inside an ordinary word do not move the depth')

    sub_sh = '```bash\n(autumn migrate && autumn dev)\n```'
    expect([d for _, d, _, _ in invocations(sub_sh)] == ['migrate', 'dev'],
           f'a plain subshell still holds two commands: {list(invocations(sub_sh))}')

    # --- a nested command line is matched with the SAME executable rule as the
    # outer one, and a `-c` value with no arguments still reaches the bare-root
    # check.
    nested = '```bash\nsh -c "/usr/local/bin/autumn migrate run"\n```'
    expect([d for _, d, _, _ in invocations(nested)] == ['migrate run'],
           f'a path-qualified exe inside a wrapper must be read: {list(invocations(nested))}')
    bare_c = '```bash\nsh -c "autumn"\n```'
    expect([a for _, _, a, _ in invocations(bare_c)] == [[]],
           f'a zero-argument `-c` value must still yield: {list(invocations(bare_c))}')
    # …but a bare `autumn` used as an option VALUE is not a command line.
    ep = '```bash\ndocker run --entrypoint autumn img:1 migrate run\n```'
    expect([d for _, d, _, _ in invocations(ep)] == ['migrate run'],
           f'--entrypoint autumn is a value, read via the image rule: {list(invocations(ep))}')

    # --- a nested command line is RE-ENTERED through the same parser, so an
    # environment prefix or a chain inside one needs no separate handling.
    envc = '```bash\nsh -c "AUTUMN_ENV=prod autumn migrate run"\n```'
    expect([d for _, d, _, _ in invocations(envc)] == ['migrate run'],
           f'an env prefix inside a -c wrapper must be read: {list(invocations(envc))}')
    subcmd = '```bash\nOUT=$(autumn migrate run)\n```'
    expect([d for _, d, _, _ in invocations(subcmd)] == ['migrate run'],
           f'a command INSIDE a substitution must be read: {list(invocations(subcmd))}')
    both = '```bash\nOUT=$(autumn routes) autumn migrate run\n```'
    expect(sorted(d for _, d, _, _ in invocations(both)) == ['migrate run', 'routes'],
           f'the substitution and what follows it are both commands: {list(invocations(both))}')
    toml_cmd = '```toml\ncommand = "autumn migrate run"\n```'
    expect([d for _, d, _, _ in invocations(toml_cmd)] == ['migrate run'],
           f'a TOML `command = "…"` value must be read: {list(invocations(toml_cmd))}')

    # --- inner operators survive the unwrap. Filtering all punctuation out of
    # a substitution deleted the `;` and collapsed two commands into one.
    inner_op = '```bash\nOUT=$(printf x; autumn migrate run)\n```'
    expect([d for _, d, _, _ in invocations(inner_op)] == ['migrate run'],
           f'an operator inside a substitution must survive: {list(invocations(inner_op))}')
    # A QUOTED substitution stays inside one token, so `$` and `(` never sit
    # adjacent and the token-level scan cannot see it.
    quoted_sub = '```bash\nOUT="$(autumn migrate run)"\n```'
    expect([d for _, d, _, _ in invocations(quoted_sub)] == ['migrate run'],
           f'a substitution embedded in a token must be read: {list(invocations(quoted_sub))}')
    quoted_chain = '```bash\nOUT="$(autumn migrate && autumn dev)"\n```'
    expect(sorted(d for _, d, _, _ in invocations(quoted_chain)) == ['dev', 'migrate'],
           'a chain inside a quoted substitution is still a chain')

    # --- a shell keyword stands in FRONT of a command without being one.
    for line, what in (('if autumn migrate run; then echo ok; fi', 'if'),
                       ('while autumn migrate run; do sleep 1; done', 'while'),
                       ('sudo autumn migrate run', 'sudo')):
        doc = '```bash\n' + line + '\n```'
        expect('migrate run' in [d for _, d, _, _ in invocations(doc)],
               f'`{what}` must not hide the command after it: {list(invocations(doc))}')

    # What marks a quoted token as a command line is its PREFIX, not its
    # contents — otherwise a message that happens to start with `autumn ` is
    # read as an invocation and a correct page is reported.
    msg = '```bash\nautumn maintenance on --reason "autumn migrate failed"\n```'
    expect([d for _, d, _, _ in invocations(msg)] == ['maintenance on --reason autumn migrate failed'],
           f'a --reason message is an argument, not a nested command: {list(invocations(msg))}')

    # A scalar key that means "a command follows".
    compose = '```yaml\nservices:\n  migrate:\n    command: autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(compose)] == ['migrate run'],
           f'a Compose `command:` value must be read: {list(invocations(compose))}')
    step = '```yaml\n- run: autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(step)] == ['migrate run'],
           'a workflow `run:` step must be read')
    instr = '```text\n2. Run: autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(instr)] == ['migrate run'],
           'a skill instruction `Run:` must be read, case-insensitively')
    expect(list(invocations('```yaml\n  image: autumn-cli:1.2\n```')) == [],
           'a key outside the bounded set is not a command position')
    # --- `--` belongs to the autumn command when the segment IS one. Reading
    # the forwarded word as a second executable reported a VALID line as broken.
    fwd = '```bash\nautumn test -- autumn\n```'
    expect([d for _, d, _, _ in invocations(fwd)] == ['test -- autumn'],
           f'`autumn test -- autumn` forwards to the harness: {list(invocations(fwd))}')
    wrapped_sep = '```bash\nkubectl exec x -- autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(wrapped_sep)] == ['migrate run'],
           "a WRAPPER's `--` still introduces a nested command")

    # --- exec-form list values, which shlex hands back with the brackets and
    # commas still attached to the words.
    execform = '```yaml\ncommand: ["autumn", "migrate", "run"]\n```'
    expect([d for _, d, _, _ in invocations(execform)] == ['migrate run'],
           f'an exec-form command list must be read: {list(invocations(execform))}')
    expect(_unbracket(['[autumn,', 'migrate]']) == ['autumn', 'migrate'],
           'list punctuation is the list syntax, not the command')
    expect(_unbracket(['autumn', 'migrate']) == ['autumn', 'migrate'],
           'a plainly written value passes through unchanged')

    # --- a single-token TOML value is a command line: `release_command =
    # "autumn"` really is a broken release command.
    lone = '```toml\nrelease_command = "autumn"\n```'
    expect([a for _, _, a, _ in invocations(lone)] == [[]],
           f'a lone executable after `key =` must reach the root check: {list(invocations(lone))}')

    # A qualifier may precede the key word — Fly's `release_command` runs on
    # every deploy. Anchoring to the whole token missed both live uses of it,
    # and a synthetic bare `command = "…"` test passed while the real lines
    # went ungated.
    fly = '```toml\nrelease_command = "autumn migrate run"\n```'
    expect([d for _, d, _, _ in invocations(fly)] == ['migrate run'],
           f'a qualified command key must be read: {list(invocations(fly))}')
    expect(_COMMAND_KEY_BARE.match('release_command')
           and _COMMAND_KEY.match('pre-run:'),
           'a `_`- or `-`-separated qualifier is allowed before the key word')
    expect(not _COMMAND_KEY_BARE.match('commandeer')
           and not _COMMAND_KEY_BARE.match('rerun'),
           'the key word must be a whole segment, not a suffix of a longer word')

    cfgs = ('```bash\nAUTUMN_CLUSTER__CLUSTER_NAME=autumn\n'
            'AUTUMN_ALERTS__WEBHOOK_URL=https://alerts.example.com/hooks/autumn\n```')
    expect(list(invocations(cfgs)) == [],
           f'config values are not commands: {list(invocations(cfgs))}')
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
    expect(surface['replay']['required_args'] == 1,
           'a bare `capsule: String` field is one required positional')
    expect(surface['upgrade']['required_args'] == 0,
           'a positional with default_value is NOT required')
    expect(surface['db pull']['required_args'] == 0, 'a plain Vec<> is not required')
    expect(surface['new']['required_args'] == 0, 'an Option<> positional is not required')
    expect(surface['controller']['required_args'] == 2,
           'required-ness is a COUNT: `name` plus a `required = true` Vec')
    expect(resolve(tk('controller pages'), surface, runnable=True) == 'autumn controller',
           'stopping at the first positional must not accept a command still '
           'missing a second required one')
    expect(resolve(tk('controller pages home'), surface, runnable=True) is None,
           'supplying both required positionals resolves')
    expect(resolve(tk('controller pages home --api'), surface, runnable=True) is None,
           'an option after the positionals is not counted as one')

    # --- a container entrypoint names the binary; the command follows the IMAGE
    dock = '```bash\ndocker run --rm --entrypoint autumn my-app:latest migrate run\n```'
    expect([d for _, d, _, _ in invocations(dock)] == ['migrate run'],
           f'a command behind a docker entrypoint must be read: {list(invocations(dock))}')
    dock_eq = '```bash\ndocker run --entrypoint=autumn img migrate run\n```'
    expect([d for _, d, _, _ in invocations(dock_eq)] == ['migrate run'],
           '--entrypoint=autumn is the same layout')
    expect(list(invocations('```bash\ndocker run --entrypoint autumn my-app:latest\n```')) == [],
           'an entrypoint with no command after the image yields nothing')
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
    # The corpus only writes the `cd … &&` form above, where the command is
    # reached because `&&` starts a fresh segment. The bare crontab line — the
    # ordinary one — put `autumn` after the schedule, where it was not a head.
    bare_cron = '```cron\n0 3 * * * autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(bare_cron)] == ['migrate run'],
           f'a bare crontab line must be read: {list(invocations(bare_cron))}')
    for sched in ('*/10 * * * *', '0 2 * * MON-FRI', '@daily'):
        doc = f'```cron\n{sched} autumn migrate run\n```'
        expect([d for _, d, _, _ in invocations(doc)] == ['migrate run'],
               f'schedule {sched!r} must be stepped over: {list(invocations(doc))}')
    cron_d = '```cron\n0 2 * * * root autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(cron_d)] == ['migrate run'],
           f'a /etc/cron.d user field must be stepped over: {list(invocations(cron_d))}')
    # The user field is stepped over ONLY when the binary follows it, so a
    # schedule running some other program stays what it is.
    other = '```cron\n0 2 * * * root /usr/bin/backup autumn\n```'
    expect(list(invocations(other)) == [],
           f'a non-autumn cron command stays quiet: {list(invocations(other))}')
    # Five schedule fields is a shape no command line has, but the head that
    # follows still has to be the binary — a numeric argv must not be eaten.
    numeric = '```bash\nautumn task backfill 1 2 3 4 5 nope\n```'
    expect([d for _, d, _, _ in invocations(numeric)] == ['task backfill 1 2 3 4 5 nope'],
           f'a numeric argv is not a cron schedule: {list(invocations(numeric))}')
    # --- block-style exec arrays. The inline form tokenizes; the block form
    # arrives one item per line, and each item read alone is not a command.
    for label, doc in [
        ('deeper indent', '```yaml\ncommand:\n  - autumn\n  - migrate\n  - run\n```'),
        ('same column',   '```yaml\ncommand:\n- autumn\n- migrate\n- run\n```'),
        ('quoted items',  '```yaml\ncommand:\n  - "autumn"\n  - "migrate"\n  - "run"\n```'),
        ('trailing comma','```yaml\ncommand:\n  - autumn,\n  - migrate,\n  - run\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'],
               f'a block exec array ({label}) must be read: {got}')
    ok_list = '```yaml\ncommand:\n  - autumn\n  - migrate\n```'
    expect([d for _, d, _, _ in invocations(ok_list)] == ['migrate'],
           f'a correct block exec array must resolve, not report: {list(invocations(ok_list))}')
    # A list whose items hold spaces is a list of shell LINES (GitLab's
    # `script:`), not argv words. Reading an exec array that way would see a
    # lone `autumn` and report a bare root the next item answers.
    script = '```yaml\nscript:\n  - autumn migrate run\n  - cargo test\n```'
    expect([d for _, d, _, _ in invocations(script)] == ['migrate run'],
           f'a script: list is read as shell lines: {list(invocations(script))}')
    # --- Kubernetes splits one argv across `command:` and a sibling `args:`.
    # Judged apart, each half is wrong in a different direction: the command
    # half is a lone `autumn` that reads as a bare root on a correct manifest,
    # and the args half carries the subcommand that would actually drift,
    # attached to no executable. Joined, both become the argv the container
    # runs — which is why an earlier one-item suppression could be deleted.
    for label, doc in [
        ('block', '```yaml\ncommand:\n  - autumn\nargs:\n  - migrate\n```'),
        ('inline', '```yaml\ncommand: ["autumn"]\nargs: ["migrate"]\n```'),
        # A YAML mapping is unordered and `image:` sits between the two often
        # enough that the wait has to survive intervening keys.
        ('split by image:',
         '```yaml\ncommand: ["autumn"]\nimage: app:1\nargs: ["migrate"]\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate'], f'command:/args: ({label}) must join: {got}')
    for label, doc in [
        ('block', '```yaml\ncommand:\n  - autumn\nargs:\n  - nope\n```'),
        ('inline', '```yaml\ncommand: ["autumn"]\nargs: ["nope"]\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'drift in the args half ({label}) must be read: {got}')
    # …but the wait ends at the next container, so two entries never merge.
    two = ('```yaml\ncontainers:\n  - name: a\n    command: ["autumn"]\n'
           '  - name: b\n    args: ["migrate"]\n```')
    expect([d for _, d, _, _ in invocations(two)] == [''],
           f'a later container must not supply the first one args: {list(invocations(two))}')
    expect([d for _, d, _, _ in invocations('```yaml\ncommand: ["autumn"]\n```')] == [''],
           'a command with no args sibling is still a bare root')
    expect(list(invocations('```yaml\nargs:\n  - nope\n```')) == [],
           'an args: key with no command before it runs nothing')
    # YAML allows a blank line or a standalone comment between sequence
    # entries. Closing the list on one reduced a valid `command:` to its first
    # element and reported a bare root on a correct manifest.
    for label, gap in [('blank', ''), ('comment', '  # the subcommand')]:
        doc = f'```yaml\ncommand:\n  - autumn\n{gap}\n  - migrate\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate'], f'a {label} line must not close the list: {got}')

    # --- a fence nested in a markdown BLOCK QUOTE carries a `>` on every line,
    # so the marker is not at the start of it. This corpus has six such pages.
    # --- a fence closes only on a run at least as long as the one that opened
    # it, and a line carrying an info string is content rather than a closer.
    long_fence = '````markdown\n```\nautumn migrate run\n````'
    expect([d for _, d, _, _ in invocations(long_fence)] == ['migrate run'],
           f'a ``` inside a ```` block is content: {list(invocations(long_fence))}')
    tagged = '```text\n```bash\nautumn migrate run\n```'
    expect([w for _, _, _, w in invocations(tagged)] == [FENCED_COMMAND],
           f'a tagged line does not close an open fence: {list(invocations(tagged))}')
    plain = '```bash\nautumn migrate run\n```'
    expect([w for _, _, _, w in invocations(plain)] == [FENCED_COMMAND],
           'an ordinary fence still opens and closes normally')
    # A closing fence may be followed only by whitespace, so a run with text
    # after it is content. Checking merely that the info-string group was
    # empty let ``` followed by a comment close the block.
    trailing = '```bash\n``` # shown as output\nautumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(trailing)] == ['migrate run'],
           f'a run with text after it is not a closer: {list(invocations(trailing))}')
    expect([w for _, _, _, w in invocations('```bash\nautumn migrate run\n```   ')]
           == [FENCED_COMMAND],
           'trailing spaces after a closer are fine')
    # …and the info string may carry more than the language.
    expect(list(invocations('```rust,ignore\nlet m = "autumn migrate";\n```')) == [],
           'a comma-suffixed info string still names the language')

    quoted = '> ```bash\n> autumn migrate run\n> ```'
    expect([d for _, d, _, _ in invocations(quoted)] == ['migrate run'],
           f'a block-quoted fence must be read: {list(invocations(quoted))}')
    expect([w for _, _, _, w in invocations(quoted)] == [FENCED_COMMAND],
           'and its contents are runnable, like any other fence')
    # …while inside an ORDINARY fence a leading `>` is a redirection, which is
    # why the prefix is only stripped for a fence that opened inside a quote.
    redir = '```bash\n>/tmp/out autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(redir)] == ['migrate run'],
           f'a leading redirection is not a quote marker: {list(invocations(redir))}')
    # A YAML mapping is unordered, so `args:` may be written FIRST. Requiring
    # the command half to arrive first made source order significant and
    # reported a valid manifest as a bare root.
    for label, doc in [
        ('inline', '```yaml\nargs: ["migrate"]\ncommand: ["autumn"]\n```'),
        ('block', '```yaml\nargs:\n  - migrate\ncommand:\n  - autumn\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate'], f'args: before command: ({label}) must join: {got}')

    # --- an exec array's elements are not shell words. Both of these are ONE
    # argv, and treating an element's contents as the signal broke them.
    comma = '```yaml\ncommand: ["autumn", "migrate", "--shard", "eu,west", "status"]\n```'
    expect([d for _, d, _, _ in invocations(comma)] == ['migrate --shard eu,west status'],
           f'a comma inside a quoted element must not split it: {list(invocations(comma))}')
    for label, doc in [
        ('inline',
         '```yaml\ncommand: ["autumn", "migrate", "--shard", "eu west", "status"]\n```'),
        ('block',
         '```yaml\ncommand:\n  - autumn\n  - migrate\n  - --shard\n  - eu west\n  - status\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate --shard eu west status'],
               f'a spaced exec element ({label}) stays one argv: {got}')
    # --- a FOLDED block scalar joins its lines with spaces, so a command
    # written across two of them is one command. Scanning the physical lines
    # let the first resolve alone and dropped the rest, which is where the
    # drift was. The literal form keeps its lines separate and is already
    # right, so it must NOT be folded.
    for label, doc in [
        ('>', '```yaml\ncommand: >\n  autumn migrate\n  run\n```'),
        ('>-', '```yaml\ncommand: >-\n  autumn migrate\n  run\n```'),
        ('script: >', '```yaml\nscript: >\n  autumn migrate\n  run\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'a folded scalar ({label}) is one command: {got}')
    literal = '```yaml\nrun: |\n  autumn migrate\n  cargo test\n```'
    expect([d for _, d, _, _ in invocations(literal)] == ['migrate'],
           f'a literal scalar keeps its lines apart: {list(invocations(literal))}')
    ends = '```yaml\ncommand: >\n  autumn migrate\nimage: a:1\n```'
    expect([d for _, d, _, _ in invocations(ends)] == ['migrate'],
           f'a folded scalar ends at the next key: {list(invocations(ends))}')
    # A blank line inside a folded scalar survives as a real newline, so it
    # separates two commands. Joining across it built one argv out of two
    # valid lines and reported the second `autumn` as a phantom subcommand.
    para = '```yaml\nrun: >\n  autumn migrate\n\n  autumn routes\n```'
    expect([d for _, d, _, _ in invocations(para)] == ['migrate', 'routes'],
           f'a blank line separates folded commands: {list(invocations(para))}')
    para_drift = '```yaml\nrun: >\n  autumn migrate\n\n  autumn run\n```'
    expect([d for _, d, _, _ in invocations(para_drift)] == ['migrate', 'run'],
           f'drift after a paragraph break is still read: {list(invocations(para_drift))}')
    # Folding joins lines at the scalar's OWN column. A more-indented line is
    # kept with its newline rather than folded into the one above, so a change
    # of column breaks the paragraph the way a blank line does.
    indented = '```yaml\nrun: >\n  autumn migrate\n    autumn routes\n```'
    expect([d for _, d, _, _ in invocations(indented)] == ['migrate', 'routes'],
           f'a more-indented line is not folded in: {list(invocations(indented))}')
    ind_drift = '```yaml\nrun: >\n  autumn migrate\n    autumn run\n```'
    expect([d for _, d, _, _ in invocations(ind_drift)] == ['migrate', 'run'],
           f'drift in a more-indented block is read: {list(invocations(ind_drift))}')
    # A more-indented block keeps EVERY newline inside it, not just the ones at
    # its edges. Breaking only on a change of depth still joined two of its
    # lines, and since the first accepts arguments the second vanished into it.
    # A folded value under an exec-family key is that container's argv, so it
    # fills the slot like the inline, block-list and plain-scalar forms do.
    # A separate accumulator for it reported a bare root on a correct recipe.
    for label, doc in [
        ('entrypoint + folded command',
         '```yaml\nentrypoint: ["autumn"]\ncommand: >\n  migrate\n```'),
        ('folded command + args',
         '```yaml\ncommand: >\n  autumn\nargs: ["migrate"]\n```'),
        ('folded command alone', '```yaml\ncommand: >\n  autumn migrate\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate'], f'{label} must assemble: {got}')
    fdrift = '```yaml\nentrypoint: ["autumn"]\ncommand: >\n  run\n```'
    expect([d for _, d, _, _ in invocations(fdrift)] == ['run'],
           f'drift in a folded slot is read: {list(invocations(fdrift))}')
    # …but `script:`/`run:` pair with nothing, and more than one paragraph is
    # more than one command, so both stay standalone.
    fscript = '```yaml\nscript: >\n  autumn migrate\n  run\n```'
    expect([d for _, d, _, _ in invocations(fscript)] == ['migrate run'],
           f'a folded script: is not a slot: {list(invocations(fscript))}')
    ind_pair = ('```yaml\nrun: >\n  autumn migrate\n    autumn routes\n'
                '    autumn run\n```')
    expect([d for _, d, _, _ in invocations(ind_pair)] == ['migrate', 'routes', 'run'],
           f'each more-indented line is its own command: {list(invocations(ind_pair))}')
    # A trailing comment is valid on a key line, and these patterns are
    # end-anchored, so the key must be matched without it.
    for label, doc in [
        ('folded header', '```yaml\nrun: > # folded for readability\n  autumn migrate\n  run\n```'),
        ('block key', '```yaml\ncommand: # the argv\n  - autumn\n  - migrate\n  - run\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'a comment on a {label} must not hide it: {got}')

    # --- an annotated array is ordinary, and keeping the annotation in the
    # argv made the element resolve to nothing, hiding the drift beside it.
    noted = '```yaml\ncommand:\n  - autumn\n  - migrate\n  - run # typo\n```'
    expect([d for _, d, _, _ in invocations(noted)] == ['migrate run'],
           f'a YAML comment must not ride along in the argv: {list(invocations(noted))}')
    for label, doc in [
        ('quoted', '```yaml\ncommand:\n  - autumn\n  - migrate\n  - "--tag=#1"\n```'),
        # YAML needs whitespace before a `#` for it to start a comment.
        ('attached', '```yaml\ncommand:\n  - autumn\n  - migrate\n  - --tag=#1\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate --tag=#1'],
               f'a # that is not a comment ({label}) survives: {got}')

    # --- a container's argv comes from up to three keys, and every runtime
    # spells them differently: Docker/Compose concatenate ENTRYPOINT + CMD,
    # Kubernetes concatenates command + args. Reading any single key as the
    # whole argv reported a valid Compose file as a bare root.
    for label, doc in [
        ('k8s', '```yaml\ncommand: ["autumn"]\nargs: ["migrate"]\n```'),
        ('compose', '```yaml\nentrypoint: ["autumn"]\ncommand: ["migrate"]\n```'),
        ('compose, reversed',
         '```yaml\ncommand: ["migrate"]\nentrypoint: ["autumn"]\n```'),
        ('entrypoint alone', '```yaml\nentrypoint: ["autumn", "migrate"]\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate'], f'the {label} spelling must assemble: {got}')
    three = ('```yaml\nentrypoint: ["autumn"]\ncommand: ["migrate"]\n'
             'args: ["--shard", "eu"]\n```')
    expect([d for _, d, _, _ in invocations(three)] == ['migrate --shard eu'],
           f'all three slots concatenate in order: {list(invocations(three))}')
    drift = '```yaml\nentrypoint: ["autumn"]\ncommand: ["nope"]\n```'
    expect([d for _, d, _, _ in invocations(drift)] == ['nope'],
           f'drift in the CMD half must be read: {list(invocations(drift))}')
    # Compose names its services with mapping keys, not list items, so a
    # sibling object has to end the wait too — otherwise two services' slots
    # run together and autumn is no longer at the head of the argv.
    two_services = ('```yaml\nservices:\n  web:\n    command: ["autumn", "run"]\n'
                    '  worker:\n    entrypoint: ["worker"]\n```')
    expect([d for _, d, _, _ in invocations(two_services)] == ['run'],
           f'a sibling service must not absorb the previous one: '
           f'{list(invocations(two_services))}')
    # …but a sibling key of the SAME object must not, which is the whole
    # reason the boundary is strictly-left rather than at-or-left.
    same_object = ('```yaml\nspec:\n  containers:\n    - name: m\n'
                   '      command: ["autumn"]\n      image: a:1\n'
                   '      args: ["migrate"]\n```')
    expect([d for _, d, _, _ in invocations(same_object)] == ['migrate'],
           f'a sibling key of the same object keeps the pair open: '
           f'{list(invocations(same_object))}')

    # Compose mixes the list and scalar forms freely, so a scalar fills its
    # slot like a list does. Reading only the bracketed form left the pair
    # half-assembled and reported a valid recipe as a bare root.
    for label, doc in [
        ('list + scalar', '```yaml\nentrypoint: ["autumn"]\ncommand: migrate\n```'),
        ('quoted scalar', '```yaml\nentrypoint: ["autumn"]\ncommand: "migrate"\n```'),
        ('scalar first', '```yaml\ncommand: migrate\nentrypoint: ["autumn"]\n```'),
        ('both scalar', '```yaml\nentrypoint: autumn\ncommand: migrate\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate'], f'a scalar slot ({label}) must assemble: {got}')
    # YAML's quoting comes off before the value is shell-tokenized. Left on,
    # the whole command became ONE token that matched no executable, so
    # quoting a scalar silently disabled the check.
    for label, doc in [
        ('double', '```yaml\ncommand: "autumn migrate run"\n```'),
        ('single', "```yaml\ncommand: 'autumn migrate run'\n```"),
        ('bare', '```yaml\ncommand: autumn migrate run\n```'),
    ]:
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'a {label} scalar reads the same: {got}')
    sdrift = '```yaml\nentrypoint: ["autumn"]\ncommand: "run"\n```'
    expect([d for _, d, _, _ in invocations(sdrift)] == ['run'],
           f'drift in a scalar slot is read: {list(invocations(sdrift))}')
    # A scalar that is a whole command on its own must still be read ONCE.
    lone = '```yaml\ncommand: autumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(lone)] == ['migrate run'],
           f'a lone scalar command is read exactly once: {list(invocations(lone))}')

    # The accumulator's state must not collide with the backslash-continuation
    # buffer it shares a scope with — they were both called `held`, and a
    # continuation inside a fence then reported the wrong line.
    both = ('```yaml\ncommand: ["autumn"]\nargs: ["migrate"]\n```\n\n'
            '```bash\nautumn maintenance on \\\n  --reason "x"\n```')
    lines = [ln for ln, _, _, _ in invocations(both)]
    expect(lines == [2, 7], f'a continuation and a slot pair keep their own lines: {lines}')

    # …and the key, not the contents, is what says a list is shell lines.
    script = '```yaml\nscript:\n  - autumn migrate run\n  - cargo test\n```'
    expect([d for _, d, _, _ in invocations(script)] == ['migrate run'],
           f'a script: list is still read as shell lines: {list(invocations(script))}')
    expect(list(invocations('```yaml\ncommand:\n  - sleep\n  - "5"\n```')) == [],
           'a block list for another program stays quiet')
    # The accumulator must never fire outside a fence: the corpus ends
    # sentences with the word "command:" and then opens a bash block.
    prose = 'Run the following command:\n\n```bash\nautumn migrate run\n```'
    expect([d for _, d, _, _ in invocations(prose)] == ['migrate run'],
           f'prose ending in "command:" must not swallow the block: {list(invocations(prose))}')
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

    # A backticked span INSIDE a fence is not a command line, but it is fenced.
    # The two facts were one boolean, so it was waivable — a marker could
    # silence real drift in `` OUT=`autumn nope` ``. Splitting them closes that
    # without reporting the seven prose-in-a-fence mentions this corpus has.
    subst_page = '\n'.join([
        '<!-- cli-surface-allow: autumn nope — a waiver must not reach a fence -->',
        '',
        '```bash',
        'OUT=`autumn nope`',
        '```',
    ])
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = pathlib.Path(tmpdir) / 'subst-selftest.md'
        tmp.write_text(subst_page)
        subst_defects, subst_waived = scan(tmp.parent, surface, [tmp.name])
    expect(subst_waived == 0 and len(subst_defects) == 1,
           f'a waiver must not reach a backtick substitution in a fence: '
           f'{subst_defects}, {subst_waived} waived')
    # …while a bare GROUP named in prose inside a fence stays quiet, because
    # such a span is not runnable. This is what `skills/autumn-web/SKILL.md`
    # and a `rust` fence's `//!` doc comment actually contain.
    # `db` is the synthetic CLI's group that requires a subcommand, standing in
    # for the real `autumn deploy` / `autumn generate` these fences name.
    for prose in ('```\n`autumn db` mirrors `autumn db` argument-for-argument.\n```',
                  '```rust\n//! Generated by `autumn db`.\n```',
                  '```bash\n# on every host `autumn db` manages\nautumn migrate\n```'):
        bad = [d for _, d, a, w in invocations(prose)
               if resolve(a, surface, runnable=w == FENCED_COMMAND)]
        expect(bad == [], f'prose inside a fence must not report a bare group: {bad}')
    # …but backticks on a real command line in a SHELL fence are legacy
    # command substitution, which runs. Classifying every backtick span as a
    # mention disabled the runnable-only checks there, so a bare group inside
    # one went unreported while its drift was still caught.
    subst = '```bash\nOUT=`autumn db`\n```'
    bad = [d for _, d, a, w in invocations(subst)
           if resolve(a, surface, runnable=w == FENCED_COMMAND)]
    expect(bad == ['db'], f'a bare group inside a shell substitution runs: {bad}')
    expect([w for _, _, _, w in invocations(subst)] == [FENCED_COMMAND],
           'a shell-fence substitution is a command line, not a mention')
    expect([w for _, _, _, w in invocations('```yaml\nkey: `autumn db`\n```')]
           == [FENCED_SPAN],
           'a backtick span in a non-shell fence stays a mention')
    # Single quotes make a backtick literal, so the shell prints it rather than
    # running it. Double quotes do NOT, which is why only single ones count.
    literal_bt = "```bash\nprintf '%s\\n' '`autumn db`'\n```"
    expect([w for _, _, _, w in invocations(literal_bt)] == [FENCED_SPAN],
           f'a single-quoted backtick is data: {list(invocations(literal_bt))}')
    expect([w for _, _, _, w in invocations('```bash\necho "`autumn db`"\n```')]
           == [FENCED_COMMAND],
           'a double-quoted backtick still runs')
    # The same rule for `$( … )`: shlex strips the quote delimiters, so a
    # literal substitution reached the recursion looking like a real one and a
    # `printf` of that text was reported as a command.
    lit_sub = "```bash\nprintf '%s\\n' '$(autumn db)'\n```"
    bad = [d for _, d, a, w in invocations(lit_sub)
           if resolve(a, surface, runnable=w == FENCED_COMMAND)]
    expect(bad == [], f'a single-quoted $( ) is printed, not run: {bad}')
    for real in ('OUT=$(autumn migrate run)', 'OUT="$(autumn migrate run)"'):
        doc = f'```bash\n{real}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'a real substitution still runs: {real} -> {got}')
    # The decision is POSITIONAL. Asking whether the body text appeared
    # anywhere single-quoted was wrong twice over: it suppressed a real
    # substitution that merely repeated a quoted one…
    both = '```bash\nprintf \'%s\' \'$(autumn run)\'; OUT="$(autumn run)"\n```'
    expect([d for _, d, _, _ in invocations(both)] == ['run'],
           f'a real substitution beside an identical quoted one runs, and the '
           f'quoted one stays quiet: {list(invocations(both))}')
    # …and one whose own body contains a quoted character, which is the
    # quoted-paren case from an earlier round reaching the same code.
    inner = '```bash\nOUT="$(printf \'(\'; autumn migrate run)"\n```'
    expect([d for _, d, _, _ in invocations(inner)] == ['migrate run'],
           f'a body containing a quote is not itself quoted: {list(invocations(inner))}')
    # …and drift inside a literal run is still reported, just not as a command.
    lit_drift = "```bash\nprintf '%s\\n' '`autumn nope`'\n```"
    bad = [d for _, d, a, w in invocations(lit_drift)
           if resolve(a, surface, runnable=w == FENCED_COMMAND)]
    expect(bad == ['nope'], f'drift in a literal backtick is still named: {bad}')

    # Docker's attached option form takes a path just as the separated one
    # does; comparing the whole token to one spelling read only the bare word.
    for form in ('--entrypoint autumn', '--entrypoint=autumn',
                 '--entrypoint=/usr/local/bin/autumn', '--entrypoint ./autumn'):
        doc = f'```bash\ndocker run --rm {form} img migrate run\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate run'], f'{form} must be read: {got}')

    # A BACKSLASH escapes the next character, so an escaped quote does not end
    # a string. Walking past it closed the string early and truncated the line
    # at a `#` that was still quoted — here that threw away the heredoc opener,
    # and the body below it was read as commands rather than as data.
    esc_q = ('```bash\nprintf \'%s\' "\\" # literal"; cat <<EOF\n'
             'autumn nope\nEOF\n```')
    expect(list(invocations(esc_q)) == [],
           f'an escaped quote keeps the heredoc opener: {list(invocations(esc_q))}')
    esc_run = '```bash\nprintf \'%s\' "\\" # literal"; autumn migrate status\n```'
    expect([d for _, d, _, _ in invocations(esc_run)] == ['migrate status'],
           f'a real command after an escaped quote is still read: '
           f'{list(invocations(esc_run))}')
    # Inside SINGLE quotes a backslash is literal and escapes nothing, so the
    # string really does end at the next quote and the `#` is a comment.
    sq_bs = "```bash\nprintf '\\' # autumn nope\n```"
    expect(list(invocations(sq_bs)) == [],
           f"a backslash inside single quotes escapes nothing: "
           f'{list(invocations(sq_bs))}')

    # `<( … )` and `>( … )` are PROCESS SUBSTITUTION: bash runs the list inside
    # and hands the caller a file. Only `$(` was recursed into, so a command
    # written in one went unread.
    psub = '```bash\ndiff <(autumn console) <(autumn nope)\n```'
    expect([d for _, d, _, _ in invocations(psub)] == ['console', 'nope'],
           f'both process substitutions run: {list(invocations(psub))}')
    expect([d for _, d, _, _ in invocations('```bash\ntee >(autumn nope)\n```')]
           == ['nope'], 'an output process substitution runs too')
    # A plain redirection is NOT a substitution — `<` followed by a word reads
    # a file, and the line itself is the only command on it.
    redir = '```bash\nautumn migrate status < input.sql\n```'
    expect([d for _, d, _, _ in invocations(redir)] == ['migrate status < input.sql'],
           f'a redirection is not a substitution: {list(invocations(redir))}')

    # `$((` opens ARITHMETIC, not a command, and `\$(` is a literal dollar the
    # shell prints. Recursing into either reported a page that runs no autumn.
    for quiet in ('echo "$(( autumn ))"', 'printf \'%s\' "\\$(autumn nope)"'):
        doc = f'```bash\n{quiet}\n```'
        expect(list(invocations(doc)) == [],
               f'{quiet} runs nothing: {list(invocations(doc))}')
    mixed = '```bash\necho "$(( 1 + 2 ))"; OUT=$(autumn console)\n```'
    expect([d for _, d, _, _ in invocations(mixed)] == ['console'],
           f'a real substitution beside arithmetic still runs: '
           f'{list(invocations(mixed))}')

    # `timeout [OPTION] DURATION COMMAND` takes a positional operand of its
    # own. Stepping over its options alone left the duration looking like the
    # executable, so the command after it was never reached.
    for form in ('timeout 5', 'timeout 30s', 'timeout -k 1 5',
                 'timeout --signal=TERM 5'):
        doc = f'```bash\n{form} autumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{form} must reach its command: {got}')
    # `nice` takes options but no operand, so consuming one would have eaten
    # the command instead.
    for form in ('nice', 'nice -n 5'):
        doc = f'```bash\n{form} autumn migrate status\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate status'], f'{form} takes no operand: {got}')

    # A quote spans physical lines, so what is written inside one is string
    # data. Read line by line the opener failed to tokenize, fell back to
    # whitespace splitting, and the text inside the string was reported as a
    # runnable command on a correct page.
    for opener, closer in (("'", "'"), ('"', '"')):
        doc = f'```bash\nprintf \'%s\' {opener}\nautumn db\n{closer}\n```'
        expect(list(invocations(doc)) == [],
               f'a {opener} spanning lines is data: {list(invocations(doc))}')
    after = "```bash\nprintf '%s' '\ndata\n'; autumn nope\n```"
    expect([d for _, d, _, _ in invocations(after)] == ['nope'],
           f'a command after the closing quote still runs: '
           f'{list(invocations(after))}')
    # Only a SHELL fence gets this reading: an apostrophe is a lifetime in
    # `rust` and an ordinary character in `toml`, and folding there would
    # swallow the rest of the block.
    rust_q = "```rust\nfn f<'a>(x: &'a str) {}\nlet _ = \"autumn nope\";\n```"
    expect(list(invocations(rust_q)) == [],
           f'a rust lifetime does not open a shell quote: '
           f'{list(invocations(rust_q))}')
    # A fence that opens a quote it never closes is a FRAGMENT: its lines are
    # handed back and read one at a time rather than dropped.
    frag = "```bash\nprintf '%s' '\nautumn nope\n```"
    expect([d for _, d, _, _ in invocations(frag)] == ['nope'],
           f'an unterminated quote falls back, it does not swallow: '
           f'{list(invocations(frag))}')

    # `2>&-` CLOSES a descriptor, so `-` is a duplication target like a digit.
    # Accepting only digits left the `-` as its own token, and as a LEADING
    # redirection it became the segment head with the command behind it unread.
    for redir in ('2>&-', '2>&1', '1>&2'):
        doc = f'```bash\n{redir} autumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'a leading {redir} is stepped over: {got}')

    # A shell's command-string option may be BUNDLED, and bash takes `c`'s
    # value from the next word wherever `c` sits in the cluster.
    for form in ('bash -lc', 'bash -cl', 'sh -ec', 'bash -c', 'bash --command'):
        doc = f"```bash\n{form} 'autumn nope'\n```"
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f"{form} runs its string: {got}")
    # …and the LETTER is per-program, which is why it cannot be shared:
    # `bash -C` is noclobber, `ssh -c` a cipher, `echo -c` nothing at all.
    for quiet in ('bash -C', 'ssh -c aes256 host', 'echo -c', 'bash --check'):
        doc = f"```bash\n{quiet} 'autumn nope'\n```"
        expect(list(invocations(doc)) == [],
               f'{quiet} does not run its argument: {list(invocations(doc))}')
    fly = "```bash\nfly ssh console -C 'autumn nope'\n```"
    expect([d for _, d, _, _ in invocations(fly)] == ['nope'],
           f"fly spells it -C: {list(invocations(fly))}")

    # `echo \\<<EOF` is a literal `<` and an ordinary input redirection, so
    # nothing below it is heredoc data. Queueing the operator anyway consumed
    # the command on the next line as the body.
    esc_hd = '```bash\ntouch EOF; echo \\<<EOF\nautumn nope\n```'
    expect([d for _, d, _, _ in invocations(esc_hd)] == ['nope'],
           f'an escaped heredoc operator opens nothing: '
           f'{list(invocations(esc_hd))}')
    real_hd = '```bash\ncat <<EOF\nautumn db\nEOF\nautumn nope\n```'
    expect([d for _, d, _, _ in invocations(real_hd)] == ['nope'],
           f'a real heredoc still swallows its body only: '
           f'{list(invocations(real_hd))}')

    # PARITY, not presence, before a substitution: backslashes pair off, so an
    # even run leaves `$(` live. This cannot be decided on the token — shlex
    # resolves `"\\\\$("` and `"\\$("` to the same characters — so the masked
    # copy decides it from the raw text.
    for run, runs in ((1, False), (2, True), (3, False), (4, True)):
        doc = '```bash\nprintf \'%s\' "' + '\\' * run + '$(autumn nope)"\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == (['nope'] if runs else []),
               f'{run} backslash(es) before $( '
               f'{"runs" if runs else "does not run"}: {got}')

    # The fold's NEIGHBOURS, probed rather than waited for. An apostrophe is
    # ordinary English, and a shell fence is full of it: a contraction in a
    # comment or in quoted program output opens a quote that never closes, and
    # if that swallowed the rest of the block the gate would go quiet on a page
    # it is meant to check. Each shape below keeps reading the command after it.
    neighbours = [
        ("a heredoc body",
         "```bash\ncat <<EOF\ndon't do this\nEOF\nautumn nope\n```", 5),
        ("a whole-line comment",
         "```bash\n# don't run this\nautumn nope\n```", 3),
        ("a trailing comment",
         "```bash\necho ok  # it's fine\nautumn nope\n```", 3),
        # No quote closes this one, so it takes the fragment path — and the
        # line numbers have to survive it, not just the readings.
        ("quoted program output",
         "```sh\nls\nwon't work\nautumn nope\n```", 4),
        ("a block-quoted fence",
         "> ```bash\n> printf '%s' '\n> a\n> '\n> autumn nope\n> ```", 5),
        ("a backtick run after a fold",
         "```bash\nprintf '%s' '\na\n'\nOUT=`autumn nope`\n```", 5),
        # `<<EOF` written INSIDE a string opens no heredoc, so the line below
        # the string is a command and not its body.
        ("a heredoc opener inside a string",
         "```bash\nprintf '%s' '\ncat <<EOF\n'\nautumn nope\n```", 5),
    ]
    for label, doc, at in neighbours:
        got = [(ln, d) for ln, d, _, _ in invocations(doc)]
        expect(got == [(at, 'nope')],
               f'{label} must not swallow the command after it: {got}')
    # …and a command written INSIDE the string is data, not a command.
    inside = "```bash\nprintf '%s' '\n`autumn nope`\n'\n```"
    expect(list(invocations(inside)) == [],
           f'a backtick run inside the string is data: {list(invocations(inside))}')

    # The `-c` reading travels through every wrapper, and stops at every
    # program that does not spell a command string that way.
    for wrapper in ('timeout 5', 'sudo -u deploy', 'kubectl exec pod --',
                    'docker run img', 'env'):
        doc = f"```bash\n{wrapper} bash -lc 'autumn nope'\n```"
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{wrapper} reaches a bundled -c: {got}')
    for quiet in ('bash -x', 'bash -l', 'tar -cf a.tar'):
        doc = f"```bash\n{quiet} 'autumn nope'\n```"
        expect(list(invocations(doc)) == [],
               f'{quiet} carries no command string: {list(invocations(doc))}')

    # A heredoc delimiter is a shell WORD, not an identifier. Reading only the
    # leading identifier took `END` out of `<<END.JSON`, so the terminator
    # never matched and the rest of the fence — the command after it
    # included — was eaten as body.
    for delim in ('END.JSON', 'END}', 'EOF-1', 'EOF'):
        doc = f'```bash\ncat <<{delim}\ndata\n{delim}\nautumn nope\n```'
        got = [(ln, d) for ln, d, _, _ in invocations(doc)]
        expect(got == [(5, 'nope')], f'<<{delim} ends on its own word: {got}')
        # …and the body is still data, whatever the delimiter is spelled like.
        body_only = f'```bash\ncat <<{delim}\nautumn db\n{delim}\n```'
        expect(list(invocations(body_only)) == [],
               f'<<{delim} still swallows its body: {list(invocations(body_only))}')
    esc = '```bash\ncat <<\\END.JSON\ndata\nEND.JSON\nautumn nope\n```'
    expect([d for _, d, _, _ in invocations(esc)] == ['nope'],
           f'a backslash-quoted punctuated delimiter works too: '
           f'{list(invocations(esc))}')
    # The here-string is still not a heredoc — the broader word class must not
    # have made `<<<` match.
    hs = '```bash\nsort <<<"$x"\nautumn nope\n```'
    expect([d for _, d, _, _ in invocations(hs)] == ['nope'],
           f'a here-string consumes no lines: {list(invocations(hs))}')

    # Several launchers run a DIRECT command operand, not only one after a
    # `--`, and each has its own operand count: `flock <file> <command>` and
    # `chroot NEWROOT COMMAND` take one, `systemd-run`, `nsenter` and `doas`
    # take none.
    for launcher in ('systemd-run', 'flock /tmp/lock', 'chroot /mnt',
                     'nsenter -t 1 -m', 'doas',
                     'systemd-run --unit=x', 'systemd-run -p CPUQuota=50%',
                     'flock -w 5 /tmp/lock', 'chroot --userspec=1000 /mnt',
                     'doas -u deploy'):
        doc = f'```bash\n{launcher} autumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{launcher} runs its operand: {got}')
    # …and a program that merely NAMES one runs nothing.
    echoed = '```bash\necho systemd-run autumn nope\n```'
    expect(list(invocations(echoed)) == [],
           f'a launcher named as an argument runs nothing: '
           f'{list(invocations(echoed))}')

    # A LITERAL block scalar is still that key's value, so under an exec key it
    # fills the container's slot exactly as every other form does — leaving it
    # out left `entrypoint: ["autumn"]` with an empty slot and emitted a bare
    # root on a correct recipe.
    lit_slot = '```yaml\nentrypoint: ["autumn"]\ncommand: |\n  migrate\n```'
    expect([d for _, d, _, _ in invocations(lit_slot)] == ['migrate'],
           f'a literal scalar fills its slot: {list(invocations(lit_slot))}')
    lit_drift = '```yaml\nentrypoint: ["autumn"]\ncommand: |\n  nope\n```'
    expect([d for _, d, _, _ in invocations(lit_drift)] == ['nope'],
           f'…and drift in one is still named: {list(invocations(lit_drift))}')
    # `|` keeps its lines apart and `>` joins them: that difference is the
    # whole reason the two styles exist, and reading either as the other
    # invents a command out of two valid lines or hides one inside another.
    lit_two = '```yaml\nrun: |\n  autumn migrate\n  autumn nope\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(lit_two)]
           == [(3, 'migrate'), (4, 'nope')],
           f'a literal scalar keeps its lines apart: {list(invocations(lit_two))}')
    fold_two = '```yaml\nrun: >\n  autumn migrate\n  status\n```'
    expect([d for _, d, _, _ in invocations(fold_two)] == ['migrate status'],
           f'a folded scalar still joins them: {list(invocations(fold_two))}')
    # Every paragraph reports its OWN first line. Reporting the key sent the
    # reader to the `run: |` above the command that actually fails, and
    # `file:line:` is the whole of what the gate hands them.
    paras = '```yaml\nrun: >\n  autumn migrate\n\n  autumn nope\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(paras)]
           == [(3, 'migrate'), (5, 'nope')],
           f'each paragraph reports its own line: {list(invocations(paras))}')

    # `args:` carries the other HALF of a Kubernetes argv, and its inline,
    # block-list and plain-scalar forms all filled their slot already — the
    # BLOCK-SCALAR form did not, so `command: ["autumn"]` beside `args: >`
    # reported a bare root on a valid manifest. Found by probing the literal
    # scalar fix against its neighbours rather than by being told: it is the
    # same omission, one key over.
    for style in ('>', '|'):
        pair = f'```yaml\ncommand: ["autumn"]\nargs: {style}\n  nope\n```'
        got = [d for _, d, _, _ in invocations(pair)]
        expect(got == ['nope'], f'args: {style} fills its slot: {got}')
        # …in either write order, since a mapping has none.
        rev = f'```yaml\nargs: {style}\n  nope\ncommand: ["autumn"]\n```'
        got = [d for _, d, _, _ in invocations(rev)]
        expect(got == ['nope'], f'args: {style} pairs written first: {got}')
    # A folded `args:` still joins its lines, and a valid pair stays quiet.
    ok_pair = '```yaml\ncommand: ["autumn"]\nargs: >\n  migrate\n  status\n```'
    expect([d for _, d, _, _ in invocations(ok_pair)] == ['migrate status'],
           f'a folded args: is one argv: {list(invocations(ok_pair))}')
    # An `entrypoint:` with no command half is NOT a false positive — that
    # recipe really does run `autumn` with no subcommand, which clap rejects.
    alone = '```yaml\nentrypoint: ["autumn"]\n```'
    expect([d for _, d, _, _ in invocations(alone)] == [''],
           f'an unpaired entrypoint is a real bare root: {list(invocations(alone))}')

    # A wrapper's own `--` ends ITS options and the command follows. For a
    # wrapper with no operands the option walk already consumed the separator,
    # which is why this only ever failed for the ones that take operands —
    # there it arrives after them and stopped the walk.
    for launcher in ('timeout 5', 'flock /tmp/lock', 'chroot /mnt',
                     'systemd-run', 'nsenter', 'doas', 'xargs', 'sudo', 'env'):
        doc = f'```bash\n{launcher} -- autumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{launcher} -- reaches its command: {got}')
    # The separator still belongs to whoever wrote it: a command's own `--`
    # forwards to itself, and a program that only prints its arguments runs
    # nothing.
    own = '```bash\nautumn test -- autumn nope\n```'
    expect([d for _, d, _, _ in invocations(own)] == ['test -- autumn nope'],
           f"autumn's own separator is not a wrapper's: {list(invocations(own))}")
    expect(list(invocations('```bash\necho -- autumn nope\n```')) == [],
           'echo prints its separator, it does not launch')

    # The `/etc/cron.d` user field is stepped over when what follows LAUNCHES —
    # the binary, or a wrapper that reaches it. Requiring the binary itself
    # could not see past `root flock /var/lock/x autumn …`.
    for line in ('*/5 * * * * deploy autumn nope',
                 '*/5 * * * * deploy flock /tmp/lock autumn nope',
                 '*/5 * * * * deploy timeout 5 autumn nope',
                 '@daily deploy flock /tmp/lock autumn nope',
                 '*/5 * * * * flock /tmp/lock autumn nope',
                 '*/5 * * * * autumn nope'):
        doc = f'```cron\n{line}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{line!r} reaches its command: {got}')
    # …and a schedule followed by something that is not autumn stays silent,
    # whether or not a user field is written.
    for quiet in ('*/5 * * * * backup.sh', '*/5 * * * * deploy backup.sh'):
        doc = f'```cron\n{quiet}\n```'
        expect(list(invocations(doc)) == [],
               f'{quiet!r} runs no autumn: {list(invocations(doc))}')

    # `ssh [options] destination [command …]` runs its command operand, which
    # the separator branch alone never reached.
    for form in ('ssh host', 'ssh -p 22 host', 'ssh deploy@host',
                 'ssh -c aes256 host', 'ssh -o X=y host', 'ssh -i ~/.k host'):
        doc = f'```bash\n{form} autumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{form} runs its remote command: {got}')
    expect(list(invocations('```bash\necho ssh host autumn nope\n```')) == [],
           'a destination named as an argument launches nothing')

    # Lines inside a literal `run: |` ARE shell, so a trailing unescaped
    # backslash continues them. Reading each on its own stopped the first at
    # the backslash and left the second with no executable, so the drift
    # resolved to nothing and went unreported.
    cont = '```yaml\nrun: |\n  autumn \\\n    nope\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(cont)] == [(3, 'nope')],
           f'a continuation inside a literal scalar joins: {list(invocations(cont))}')
    three = '```yaml\nrun: |\n  autumn \\\n    migrate \\\n    status\n```'
    expect([d for _, d, _, _ in invocations(three)] == ['migrate status'],
           f'…across three lines too: {list(invocations(three))}')
    # Parity holds here as everywhere: an even run is literal backslashes and
    # continues nothing, and two ordinary lines stay two commands.
    evenbs = '```yaml\nrun: |\n  printf %s \\\\\n  autumn nope\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(evenbs)] == [(4, 'nope')],
           f'an even backslash run continues nothing: {list(invocations(evenbs))}')
    two = '```yaml\nrun: |\n  autumn migrate\n  autumn nope\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(two)]
           == [(3, 'migrate'), (4, 'nope')],
           f'…and plain lines stay apart: {list(invocations(two))}')

    # A closing fence may be indented at most three spaces past the block it
    # closes; further in, the line is CONTENT. Ignoring the indent ended a
    # fence early and everything below read as prose, where a runnable command
    # is not judged.
    deep = '```bash\nautumn migrate\n    ```\nautumn nope\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(deep)]
           == [(2, 'migrate'), (4, 'nope')],
           f'a four-space ``` is fence content: {list(invocations(deep))}')
    for closer in ('```', '   ```'):
        doc = f'```bash\nautumn migrate\n{closer}\nautumn nope'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['migrate'], f'{closer!r} still closes the fence: {got}')
    # The bound is RELATIVE to the container's content column, so a fence
    # nested in a list item still opens — but only within three spaces of that
    # column. This assertion used to demand that a SIX-space fence under
    # `- item` be read, which a CommonMark implementation says is an indented
    # code block: a test written from this file's behaviour rather than from
    # the grammar, agreeing with the defect it should have caught.
    for pad, is_fence in ((2, True), (5, True), (6, False)):
        doc = f'- item\n\n{" " * pad}```bash\n{" " * pad}autumn nope\n{" " * pad}```\n'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == (['nope'] if is_fence else []),
               f'a fence {pad} spaces under a list item: {got}')
    # …and at the TOP level the container column is zero, so four spaces is an
    # indented code block and what follows it is prose.
    for pad, is_fence in ((0, True), (3, True), (4, False)):
        doc = f'{" " * pad}```bash\n{" " * pad}autumn nope\n{" " * pad}```\n'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == (['nope'] if is_fence else []),
               f'a top-level fence at {pad} spaces: {got}')

    # A delimiter is a shell word, and a word may be spelled in PIECES —
    # quote removal joins `<<'END'.JSON` into one `END.JSON`.
    for spelling in ("<<'END'.JSON", '<<"END".JSON', '<<END.JSON',
                     "<<'END.JSON'", '<<END".JS"ON'):
        doc = f'```bash\ncat {spelling}\ndata\nEND.JSON\nautumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{spelling} ends on END.JSON: {got}')
    pieces_body = "```bash\ncat <<'END'.JSON\nautumn db\nEND.JSON\n```"
    expect(list(invocations(pieces_body)) == [],
           f'…and its body is still data: {list(invocations(pieces_body))}')

    # Every wrapper option table, checked against the spelling the installed
    # binary documents. Two kinds of error live here and they fail in opposite
    # directions: a value-taking option missing from the table leaves its
    # VALUE in command position, and an option wrongly listed EATS the command
    # as its value. Both end in silence, which is why neither showed up until
    # each spelling was walked one at a time.
    takes_value = {
        'systemd-run': ['-H host', '--host host', '--host=host', '-M c',
                        '--machine c', '-u n', '--unit n', '-p X=1',
                        '--property X=1', '-E A=b', '--setenv A=b',
                        '--description d', '--slice s',
                        '--expand-environment yes', '--service-type exec',
                        '--uid 1000', '--gid 1000', '--nice 5',
                        '--working-directory /x', '--path-property X=1',
                        '--socket-property X=1', '--on-active 5',
                        '--on-boot 5', '--on-startup 5', '--on-unit-active 5',
                        '--on-unit-inactive 5', '--on-calendar daily',
                        '--timer-property X=1'],
        'nsenter': ['-t 1', '--target 1', '-W /x', '--wdns /x'],
        'xargs': ['-a f', '--arg-file f', '-E END', '-I{}', '-I {}', '-L 1',
                  '-n 1', '-P 2', '-s 100', '-d ,', '--process-slot-var V'],
        'flock': ['-w 5 /tmp/l', '--timeout 5 /tmp/l', '-E 1 /tmp/l'],
        'timeout': ['-k 1 5', '--signal=TERM 5'],
    }
    # …and the FLAGS beside them, which must not swallow anything. nsenter
    # spells almost everything with an optional attached value
    # (`--setuid[=<uid>]`), and xargs spells `-e`, `-i` and `-l` that way, so
    # bare they are flags — the tables had them the other way round.
    flags = {
        'systemd-run': ['-t', '--pty', '-q', '--quiet', '-d', '--same-dir',
                        '-r', '--remain-after-exit', '-G', '--collect',
                        '--wait', '--user', '--system', '--scope'],
        'nsenter': ['-S', '-G', '--setuid', '--setgid', '-r', '--root',
                    '-w', '--wd', '-m', '-C', '-U', '-T'],
        'xargs': ['-e', '--eof', '-i', '-l', '--replace', '-r', '-t',
                  '-0', '--null'],
    }
    for table, kind in ((takes_value, 'value'), (flags, 'flag')):
        for wrapper, opts in table.items():
            for opt in opts:
                doc = f'```bash\n{wrapper} {opt} autumn nope\n```'
                got = [d for _, d, _, _ in invocations(doc)]
                expect(got == ['nope'],
                       f'{wrapper} {opt} ({kind}) must reach its command: {got}')

    # A YAML COMPACT SEQUENCE writes the first key of a mapping on the same
    # line as its `- ` marker, which is the ordinary way to spell a Kubernetes
    # container. The marker hid the key, so the pair never assembled and the
    # generic scan reported a bare root on a correct manifest.
    compact = '```yaml\n- command: ["autumn"]\n  args: ["migrate"]\n```'
    expect([d for _, d, _, _ in invocations(compact)] == ['migrate'],
           f'a compact sequence assembles its pair: {list(invocations(compact))}')
    for half in ('args: ["nope"]', 'args: |\n    nope'):
        doc = f'```yaml\n- command: ["autumn"]\n  {half}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'…and still names drift in it: {got}')
    # Each item in the sequence is its own container, with its own line.
    two = ('```yaml\n- command: ["autumn"]\n  args: ["migrate"]\n'
           '- command: ["autumn"]\n  args: ["nope"]\n```')
    expect([(ln, d) for ln, d, _, _ in invocations(two)]
           == [(2, 'migrate'), (4, 'nope')],
           f'two containers stay two: {list(invocations(two))}')
    # …and a plain block list item is still an ARGV element, not a key.
    plain = '```yaml\ncommand:\n  - autumn\n  - nope\n```'
    expect([d for _, d, _, _ in invocations(plain)] == ['nope'],
           f'a block list item is not a compact key: {list(invocations(plain))}')

    # Inside DOUBLE quotes a backslash escapes the next character, so hunting
    # for the next quote mistook an escaped one for the terminator and
    # rejected the delimiter — the heredoc opened nothing and its data was
    # scanned as commands, failing a correct page.
    esc_delim = '```bash\ncat <<"END\\"X"\nautumn db\nEND"X\nautumn migrate\n```'
    expect([d for _, d, _, _ in invocations(esc_delim)] == ['migrate'],
           f'an escaped quote inside a delimiter: {list(invocations(esc_delim))}')
    # Inside SINGLE quotes a backslash is literal, which is why the walk
    # differs by quote kind.
    sq_delim = "```bash\ncat <<'END\\\\X'\nautumn db\nEND\\\\X\nautumn migrate\n```"
    expect([d for _, d, _, _ in invocations(sq_delim)] == ['migrate'],
           f'a backslash is literal in a single-quoted delimiter: '
           f'{list(invocations(sq_delim))}')
    # An unterminated quote is not a delimiter at all, so nothing opens and
    # the line below is read as the command it is.
    unterm = '```bash\ncat <<"END\nautumn nope\n```'
    expect([d for _, d, _, _ in invocations(unterm)] == ['nope'],
           f'an unterminated delimiter opens nothing: {list(invocations(unterm))}')

    # A line that LOOKS like a fence marker is a boundary only if it can
    # actually open or close one, and the state reset is what a boundary does.
    # Running it on a marker that turns out to be CONTENT threw away the
    # heredoc queue, so the data under it was reported as commands.
    for label, doc in (
            ('a heredoc', '```bash\ncat <<EOF\n    ```\nautumn nope\nEOF\n```'),
            ('a multi-line quote',
             "```bash\nprintf '%s' '\n    ```\nautumn nope\n'\n```")):
        expect(list(invocations(doc)) == [],
               f'an indented ``` inside {label} is data: {list(invocations(doc))}')
    # …while a REAL closer still ends everything the fence was holding.
    real = '```bash\ncat <<EOF\ndata\n```\nautumn nope'
    expect(list(invocations(real)) == [],
           f'a real closer ends the fence and its heredoc: {list(invocations(real))}')

    # A literal `run: |` block is a shell SCRIPT, so its lines carry heredoc
    # and quote state across one another. Emitting each on its own reported a
    # heredoc body as runnable and failed a correct workflow.
    hd_body = '```yaml\nrun: |\n  cat <<EOF\n  autumn db\n  EOF\n```'
    expect(list(invocations(hd_body)) == [],
           f'a heredoc body inside a literal scalar is data: '
           f'{list(invocations(hd_body))}')
    hd_after = ('```yaml\nrun: |\n  cat <<EOF\n  autumn db\n  EOF\n'
                '  autumn nope\n```')
    expect([(ln, d) for ln, d, _, _ in invocations(hd_after)] == [(6, 'nope')],
           f'…and the command after it is still read: {list(invocations(hd_after))}')
    q_body = "```yaml\nrun: |\n  printf '%s' '\n  autumn db\n  '\n```"
    expect(list(invocations(q_body)) == [],
           f'a multi-line quote inside one is data too: {list(invocations(q_body))}')
    q_frag = "```yaml\nrun: |\n  printf '%s' '\n  autumn nope\n```"
    expect([d for _, d, _, _ in invocations(q_frag)] == ['nope'],
           f'an unterminated quote hands its lines back: {list(invocations(q_frag))}')
    # A FOLDED scalar is one logical value per paragraph and keeps no such
    # state, so it must not be routed through the script reader.
    fold_ok = '```yaml\nrun: >\n  autumn migrate\n  status\n```'
    expect([d for _, d, _, _ in invocations(fold_ok)] == ['migrate status'],
           f'a folded scalar is unaffected: {list(invocations(fold_ok))}')

    # Bash REMOVES the backslash-newline pair rather than replacing it with a
    # space, so a continuation may split a word: `autumn mig\` + `rate` runs
    # `autumn migrate`. Inserting a space made that `autumn mig rate` and
    # failed a correct page. Whitespace at the start of the next line is still
    # there to separate the words, which is the second case below — both
    # verified against the installed bash.
    for doc, want in (
            ('```bash\nautumn mig\\\nrate\n```', 'migrate'),
            ('```bash\nautumn mig\\\n    rate\n```', 'mig rate'),
            ('```bash\nautumn migrate \\\n  --shard x\n```', 'migrate --shard x'),
            ('```yaml\nrun: |\n  autumn mig\\\n  rate\n```', 'migrate')):
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == [want], f'a continuation joins with nothing: {got} != {[want]}')
    # …and a word split across the break is still resolved as one word, so
    # drift written that way is still named.
    split = '```bash\nautumn no\\\npe\n```'
    expect([d for _, d, _, _ in invocations(split)] == ['nope'],
           f'drift across a word break is named: {list(invocations(split))}')

    # A `<<` inside `$(( … ))` is a LEFT SHIFT, not a heredoc operator.
    # Reading it as one made the scanner wait for a `2` terminator and eat the
    # rest of the fence.
    for shift in ('echo $((1 << 2))', 'echo $(( (1 << 2) + 3 ))'):
        doc = f'```bash\n{shift}\nautumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{shift} opens no heredoc: {got}')
    # …and a real heredoc on the SAME line as a shift still opens.
    both = '```bash\necho $((1 << 2)); cat <<EOF\nautumn db\nEOF\nautumn nope\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(both)] == [(5, 'nope')],
           f'a shift beside a real heredoc: {list(invocations(both))}')

    # shlex COALESCES a run of punctuation, so a substitution's closing paren
    # and the operator after it arrive as ONE token — `);`, `)&&`, `));`.
    # Testing the whole token against the operator set matched neither half,
    # the line never split, and the command after the separator went unread.
    for line in ('echo $(date); autumn nope', 'echo $(date)&& autumn nope',
                 'echo $(date)|| autumn nope', 'echo $(date)| autumn nope',
                 'echo $(date)& autumn nope', 'echo $(a $(b)); autumn nope'):
        doc = f'```bash\n{line}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{line!r} splits at the operator: {got}')
    # The spaced spellings, which already worked, must keep working…
    for line in ('echo $(date) && autumn nope', 'echo $(date) | autumn nope'):
        doc = f'```bash\n{line}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{line!r} still splits: {got}')
    # …and an operator that is DATA must still not split a line. A `;` inside
    # quotes is text, and a substitution's own operators belong to it.
    for quiet in ('echo "a;b"; autumn nope', "printf '%s' '$(x);'; autumn nope"):
        doc = f'```bash\n{quiet}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{quiet!r} splits only at the real one: {got}')
    both = '```bash\nOUT=$(autumn migrate); autumn nope\n```'
    expect([d for _, d, _, _ in invocations(both)] == ['migrate', 'nope'],
           f'a substitution and the command after it are both read: '
           f'{list(invocations(both))}')

    # A continuation keeps the next line's RELATIVE indentation, because that
    # whitespace is what separates the words once bash removes the
    # backslash-newline. Stripping it concatenated `autumn\` and `    nope`
    # into one unreadable token and the drift went unreported — the limit I
    # wrote down as acceptable one round earlier, which it was not.
    for style, want in (('  autumn\\\n      nope', 'nope'),
                        ('  autumn\\\n      migrate', 'migrate'),
                        ('  autumn mig\\\n  rate', 'migrate'),
                        ('  autumn\\\n      migrate\\\n      status',
                         'migrate status')):
        doc = f'```yaml\nrun: |\n{style}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == [want], f'a literal continuation keeps its indent: '
                              f'{got} != {[want]}')
    # …including under an explicit indentation indicator, which SETS the column
    # the relative indent is measured from.
    ind = '```yaml\nrun: |2\n   autumn\\\n       nope\n```'
    expect([d for _, d, _, _ in invocations(ind)] == ['nope'],
           f'the indicator sets the column: {list(invocations(ind))}')

    # A `#` opens a comment at the start of a WORD, and an operator ends the
    # word before it: `echo ok;# note` is a comment and bash runs the line
    # after. Requiring whitespace missed that, so the comment's own trailing
    # backslash read as a continuation and the next line was joined INTO the
    # comment and discarded.
    for op in (';', '&'):
        doc = f'```bash\ntrue {op}# comment \\\nautumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'a comment after {op!r} ends the command: {got}')
    # …but a `#` INSIDE a word is not a comment, and one after whitespace
    # already was, and a real continuation still continues.
    expect([d for _, d, _, _ in invocations('```bash\necho ok#x\nautumn nope\n```')]
           == ['nope'], 'a # inside a word is not a comment')
    cont = '```bash\nautumn migrate \\\n  --shard x\n```'
    expect([d for _, d, _, _ in invocations(cont)] == ['migrate --shard x'],
           f'a real continuation still joins: {list(invocations(cont))}')
    expect([d for _, d, _, _ in invocations('```bash\necho "a;#b"; autumn nope\n```')]
           == ['nope'], 'a # inside quotes is data')

    # `case … in pattern)` — the `)` ends the PATTERN and the arm's command
    # follows it, but the whole line stayed in one segment headed by `case`.
    # `;;` (and the `;&` fallthrough form) terminate an arm, so they separate
    # commands as `;` does.
    for arm, want in (('case x in x) autumn nope;; esac', ['nope']),
                      ('case x in x) autumn migrate;; esac', ['migrate']),
                      ('case x in a|b) autumn nope;; esac', ['nope']),
                      ('case x in a) autumn nope;; b) autumn nada;; esac',
                       ['nope', 'nada']),
                      ('case x in a) autumn nope;& b) exit;; esac', ['nope'])):
        doc = f'```bash\n{arm}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == want, f'{arm!r} -> {got} != {want}')
    multi = ('```bash\ncase "$1" in\n  start) autumn nope ;;\n'
             '  *) exit 1 ;;\nesac\n```')
    expect([(ln, d) for ln, d, _, _ in invocations(multi)] == [(3, 'nope')],
           f'a multi-line case reads its arm: {list(invocations(multi))}')
    # The other parens must not become boundaries. A PROCESS substitution
    # brackets its contents like `$(` does — tracking only `$(` left its
    # closer at depth zero, where the arm rule took it for a boundary and the
    # command inside was reported TWICE, once from the substitution walk and
    # once as a segment head.
    for doc, want in (
            ('```bash\ndiff <(autumn migrate) <(autumn nope)\n```',
             ['migrate', 'nope']),
            ('```bash\ntee >(autumn nope)\n```', ['nope']),
            ('```bash\n(autumn migrate && autumn nope)\n```', ['migrate', 'nope']),
            ('```bash\nOUT=$(autumn nope)\n```', ['nope']),
            ("```bash\nprintf '%s' 'x)'; autumn nope\n```", ['nope'])):
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == want, f'paren control: {got} != {want}')

    # posix shlex STRIPS quotes, so a quoted `';'` arrives looking exactly
    # like a separator: the line split there and the words after it were
    # reported as a command on a page that only prints them. The decision is
    # made on a copy with every quoted run blanked, so it is positional.
    for quiet in ("printf '%s\\n' ';' autumn nope", 'echo ";" autumn nope',
                  "echo '&&' autumn nope", 'echo "|" autumn nope'):
        doc = f'```bash\n{quiet}\n```'
        expect(list(invocations(doc)) == [],
               f'a quoted operator is data: {quiet!r} -> {list(invocations(doc))}')
    # …and every real separator still separates.
    for real in ('echo x; autumn nope', 'echo x && autumn nope',
                 'echo x | autumn nope', 'echo $(date); autumn nope',
                 'case x in a) autumn nope;; esac'):
        doc = f'```bash\n{real}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{real!r} still splits: {got}')

    # `flock [options] <file> -c <command>` puts the option AFTER its operand,
    # so neither the preceding word nor the command position names the program
    # whose option it is — the segment head does.
    for form in ("flock /tmp/lock -c 'autumn nope'",
                 "flock -c 'autumn nope' /tmp/lock",
                 "flock --command 'autumn nope' /tmp/lock",
                 "flock -w 5 /tmp/lock -c 'autumn nope'"):
        doc = f'```bash\n{form}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{form} runs its string: {got}')
    # …and the head owning an option must not make every program own one.
    expect(list(invocations("```bash\necho -c 'autumn nope'\n```")) == [],
           'echo does not own a -c')
    expect([d for _, d, _, _ in invocations(
        '```bash\nssh -c aes256 host autumn nope\n```')] == ['nope'],
        "ssh's -c is still a cipher")

    # The compact sequence marker belongs on the PLAIN SCALAR key too — it was
    # added to the inline-list and block-scalar patterns and missed here, so
    # `- command: autumn` with `args:` under it still reported a bare root.
    for half, want in (('migrate', 'migrate'), ('nope', 'nope')):
        doc = f'```yaml\n- command: autumn\n  args: {half}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == [want], f'a compact scalar pair assembles: {got}')

    # A fence may OPEN on a list item's own line, with its body and closer
    # indented under the item. Only spaces and block-quote markers were
    # accepted, so the whole block read as prose and its commands were never
    # judged as runnable.
    for marker in ('-', '*', '+', '1.'):
        pad = ' ' * (len(marker) + 1)
        doc = f'{marker} ```bash\n{pad}autumn nope\n{pad}```\n'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'a fence opened by {marker!r} is read: {got}')

    # `name() { … }` coalesces to a single `()` token, so the declaration
    # stayed in one segment headed by the function's name and its body was
    # never reached.
    for doc in ('```bash\nf() { autumn nope; }; f\n```',
                '```bash\ndeploy() {\n  autumn nope\n}\ndeploy\n```'):
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'a function body is scanned: {got}')

    # An UNQUOTED heredoc delimiter leaves the body subject to expansion, so
    # `cat <<EOF` with `$(autumn …)` under it really runs that command, while
    # `cat <<'EOF'` prints it. Skipping every body line alike missed the first.
    live = '```bash\ncat <<EOF\n$(autumn nope)\nEOF\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(live)] == [(3, 'nope')],
           f'an unquoted heredoc expands its substitutions: {list(invocations(live))}')
    for delim in ("'EOF'", '"EOF"', '\\EOF'):
        doc = f'```bash\ncat <<{delim}\n$(autumn nope)\nEOF\n```'
        expect(list(invocations(doc)) == [],
               f'<<{delim} prints it instead: {list(invocations(doc))}')
    # Quoting is not special INSIDE a body, so a single-quoted substitution
    # there still expands — the opposite of the rule on a command line.
    inner_q = "```bash\ncat <<EOF\n'$(autumn nope)'\nEOF\n```"
    expect([d for _, d, _, _ in invocations(inner_q)] == ['nope'],
           f'quotes in a body are literal text: {list(invocations(inner_q))}')
    # …and ordinary body text is still data.
    expect(list(invocations('```bash\ncat <<EOF\nautumn nope\nEOF\n```')) == [],
           'plain heredoc body text is not a command')

    # `eval` combines its arguments into shell input and executes them.
    for form in ("eval 'autumn nope'", 'eval "autumn nope"', 'eval autumn nope',
                 "sudo eval 'autumn nope'"):
        doc = f'```bash\n{form}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{form} runs its argument: {got}')
    # The keyword owns only a COMMAND STRING. An unquoted `eval autumn migrate`
    # is already reached by the prefix walk, and marking its bare `autumn` as a
    # nested line too reported the page twice — once really, once as an empty
    # argv.
    expect([d for _, d, _, _ in invocations('```bash\neval autumn migrate\n```')]
           == ['migrate'], 'an unquoted eval is read exactly once')
    expect(list(invocations("```bash\necho 'autumn nope'\n```")) == [],
           'echo does not eval its argument')

    # In a heredoc BODY, quoting is not special but a BACKSLASH is: bash's
    # here-document rules let it quote `$` and a backtick. And a body expands
    # legacy backtick substitutions too, which the `$(` scan alone missed.
    for body, want in (('$(autumn nope)', ['nope']),
                       ('\\$(autumn nope)', []),
                       ('\\\\$(autumn nope)', ['nope']),
                       ('`autumn nope`', ['nope']),
                       ('\\`autumn nope\\`', []),
                       ('`autumn nope', []),
                       ('autumn nope', []),
                       ('$(autumn nope) `autumn nada`', ['nope', 'nada'])):
        doc = f'```bash\ncat <<EOF\n{body}\nEOF\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == want, f'heredoc body {body!r} -> {got} != {want}')
    # A QUOTED delimiter expands nothing at all, whichever form is written.
    none_doc = "```bash\ncat <<'EOF'\n$(autumn nope) `autumn nada`\nEOF\n```"
    expect(list(invocations(none_doc)) == [],
           f'a quoted delimiter expands nothing: {list(invocations(none_doc))}')
    # The same rules inside a literal `run: |`, which reaches them by a
    # different path and so has to be checked separately.
    for body, want in (('$(autumn nope)', ['nope']), ('\\$(autumn nope)', []),
                       ('`autumn nope`', ['nope'])):
        doc = f'```yaml\nrun: |\n  cat <<EOF\n  {body}\n  EOF\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == want, f'literal-block heredoc body {body!r}: {got}')
    # …and the COMMAND-LINE rules are unchanged, which matters because the two
    # contexts answer the same question by different means: on a command line
    # tokenization destroys backslash parity, so the decision is made from a
    # masked copy; in a body nothing is tokenized and it is read off the text.
    for line, want in (('printf \'%s\' "\\$(autumn nope)"', []),
                       ('printf \'%s\' "\\\\$(autumn nope)"', ['nope'])):
        doc = f'```bash\n{line}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == want, f'command line {line!r} -> {got} != {want}')

    # A wrapper's short options may be BUNDLED. `ssh -vp 22` is `-v` (flag)
    # then `-p` (value); testing the whole `-vp` against the value set found
    # nothing, so `22` was read as the destination and the command after it
    # was lost. Only the last letter of a cluster takes a separate token.
    for form in ('ssh -vp 22 host', 'ssh -4vp 22 host', 'ssh -v host',
                 'ssh -p 22 host', 'ssh host'):
        doc = f'```bash\n{form} autumn nope\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{form} reaches its remote command: {got}')

    # Inside DOUBLE quotes a backslash quotes only a special character; before
    # anything else it is literal and stays in the delimiter word. `<<"END\\q"`
    # terminates on `END\\q`, so dropping the backslash recorded `ENDq`, the
    # terminator never matched, and the body was swallowed.
    lit = 'cat <<"END\\q"\nautumn db\nEND\\q\nautumn nope'
    expect([d for _, d, _, _ in invocations(f'```bash\n{lit}\n```')] == ['nope'],
           f'a literal backslash in a delimiter is kept: '
           f'{list(invocations(chr(96)*3 + "bash" + chr(10) + lit + chr(10) + chr(96)*3))}')
    # …and a backslash before a SPECIAL char still quotes it, so `END\\$X`
    # terminates on `END$X`.
    spec = 'cat <<"END\\$X"\nautumn db\nEND$X\nautumn nope'
    expect([d for _, d, _, _ in invocations(f'```bash\n{spec}\n```')] == ['nope'],
           'a backslash before $ still quotes it in a delimiter')

    # `env -S '…'` / `--split-string` splits the value into an argv and runs
    # its first word, so the value is a command line env owns — every spelling.
    for form in ("env -S 'autumn nope'", "env --split-string='autumn nope'",
                 "env -S'autumn nope'", "env -u X -S 'autumn nope'",
                 "sudo env -S 'autumn nope'"):
        doc = f'```bash\n{form}\n```'
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == ['nope'], f'{form} runs its split string: {got}')
    # …but only env owns it: the same option on another program is an ordinary
    # value.
    expect(list(invocations("```bash\necho --split-string='autumn nope'\n```")) == [],
           'a split-string option on echo runs nothing')

    # A continuation followed by a BLANK line: bash joins the backslash to the
    # empty next line, so `autumn migrate \` + blank is the finished command.
    # Leaving the continuation state set appended onto an empty paragraph on
    # the next content line and raised IndexError — a crash of the whole docs
    # job on a valid script, so this is asserted first among the round's cases.
    crash = '```yaml\nrun: |\n  autumn migrate \\\n\n  autumn nope\n```'
    got = [(ln, d) for ln, d, _, _ in invocations(crash)]
    expect(got == [(3, 'migrate'), (5, 'nope')],
           f'a continuation before a blank line does not crash: {got}')

    # A command substitution in a heredoc body may span physical lines. Read
    # line by line, `$(` on one line and `)` on another was never seen whole,
    # so the command between them was left as body data.
    for doc, want in (
            ('```bash\ncat <<EOF\n$(autumn nope\n)\nEOF\n```', ['nope']),
            ('```bash\ncat <<EOF\n$(autumn\nnope\n)\nEOF\n```', ['nope']),
            ('```bash\ncat <<EOF\n`autumn\nnope`\nEOF\n```', ['nope']),
            ('```yaml\nrun: |\n  cat <<EOF\n  $(autumn nope\n  )\n  EOF\n```',
             ['nope'])):
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == want, f'a multiline body substitution is read: {got}')
    # Two separate single-line substitutions stay two, each on its own line.
    two = '```bash\ncat <<EOF\n$(autumn nope)\n$(autumn nada)\nEOF\n```'
    expect([(ln, d) for ln, d, _, _ in invocations(two)]
           == [(3, 'nope'), (4, 'nada')],
           f'separate body substitutions keep their lines: {list(invocations(two))}')
    # An escaped or quoted-delimiter span expands nothing, and an UNTERMINATED
    # one is dropped at the terminator rather than swallowing the line after.
    for doc, want in (
            ('```bash\ncat <<EOF\n\\$(autumn nope\n)\nEOF\n```', []),
            ("```bash\ncat <<'EOF'\n$(autumn nope\n)\nEOF\n```", []),
            ('```bash\ncat <<EOF\n$(autumn nope\nEOF\nautumn after\n```', ['after'])):
        got = [d for _, d, _, _ in invocations(doc)]
        expect(got == want, f'multiline body edge case: {got} != {want}')

    for f in failures:
        print('SELF-TEST FAILURE: ' + f, file=sys.stderr)
    print(f"self-test: {len(checked) - len(failures)} passed, {len(failures)} failed")
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
            elif bad != 'autumn' and surface[path]['required_args'] \
                    and not surface[path]['requires_sub']:
                n = surface[path]['required_args']
                note = f'needs {n} argument' + ('s' if n > 1 else '')
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
