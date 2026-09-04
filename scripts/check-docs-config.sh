#!/usr/bin/env bash
# Config-key drift gate: every `AUTUMN_*` environment variable the reader-facing
# docs tell someone to SET must name a config key that exists.
#
# WHY THIS EXISTS: the corpus already gates the two things a reader copies off a
# page and finds out about immediately — its links (`check-docs-links.sh`, a 404)
# and its commands (`check-docs-cli.sh`, `unrecognized subcommand`). This gates
# the third thing they copy, and the only one of the three that fails *silently*.
#
# A wrong link 404s. A wrong command exits 2. A wrong environment variable is
# simply not read: the process starts, the default stands, and nothing anywhere
# says the override was ignored. `autumn check --config` and
# `server.strict_config` reject an unknown key in `autumn.toml`, but neither sees
# the env layer — an override is applied by name at load time or not at all. So
# `AUTUMN_DATABASE_URL` (one underscore, the pre-0.2 spelling) next to a
# production Postgres URL reads exactly like a working line, and the app comes up
# on the default database. That is the corpus's worst failure mode: the reader
# cannot detect the wrongness, and neither can the framework.
#
# The reader-facing corpus names 636 `AUTUMN_*` variables across 175 pages. This
# gate resolves every one of them.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. A CONFIG-FORM variable (`AUTUMN_SECTION__FIELD`, the double underscore
#      separating section from field) maps to a config path — `AUTUMN_LOG__LEVEL`
#      to `log.level` — and that path must exist in the compiled schema.
#   2. A STANDALONE variable (`AUTUMN_ENV`, `AUTUMN_ROLE` — no `__`, read
#      directly via `env::var` rather than layered into the config) must appear
#      somewhere in the tracked non-markdown tree, i.e. something actually reads
#      it.
#   3. The hand-maintained `AUTUMN_* -> config path` table in `config.rs`'s
#      module docs — 142 rows, the mapping readers meet on docs.rs — must agree
#      with the schema, both in the paths it names and in the variable spellings
#      it derives them from. Every row shaped like a mapping is accounted for:
#      one this script cannot parse is REPORTED, not skipped.
#
# TRUTH SETS (all already in the tree; nothing to regenerate here):
#   - `autumn/tests/fixtures/schema_keys.snapshot` — 484 config paths of
#     `AutumnConfig`, of which the 397 LEAVES are the settable keys (a branch
#     like `server.upgrade` is not one; the runtime probes the leaves beneath
#     it). Produced by the same `get_schema_keys()` walk that backs
#     strict unknown-key validation, and kept honest by
#     `schema_keys_snapshot_guard`. Using the snapshot rather than a fresh walk
#     is what keeps this gate toolchain-free; the snapshot cannot drift from the
#     schema without that test failing first.
#   - The variable names the runtime BUILDS, read from the `format!` calls that
#     build them. The schema walk cannot enumerate a key that does not exist yet
#     — an OAuth2 provider the reader has not configured — so these are matched
#     as patterns instead.
#   - `AUTUMN_*` names the tracked non-markdown tree USES. The snapshot covers
#     `autumn-web` only, and `autumn.toml` has more than one reader:
#     `autumn-search` and `autumn-media-plugin` layer their own
#     `AUTUMN_SEARCH__*` / `AUTUMN_MEDIA__*` overrides in their own crates,
#     `autumn-cli` owns `[dev] watch_dirs`, and `AUTUMN_SYNC__*` is read by the
#     Tauri shell the CLI generates into the reader's app. Gating those against
#     `AutumnConfig` alone would report four subsystems as broken.
#
# RESOLUTION, and why each rung is derived rather than listed:
#   a. The path is a schema LEAF (a branch sets nothing; see SETTABLE_BRANCHES
#      for the one untagged exception).
#   b. Sequence indices are elided first: the schema spells a shard field
#      `database.shards.name`, so `AUTUMN_DATABASE__SHARDS__0__NAME` and the
#      table's `{i}` placeholder both normalise onto it.
#   c. The variable matches a name the runtime BUILDS —
#      `format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_ID")`, once per configured
#      provider. The provider segment is open and the field is EXACT, which is
#      the runtime's own behaviour: it probes only the names it builds, so
#      `…__CLIENT_SECRT` is ignored in production and must be reported here.
#   d. The variable appears in the non-markdown tree in a shape that USES it (a
#      string literal, a shell assignment, an expansion) — this is how the
#      separate-crate configs resolve. A name that appears only in a negative
#      assertion does not count.
#   e. The page itself introduces the name as one the READER chooses —
#      `access_key_id_env = "AUTUMN_OFFSITE_ACCESS_KEY_ID"`, or
#      `InMemoryApiTokenStore::from_env("AUTUMN_API_TOKEN", …)`. Here the string
#      is an example of a name the reader invents and the framework reads back;
#      there is nothing for it to match, and demanding a match would push
#      13 correct lines into waivers.
#   f. The variable is a naming-rule example (`AUTUMN_SECTION__FIELD`) or an
#      identifier the page declares in its own example code.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - TOML config keys in fenced blocks. `[dev] watch_dirs` in the README is an
#     `autumn.toml` key; `[deploy] release_command` in `deployment.md` is a
#     **fly.toml** key, and `[dependencies]`, `[package]`, `[profile]`,
#     `[lints]`, `[advisories]` and 30 other roots in the corpus belong to
#     Cargo, cargo-deny, or a plugin manifest. A TOML fence carries no statement
#     of which file it is, so gating its keys means guessing, and a gate that
#     guesses reports correct pages. `AUTUMN_*` needs no such guess: the prefix
#     names its own namespace wherever it appears.
#   - Values. `AUTUMN_LOG__FORMAT=text` names a real key with an invalid value;
#     the key set is what the schema snapshot knows, and inventing a value
#     grammar here would duplicate the deserializer badly.
#   - `CHANGELOG.md`, `docs/plans/`, `docs/stories/`, `docs/adr/`,
#     `docs/reports/`, `docs/design/`. A changelog entry records the name a key
#     had at the release it describes, and a plan's job includes naming keys
#     that were never built (`AUTUMN_HARVEST__MODE`, from the subsystem that was
#     renamed away) or explicitly rejected (`AUTUMN_DATABASE__POOL__MAX_SIZE`,
#     in the story that ruled out three-level nesting). Gating either would make
#     the gate a tax on writing history down.
#
# WAIVERS: a reader-facing page sometimes has to name a key that does not
# resolve — a migration guide's job is to name the OLD key. Waive it with a
# marker directly below the passage that names it:
#
#     <!-- config-key-allow: AUTUMN_SECTION__OLD_KEY — template placeholder -->
#
# The marker sits beside the claim, so when the passage is deleted the waiver
# goes with it; a central allowlist would outlive both. Every waiver must carry
# a reason after the variable. Scope is deliberately narrow — a waiver covers
# only its own blank-line-separated block and the one directly above it, so the
# same variable misspelled further down the page is still reported.
#
# USAGE:
#   scripts/check-docs-config.sh              # gate the corpus
#   scripts/check-docs-config.sh --list       # print the resolved key surface
#   scripts/check-docs-config.sh --self-test  # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# Kept in Python for the same reason as its two sibling gates: snapshot
# prefix matching and code-fence-aware scanning are both work bash renders
# unreadable, and python3 is already a dependency of check-docs-links.sh,
# check-docs-cli.sh and check-plugin-freshness.sh.
run_py() {
  python3 - "$@" <<'PYEOF'
import os, re, subprocess, sys, pathlib, collections

MODE = sys.argv[1]
ROOT = pathlib.Path(sys.argv[2])

SNAPSHOT = ROOT / 'autumn' / 'tests' / 'fixtures' / 'schema_keys.snapshot'
CONFIG_RS = ROOT / 'autumn' / 'src' / 'config.rs'

# This script is not evidence for its own check. Its header documents the waiver
# syntax with a worked example (`AUTUMN_SECTION__OLD_KEY`), and the moment the
# file was committed that example landed in the token sweep below and started
# resolving the very variable the waiver existed for — silently making two
# waivers inert and leaving the gate green for a key that does not exist. A
# checker that reads its own prose as truth cannot fail, which is the one
# outcome worse than not having it. `gate_script_is_not_its_own_truth_set` in
# --self-test pins this.
SELF = 'scripts/check-docs-config.sh'

# The corpus a reader lands in, matching check-docs-cli.sh exactly. Kept
# identical on purpose: three gates disagreeing about what "reader-facing"
# means is how a page ends up covered by one and not the others.
INCLUDE_DIRS = ('docs/guide/', 'docs/migrations/', 'skills/', 'agents/')
INCLUDE_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md')

# `AUTUMN_` followed by at least one more character, ending on an
# alphanumeric so a trailing underscore in prose (`AUTUMN_UPGRADE_*`) is not
# swallowed into the name.
#
# A `{i}`-style placeholder is part of a name, not a delimiter: a page
# documenting a positional section writes the whole family in one row, as
# `docs/guide/sharding.md` does for `AUTUMN_DATABASE__SHARDS__{i}__NAME`. Before
# this admitted lowercase inside braces the pattern matched NOTHING on those
# seven lines — not a truncated name, no name at all — so the shard family was
# absent from the corpus scan entirely and a typo there could not be reported.
# (The same oversight in the module-doc row pattern hid seven table rows; fixing
# one and not the other is how a gate ends up half-applied.) `to_path` elides
# the placeholder, so these land on `database.shards.name` like a literal index.
# Lowercase OUTSIDE a placeholder is admitted here on purpose, so that a casing
# typo is reported rather than skipped. `AUTUMN_LOG__LEVeL=debug` is a name the
# runtime will not read, but under an upper-case-only pattern it matched nothing
# at all — the claim was invisible, and the occurrence count did not even move.
# Case is validated in `malformed` below instead, where it can be reported.
VAR = re.compile(r'\bAUTUMN_(?:\{[a-z]+\}|[A-Za-z0-9_])*[A-Za-z0-9]\b')

# Everything outside a `{placeholder}` must be upper case, digits, or `_`.
def malformed(var):
    return any(c.islower() for c in re.sub(r'\{[a-z]+\}', '', var))

# A path segment that is a sequence index rather than a field name: a literal
# index as written in a shell line, or the `{i}` / `{N}` placeholder the
# module-doc table uses for the same position.
INDEX_SEG = re.compile(r'^(?:\d+|\{\w+\}|N|I)$')

# The two shapes in which a page hands the reader a variable name to invent:
# a `*_env` config key whose value IS the name, and `from_env("NAME", …)`.
CHOSEN = (re.compile(r'_env\s*=\s*"(AUTUMN_[A-Z0-9_]+)"'),
          re.compile(r'from_env\(\s*"(AUTUMN_[A-Z0-9_]+)"'))

WAIVER = re.compile(r'<!--\s*config-key-allow:\s*(AUTUMN_[A-Z0-9_]+)\s*(.*?)\s*-->')

# Variable names the runtime BUILDS at load time, for sections whose keys the
# reader chooses — `format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_ID")`, once
# per configured provider. The schema walk cannot enumerate these (it never sees
# a provider that does not exist yet), so they are read from the source that
# constructs them and matched as patterns.
#
# Derived rather than listed. An earlier cut named the four map SECTIONS
# (`auth.oauth2`, `jobs.queues`, …) and accepted any descendant path, which
# resolved `AUTUMN_AUTH__OAUTH2__GITHUB__CLIENT_SECRT` — a typo the runtime
# silently ignores, since it probes only the two names it builds. Matching the
# template instead keeps the provider segment open and the FIELD exact, which
# is precisely the runtime's own behaviour.
#
# A template whose final segment is itself a placeholder
# (`AUTUMN_DATABASE__SHARDS__{i}__{field}`, where `field` is a closure
# parameter) constrains nothing and is skipped: it would re-admit
# `…__SHARDS__0__NOPE`. Shard fields need no template anyway — the schema
# enumerates them as `database.shards.*`, reached by eliding the index.
TEMPLATE = re.compile(r'"(AUTUMN_[A-Z0-9_]*(?:\{[a-z_]+\}[A-Z0-9_]*)+)"')

# Metasyntactic roots: a page teaching the NAMING RULE rather than naming a key.
# Seven pages write `AUTUMN_SECTION__FIELD` to explain that a double underscore
# separates section from field, and `docs/migrations/TEMPLATE.md` writes
# `AUTUMN_SECTION__OLD_KEY` in the row an author fills in per release. `section`
# is not a config root and cannot become one, so everything under it is a worked
# example by construction — which is why this is a rule rather than eight waiver
# markers repeating the same reason on eight pages.
PLACEHOLDER_ROOTS = ('section',)

# The snapshot records BRANCHES as well as settable leaves — `server.upgrade`
# sits above `server.upgrade.enabled` and `…ready_timeout_secs` — but the
# runtime probes only the leaf names. Resolving against the whole snapshot
# therefore blessed a truncated key: `AUTUMN_SERVER__UPGRADE` counted as found
# while setting nothing, which is precisely the silent no-op this gate exists to
# catch. Only leaves resolve.
#
# One branch really is settable: `SigningSecretConfig`'s untagged deserializer
# accepts a bare string, so `AUTUMN_SECURITY__SIGNING_SECRET` sets
# `security.signing_secret` whole (38 occurrences across the corpus). Measured
# before narrowing the rule — it is the only corpus variable that lands on a
# branch, and a second one has to be added here deliberately.
SETTABLE_BRANCHES = ('security.signing_secret',)

# A page may declare a Rust `const`/`static` whose name matches the variable
# shape — `docs/guide/wasm-islands.md` has
# `pub const AUTUMN_SOURCE: &str = include_str!("corpus.txt")` — and then use it
# in later snippets. That is an identifier in example code, not an environment
# variable, so it is recognised per page: the declaration is the page's own
# statement of what the name is, and it does not leak to any other page.
DECLARED_CONST = re.compile(
    r'\b(?:const|static)\s+(AUTUMN_[A-Z0-9_]+)\s*:')

# --------------------------------------------------------------- truth sets

def schema_paths(text):
    return {l.strip() for l in text.splitlines() if l.strip()}


def leaf_paths(paths):
    """Settable keys: a path with nothing below it.

    The snapshot records every node the walk visits, branches included, and a
    branch is not a key you can set — see SETTABLE_BRANCHES.
    """
    return {p for p in paths if not any(o.startswith(p + '.') for o in paths)}


def built_patterns(root):
    """Regexes for the variable names the runtime constructs at load time.

    Each `{placeholder}` becomes one reader-chosen segment; a template ending in
    a placeholder is skipped, since it fixes nothing.
    """
    out = subprocess.run(['git', 'ls-files', '-z', '*.rs'], cwd=root,
                         capture_output=True, text=True).stdout
    pats = {}
    for rel in out.split('\0'):
        if not rel:
            continue
        try:
            body = (root / rel).read_text(encoding='utf-8', errors='replace')
        except OSError:
            continue
        for tpl in TEMPLATE.findall(body):
            if tpl.split('__')[-1].startswith('{'):
                continue
            pats[tpl] = re.compile(
                '^' + re.sub(r'\\\{[a-z_]+\\\}', '[A-Z0-9_]+',
                             re.escape(tpl)) + '$')
    return pats


# The shapes in which a file USES an environment variable, as opposed to
# merely talking about one: a string literal (`env::var("AUTUMN_X")`, a YAML or
# TOML value), a shell assignment or export, and a shell expansion.
#
# The distinction is load-bearing, and was not in the first cut of this script.
# A plain token sweep counts prose: this gate's own CI step comment explains it
# with the words `AUTUMN_DATABASE_URL`, and that mention alone was enough to
# resolve the very variable the gate exists to catch — the injected regression
# probe stopped failing. Requiring a use-shape means a comment can name a
# variable to explain it without thereby asserting that something reads it.
QUOTED = re.compile(r'["\'](AUTUMN_[A-Z0-9_]+)["\']')
ASSIGNED = re.compile(r'(?:^|[;&|(\s])(?:export\s+)?(AUTUMN_[A-Z0-9_]+)=')
EXPANDED = re.compile(r'\$\{?(AUTUMN_[A-Z0-9_]+)\}?')

# A quoted name counts only where the code BINDS it as an environment-variable
# name or READS the environment with it. Any quoted string was the earlier rule
# and it kept admitting fixtures: a test may name a variable precisely to prove
# the runtime ignores it. `autumn-cli/src/doctor.rs` sets
# `MockEnv::new().with("AUTUMN_SERVER__TLS__ENABLED", "false")` under a test
# named `unrecognized_tls_env_var_is_not_detected` — the key is absent from the
# schema and the server serves plain HTTP for it — and
# `autumn-cli/tests/generate.rs` asserts `!test.contains(
# "AUTUMN_TEST_SESSION_COOKIE")`. Both are assertions that a name does NOT work,
# and both were being read as proof that it does.
#
# The binding form is what makes this affordable. An earlier attempt required an
# accessor near the string and reported 25 correct pages, because the dominant
# shape here is `const CANARY_ENV: &str = "AUTUMN_CANARY"` — the read happens
# wherever the constant is later used, arbitrarily far away. Recognising the
# binding itself covers those without needing to follow the constant.
BOUND = re.compile(
    r'\b(?:const|static)\s+\w+\s*:\s*&\s*(?:\'\w+\s+)?str\s*=\s*"(AUTUMN_[A-Z0-9_]+)"')

# The accessors that read or write the environment. `with` is deliberately NOT
# here: it supplies a fixture environment, which is how a test names a variable
# that does not exist.
ACCESSOR = re.compile(r'\b(?:var|var_os|set_var|remove_var|env_trimmed'
                      r'|parse_env\w*|env_bool\w*|getenv|get)\s*\(')

# …and never where the line asserts the name is absent.
NEGATED = re.compile(r'assert(?:_ne)?!\s*\(\s*!|!\s*[\w.]*\bcontains\b|\bassert_ne!')


def source_tokens(root):
    """Every `AUTUMN_*` variable the tracked NON-markdown tree actually uses.

    This is the truth set for the four subsystems outside `AutumnConfig`:
    `autumn-search` and `autumn-media-plugin` layer their own overrides in their
    own crates, `autumn-cli` owns `[dev] watch_dirs`, and `AUTUMN_SYNC__*` is
    read by the Tauri shell the CLI generates. Rung (a) covers everything the
    schema knows; this rung exists for those.
    """
    out = subprocess.run(['git', 'ls-files', '-z'], cwd=root,
                         capture_output=True, text=True).stdout
    tokens = set()
    for rel in out.split('\0'):
        if not rel or rel.endswith('.md') or rel == SELF:
            continue
        try:
            body = (root / rel).read_text(encoding='utf-8', errors='replace')
        except OSError:
            continue
        if 'AUTUMN_' not in body:
            continue
        lines = body.splitlines()
        for n, line in enumerate(lines):
            tokens.update(ASSIGNED.findall(line))
            tokens.update(EXPANDED.findall(line))
            tokens.update(BOUND.findall(line))
            if NEGATED.search(line):
                continue
            # The accessor may open a line or two above its argument —
            # `parse_env(\n    env,\n    "AUTUMN_MEDIA__ROOM_NAMESPACE",` is the
            # house style — so look back a little for it.
            if ACCESSOR.search('\n'.join(lines[max(0, n - 3):n + 1])):
                tokens.update(QUOTED.findall(line))
    return tokens


def corpus(root):
    # NUL-delimited so a path containing whitespace is not split into fragments.
    out = subprocess.run(['git', 'ls-files', '-z', '*.md'], cwd=root,
                         capture_output=True, text=True).stdout
    return [f for f in out.split('\0')
            if f and (f.startswith(INCLUDE_DIRS) or f in INCLUDE_FILES)]


# -------------------------------------------------------------- resolution

def to_path(var):
    """`AUTUMN_DATABASE__SHARDS__0__NAME` -> `database.shards.name`."""
    segs = [s.lower() for s in var[len('AUTUMN_'):].split('__')]
    return '.'.join(s for s in segs if not INDEX_SEG.match(s))


def is_config_form(var):
    """Config-form iff a `__` separates a section from a field.

    A trailing or doubled separator (`AUTUMN_SESSION__`, written in prose to
    name a whole section) leaves an empty segment and is not a key claim.
    """
    body = var[len('AUTUMN_'):]
    return '__' in body and all(body.split('__'))


def resolve(var, leaves, built, tokens):
    """Return the rung that resolves `var`, or None. Order is cost, not rank."""
    if is_config_form(var):
        path = to_path(var)
        if path in leaves or path in SETTABLE_BRANCHES:
            return 'schema'
        if path.split('.')[0] in PLACEHOLDER_ROOTS:
            return 'naming-rule example'
    if any(p.match(var) for p in built.values()):
        return 'runtime-built name'
    if var in tokens:
        return 'source'
    return None


# ----------------------------------------------------------------- waivers

def blocks(lines):
    """Map each 1-based line to the index of its blank-line-separated block."""
    out, block, blank = {}, 0, True
    for i, line in enumerate(lines, 1):
        if not line.strip():
            blank = True
        else:
            if blank:
                block += 1
            blank = False
        out[i] = block
    return out


def waivers(lines):
    """(variable, block) pairs a waiver marker covers.

    A marker covers its own block and the one above it — the passage it was
    written for. Anything wider silently re-admits the defect the gate exists
    to catch.
    """
    at = blocks(lines)
    out = collections.defaultdict(set)
    for i, line in enumerate(lines, 1):
        m = WAIVER.search(line)
        if not m:
            continue
        if not m.group(2):
            continue  # a waiver without a reason is not a waiver
        out[m.group(1)].update({at[i], at[i] - 1})
    return out


# ------------------------------------------------------------- corpus scan

def scan(files, read, leaves, built, tokens):
    stats, defects = collections.Counter(), []
    for rel in files:
        text = read(rel)
        lines = text.splitlines()
        chosen = {m for pat in CHOSEN for m in pat.findall(text)}
        # Names the page itself declares as Rust items: identifiers in example
        # code, scoped to this page only.
        consts = set(DECLARED_CONST.findall(text))
        at, waived = blocks(lines), waivers(lines)
        for i, line in enumerate(lines, 1):
            if 'AUTUMN_' not in line:
                continue
            # A waiver marker names the variable in order to waive it. That
            # mention is metadata addressed to this script, not a key claim
            # addressed to a reader, so it is not an occurrence — counting it
            # made an unreasoned waiver report its own subject twice.
            line = WAIVER.sub('', line)
            for var in VAR.findall(line):
                if var in chosen:
                    stats['reader-chosen name'] += 1
                    continue
                if var in consts:
                    stats['example-code identifier'] += 1
                    continue
                rung = None if malformed(var) else resolve(
                    var, leaves, built, tokens)
                if rung:
                    stats[rung] += 1
                elif at[i] in waived.get(var, ()):
                    stats['waived'] += 1
                else:
                    defects.append((rel, i, var, line.strip()))
    return stats, defects


# --------------------------------------------------- module-doc table scan

# A mapping row: a variable cell, then a backticked config path.
#
# The variable class admits a lowercase `{i}` placeholder. Getting that wrong is
# how the first cut silently skipped all seven `AUTUMN_DATABASE__SHARDS__{i}__*`
# rows — a class of `[A-Z0-9_{}]` matches the braces but not the `i` between
# them — so a misspelling in any shard row left the gate green. `table_rows`
# below now accounts for every row it sees rather than quietly dropping the ones
# it cannot parse.
TABLE_ROW = re.compile(
    r'^//! \| `(AUTUMN_(?:[A-Z0-9_]|\{[a-z]\})+)` \| `([^`]+)` \|')

# The rows whose second cell is prose rather than a backticked path:
# `AUTUMN_ENV` and `AUTUMN_PROFILE` map to "active profile", not to a config
# key, so there is no mapping for this check to verify.
#
# Enumerated by NAME, not by the shape "second cell is not backticked". The
# shape was the first cut and it re-opened the hole this pair of patterns was
# added to close: a mapping row that loses its opening backtick —
# `| database.urll |` — stops matching TABLE_ROW, gets classified as intentional
# prose, and produces neither a table defect nor an unparsed-row defect. The
# whole point of accounting for every row is that a row cannot leave the check
# by being malformed.
TABLE_PROSE_ROWS = ('AUTUMN_ENV', 'AUTUMN_PROFILE')
TABLE_PROSE_ROW = re.compile(
    r'^//! \| `(' + '|'.join(TABLE_PROSE_ROWS) + r')` \| [^`]')

# Anything shaped like a table row at all, used to prove the two patterns above
# between them account for every one.
TABLE_ANY_ROW = re.compile(r'^//! \| `AUTUMN_')

# The one row whose two columns legitimately disagree. `SigningSecretConfig`
# has an untagged deserializer that accepts a bare string, so
# `AUTUMN_SECURITY__SIGNING_SECRET` addresses `security.signing_secret` while
# the row documents the field that string lands in.
#
# Enumerated rather than allowed as a general "the variable may name any prefix
# of the row's path", which was the first cut: that let ANY truncated variable
# column pass so long as the declared path existed, so a row edited from
# `AUTUMN_DATABASE__URL` to `AUTUMN_DATABASE` still "agreed" with
# `database.url` — defeating the one-sided-edit detection this check exists for.
# All 135 current rows agree exactly except this one.
TABLE_PREFIX_OK = {('AUTUMN_SECURITY__SIGNING_SECRET',
                    'security.signing_secret.secret')}


def table_rows(text):
    """Mapping rows, plus any row shaped like one that neither pattern claims.

    The unparsed list is returned rather than discarded: a row this script
    cannot read is a row it is not checking, and silence about that is what let
    seven shard rows sit unverified.
    """
    rows, unparsed = [], []
    for i, line in enumerate(text.splitlines(), 1):
        if not TABLE_ANY_ROW.match(line):
            continue
        if m := TABLE_ROW.match(line):
            rows.append((i, m.group(1), m.group(2)))
        elif not TABLE_PROSE_ROW.match(line):
            unparsed.append((i, line.strip()))
    return rows, unparsed


def check_table(rows, leaves, built):
    """The module-doc table must agree with the schema on both columns.

    The variable column and the path column are two spellings of one mapping, so
    each row is checked twice: the path it names must exist, and the variable it
    names must derive that path. The second check is what catches a row edited
    on one side only.
    """
    out = []
    for i, var, declared in rows:
        path = re.sub(r'\[\w+\]', '', declared)
        segs = path.split('.')
        known = path in leaves or any(p.match(var) for p in built.values())
        if not known:
            out.append((i, var, declared, 'path is not in the schema'))
            continue
        derived = to_path(var)
        # Exact agreement, save for the enumerated untagged-deserializer row.
        # Anything else is a row edited on one side only.
        if derived != path and (var, path) not in TABLE_PREFIX_OK:
            out.append((i, var, declared,
                        f'variable derives `{derived}`, row says `{path}`'))
    return out


# ------------------------------------------------------------------- modes

def load():
    paths = schema_paths(SNAPSHOT.read_text(encoding='utf-8'))
    return leaf_paths(paths), built_patterns(ROOT), source_tokens(ROOT)


def main():
    if not SNAPSHOT.exists():
        print(f'error: schema snapshot missing at {SNAPSHOT}', file=sys.stderr)
        return 2
    leaves, built, tokens = load()
    files = corpus(ROOT)
    read = lambda rel: (ROOT / rel).read_text(encoding='utf-8', errors='replace')
    stats, defects = scan(files, read, leaves, built, tokens)

    rows, unparsed = table_rows(CONFIG_RS.read_text(encoding='utf-8'))
    table_defects = check_table(rows, leaves, built)

    print(f'corpus: {len(files)} reader-facing markdown files')
    print(f'surface: {len(leaves)} schema leaves, '
          f'{len(built)} runtime-built name patterns, '
          f'{len(tokens)} variables named in the non-markdown tree')
    print(f'checked: {sum(stats.values())} `AUTUMN_*` occurrences, '
          f'{len(rows)} module-doc table rows')
    for rung, n in sorted(stats.items(), key=lambda kv: -kv[1]):
        if rung != 'waived':
            print(f'  resolved via {rung}: {n}')

    for rel, line, var, text in defects:
        print(f'\n{rel}:{line}: `{var}` does not name a config key')
        print(f'    {text}')
        if is_config_form(var):
            print(f'    derives the path `{to_path(var)}`, '
                  f'which is not in {SNAPSHOT.relative_to(ROOT)}')
        else:
            print('    nothing in the non-markdown tree reads it')
    for line, var, declared, why in table_defects:
        print(f'\nautumn/src/config.rs:{line}: '
              f'module-doc row `{var}` -> `{declared}`: {why}')
    for line, text in unparsed:
        print(f'\nautumn/src/config.rs:{line}: module-doc row not understood, '
              f'so not checked')
        print(f'    {text}')

    total = len(defects) + len(table_defects) + len(unparsed)
    print(f'\ndefects: {total}'
          + (f' ({stats["waived"]} waived)' if stats['waived'] else ''))
    if total:
        print('\nA config key that does not exist is not read and not reported:'
              '\nthe app starts on the default. Fix the spelling, or waive it'
              '\nbeside the passage with'
              '\n  <!-- config-key-allow: AUTUMN_X__Y — why -->', file=sys.stderr)
        return 1
    print('Config key gate OK.')
    return 0


def list_surface():
    leaves, built, tokens = load()
    for leaf in sorted(leaves):
        print(leaf)
    for tpl in sorted(built):
        print(f'{tpl}  (runtime-built)')
    print(f'\n{len(leaves)} schema leaves; '
          f'{len(tokens)} AUTUMN_* variables in the non-markdown tree')
    return 0


# --------------------------------------------------------------- self-test

def self_test():
    leaves = leaf_paths(schema_paths(
        'log\nlog.level\nauth\nauth.oauth2\ndatabase\ndatabase.shards\n'
        'database.shards.name\nsecurity\nsecurity.signing_secret\n'
        'security.signing_secret.secret\nserver\nserver.upgrade\n'
        'server.upgrade.enabled\n'))
    built = {'AUTUMN_AUTH__OAUTH2__{p}__CLIENT_SECRET':
             re.compile(r'^AUTUMN_AUTH__OAUTH2__[A-Z0-9_]+__CLIENT_SECRET$')}
    tokens = {'AUTUMN_ENV', 'AUTUMN_SEARCH__QUEUE'}
    checked, failures = [], []

    def case(name, got, want):
        checked.append(name)
        if got != want:
            failures.append(f'{name}: got {got!r}, want {want!r}')

    r = lambda v: resolve(v, leaves, built, tokens)
    case('plain key resolves', r('AUTUMN_LOG__LEVEL'), 'schema')
    case('missing key fails', r('AUTUMN_LOG__LEVL'), None)
    # The single-underscore spelling is the whole point of the gate: it derives
    # a one-segment path that no section can match.
    case('single underscore fails', r('AUTUMN_DATABASE_URL'), None)
    case('sequence index elided',
         r('AUTUMN_DATABASE__SHARDS__0__NAME'), 'schema')
    # A branch is not a settable key: the runtime probes the leaves under it.
    case('a schema branch is not a key', r('AUTUMN_SERVER__UPGRADE'), None)
    case('its leaf is', r('AUTUMN_SERVER__UPGRADE__ENABLED'), 'schema')
    case('the one settable branch still resolves',
         r('AUTUMN_SECURITY__SIGNING_SECRET'), 'schema')
    # The corpus scanner must SEE a placeholder name at all — before it did not
    # match `…SHARDS__{i}__NAME` in any form, so those seven documented
    # variables were invisible rather than merely unresolved.
    case('a placeholder name is scanned',
         VAR.findall('| `AUTUMN_DATABASE__SHARDS__{i}__NAME` | x |'),
         ['AUTUMN_DATABASE__SHARDS__{i}__NAME'])
    # A casing typo must be SEEN, then reported — under an upper-case-only
    # pattern it matched nothing and the claim was invisible.
    case('a casing typo is scanned',
         VAR.findall('export AUTUMN_LOG__LEVeL=debug'), ['AUTUMN_LOG__LEVeL'])
    case('a casing typo is malformed', malformed('AUTUMN_LOG__LEVeL'), True)
    case('a placeholder is not malformed',
         malformed('AUTUMN_DATABASE__SHARDS__{i}__NAME'), False)
    _, dm = scan(['d.md'], lambda _: 'export AUTUMN_LOG__LEVeL=debug\n',
                 leaves, built, tokens)
    case('a casing typo is reported', len(dm), 1)
    case('a placeholder name resolves',
         r('AUTUMN_DATABASE__SHARDS__{i}__NAME'), 'schema')
    case('a typo beside a placeholder is caught',
         r('AUTUMN_DATABASE__SHARDS__{i}__NOPE'), None)
    case('table placeholder elided',
         r('AUTUMN_DATABASE__SHARDS__{i}__NAME'), 'schema')
    case('unknown shard field fails',
         r('AUTUMN_DATABASE__SHARDS__0__NOPE'), None)
    case('reader-keyed map resolves',
         r('AUTUMN_AUTH__OAUTH2__GITLAB__CLIENT_SECRET'), 'runtime-built name')
    # `database.shards` has children, so it is a sequence and not a map: an
    # unknown field under it must NOT be swallowed by the map rung.
    case('sequence is not a map', r('AUTUMN_DATABASE__NOPE'), None)
    # A scalar leaf has no children either. Treating "childless" as "map" — the
    # first cut of this rung — resolved anything under any scalar and would
    # have hidden drift beneath all 397 of them.
    case('scalar is not a map', r('AUTUMN_LOG__LEVEL__NOPE'), None)
    case('naming-rule example resolves',
         r('AUTUMN_SECTION__FIELD'), 'naming-rule example')
    case('naming-rule root does not cover a real section',
         r('AUTUMN_LOG__NOPE'), None)
    case('standalone from source', r('AUTUMN_ENV'), 'source')
    case('standalone unknown fails', r('AUTUMN_NOPE'), None)
    case('other crate from source', r('AUTUMN_SEARCH__QUEUE'), 'source')
    case('trailing separator is not a key claim',
         is_config_form('AUTUMN_SESSION__'), False)
    case('path derivation', to_path('AUTUMN_A__B_C'), 'a.b_c')

    # Waiver scope: covers its own block and the one above, nothing further.
    doc = ('AUTUMN_BAD__ONE\n\nprose\n<!-- config-key-allow: AUTUMN_BAD__ONE — why -->\n'
           '\nAUTUMN_BAD__ONE\n')
    stats, defects = scan(['d.md'], lambda _: doc, leaves, built, tokens)
    case('waiver covers block above and its own', stats['waived'], 1)
    case('waiver does not reach further', len(defects), 1)

    doc2 = 'AUTUMN_BAD__ONE\n<!-- config-key-allow: AUTUMN_BAD__ONE -->\n'
    _, d2 = scan(['d.md'], lambda _: doc2, leaves, built, tokens)
    case('waiver without a reason does not waive', len(d2), 1)

    doc3 = 'key_env = "AUTUMN_WHATEVER"\nexport AUTUMN_WHATEVER=x\n'
    s3, d3 = scan(['d.md'], lambda _: doc3, leaves, built, tokens)
    case('reader-chosen name resolves', (s3['reader-chosen name'], len(d3)), (2, 0))

    # A const the page declares is an identifier in example code, on that page.
    doc4 = 'pub const AUTUMN_SOURCE: &str = "x";\nWorld::new(AUTUMN_SOURCE)\n'
    s4, d4 = scan(['d.md'], lambda _: doc4, leaves, built, tokens)
    case('declared const is example code',
         (s4['example-code identifier'], len(d4)), (2, 0))
    _, d5 = scan(['d.md'], lambda _: 'World::new(AUTUMN_SOURCE)\n',
                 leaves, built, tokens)
    case('an undeclared name is not excused by another page', len(d5), 1)

    # The gate's own header names variables in order to explain itself. If the
    # sweep reads this file, every one of them resolves for free — including the
    # waiver example, which is how two live waivers went inert and the gate
    # stayed green on a key that does not exist.
    swept = source_tokens(ROOT)
    case('gate script is not its own truth set',
         'AUTUMN_SECTION__OLD_KEY' in swept, False)
    # …while the sweep still does its real job: a variable read by a crate
    # outside AutumnConfig must resolve.
    case('other-crate variables still sweep in',
         'AUTUMN_SEARCH__QUEUE' in swept, True)

    # A runtime-built name keeps the reader-chosen segment open and the field
    # EXACT — the earlier "any descendant of the map section" rung resolved
    # `…__CLIENT_SECRT`, a typo the runtime silently ignores.
    case('runtime-built name resolves any provider',
         r('AUTUMN_AUTH__OAUTH2__GITLAB__CLIENT_SECRET'), 'runtime-built name')
    case('a typo in the fixed field is caught',
         r('AUTUMN_AUTH__OAUTH2__GITHUB__CLIENT_SECRT'), None)

    # Templates are read from the real tree: the oauth2 pair must be found, and
    # `…__SHARDS__{i}__{field}` must be skipped (it would re-admit any field).
    real_built = built_patterns(ROOT)
    case('oauth2 templates are derived from source',
         any('OAUTH2' in t and t.endswith('CLIENT_SECRET') for t in real_built),
         True)
    case('a template ending in a placeholder is skipped',
         any(t.endswith('{field}') for t in real_built), False)
    case('an unknown shard field is not swallowed by a template',
         any(p.match('AUTUMN_DATABASE__SHARDS__0__NOPE')
             for p in real_built.values()), False)

    # A test may name a variable precisely to prove the runtime ignores it.
    # Neither shape may enter the truth set.
    case('a negatively-asserted name is not swept in',
         'AUTUMN_TEST_SESSION_COOKIE' in swept, False)
    case('a fixture environment name is not swept in',
         'AUTUMN_SERVER__TLS__ENABLED' in swept, False)
    # …while the binding form, which has no accessor anywhere near it, is.
    case('a const-bound name is swept in', 'AUTUMN_CANARY' in swept, True)

    # A mapping row that loses a backtick must not escape by looking like prose.
    broken = '//! | `AUTUMN_DATABASE__URL` | database.urll | `String` |'
    rows_b, unparsed_b = table_rows(broken)
    case('a malformed mapping row is reported, not called prose',
         (len(rows_b), len(unparsed_b)), (0, 1))
    prose = '//! | `AUTUMN_ENV` | active profile | `String` |'
    rows_p, unparsed_p = table_rows(prose)
    case('the enumerated prose rows still pass',
         (len(rows_p), len(unparsed_p)), (0, 0))

    good = [(1, 'AUTUMN_LOG__LEVEL', 'log.level'),
            (2, 'AUTUMN_SECURITY__SIGNING_SECRET', 'security.signing_secret.secret'),
            (3, 'AUTUMN_DATABASE__SHARDS__{i}__NAME', 'database.shards[i].name')]
    case('sound table rows pass', check_table(good, leaves, built), [])
    case('table row with a dead path fails',
         len(check_table([(9, 'AUTUMN_LOG__NOPE', 'log.nope')], leaves, built)), 1)
    case('table row edited on one side fails',
         len(check_table([(9, 'AUTUMN_LOG__LEVEL', 'auth.oauth2')], leaves, built)), 1)
    # A truncated variable column must NOT pass just because the row's path
    # exists: `AUTUMN_DATABASE` is not how you set `database.shards.name`.
    case('a truncated variable column fails',
         len(check_table([(9, 'AUTUMN_DATABASE', 'database.shards.name')],
                         leaves, built)), 1)
    # …and the one enumerated untagged-deserializer row still passes.
    case('the signing-secret exception still passes',
         check_table([(9, 'AUTUMN_SECURITY__SIGNING_SECRET',
                       'security.signing_secret.secret')], leaves, built), [])

    for f in failures:
        print(f'FAIL {f}')
    print(f'self-test: {len(checked) - len(failures)} passed, {len(failures)} failed')
    return 1 if failures else 0


sys.exit(self_test() if MODE == '--self-test'
         else list_surface() if MODE == '--list'
         else main())
PYEOF
}

case "${1:-}" in
  "")          echo "Checking AUTUMN_* config keys across the reader-facing docs..."
               run_py "" "$root" ;;
  --list)      run_py --list "$root" ;;
  --self-test) run_py --self-test "$root" ;;
  *)           echo "usage: $0 [--list|--self-test]" >&2; exit 2 ;;
esac
