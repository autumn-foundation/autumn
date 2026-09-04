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
# The reader-facing corpus names 626 `AUTUMN_*` variables across 175 pages. This
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
#      module docs — 135 rows, the mapping readers meet on docs.rs — must agree
#      with the schema, both in the paths it names and in the variable spellings
#      it derives them from.
#
# TRUTH SETS (both already in the tree; nothing to regenerate here):
#   - `autumn/tests/fixtures/schema_keys.snapshot` — the 484 leaf paths of
#     `AutumnConfig`, produced by the same `get_schema_keys()` walk that backs
#     strict unknown-key validation, and kept honest by
#     `schema_keys_snapshot_guard`. Using the snapshot rather than a fresh walk
#     is what keeps this gate toolchain-free; the snapshot cannot drift from the
#     schema without that test failing first.
#   - Every `AUTUMN_[A-Z0-9_]+` token in the tracked non-markdown tree. The
#     snapshot covers `autumn-web` only, and `autumn.toml` has more than one
#     reader: `autumn-search` and `autumn-media-plugin` layer their own
#     `AUTUMN_SEARCH__*` / `AUTUMN_MEDIA__*` overrides in their own crates,
#     `autumn-cli` owns `[dev] watch_dirs`, and `AUTUMN_SYNC__*` is read by the
#     Tauri shell the CLI generates into the reader's app. Gating those against
#     `AutumnConfig` alone would report four subsystems as broken.
#
# RESOLUTION, and why each rung is derived rather than listed:
#   a. The path is a schema leaf.
#   b. Sequence indices are elided first: the schema spells a shard field
#      `database.shards.name`, so `AUTUMN_DATABASE__SHARDS__0__NAME` and the
#      table's `{i}` placeholder both normalise onto it.
#   c. A prefix of the path is a CHILDLESS leaf — a map whose keys the reader
#      chooses. `auth.oauth2`, `jobs.queues` and `http.client.base_urls` are
#      leaves with no children in the snapshot precisely because the walk cannot
#      enumerate user keys, so `AUTUMN_AUTH__OAUTH2__GITHUB__CLIENT_SECRET`
#      resolves under `auth.oauth2` without this script knowing what a provider
#      is. Listing the three sections by name instead would have gone stale the
#      first time someone documented a fourth provider.
#   d. The variable appears verbatim in the non-markdown tree (rung 2's truth
#      set, reused: this is how the separate-crate configs resolve).
#   e. The page itself introduces the name as one the READER chooses —
#      `access_key_id_env = "AUTUMN_OFFSITE_ACCESS_KEY_ID"`, or
#      `InMemoryApiTokenStore::from_env("AUTUMN_API_TOKEN", …)`. Here the string
#      is an example of a name the reader invents and the framework reads back;
#      there is nothing for it to match, and demanding a match would push
#      13 correct lines into waivers.
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

# The corpus a reader lands in, matching check-docs-cli.sh exactly. Kept
# identical on purpose: three gates disagreeing about what "reader-facing"
# means is how a page ends up covered by one and not the others.
INCLUDE_DIRS = ('docs/guide/', 'docs/migrations/', 'skills/', 'agents/')
INCLUDE_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md')

# `AUTUMN_` followed by at least one more character, ending on an
# alphanumeric so a trailing underscore in prose (`AUTUMN_UPGRADE_*`) is not
# swallowed into the name.
VAR = re.compile(r'\bAUTUMN_[A-Z0-9_]*[A-Z0-9]\b')

# A path segment that is a sequence index rather than a field name: a literal
# index as written in a shell line, or the `{i}` / `{N}` placeholder the
# module-doc table uses for the same position.
INDEX_SEG = re.compile(r'^(?:\d+|\{\w+\}|N|I)$')

# The two shapes in which a page hands the reader a variable name to invent:
# a `*_env` config key whose value IS the name, and `from_env("NAME", …)`.
CHOSEN = (re.compile(r'_env\s*=\s*"(AUTUMN_[A-Z0-9_]+)"'),
          re.compile(r'from_env\(\s*"(AUTUMN_[A-Z0-9_]+)"'))

WAIVER = re.compile(r'<!--\s*config-key-allow:\s*(AUTUMN_[A-Z0-9_]+)\s*(.*?)\s*-->')

# Sections whose child keys the READER names: an OAuth2 provider, a queue, a
# service under `base_urls`, a host under `circuit_breaker`. These deserialize
# through `SchemaDeserializer::deserialize_map`, which registers no schema
# entry, so the walk records the section itself and can never see beneath it.
#
# Listed rather than inferred, because the snapshot cannot tell a map from a
# scalar: both are leaves with no children, so "has no children" would also
# swallow `AUTUMN_LOG__LEVEL__NOPE` under `log.level` and hide exactly the
# drift this gate is for. The list cannot go stale silently — `map_sections`
# below fails the run if an entry stops being a childless leaf, which is what a
# rename or a newly-enumerable section would look like. The same four are named
# in `schema_drift_guard.rs`; that test asserts they have no restrictive schema
# entry, and this one that the docs may key freely under them.
READER_KEYED = ('auth.oauth2', 'jobs.queues', 'http.client.base_urls',
                'resilience.circuit_breaker.hosts')

# --------------------------------------------------------------- truth sets

def schema_leaves(text):
    return {l.strip() for l in text.splitlines() if l.strip()}


def map_sections(leaves):
    """The reader-keyed sections, checked against the snapshot as we go.

    Raises if a listed section is missing or has grown children — either means
    the section is no longer an open map, and resolving arbitrary keys beneath
    it would start hiding drift.
    """
    out = set()
    for section in READER_KEYED:
        if section not in leaves:
            raise KeyError(
                f'{section} is listed as a reader-keyed map but is not in the '
                f'schema snapshot; if it was renamed or removed, update '
                f'READER_KEYED in this script')
        if any(o.startswith(section + '.') for o in leaves):
            raise KeyError(
                f'{section} is listed as a reader-keyed map but the schema now '
                f'enumerates keys beneath it; it is a fixed section, so remove '
                f'it from READER_KEYED and let its children be checked')
        out.add(section)
    return out


def source_tokens(root):
    """Every `AUTUMN_*` token in the tracked NON-markdown tree.

    Deliberately a plain token sweep rather than a parse of `env::var` call
    sites: a variable read through a helper, named in a generated file, or
    exported by a shell script counts just as much as one read inline, and the
    cost of the loose reading is that this rung cannot catch a variable that is
    only ever *mentioned* in a comment. Rung (a) covers everything in the
    schema; this rung exists for the four subsystems outside it.
    """
    out = subprocess.run(['git', 'ls-files', '-z'], cwd=root,
                         capture_output=True, text=True).stdout
    tokens = set()
    for rel in out.split('\0'):
        if not rel or rel.endswith('.md'):
            continue
        try:
            body = (root / rel).read_text(encoding='utf-8', errors='replace')
        except OSError:
            continue
        if 'AUTUMN_' in body:
            tokens.update(VAR.findall(body))
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


def resolve(var, leaves, leaf_maps, tokens):
    """Return the rung that resolves `var`, or None. Order is cost, not rank."""
    if is_config_form(var):
        path = to_path(var)
        if path in leaves:
            return 'schema'
        segs = path.split('.')
        for k in range(1, len(segs)):
            if '.'.join(segs[:k]) in leaf_maps:
                return 'dynamic-map'
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

def scan(files, read, leaves, leaf_maps, tokens):
    stats, defects = collections.Counter(), []
    for rel in files:
        text = read(rel)
        lines = text.splitlines()
        chosen = {m for pat in CHOSEN for m in pat.findall(text)}
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
                rung = resolve(var, leaves, leaf_maps, tokens)
                if rung:
                    stats[rung] += 1
                elif at[i] in waived.get(var, ()):
                    stats['waived'] += 1
                else:
                    defects.append((rel, i, var, line.strip()))
    return stats, defects


# --------------------------------------------------- module-doc table scan

TABLE_ROW = re.compile(r'^//! \| `(AUTUMN_[A-Z0-9_{}]+)` \| `([^`]+)` \|')


def table_rows(text):
    return [(i, m.group(1), m.group(2))
            for i, line in enumerate(text.splitlines(), 1)
            if (m := TABLE_ROW.match(line))]


def check_table(rows, leaves, leaf_maps):
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
        known = path in leaves or any('.'.join(segs[:k]) in leaf_maps
                                      for k in range(1, len(segs)))
        if not known:
            out.append((i, var, declared, 'path is not in the schema'))
            continue
        derived = to_path(var)
        # The variable may address a shorter path than the row documents:
        # `AUTUMN_SECURITY__SIGNING_SECRET` sets `security.signing_secret`,
        # whose untagged deserializer accepts a bare string, and the row names
        # the field that string lands in (`…signing_secret.secret`). A prefix is
        # therefore agreement; anything else is a row edited on one side only.
        if not (derived == path or path.startswith(derived + '.')):
            out.append((i, var, declared,
                        f'variable derives `{derived}`, row says `{path}`'))
    return out


# ------------------------------------------------------------------- modes

def load():
    leaves = schema_leaves(SNAPSHOT.read_text(encoding='utf-8'))
    return leaves, map_sections(leaves), source_tokens(ROOT)


def main():
    if not SNAPSHOT.exists():
        print(f'error: schema snapshot missing at {SNAPSHOT}', file=sys.stderr)
        return 2
    leaves, leaf_maps, tokens = load()
    files = corpus(ROOT)
    read = lambda rel: (ROOT / rel).read_text(encoding='utf-8', errors='replace')
    stats, defects = scan(files, read, leaves, leaf_maps, tokens)

    rows = table_rows(CONFIG_RS.read_text(encoding='utf-8'))
    table_defects = check_table(rows, leaves, leaf_maps)

    print(f'corpus: {len(files)} reader-facing markdown files')
    print(f'surface: {len(leaves)} schema leaves '
          f'({len(leaf_maps)} of them reader-keyed maps), '
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

    total = len(defects) + len(table_defects)
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
    leaves, leaf_maps, tokens = load()
    for leaf in sorted(leaves):
        print(f'{leaf}{" (reader-keyed map)" if leaf in leaf_maps else ""}')
    print(f'\n{len(leaves)} schema leaves; '
          f'{len(tokens)} AUTUMN_* variables in the non-markdown tree')
    return 0


# --------------------------------------------------------------- self-test

def self_test():
    leaves = schema_leaves(
        'log\nlog.level\nauth\nauth.oauth2\ndatabase\ndatabase.shards\n'
        'database.shards.name\nsecurity\nsecurity.signing_secret\n'
        'security.signing_secret.secret\n')
    maps = {'auth.oauth2'}
    tokens = {'AUTUMN_ENV', 'AUTUMN_SEARCH__QUEUE'}
    checked, failures = [], []

    def case(name, got, want):
        checked.append(name)
        if got != want:
            failures.append(f'{name}: got {got!r}, want {want!r}')

    r = lambda v: resolve(v, leaves, maps, tokens)
    case('plain key resolves', r('AUTUMN_LOG__LEVEL'), 'schema')
    case('missing key fails', r('AUTUMN_LOG__LEVL'), None)
    # The single-underscore spelling is the whole point of the gate: it derives
    # a one-segment path that no section can match.
    case('single underscore fails', r('AUTUMN_DATABASE_URL'), None)
    case('sequence index elided',
         r('AUTUMN_DATABASE__SHARDS__0__NAME'), 'schema')
    case('table placeholder elided',
         r('AUTUMN_DATABASE__SHARDS__{i}__NAME'), 'schema')
    case('unknown shard field fails',
         r('AUTUMN_DATABASE__SHARDS__0__NOPE'), None)
    case('reader-keyed map resolves',
         r('AUTUMN_AUTH__OAUTH2__GITLAB__CLIENT_SECRET'), 'dynamic-map')
    # `database.shards` has children, so it is a sequence and not a map: an
    # unknown field under it must NOT be swallowed by the map rung.
    case('sequence is not a map', r('AUTUMN_DATABASE__NOPE'), None)
    # A scalar leaf has no children either. Treating "childless" as "map" — the
    # first cut of this rung — resolved anything under any scalar and would
    # have hidden drift beneath all 397 of them.
    case('scalar is not a map', r('AUTUMN_LOG__LEVEL__NOPE'), None)
    case('standalone from source', r('AUTUMN_ENV'), 'source')
    case('standalone unknown fails', r('AUTUMN_NOPE'), None)
    case('other crate from source', r('AUTUMN_SEARCH__QUEUE'), 'source')
    case('trailing separator is not a key claim',
         is_config_form('AUTUMN_SESSION__'), False)
    case('path derivation', to_path('AUTUMN_A__B_C'), 'a.b_c')

    # Waiver scope: covers its own block and the one above, nothing further.
    doc = ('AUTUMN_BAD__ONE\n\nprose\n<!-- config-key-allow: AUTUMN_BAD__ONE — why -->\n'
           '\nAUTUMN_BAD__ONE\n')
    stats, defects = scan(['d.md'], lambda _: doc, leaves, maps, tokens)
    case('waiver covers block above and its own', stats['waived'], 1)
    case('waiver does not reach further', len(defects), 1)

    doc2 = 'AUTUMN_BAD__ONE\n<!-- config-key-allow: AUTUMN_BAD__ONE -->\n'
    _, d2 = scan(['d.md'], lambda _: doc2, leaves, maps, tokens)
    case('waiver without a reason does not waive', len(d2), 1)

    doc3 = 'key_env = "AUTUMN_WHATEVER"\nexport AUTUMN_WHATEVER=x\n'
    s3, d3 = scan(['d.md'], lambda _: doc3, leaves, maps, tokens)
    case('reader-chosen name resolves', (s3['reader-chosen name'], len(d3)), (2, 0))

    # The listed maps must stay maps, or the rung starts hiding drift.
    def raises(leafset):
        try:
            map_sections(leafset)
            return False
        except KeyError:
            return True
    real = schema_leaves(SNAPSHOT.read_text(encoding='utf-8')) \
        if SNAPSHOT.exists() else set(READER_KEYED)
    case('listed maps are childless leaves in the real snapshot',
         raises(real), False)
    case('a listed map that vanished is caught',
         raises(real - {'jobs.queues'}), True)
    case('a listed map that gained children is caught',
         raises(real | {'jobs.queues.default'}), True)

    good = [(1, 'AUTUMN_LOG__LEVEL', 'log.level'),
            (2, 'AUTUMN_SECURITY__SIGNING_SECRET', 'security.signing_secret.secret'),
            (3, 'AUTUMN_DATABASE__SHARDS__{i}__NAME', 'database.shards[i].name')]
    case('sound table rows pass', check_table(good, leaves, maps), [])
    case('table row with a dead path fails',
         len(check_table([(9, 'AUTUMN_LOG__NOPE', 'log.nope')], leaves, maps)), 1)
    case('table row edited on one side fails',
         len(check_table([(9, 'AUTUMN_LOG__LEVEL', 'auth.oauth2')], leaves, maps)), 1)

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
