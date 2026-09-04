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
# The reader-facing corpus names 658 `AUTUMN_*` variables across 175 pages. This
# gate resolves every one of them.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Every `AUTUMN_*` name a page tells someone to set must be one the
#      runtime actually reads — whether it is layered into the config
#      (`AUTUMN_LOG__LEVEL`) or read directly (`AUTUMN_ENV`).
#   2. The hand-maintained `AUTUMN_* -> config path` table in `config.rs`'s
#      module docs — 142 rows, the mapping readers meet on docs.rs — must agree
#      with the schema, both in the paths it names and in the variable spellings
#      it derives them from. Every row shaped like a mapping is accounted for:
#      one this script cannot parse is REPORTED, not skipped.
#
# TRUTH SETS (all already in the tree; nothing to regenerate here):
#   - `autumn/tests/fixtures/schema_keys.snapshot` — 484 config paths of
#     `AutumnConfig` (397 of them leaves), used to bound the open-ended shard
#     template and to check the module-doc table's declared paths. Produced by
#     the same `get_schema_keys()` walk that backs
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
# RESOLUTION — the question is "does anything READ this name", never "is there a
# config key spelled like it". Those are different sets: the env layer is
# written field by field (`parse_env(env, "AUTUMN_LOG__LEVEL", …)`), so a TOML
# key with no override of its own has no environment spelling at all.
# `openapi.enabled` is a real schema leaf and `AUTUMN_OPENAPI__ENABLED` is read
# by nothing; 90 of the 397 leaves are in that position. Resolving against the
# schema blessed every one of them, so the schema no longer answers this
# question — it bounds the shard template and checks the module-doc table's
# declared PATHS, which are questions about config keys.
#
#   a. The name is one the tracked non-markdown tree BINDS or READS —
#      `const CANARY_ENV: &str = "AUTUMN_CANARY"`, or an env accessor. This
#      carries the bulk, including the four subsystems outside `AutumnConfig`.
#   b. The name matches one the runtime BUILDS —
#      `format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_ID")`. The filled-in
#      segment is open and the rest is exact, which is the runtime's own
#      behaviour: it probes only the names it builds, so `…__CLIENT_SECRT` is
#      ignored in production and is reported here. A template whose FINAL
#      segment is a placeholder (`…SHARDS__{i}__{field}`, a closure parameter)
#      is bounded by the schema's children of the path it addresses.
#   c. The page introduces the name as one the READER chooses —
#      `access_key_id_env = "AUTUMN_OFFSITE_ACCESS_KEY_ID"`, or
#      `InMemoryApiTokenStore::from_env("AUTUMN_API_TOKEN", …)`.
#   d. The sentence names a FAMILY rather than a variable (`AUTUMN_ALERTS__*`,
#      `AUTUMN_MEDIA__<TABLE>__<FIELD>`), a naming-rule example
#      (`AUTUMN_SECTION__FIELD`), or an identifier the page declares in its own
#      example code.
#
# A name that is malformed — lower case outside a placeholder, or a dangling
# separator with no `*` or `<PLACEHOLDER>` after it — is REPORTED rather than
# resolved. Both spellings used to match nothing at all, which made the claim
# invisible: an unresolved name is reported, an unseen one cannot be.
#
# HOW THIS CHECK HAS FAILED BEFORE, so the next change does not repeat it. Every
# defect found in review has been one of these, and none was caught by the gate
# being green:
#
#   * A rule generalised past the single case that motivated it. An exception
#     written for one real row became "any prefix"; a map section became "any
#     suffix". Enumerate the exception; do not widen the rule.
#   * Normalisation discarding information before validation. Erasing every
#     `[index]` made `shards[i][i]` indistinguishable from `shards[i]`; letting a
#     placeholder match `_` made it swallow `0__NOPE`. Validate the shape FIRST,
#     then canonicalise.
#   * A name that matches NOTHING, rather than matching wrongly. A casing typo,
#     a trailing separator, a `{i}` placeholder, an escaped quote and a misspelt
#     NAMESPACE (`AUTMN_LOG__LEVEL`) each made a claim invisible — and an
#     unresolved name is reported, while an unseen one cannot be. When
#     tightening a pattern, check what it stops seeing.
#   * Proximity used as a proxy for use. "Is there an accessor near this string"
#     reported 25 correct pages once and would have dropped six real bindings
#     another time. Prefer a structural signal — the binding form, the naming
#     convention — and measure it before relying on it.
#   * A fix applied where the bug was found rather than everywhere the same
#     question is asked. The placeholder lived in two regexes; the schema-vs-read
#     question lived in the resolver and the table checker; "a comment is not
#     code" was fixed in the template reader while the token sweep still read
#     `// std::env::var("AUTUMN_LOG__LEVL")` as a read. Grep for the rule.
#   * A rule about the LAYOUT of the text standing in for a rule about its
#     structure. "The comment starts the line", "the `}` is in column zero",
#     "the `#[cfg(test)]` is not indented" — each held for the cases in front of
#     me and broke on the first file laid out differently, in both directions:
#     an attribute inside a generated-code template masked a whole file, and a
#     brace inside a Rust fixture ended a mask 8,000 lines early. "The cfg
#     mentions `test`" masked `any(test, feature = "mail")`, which is production
#     code whenever `mail` is on. "A block comment ends at `*/`" forgot that
#     Rust's nest. Parse enough to ask the real question — here, one scan that
#     says which characters are code, string and comment, and an evaluation of
#     the cfg predicate rather than a match against its text.
#   * A source of coverage removed without checking what was leaning on it.
#     Masking test code is right, and `AUTUMN_MEDIA__FFMPEG__BIN` was in the
#     truth set ONLY through a `${…}` expansion inside a test — so the correct
#     tightening would have reported a correct page, had measuring it not turned
#     up the real read (`override_string`) the accessor list was missing. When a
#     rung stops carrying something, look at what it was carrying first.
#   * FAILING OPEN — the check not running at all. Every other entry here is the
#     gate resolving something it should have reported; this one is the gate
#     asking nothing. Bounding rows to a table region meant a cosmetic header
#     rename turned off all 142 mapping checks with a green result, because
#     "no defects found" and "no checks run" are indistinguishable from outside.
#     It then recurred at the other end of the same region: a late row that lost
#     its leading pipe read as the end of the table, so 15 mappings went
#     unchecked at 127 rows — over the floor, and green. Anything that scopes a
#     check must assert it found its subject AND that it reached the end of it.
#
# EVERY RUNG HAS NOW BEEN AUDITED against the list above, rather than tightened
# one at a time as a reviewer found it — which is how five of these survived
# into later rounds. Where each stands:
#
#   * built templates — from `format!(` construction sites only.
#   * const bindings — must be NAMED as env bindings (`*_ENV`/`ENV_*`/`VAR`).
#   * quoted names — need an env accessor nearby, never a negative assertion,
#     and never inside test code. The accessor list is the STATIC one below plus
#     every env helper declared in the tree, read out of it like the templates.
#   * shell assignments — must reach a process (`export`, or a prefix form), and
#     a file that assigns a name owns its own expansions of it.
#   * family wildcards — the prefix must begin a real name.
#   * naming-rule examples — bounded to the root `section`, which is absent from
#     the schema and cannot become a root; all 8 uses are the two literal
#     placeholders.
#   * reader-chosen names and example-code identifiers — page-scoped: a page
#     vouches for a name it declares. This is deliberate, since the reader
#     genuinely invents these (`access_key_id_env = "…"`), and audited: all 9
#     current declarations are S3 credential names and the one const is
#     `AUTUMN_SOURCE`, none a near-miss of a framework variable. The residual
#     exposure is that a page could mask a typo by declaring it, which is
#     narrower than a blanket exemption and is what the `*_env` key signals.
#
# The self-tests assert on each STEP — detect, scan, classify, resolve — because
# a test that checks only the final verdict cannot tell "handled correctly" from
# "never seen", and that gap let several of the above survive a round each.
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
VAR = re.compile(r'\bAUTUMN_(?:\{[a-z]+\}|[A-Za-z0-9_])*[A-Za-z0-9_]')

# A trailing `_` is part of the token, so `export AUTUMN_LOG__LEVEL_=debug` is
# captured and reported rather than skipped — requiring an alphanumeric last
# character made that whole assignment match nothing at all. Prose wildcards
# (`AUTUMN_UPGRADE_*`, naming a family rather than a variable) are the reason
# the trailing separator cannot simply be trimmed: trimming would silently
# rewrite a typo into the valid name next to it.
TRAILING_WILDCARD = re.compile(r'^AUTUMN_[A-Za-z0-9_]*_$')


def family_exists(var, built, tokens):
    """Some real variable must begin with this prefix.

    Without it a wildcard was a blanket exemption: `AUTUMN_SESION__*` — one `S`
    short — was accepted as a family mention and skipped, so the shape `*`
    excused any spelling in front of it. The prefix is a claim about a family
    of variables, and it is checked like any other claim.

    It must also END where a family ends. Plain `startswith` accepted
    `AUTUMN_SESSION_*` — one underscore short of the documented
    `AUTUMN_SESSION__*` — because real names do begin with those characters,
    and it would have accepted `AUTUMN_A*` for the same reason. So the mention's
    separator run has to be the one the real name uses at that position: both
    depths occur here, `AUTUMN_SESSION__*` in the config namespace and
    `AUTUMN_ACME_DNS_*` in the flat one, across 14 distinct mentions.
    """
    head = var.rstrip('_')
    seps = len(var) - len(head)
    if not seps:
        return False
    names = [t for t in tokens]
    names += [p.pattern.lstrip('^') for p in built.values()]
    for t in names:
        if not t.startswith(head):
            continue
        rest = t[len(head):]
        if rest[:seps] == '_' * seps and rest[seps:seps + 1] != '_':
            return True
    return False


def family(var, line):
    """`AUTUMN_ALERTS__*` — prose naming a family, not a variable to set.

    Written this way on 22 reader-facing lines: 19 with a `*` ("every key has an
    `AUTUMN_SESSION__*` environment override") and 3 with an angle-bracket
    stand-in for the part the reader supplies (`AUTUMN_MEDIA__<TABLE>__<FIELD>`,
    `AUTUMN_ALERTS__<KEY>`).

    What follows the trailing separator is the whole distinction: a `*` or a
    `<PLACEHOLDER>` means the sentence is describing a family, while nothing at
    all means `AUTUMN_LOG__LEVEL_=debug` — the same shape with a dangling
    separator, and a typo the runtime will not read.
    """
    return bool(re.search(re.escape(var) + r'(?:\*|<[A-Z_]+>)', line))


def malformed(var):
    """A name the runtime cannot read: wrong case, or a dangling separator."""
    bare = re.sub(r'\{[a-z]+\}', '', var)
    return any(c.islower() for c in bare) or bool(TRAILING_WILDCARD.match(var))

# A path segment that is a sequence index rather than a field name: a literal
# index as written in a shell line, or the `{i}` / `{N}` placeholder the
# module-doc table uses for the same position.
INDEX_SEG = re.compile(r'^(?:\d+|\{\w+\}|N|I)$')

# The two shapes in which a page hands the reader a variable name to invent:
# a `*_env` config key whose value IS the name, and `from_env("NAME", …)`.
CHOSEN = (re.compile(r'_env\s*=\s*"(AUTUMN_[A-Z0-9_]+)"'),
          re.compile(r'from_env\(\s*"(AUTUMN_[A-Z0-9_]+)"'))

WAIVER = re.compile(r'<!--\s*config-key-allow:\s*([A-Z][A-Z0-9_]+)\s*(.*?)\s*-->')

# A misspelling of the NAMESPACE itself matches nothing above, so the claim is
# invisible rather than unresolved — the failure this script keeps relearning.
# `export AUTMN_LOG__LEVEL=debug` left the occurrence count unmoved and the gate
# green, though the runtime ignores it exactly as it ignores `AUTUMN_LOG__LEVL`.
#
# A near miss is one edit from `AUTUMN` and is not `AUTUMN`: substitution
# (`AUTUNM`), deletion (`AUTMN`) or insertion (`AUTUUMN`). Anything further away
# is somebody else's variable — `DATABASE_URL` and `RUST_LOG` are not typos of
# this namespace — and two edits would start reaching them.
NEAR = re.compile(r'\b([A-Z][A-Z0-9]{3,8})_(?=[A-Z0-9_]*[A-Z0-9])[A-Z0-9_]+\b')


def near_miss(word, target='AUTUMN'):
    """One insertion, deletion, substitution or transposition from `target`."""
    if word == target or abs(len(word) - len(target)) > 1:
        return False
    if len(word) == len(target):
        if sum(a != b for a, b in zip(word, target)) == 1:
            return True
        # A swap of two adjacent letters is two substitutions and one typo:
        # `AUTUNM` is the shape a human actually types.
        return any(word[:k] + word[k + 1] + word[k] + word[k + 2:] == target
                   for k in range(len(word) - 1))
    longer, shorter = ((word, target) if len(word) > len(target)
                       else (target, word))
    return any(longer[:k] + longer[k + 1:] == shorter
               for k in range(len(longer)))

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
# Only a CONSTRUCTION SITE counts. Matching any quoted string with a
# placeholder made a comment or a test fixture into runtime truth: a stray
# `"AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_SECRT"` anywhere in the tree would have
# blessed that typo for the whole corpus. All five real templates are arguments
# to `format!`, which is what actually builds a name at load time.
TEMPLATE = re.compile(r'format!\(\s*"(AUTUMN_[A-Z0-9_]*(?:\{[a-z_]+\}[A-Z0-9_]*)+)"')

# Metasyntactic roots: a page teaching the NAMING RULE rather than naming a key.
# Seven pages write `AUTUMN_SECTION__FIELD` to explain that a double underscore
# separates section from field, and `docs/migrations/TEMPLATE.md` writes
# `AUTUMN_SECTION__OLD_KEY` in the row an author fills in per release. `section`
# is not a config root and cannot become one, so everything under it is a worked
# example by construction — which is why this is a rule rather than eight waiver
# markers repeating the same reason on eight pages.
PLACEHOLDER_ROOTS = ('section',)


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

    The snapshot records every node the walk visits, branches included. Used to
    bound the open-ended shard template and to check the module-doc table's
    declared paths — NOT to decide whether an environment variable is settable,
    which is a question about what the runtime reads.
    """
    return {p for p in paths if not any(o.startswith(p + '.') for o in paths)}


# One segment the runtime fills in: an index, a provider name, or — in a page
# documenting the whole family at once — the `{i}` placeholder itself.
# One segment the runtime fills in. It may contain a single `_` (a provider
# name like `my-idp` uppercases to `MY_IDP`) but never `__`, which is the
# separator between segments — without that bound the shard placeholder
# swallowed `0__NOPE`, so `…SHARDS__0__NOPE__NAME` matched a name the runtime
# builds from an integer and never reads.
SEGMENT = r'(?:[A-Z0-9]+(?:_[A-Z0-9]+)*|\{[a-z]+\})'


# A comment is prose ABOUT the code, never the code. Requiring `format!(` was an
# earlier fix for a bare quoted string and still read
# `// format!("AUTUMN_…__CLIENT_SECRT")` as a name the runtime builds; stripping
# comments only there left `// std::env::var("AUTUMN_LOG__LEVL")` entering the
# truth set through `source_tokens` — the same defect, found once and fixed in
# one place. Both readers of the tree strip comments now.
#
# EVERY comment goes, not only a whole-line one. Restricting it to whole lines
# was a way of never mistaking a `//` inside a string literal for a comment, and
# it left `const _: () = (); // std::env::var("AUTUMN_LOG__LEVL")` reading as a
# read. The right answer to "is this `//` inside a string" is to know where the
# strings are, so the Rust reader below is a small scanner over string, raw
# string and block-comment state rather than a line rule. It is worth the
# machinery: `tauri_mobile.rs` asserts on a raw string that CONTAINS a
# commented-out `set_var("AUTUMN_SYNC__TOKEN")`, which a naive truncate-at-`//`
# would have blanked — a real read dropped, reporting a correct page.
#
# The leader is per file type and deliberately not one pattern: `#` opens a
# comment in TOML, YAML and shell, but a Rust ATTRIBUTE and a markdown HEADING.
# Keyed on the effective suffix, so `Cargo.toml.tmpl` is read as TOML and
# `README.md.tmpl` as markdown (no leader, nothing stripped). A type not listed
# here keeps every line.
COMMENT_LEADER = {
    '.rs': '//', '.ts': '//', '.js': '//',
    '.sh': '#', '.bash': '#', '.ps1': '#',
    '.toml': '#', '.yml': '#', '.yaml': '#', '.example': '#',
}

# Some file types are named rather than suffixed. `Dockerfile.api.tmpl` strips
# to `Dockerfile.api`, whose "suffix" is `.api` — so the whole family was
# getting no comment handling at all, and its commented `--build-arg
# AUTUMN_BUILD_*=…` examples were reading as source truth.
COMMENT_LEADER_NAMED = {'Dockerfile': '#', 'Makefile': '#', 'Justfile': '#'}


def comment_leader(rel):
    p = pathlib.PurePath(rel)
    if p.suffix == '.tmpl':
        p = pathlib.PurePath(p.stem)
    named = COMMENT_LEADER_NAMED.get(p.name.split('.')[0])
    return named if named else COMMENT_LEADER.get(p.suffix)


def _rust_classes(body):
    """Classify every character: `c`ode, co`m`ment, or `s`tring.

    One scan answers both questions this script asks of Rust source — which
    text is a comment, and which braces are real — and the second is why the
    classification is kept rather than a stripped string: a `}` inside a string
    literal is not a closing brace, and reading one as if it were is what ended
    a `#[cfg(test)]` mask 8,000 lines early.

    Character literals ARE tracked, and my reasoning for skipping them was
    wrong. A `'…'` cannot hold `//` or `/*` — true, and not the point: it can
    hold a `"`, and `dotenv.rs:189` writes `quote == b'"'`, which opened a
    string that ran to the next quote and left the rest of that file classified
    as string. A lifetime is still left alone, because the two are actually
    distinguishable: `'a` is never followed by a closing quote.
    """
    cls = ['c'] * len(body)
    i, n, state, hashes, depth = 0, len(body), None, 0, 0
    while i < n:
        c = body[i]
        if state is None:
            if c == '/' and body[i + 1:i + 2] == '/':
                while i < n and body[i] != '\n':
                    cls[i] = 'm'
                    i += 1
                continue
            if c == '/' and body[i + 1:i + 2] == '*':
                cls[i] = cls[i + 1] = 'm'
                state, depth, i = 'block', 1, i + 2
                continue
            if c == '"':
                cls[i] = 's'
                state, i = 'str', i + 1
                continue
            if c == 'r':
                j = i + 1
                while body[j:j + 1] == '#':
                    j += 1
                if body[j:j + 1] == '"':
                    cls[i:j + 1] = 's' * (j + 1 - i)
                    state, hashes, i = 'raw', j - i - 1, j + 1
                    continue
            if c == "'":
                # Skip a char literal whole. An escape is short and ends at the
                # next quote; a plain one is three characters; anything else is
                # a lifetime, which needs no handling.
                if body[i + 1:i + 2] == '\\':
                    j = body.find("'", i + 2)
                    if 0 <= j - i <= 12:
                        i = j + 1
                        continue
                elif body[i + 2:i + 3] == "'":
                    i += 3
                    continue
            i += 1
        elif state == 'block':
            cls[i] = 'm'
            # Rust block comments NEST: `/* a /* b */ c */` closes once, at the
            # end. Leaving at the first `*/` handed `c` back as code.
            if c == '/' and body[i + 1:i + 2] == '*':
                cls[i + 1] = 'm'
                depth, i = depth + 1, i + 2
                continue
            if c == '*' and body[i + 1:i + 2] == '/':
                cls[i + 1] = 'm'
                depth, i = depth - 1, i + 2
                if depth == 0:
                    state = None
                continue
            i += 1
        elif state == 'str':
            cls[i] = 's'
            if c == '\\' and i + 1 < n:
                cls[i + 1] = 's'
                i += 2
                continue
            if c == '"':
                state = None
            i += 1
        else:
            cls[i] = 's'
            if c == '"' and body[i + 1:i + 1 + hashes] == '#' * hashes:
                cls[i:i + 1 + hashes] = 's' * (1 + hashes)
                state, i = None, i + 1 + hashes
                continue
            i += 1
    return cls


def _rust_uncommented(body):
    """Drop `//` and `/* … */` comments, keeping strings and line numbering."""
    return ''.join(c for c, k in zip(body, _rust_classes(body))
                   if k != 'm' or c == '\n')


def _rust_skeleton(body):
    """The same text with comments AND string contents blanked to spaces.

    Length-preserving, so an offset in the skeleton is the same offset in the
    body — which is what makes brace matching on it usable for masking.
    """
    return ''.join(c if k == 'c' or c == '\n' else ' '
                   for c, k in zip(body, _rust_classes(body)))


def _hash_uncommented(body):
    """Drop `#` comments in shell, TOML and YAML — but only where one starts.

    A `#` opens a comment at the start of a word, so `${VAR#prefix}` and `$#`
    keep their meaning. Quote state is tracked per line rather than across the
    file: an unbalanced quote then costs one line, not the rest of the file.
    """
    out = []
    for l in body.splitlines():
        q, cut = None, None
        for i, c in enumerate(l):
            if q:
                if c == q:
                    q = None
            elif c in '"\'':
                q = c
            elif c == '#' and (i == 0 or l[i - 1].isspace()):
                cut = i
                break
        out.append(l if cut is None else l[:cut])
    return '\n'.join(out)


def uncommented(body, leader='//'):
    """Drop comments, keeping string literals and line numbering intact."""
    if leader == '//':
        return _rust_uncommented(body)
    if leader == '#':
        return _hash_uncommented(body)
    return body


def built_patterns(root, leaves):
    """Regexes for the variable names the runtime constructs at load time.

    Each `{placeholder}` becomes one segment the runtime fills in. A template
    whose FINAL segment is a placeholder would otherwise fix nothing — `key` in
    `format!("AUTUMN_DATABASE__SHARDS__{i}__{field}")` is a closure parameter
    applied to literal field names — so its final segment is constrained to the
    schema's children of the path it addresses. That is how
    `…SHARDS__{i}__SLOTS` resolves while `…SHARDS__0__NOPE` does not, without
    this script holding a list of shard fields.
    """
    out = subprocess.run(['git', 'ls-files', '-z', '*.rs'], cwd=root,
                         capture_output=True, text=True).stdout
    pats = {}
    for rel in out.split('\0'):
        # Same two exclusions as the token sweep, for the same reason and in the
        # same order: a comment is not code, and a name a test builds is not a
        # name the runtime builds. Applied here as well because splitting a rule
        # across the two readers of the tree is how the last two rounds' defects
        # got in. Measured: the five templates are unchanged by it.
        if not rel or TEST_PATH.search(rel):
            continue
        try:
            body = (root / rel).read_text(encoding='utf-8', errors='replace')
        except OSError:
            continue
        # Every template starts with the literal `AUTUMN_`, so a file without it
        # cannot hold one — and the comment scanner is per character, so not
        # running it over the other 3,000 Rust files is most of this gate's
        # runtime.
        if 'AUTUMN_' not in body:
            continue
        for tpl in TEMPLATE.findall(untested(uncommented(body))):
            segs = tpl[len('AUTUMN_'):].split('__')
            head = re.sub(r'\\\{[a-z_]+\\\}', SEGMENT,
                          re.escape('AUTUMN_' + '__'.join(segs[:-1])))
            if not segs[-1].startswith('{'):
                pats[tpl] = re.compile(
                    '^' + re.sub(r'\\\{[a-z_]+\\\}', SEGMENT, re.escape(tpl))
                    + '$')
                continue
            prefix = '.'.join(s.lower() for s in segs[:-1]
                              if not s.startswith('{'))
            kids = {p.rsplit('.', 1)[1] for p in leaves
                    if p.startswith(prefix + '.')
                    and p.count('.') == prefix.count('.') + 1}
            if kids:
                pats[tpl] = re.compile(
                    '^' + head + '__('
                    + '|'.join(sorted(k.upper() for k in kids)) + ')$')
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
# The quote may be escaped: `autumn-cli/src/generate/admin.rs` emits the
# reader's test file as a Rust string, so the read inside it is written
# `std::env::var(\"AUTUMN_TEST_ADMIN_SESSION\")`. Requiring a bare quote missed
# every name in a generated code template — a real read, in the source that
# writes the reader's project.
QUOTED = re.compile(r'\\?["\'](AUTUMN_[A-Z0-9_]+)\\?["\']')
# A shell assignment counts when it reaches a process: `export NAME=…`, or the
# prefix form `NAME=… some-command`. A bare `NAME=…` on its own line is a
# script-local variable — `scripts/check-panic-gate.sh` sets
# `AUTUMN_MANIFEST="autumn/Cargo.toml"` as a path it reads itself, and that was
# blessing `AUTUMN_MANIFEST` as application configuration. The names this drops
# that ARE real (`AUTUMN_LOG__LEVEL`, `AUTUMN_SERVER__HOST`, …) resolve through
# the source rung instead, which is where their evidence actually lives.
ASSIGNED = re.compile(r'(?:^|[;&|(\s])export\s+(AUTUMN_[A-Z0-9_]+)=')
# The separator here is explicitly NOT a newline: `\s` spans line breaks, so
# applied to a whole file this matched a bare assignment against the first
# word of the NEXT line and read it as a prefix form.
ASSIGNED_PREFIX = re.compile(
    r'(?:^|[;&|(\s])(AUTUMN_[A-Z0-9_]+)=\S*[^\S\n]+\S')

# A file that assigns a name to itself OWNS it: `check-panic-gate.sh` sets
# `AUTUMN_MANIFEST="autumn/Cargo.toml"` and later reads `"$AUTUMN_MANIFEST"`,
# which is a script-local variable being used, not evidence that the
# application reads one. Expansions of a locally-assigned name are therefore
# ignored in the file that assigns it — while `${AUTUMN_MEDIA__FFMPEG__BIN}`,
# which nothing assigns, still counts wherever it appears.
ASSIGNED_ANY = re.compile(r'(?:^|[;&|(\s])(?:export\s+)?(AUTUMN_[A-Z0-9_]+)=')

# A Dockerfile DECLARES a variable with `ARG` or `ENV`, and `ARG AUTUMN_BUILD_
# GIT_SHA=` — an empty default — matches none of the shell shapes above. Added
# alongside stripping Dockerfile comments rather than after it: the commented
# `--build-arg` examples were the only thing carrying five of these names, and
# removing that cover without the declaration would have reported correct pages.
DECLARED = re.compile(r'^\s*(?:ARG|ENV)\s+(AUTUMN_[A-Z0-9_]+)')
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
# A constant binding a variable name — and the constant must be NAMED as one.
# A string starting with `AUTUMN_` is not automatically a variable:
# `const NONCE_PLACEHOLDER: &str = "AUTUMN_CSP_NONCE"` is a token substituted
# into a CSP template, with no env accessor anywhere, and accepting every
# binding made it settable in the docs.
#
# The repo names these by convention and does so consistently — every binding of
# a real variable is `*_ENV` or `ENV_*`.
#
# `VAR` alone is NOT accepted, though a binding by that name exists: it is
# `const VAR: &str = "AUTUMN_TEST_MCP_TOKEN_1970_UNSET"` in `auth.rs`, a
# deliberately-unset test sentinel whose own assertion is that nothing reads it.
# I had included it when auditing this rung, reading the name as conventional
# rather than checking what it bound. Preferred over "the constant is used near an env
# accessor", which was measured and is too strict: six real ones
# (`ENV_API_TOKEN`, `REPLAY_CAPSULE_ENV`, …) reach the accessor through a helper
# and would have been dropped, reporting correct pages. A future binding named
# outside the convention reports a correct page rather than hiding a wrong one,
# which is the safer direction for this rung to fail in.
BOUND = re.compile(
    r'\b(?:const|static)\s+((?:\w+_)?ENV(?:_\w+)?)\s*:\s*&\s*'
    r'(?:\'\w+\s+)?str\s*=\s*"(AUTUMN_[A-Z0-9_]+)"')

# The accessors that read or write the environment. `with` is deliberately NOT
# here: it supplies a fixture environment, which is how a test names a variable
# that does not exist.
# `get` is qualified by its receiver, because every collection has one:
# `map.get("AUTUMN_LOG__LEVL")` in a test was reading as an environment read,
# while `env.get(…)` in the media plugin is a real one. The other accessors are
# specific enough to stand alone.
ACCESSOR = re.compile(r'\b(?:var|var_os|set_var|remove_var|env_trimmed'
                      r'|parse_env\w*|env_bool\w*|getenv)\s*\('
                      r'|\benv\w*\.get\s*\(')

# …plus the crates' own env helpers, READ OUT OF THE TREE rather than listed
# here, the same way the runtime's `format!` templates are. A helper takes an
# environment and a key: `fn override_string(target: &mut String, env: &HashMap<
# String, String>, key: &str)`. The static list above is the floor, so a helper
# this misses costs coverage and never correctness.
#
# Found by measuring, not by the one report that exposed it: the media plugin's
# `override_string` / `override_opt` and `arroyo_value` carry SEVENTEEN real
# variables (`AUTUMN_MEDIA__STORAGE__*`, `AUTUMN_MEDIA__MEDIAMTX__*`) that the
# gate did not know the runtime reads. `AUTUMN_MEDIA__FFMPEG__BIN` was in the
# truth set only by accident — through a `${…}` expansion in a test — so
# masking test code without this would have reported a correct page.
ENV_HELPER = re.compile(r'\bfn\s+(\w+)\s*\([^)]*\benv\s*:\s*&[^)]*'
                        r'\bkey\s*:\s*&str', re.S)


def accessor(root):
    """`ACCESSOR`, widened by the env helpers this tree declares."""
    out = subprocess.run(['git', 'ls-files', '-z', '*.rs'], cwd=root,
                         capture_output=True, text=True).stdout
    names = set()
    for rel in out.split('\0'):
        if not rel:
            continue
        try:
            names.update(ENV_HELPER.findall(
                (root / rel).read_text(encoding='utf-8', errors='replace')))
        except OSError:
            continue
    if not names:
        return ACCESSOR
    return re.compile(ACCESSOR.pattern + r'|\b(?:'
                      + '|'.join(sorted(map(re.escape, names))) + r')\s*\(')


# …and never where the line asserts the name is absent.
NEGATED = re.compile(r'assert(?:_ne)?!\s*\(\s*!|!\s*[\w.]*\bcontains\b|\bassert_ne!')

# Test code is not the runtime. A test names a variable to prove the runtime
# IGNORES it — `temp_env::with_var_unset("AUTUMN_TEST_DOTENV_OVERLAY_UNSET", …)`
# reads that name through a real accessor, and `AUTUMN_DEV` is read exactly once
# in the whole tree, inside a `#[test]` asserting it is unset. Excluding the
# `const VAR` sentinel did not close this: the same class walked in through an
# accessor instead.
#
# Safe because of what this rung IS: names in `AutumnConfig` resolve against the
# schema, so everything here is a name from OUTSIDE it. A name outside the
# schema that only test code ever mentions is a sentinel by construction.
#
# The region is the whole `#[cfg(…test…)]` ITEM, found by matching braces on the
# skeleton — where a brace inside a string literal is not a brace. Two cheaper
# rules failed first, in opposite directions: any indentation matched a
# `#[cfg(test)]` written inside a generated-code string template and masked the
# rest of the file, and column-zero-to-column-zero-`}` ended the mask at a `}`
# that opens column zero inside a multi-line Rust FIXTURE — `doctor.rs:10173`,
# some 8,000 lines before its test module actually closes, handing every test
# after it back to the truth set. Neither is a rule about braces; both were
# rules about how the text happens to be laid out.
#
# WHICH cfg is a test cfg is decided by evaluating the predicate, not by
# matching its text. "Contains `test`" masked `#[cfg(any(test, feature =
# "mail"))]`, which is ordinary production code whenever `mail` is on — 14 items
# here are that shape — and "`test` right after `all(`" would miss
# `#[cfg(all(feature = "redis", test))]`, which is genuinely test-only. Both are
# rules about the layout of the predicate rather than its meaning.
#
# The question is whether the item can compile with `test` OFF. Every other
# atom is free, so each node reports whether it can be true and whether it can
# be false, and an item is test-only when the whole predicate can never be true.
# That gives `cfg(test)`, `all(test, …)` and `all(…, test)` as test-only, and
# `any(test, …)`, `not(test)` and `feature = "test-support"` as not. Anything
# unparseable falls through as a free atom, which keeps the code.
CFG_ATTR = re.compile(r'#\[cfg\(')
TEST_PATH = re.compile(r'(^|/)(?:tests|benches)/|_test\.rs$')


def _split_top(s):
    """Split on commas that are not inside parentheses."""
    parts, depth, start = [], 0, 0
    for i, c in enumerate(s):
        if c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
        elif c == ',' and depth == 0:
            parts.append(s[start:i])
            start = i + 1
    parts.append(s[start:])
    return [p for p in parts if p.strip()]


def _cfg_truth(pred):
    """`(can be true, can be false)` for a cfg predicate with `test` false."""
    pred = pred.strip()
    m = re.match(r'^(all|any|not)\s*\((.*)\)$', pred, re.S)
    if m:
        kids = [_cfg_truth(p) for p in _split_top(m.group(2))]
        if not kids:
            return True, True
        if m.group(1) == 'not':
            return kids[0][1], kids[0][0]
        if m.group(1) == 'all':
            return all(k[0] for k in kids), any(k[1] for k in kids)
        return any(k[0] for k in kids), all(k[1] for k in kids)
    if pred == 'test':
        return False, True
    return True, True


def _balanced(s, i):
    """The text inside the parenthesis at `s[i]`, and the index after it."""
    depth = 0
    for j in range(i, len(s)):
        if s[j] == '(':
            depth += 1
        elif s[j] == ')':
            depth -= 1
            if depth == 0:
                return s[i + 1:j], j + 1
    return None, len(s)


def untested(body):
    """Blank every test-only `#[cfg(…)]` item, keeping line numbering intact."""
    if '#[cfg(' not in body:
        return body
    skel = _rust_skeleton(body)
    masked = []
    for m in CFG_ATTR.finditer(skel):
        pred, after = _balanced(skel, m.end() - 1)
        if pred is None or _cfg_truth(pred)[0]:
            continue
        depth, end = 0, None
        for i in range(after, len(skel)):
            c = skel[i]
            if c == '{':
                depth += 1
            elif c == '}':
                depth -= 1
                if depth == 0:
                    end = i
                    break
            elif c == ';' and depth == 0:
                # `#[cfg(test)] use uuid::Uuid;` — an item with no block, which
                # a brace search alone would run to the end of the file.
                end = i
                break
        masked.append((m.start(), len(skel) if end is None else end))
    if not masked:
        return body
    lines, out, pos = body.splitlines(), [], 0
    for l in lines:
        span = (pos, pos + len(l))
        out.append('' if any(a <= span[1] and span[0] <= b for a, b in masked)
                   else l)
        pos += len(l) + 1
    return '\n'.join(out)


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
    acc = accessor(root)
    tokens = set()
    for rel in out.split('\0'):
        if not rel or rel.endswith('.md') or rel == SELF:
            continue
        # A file under `tests/` or `benches/` is test code whatever it is
        # written in.
        if TEST_PATH.search(rel):
            continue
        try:
            body = (root / rel).read_text(encoding='utf-8', errors='replace')
        except OSError:
            continue
        if 'AUTUMN_' not in body:
            continue
        # Prose about a variable is not a use of it, in any of these rungs: a
        # commented `env::var`, `export`, `${…}` or `const …_ENV` is a note, and
        # the note is often about a name that is deliberately wrong. Neither is
        # a test, which names a variable to prove the runtime ignores it.
        body = uncommented(body, comment_leader(rel))
        if rel.endswith('.rs'):
            body = untested(body)
        lines = body.splitlines()
        # Names this file assigns without exporting them, and without handing
        # them to a command: its own variables.
        local = (set(ASSIGNED_ANY.findall(body))
                 - set(ASSIGNED.findall(body))
                 - set(ASSIGNED_PREFIX.findall(body)))
        for n, line in enumerate(lines):
            # `NAME=` is how a SHELL names a variable; in Rust it is just text
            # inside a string, and the text is often not an environment variable
            # at all. `autumn-cli/src/db/retention.rs` frames a line of stdout
            # with the prefix `AUTUMN_DB_RETENTION_REPORT=`, and a test fixture
            # contains that framed line verbatim — neither is an env read, and
            # both were putting the name into the truth set. `${NAME}` has no
            # such ambiguity: it is a reference wherever it appears, including
            # inside a Rust string, which is how the media plugin passes
            # `${AUTUMN_MEDIA__FFMPEG__BIN}` through to a service file.
            if not rel.endswith('.rs'):
                tokens.update(ASSIGNED.findall(line))
                tokens.update(ASSIGNED_PREFIX.findall(line))
                tokens.update(DECLARED.findall(line))
            tokens.update(v for v in EXPANDED.findall(line) if v not in local)
            tokens.update(v for _, v in BOUND.findall(line))
            if NEGATED.search(line):
                continue
            # The accessor may open a line or two above its argument —
            # `parse_env(\n    env,\n    "AUTUMN_MEDIA__ROOM_NAMESPACE",` is the
            # house style — so look back a little for it.
            if acc.search('\n'.join(lines[max(0, n - 3):n + 1])):
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
    """Return the rung that resolves `var`, or None.

    The question is "does anything READ this name", not "is there a config key
    spelled like it". Those are different sets, and conflating them was the
    gate's largest hole: the env layer is written field by field
    (`parse_env(env, "AUTUMN_LOG__LEVEL", …)`), so a TOML key with no override
    of its own has no environment spelling at all. `openapi.enabled` is a real
    key in the schema, and `AUTUMN_OPENAPI__ENABLED` is read by nothing —
    setting it leaves the value untouched. Resolving against schema leaves
    blessed that name, and 90 of the 397 leaves are in the same position.

    So the schema no longer answers this question. It still bounds the
    open-ended shard template above, and it still checks the module-doc table's
    declared PATHS, which is a question about config keys rather than about
    environment variables.
    """
    if is_config_form(var) and to_path(var).split('.')[0] in PLACEHOLDER_ROOTS:
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
            # A waiver marker names the variable in order to waive it. That
            # mention is metadata addressed to this script, not a key claim
            # addressed to a reader, so it is not an occurrence — counting it
            # made an unreasoned waiver report its own subject twice.
            line = WAIVER.sub('', line)
            # Before the well-formed names, the misspelt namespace — checked
            # first because it is invisible to every pattern that follows.
            for m in NEAR.finditer(line):
                if not near_miss(m.group(1)):
                    continue
                if at[i] in waived.get(m.group(0), ()):
                    stats['waived'] += 1
                else:
                    defects.append((rel, i, m.group(0), line.strip()))
            if 'AUTUMN_' not in line:
                continue
            for var in VAR.findall(line):
                if var in chosen:
                    stats['reader-chosen name'] += 1
                    continue
                if var in consts:
                    stats['example-code identifier'] += 1
                    continue
                if family(var, line):
                    if family_exists(var, built, tokens):
                        stats['family wildcard'] += 1
                        continue
                    defects.append((rel, i, var, line.strip()))
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
    r'^//!\s*\|\s*`(AUTUMN_(?:[A-Z0-9_]|\{[a-z]\})+)`\s*\|\s*`([^`]+)`\s*\|')

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
# A ratchet at the CURRENT count, not a loose floor. At 120 the check answered
# "is the table still being read at all", and a slack of 22 rows meant deleting
# a documented override left the gate green: 141 rows, every one of them valid,
# and a supported variable gone from the published reference without a word.
#
# Deliberately a count and not a completeness check, because there is no set to
# be complete against. `config.rs` reads 309 names and this table documents 142
# — it is a curated selection, so "every name the runtime reads must appear
# here" would be a demand for 178 new rows rather than a correctness rule. What
# a ratchet does state is that removing a row is a decision someone makes on
# purpose: raise this when rows are added, lower it only in the commit that
# takes rows away, and say why there.
TABLE_ROW_FLOOR = 142

TABLE_PROSE_ROWS = ('AUTUMN_ENV', 'AUTUMN_PROFILE')
TABLE_PROSE_ROW = re.compile(
    r'^//!\s*\|\s*`(' + '|'.join(TABLE_PROSE_ROWS) + r')`\s*\|\s*[^`\s]')

# Anything shaped like a table row at all, used to prove the two patterns above
# between them account for every one.
# A declared path: dotted segments, with at most one `[index]` and only
# between segments.
INDEXED_PATH = re.compile(r'[a-z0-9_]+(?:\[\w+\])?(?:\.[a-z0-9_]+(?:\[\w+\])?)*')

# Candidate rows are found from the TABLE, not from the text in them. Requiring
# a well-formed `AUTUMN_` prefix to notice a row was the fourth way a row could
# leave the check by being malformed — a typo before the underscore
# (`AUTMN_SERVER__HOST`) made the line invisible and took the count 142 -> 141,
# green. The three before it were a missing backtick, a non-backticked cell and
# leading whitespace, each fixed where it was found; this bounds the whole
# region instead, so a row can only leave the check by leaving the table.
TABLE_HEADER = re.compile(r'^//!\s*\|\s*Variable\s*\|')
TABLE_SEPARATOR = re.compile(r'^//!\s*\|[-\s|]+\|\s*$')
TABLE_ANY_ROW = re.compile(r'^//!\s*\|')
DOC_BLANK = re.compile(r'^//!\s*$')

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
    """Mapping rows, plus any row inside the table that neither pattern claims.

    The region runs from the `| Variable | …` header to the first line that is
    no longer a table row. Everything inside it is a row this script is
    responsible for; the unparsed list is returned rather than discarded,
    because a row it cannot read is a row it is not checking, and silence about
    that is what let seven shard rows sit unverified.
    """
    rows, unparsed, inside, found = [], [], False, 0
    for i, line in enumerate(text.splitlines(), 1):
        if TABLE_HEADER.match(line):
            inside, found = True, found + 1
            continue
        if not inside:
            continue
        if TABLE_SEPARATOR.match(line):
            continue
        if not TABLE_ANY_ROW.match(line):
            # A markdown table ends at a BLANK line or at the end of the doc
            # block — never at a line with content, which GFM reads as a
            # continuation of the row above. Ending the region on any non-row
            # line meant a late row that lost its leading pipe terminated the
            # scan: dropping the `AUTUMN_CLUSTER__BIND_ADDR` pipe left 127 rows
            # parsed, still over the floor, and silently stopped checking the
            # last 15 mappings with the gate green. Failing open, again.
            if not line.startswith('//!') or DOC_BLANK.match(line):
                inside = False
            else:
                unparsed.append((i, line.strip()))
            continue
        if m := TABLE_ROW.match(line):
            rows.append((i, m.group(1), m.group(2)))
        elif not TABLE_PROSE_ROW.match(line):
            unparsed.append((i, line.strip()))
    return rows, unparsed, found


def indexable(built, leaves):
    """Paths the runtime addresses positionally: a list, not a struct or a map."""
    out = set()
    for tpl in built:
        segs = tpl[len('AUTUMN_'):].split('__')
        for k, seg in enumerate(segs):
            if not seg.startswith('{') or k == 0:
                continue
            head = '.'.join(x.lower() for x in segs[:k])
            if any(o.startswith(head + '.') for o in leaves):
                out.add(head)
    return out


def check_table(rows, leaves, built, tokens):
    """Each row is a claim about two different things, checked separately.

    The PATH column claims a config key exists — answered by the schema. The
    VARIABLE column claims an environment override exists — answered by what the
    runtime reads, exactly as for the corpus. Checking the variable against the
    schema conflated them, so a row could document
    `AUTUMN_OPENAPI__ENABLED | openapi.enabled` and pass on the strength of the
    path alone, publishing an override that sets nothing to every reader on
    docs.rs. Third check: the two columns must still agree with each other,
    which catches a row edited on one side.
    """
    out = []
    for i, var, declared in rows:
        # The shards list is one-dimensional, so a path may carry AT MOST ONE
        # index, and it must sit between two segments. Erasing every bracket
        # group unconditionally let `database.shards[i][i].name` normalise onto
        # `database.shards.name` and pass, documenting two levels of indexing
        # into a flat list.
        if declared.count('[') > 1 or not INDEXED_PATH.fullmatch(declared):
            out.append((i, var, declared, 'malformed index in the path'))
            continue
        # An index belongs on the segment that IS a list. Validating only the
        # shape let `database.shards.name[i]` through — indexing the scalar
        # `name` — because erasing the bracket produced the same valid leaf.
        # A segment is indexable when the schema records children beneath it.
        if '[' in declared:
            head = declared[:declared.index('[')]
            # Having schema descendants does NOT make a path a list —
            # `security.headers` and `server.upgrade` are ordinary structs with
            # children. A list is a path the runtime addresses POSITIONALLY,
            # which is exactly what a template with a filled-in segment after it
            # records; the schema children then rule out an open map like
            # `auth.oauth2`, which has none.
            if head not in indexable(built, leaves):
                out.append((i, var, declared,
                            f'`{head}` is not a list, so it cannot be indexed'))
                continue
        path = re.sub(r'\[\w+\]', '', declared)
        if path not in leaves:
            out.append((i, var, declared, 'path is not in the schema'))
            continue
        if not (var in tokens or any(p.match(var) for p in built.values())):
            out.append((i, var, declared,
                        'nothing reads this variable, so it overrides nothing'))
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
    leaves = leaf_paths(paths)
    return leaves, built_patterns(ROOT, leaves), source_tokens(ROOT)


def main():
    if not SNAPSHOT.exists():
        print(f'error: schema snapshot missing at {SNAPSHOT}', file=sys.stderr)
        return 2
    leaves, built, tokens = load()
    files = corpus(ROOT)
    read = lambda rel: (ROOT / rel).read_text(encoding='utf-8', errors='replace')
    stats, defects = scan(files, read, leaves, built, tokens)

    rows, unparsed, found = table_rows(CONFIG_RS.read_text(encoding='utf-8'))
    # FAIL CLOSED. Moving row detection into a region last round created a way
    # for the whole table to vanish: rename the header cosmetically and `inside`
    # never turns on, so the checker returns no rows, no unparsed entries, and
    # nothing to report — silently disabling all 142 mapping checks at once.
    # Finding no table, or implausibly few rows, is itself the defect.
    table_missing = []
    if found != 1:
        table_missing.append(
            f'expected exactly one `| Variable | …` table in config.rs module '
            f'docs, found {found} — the mapping checks did not run')
    elif len(rows) < TABLE_ROW_FLOOR:
        table_missing.append(
            f'only {len(rows)} module-doc table rows parsed, below the ratchet '
            f'of {TABLE_ROW_FLOOR} — either the table stopped being read, or a '
            f'documented override was removed from the published reference. If '
            f'the removal is deliberate, lower TABLE_ROW_FLOOR in '
            f'{SELF} in the same commit and say why')
    table_defects = check_table(rows, leaves, built, tokens)

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
    for why in table_missing:
        print(f'\n{CONFIG_RS.relative_to(ROOT)}: {why}')
    for line, var, declared, why in table_defects:
        print(f'\nautumn/src/config.rs:{line}: '
              f'module-doc row `{var}` -> `{declared}`: {why}')
    for line, text in unparsed:
        print(f'\nautumn/src/config.rs:{line}: module-doc row not understood, '
              f'so not checked')
        print(f'    {text}')

    total = len(defects) + len(table_defects) + len(unparsed) + len(table_missing)
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
             re.compile(r'^AUTUMN_AUTH__OAUTH2__' + SEGMENT + r'__CLIENT_SECRET$'),
             'AUTUMN_DATABASE__SHARDS__{i}__{field}':
             re.compile(r'^AUTUMN_DATABASE__SHARDS__' + SEGMENT + r'__(NAME)$')}
    # Names something in the tree binds or reads — the only thing that makes an
    # environment variable settable.
    tokens = {'AUTUMN_ENV', 'AUTUMN_SEARCH__QUEUE', 'AUTUMN_LOG__LEVEL',
              'AUTUMN_SERVER__UPGRADE__ENABLED', 'AUTUMN_SECURITY__SIGNING_SECRET',
              'AUTUMN_ALERTS__ENABLED'}
    checked, failures = [], []

    def case(name, got, want):
        checked.append(name)
        if got != want:
            failures.append(f'{name}: got {got!r}, want {want!r}')

    r = lambda v: resolve(v, leaves, built, tokens)
    case('a read name resolves', r('AUTUMN_LOG__LEVEL'), 'source')
    # A config key with no env override of its own has no environment spelling.
    # `openapi.enabled` is a real schema leaf that nothing reads; resolving
    # against the schema blessed it, and 90 of 397 leaves are in that position.
    case('a schema leaf nothing reads does NOT resolve',
         r('AUTUMN_OPENAPI__ENABLED'), None)
    case('missing key fails', r('AUTUMN_LOG__LEVL'), None)
    # The single-underscore spelling is the whole point of the gate: it derives
    # a one-segment path that no section can match.
    case('single underscore fails', r('AUTUMN_DATABASE_URL'), None)
    case('sequence index elided',
         r('AUTUMN_DATABASE__SHARDS__0__NAME'), 'runtime-built name')
    # A branch is not a settable key: the runtime probes the leaves under it.
    case('a schema branch is not a key', r('AUTUMN_SERVER__UPGRADE'), None)
    case('its leaf is', r('AUTUMN_SERVER__UPGRADE__ENABLED'), 'source')
    case('the untagged branch still resolves',
         r('AUTUMN_SECURITY__SIGNING_SECRET'), 'source')
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
    # A misspelling of the NAMESPACE matches no `AUTUMN_` pattern at all, so it
    # has to be looked for on its own terms.
    case('a misspelt namespace is scanned',
         [m.group(0) for m in NEAR.finditer('export AUTMN_LOG__LEVEL=debug')
          if near_miss(m.group(1))], ['AUTMN_LOG__LEVEL'])
    case('one edit away in each direction',
         (near_miss('AUTMN'), near_miss('AUTUNM'), near_miss('AUTUUMN')),
         (True, True, True))
    case('the correct namespace is not a near miss', near_miss('AUTUMN'), False)
    case('somebody else\'s variable is not a near miss',
         (near_miss('DATABASE'), near_miss('RUST'), near_miss('CARGO')),
         (False, False, False))
    _, dn = scan(['d.md'], lambda _: 'export AUTMN_LOG__LEVEL=debug\n',
                 leaves, built, tokens)
    case('a misspelt namespace is reported', len(dn), 1)
    sn, _ = scan(['d.md'],
                 lambda _: ('export AUTMN_LOG__LEVEL=debug\n'
                            '<!-- config-key-allow: AUTMN_LOG__LEVEL — why -->\n'),
                 leaves, built, tokens)
    case('and can be waived like any other', sn['waived'], 1)
    _, dok = scan(['d.md'], lambda _: 'export DATABASE_URL=x\nRUST_LOG=debug\n',
                  leaves, built, tokens)
    case('an unrelated variable is not reported', len(dok), 0)
    # A trailing separator: captured, then judged by what follows it.
    case('a trailing separator is scanned',
         VAR.findall('export AUTUMN_LOG__LEVEL_=debug'), ['AUTUMN_LOG__LEVEL_'])
    case('a dangling separator is malformed',
         malformed('AUTUMN_LOG__LEVEL_'), True)
    # A wildcard is a claim about a family, checked like any other claim: the
    # prefix must actually begin a real name, or `*` becomes a blanket excuse.
    case('a real family prefix exists',
         family_exists('AUTUMN_SEARCH__', built, tokens), True)
    case('a misspelled family prefix does not',
         family_exists('AUTUMN_SESION__', built, tokens), False)
    # …and the prefix must end where a family ends. `AUTUMN_SEARCH_*` — one
    # underscore short — passed on `startswith` alone, because real names do
    # begin with those characters.
    case('a prefix ending mid-separator does not',
         family_exists('AUTUMN_SEARCH_', built, tokens), False)
    case('nor does a prefix ending mid-segment',
         family_exists('AUTUMN_S', built, tokens), False)
    # A single-separator family is real in the flat namespace, so the rule is
    # "the same separator run the name uses", not "must be `__`".
    case('a flat-namespace family still resolves',
         family_exists('AUTUMN_ACME_DNS_',
                       {}, {'AUTUMN_ACME_DNS_API_TOKEN'}), True)
    case('and its double-separator spelling does not',
         family_exists('AUTUMN_ACME_DNS__',
                       {}, {'AUTUMN_ACME_DNS_API_TOKEN'}), False)
    sf, dfam = scan(['d.md'], lambda _: 'set `AUTUMN_SESION__*` to override\n',
                    leaves, built, tokens)
    case('a misspelled family is reported',
         (sf['family wildcard'], len(dfam)), (0, 1))
    case('a `*` family mention is not a variable',
         family('AUTUMN_ALERTS__', 'set `AUTUMN_ALERTS__*` to override'), True)
    case('an angle-bracket family mention is not a variable',
         family('AUTUMN_MEDIA__', '`AUTUMN_MEDIA__<TABLE>__<FIELD>` overrides'),
         True)
    case('a dangling separator in an assignment is not a family',
         family('AUTUMN_LOG__LEVEL_', 'export AUTUMN_LOG__LEVEL_=debug'), False)
    _, dt = scan(['d.md'], lambda _: 'export AUTUMN_LOG__LEVEL_=debug\n',
                 leaves, built, tokens)
    case('a dangling separator is reported', len(dt), 1)
    st, df = scan(['d.md'], lambda _: 'set `AUTUMN_ALERTS__*` to override\n',
                  leaves, built, tokens)
    case('a family mention is not reported',
         (st['family wildcard'], len(df)), (1, 0))
    case('a placeholder is not malformed',
         malformed('AUTUMN_DATABASE__SHARDS__{i}__NAME'), False)
    _, dm = scan(['d.md'], lambda _: 'export AUTUMN_LOG__LEVeL=debug\n',
                 leaves, built, tokens)
    case('a casing typo is reported', len(dm), 1)
    case('a placeholder name resolves',
         r('AUTUMN_DATABASE__SHARDS__{i}__NAME'), 'runtime-built name')
    case('a typo beside a placeholder is caught',
         r('AUTUMN_DATABASE__SHARDS__{i}__NOPE'), None)
    case('table placeholder elided',
         r('AUTUMN_DATABASE__SHARDS__{i}__NAME'), 'runtime-built name')
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
    # A filled-in segment is ONE segment: it may hold a single `_` (a provider
    # name uppercases that way) but never `__`, or it swallows a whole extra
    # path segment.
    case('a placeholder does not swallow a path segment',
         r('AUTUMN_AUTH__OAUTH2__GITHUB__NOPE__CLIENT_SECRET'), None)
    case('a provider name with one underscore still resolves',
         r('AUTUMN_AUTH__OAUTH2__MY_IDP__CLIENT_SECRET'), 'runtime-built name')

    # Templates are read from the real tree: the oauth2 pair must be found, and
    # `…__SHARDS__{i}__{field}` must be skipped (it would re-admit any field).
    real_leaves = leaf_paths(schema_paths(SNAPSHOT.read_text(encoding="utf-8")))
    real_built = built_patterns(ROOT, real_leaves)
    # A template is only truth at a CONSTRUCTION site; a quoted string in a
    # comment or fixture is not.
    case('a bare quoted template is not a construction site',
         TEMPLATE.findall('// "AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_SECRT"'), [])
    case('a format! template is',
         TEMPLATE.findall('let v = format!("AUTUMN_X__{i}__Y");'),
         ['AUTUMN_X__{i}__Y'])
    # …and a whole `format!` call sitting in a comment is not one either. The
    # bare-string case above was the previous fix; it left the commented CALL
    # matching, so a typo written in a comment became a name the gate believed
    # the runtime builds.
    case('a commented format! is not a construction site',
         TEMPLATE.findall(uncommented(
             '    // let v = format!("AUTUMN_AUTH__OAUTH2__{u}__CLIENT_SECRT");')),
         [])
    case('an uncommented format! survives stripping',
         TEMPLATE.findall(uncommented('    let v = format!("AUTUMN_X__{i}__Y");')),
         ['AUTUMN_X__{i}__Y'])
    # A trailing comment must not blank the code before it.
    case('a trailing comment does not drop the code on its line',
         TEMPLATE.findall(uncommented('let v = format!("AUTUMN_X__{i}__Y"); // note')),
         ['AUTUMN_X__{i}__Y'])
    case('oauth2 templates are derived from source',
         any('OAUTH2' in t and t.endswith('CLIENT_SECRET') for t in real_built),
         True)
    # An open-final template is kept, but its final segment is constrained to
    # the schema's children of the path it addresses — so the documented shard
    # fields resolve and an invented one does not.
    case('an open-final template is schema-constrained',
         any(t.endswith('{field}') for t in real_built), True)
    case('an unknown shard field is not swallowed by a template',
         any(p.match('AUTUMN_DATABASE__SHARDS__0__NOPE')
             for p in real_built.values()), False)

    # A test may name a variable precisely to prove the runtime ignores it.
    # Neither shape may enter the truth set.
    case('a negatively-asserted name is not swept in',
         'AUTUMN_TEST_SESSION_COOKIE' in swept, False)
    case('a fixture environment name is not swept in',
         'AUTUMN_SERVER__TLS__ENABLED' in swept, False)
    # `.get` belongs to every collection, so it is an accessor only on an
    # environment map. A test asserting `map.get("AUTUMN_LOG__LEVL") == None`
    # names a variable precisely to prove nothing reads it.
    # A commented read is prose about a variable, not a read of it — the same
    # defect as the commented `format!`, which was fixed only in the template
    # reader and left this one open.
    read = '  std::env::var("AUTUMN_LOG__LEVL");'
    case('a commented read yields no accessor',
         bool(ACCESSOR.search(uncommented('  //' + read.strip(),
                                          comment_leader('x.rs')))), False)
    case('the same read uncommented does',
         bool(ACCESSOR.search(uncommented(read, comment_leader('x.rs')))), True)
    # A `/* … */` block is a comment too, wherever it sits — and the code around
    # it survives, which is what distinguishes knowing where the strings are
    # from blanking whole lines.
    case('a block comment on one line is stripped',
         uncommented('  /* std::env::var("AUTUMN_LOG__LEVL"); */'), '  ')
    # Rust block comments NEST. Leaving at the first `*/` handed the rest of the
    # outer comment back as code.
    case('a nested block comment closes once, at the end',
         uncommented('/* a /* b */ std::env::var("AUTUMN_LOG__LEVL"); */ let x = 1;'),
         ' let x = 1;')
    case('a multi-line block is stripped to its close',
         uncommented('/*\nstd::env::var("AUTUMN_LOG__LEVL");\n*/\nlet x = 1;'),
         '\n\n\nlet x = 1;')
    case('code after a closing block survives',
         uncommented('/* note */ let v = format!("AUTUMN_X__{i}__Y");\nlet y = 2;'),
         ' let v = format!("AUTUMN_X__{i}__Y");\nlet y = 2;')
    # A comment marker INSIDE a string is not a comment. Both directions matter:
    # a trailing comment must go, and a `//` or `/*` in a literal must not take
    # the code with it.
    case('a trailing comment is stripped',
         uncommented('const _: () = (); // std::env::var("AUTUMN_LOG__LEVL");'),
         'const _: () = (); ')
    case('a `//` inside a string is not a comment',
         uncommented('let u = "https://example.com/x"; std::env::var("AUTUMN_LOG__LEVEL");'),
         'let u = "https://example.com/x"; std::env::var("AUTUMN_LOG__LEVEL");')
    case('a mid-line `/*` inside a string does not blank the line',
         uncommented('let p = "/*"; std::env::var("AUTUMN_LOG__LEVEL");'),
         'let p = "/*"; std::env::var("AUTUMN_LOG__LEVEL");')
    # A raw string keeps its contents, comment markers included. `tauri_mobile.rs`
    # asserts on generated code containing a commented-out `set_var`, and
    # truncating at the `//` would have dropped a real read from the truth set.
    case('a raw string keeps a commented line',
         uncommented('assert!(x.contains(r#"// set_var("AUTUMN_SYNC__TOKEN");"#));'),
         'assert!(x.contains(r#"// set_var("AUTUMN_SYNC__TOKEN");"#));')
    case('an escaped quote does not end a string',
         uncommented(r'let s = "a\" // b"; let c = 1;'),
         r'let s = "a\" // b"; let c = 1;')
    # `#` opens a comment at the start of a WORD, so shell parameter expansion
    # survives.
    case('a trailing shell comment is stripped',
         uncommented('export AUTUMN_X=1 # AUTUMN_LOG__LEVL', '#'),
         'export AUTUMN_X=1 ')
    case('a parameter expansion is not a comment',
         uncommented('echo "${AUTUMN_X#prefix}" $#', '#'),
         'echo "${AUTUMN_X#prefix}" $#')
    # Test code is not the runtime: a test names a variable to prove the runtime
    # ignores it. `AUTUMN_DEV` is read exactly once in the whole tree, inside a
    # `#[test]` asserting it is unset.
    case('a `#[cfg(test)]` region is masked',
         untested('let a = 1;\n#[cfg(test)]\nmod tests {\n  let b = 2;\n}\nlet c = 3;'),
         'let a = 1;\n\n\n\n\nlet c = 3;')
    # A `#[cfg(test)]` written INSIDE a string is not a region — two files put
    # one in a generated-code template, and matching it masked the rest of each.
    case('a cfg(test) inside a string is not a region',
         untested('let t = "#[cfg(test)]\\nmod tests {";\nlet a = 1;'),
         'let t = "#[cfg(test)]\\nmod tests {";\nlet a = 1;')
    # …and a `}` inside a string does not END one. `doctor.rs` embeds a Rust
    # fixture whose `}` opens column zero, 8,000 lines before the test module
    # closes; a column-zero rule handed every test after it back to the truth set.
    case('a brace inside a string does not end the region',
         untested('#[cfg(test)]\nmod tests {\n  let f = "\n}\n";\n  let b = 2;\n}\nlet c = 3;'),
         '\n\n\n\n\n\n\nlet c = 3;')
    # An item with no block ends at its semicolon: `#[cfg(test)] use uuid::Uuid;`
    # occurs here, and a brace search alone would run to the end of the file.
    case('a cfg(test) item with no block ends at its semicolon',
         untested('#[cfg(test)]\nuse uuid::Uuid;\nlet a = 1;'),
         '\n\nlet a = 1;')
    # Which cfg is test-only is decided by evaluating the predicate. A test-only
    # item is one that can NEVER compile with `test` off.
    testonly = lambda p: not _cfg_truth(p)[0]
    case('cfg(test) is test-only', testonly('test'), True)
    case('all(test, …) is', testonly('all(test, feature = "maud")'), True)
    # …in either order: `all(feature = "redis", test)` occurs here, and a rule
    # anchored on `test` following `all(` would have missed it.
    case('all(…, test) is', testonly('all(feature = "redis", test)'), True)
    # `any(test, feature = "mail")` compiles in production with `mail` on — 14
    # items here are that shape, and masking them dropped real reads.
    case('any(test, …) is NOT', testonly('any(test, feature = "mail")'), False)
    case('not(test) is NOT', testonly('not(test)'), False)
    case('a feature named test-support is NOT',
         testonly('feature = "test-support"'), False)
    case('a nested predicate still evaluates',
         (testonly('all(any(test, feature = "mail"), unix)'),
          testonly('any(all(test, unix), test)')), (False, True))
    case('an unparseable predicate keeps the code',
         testonly('some_new_syntax!!'), False)
    # A char literal can hold a `"`. `dotenv.rs:189` writes `quote == b'"'`, and
    # skipping char literals left the rest of that file classified as string —
    # so nothing in it was masked, commented, or read correctly.
    case('a char literal holding a quote does not open a string',
         _rust_skeleton('if q == b\'"\' { let s = "x"; }'),
         'if q == b\'"\' { let s =    ; }')
    case('a lifetime is not a char literal',
         _rust_skeleton("fn f<'a>(s: &'a str) { }"),
         "fn f<'a>(s: &'a str) { }")
    case('a test-only name is not swept in', 'AUTUMN_DEV' in swept, False)
    case('a test path is not swept',
         (bool(TEST_PATH.search('autumn/tests/integration/a11y.rs')),
          bool(TEST_PATH.search('autumn/src/config.rs'))), (True, False))
    # The crates' own env helpers are read out of the tree, not listed here: the
    # media plugin reads 17 real variables through `override_string`, and
    # `AUTUMN_MEDIA__FFMPEG__BIN` was in the truth set only through a `${…}`
    # expansion inside a test — so masking tests without this would have
    # reported a correct page.
    case('an env helper declaration is recognised',
         ENV_HELPER.findall('fn override_string(target: &mut String, '
                            'env: &HashMap<String, String>, key: &str) {'),
         ['override_string'])
    case('a helper-read name is swept in',
         'AUTUMN_MEDIA__STORAGE__BUCKET' in swept, True)
    case('and the one that was only in a test expansion still is',
         'AUTUMN_MEDIA__FFMPEG__BIN' in swept, True)
    # The leader is per file type: `#` opens a comment in TOML and shell, but a
    # Rust attribute and a markdown heading, and stripping it there would drop
    # real code.
    case('a Rust attribute is not a comment',
         uncommented('#[derive(Deserialize)]', comment_leader('a.rs')),
         '#[derive(Deserialize)]')
    case('a shell comment is', uncommented('# export AUTUMN_X=1',
                                           comment_leader('a.sh')), '')
    case('a template is read as its inner type',
         (comment_leader('Cargo.toml.tmpl'), comment_leader('README.md.tmpl')),
         ('#', None))
    # A Dockerfile is named, not suffixed. `Dockerfile.api.tmpl` strips to
    # `Dockerfile.api`, whose suffix is `.api`, so the whole family was getting
    # no comment handling and its commented `--build-arg` examples read as truth.
    case('a Dockerfile is recognised by name',
         (comment_leader('Dockerfile'),
          comment_leader('autumn-cli/src/templates/Dockerfile.api.tmpl'),
          comment_leader('benchmarks/runtime/autumn/Dockerfile')),
         ('#', '#', '#'))
    # …and its declaration form is recognised alongside, because `ARG NAME=`
    # with an empty default matches none of the shell shapes.
    case('an ARG/ENV declaration is a declaration',
         (DECLARED.findall('ARG AUTUMN_BUILD_GIT_SHA='),
          DECLARED.findall('ENV AUTUMN_PROFILE=prod')),
         (['AUTUMN_BUILD_GIT_SHA'], ['AUTUMN_PROFILE']))
    case('a build arg declared in a Dockerfile is swept in',
         'AUTUMN_CLI_VERSION' in swept, True)
    case('an unlisted type keeps every line',
         uncommented('# AUTUMN_X', comment_leader('a.golden')), '# AUTUMN_X')
    case('a collection lookup is not an env accessor',
         bool(ACCESSOR.search('assert_eq!(map.get("AUTUMN_LOG__LEVL"), None);')),
         False)
    case('an environment map lookup is',
         bool(ACCESSOR.search('let v = env.get("AUTUMN_LOG__LEVEL");')), True)
    # …while the binding form, which has no accessor anywhere near it, is.
    case('a const-bound name is swept in', 'AUTUMN_CANARY' in swept, True)
    # `NAME=` inside a Rust string is text, not a shell assignment:
    # `AUTUMN_DB_RETENTION_REPORT=` frames a line of stdout.
    case('an output-framing prefix is not swept in',
         'AUTUMN_DB_RETENTION_REPORT' in swept, False)
    # …but a read inside a generated code template, where the quotes are
    # escaped, IS a read.
    case('a read in a generated code template is swept in',
         'AUTUMN_TEST_ADMIN_SESSION' in swept, True)
    case('escaped quotes are recognised',
         QUOTED.findall(r'std::env::var(\"AUTUMN_X\")'), ['AUTUMN_X'])
    # A constant holding an `AUTUMN_`-prefixed string is only a variable if it is
    # named as one: the CSP template token is not.
    case('a non-env constant is not swept in',
         'AUTUMN_CSP_NONCE' in swept, False)
    case('an env-named binding is recognised',
         BOUND.findall('const CANARY_ENV: &str = "AUTUMN_CANARY";'),
         [('CANARY_ENV', 'AUTUMN_CANARY')])
    case('a placeholder binding is not',
         BOUND.findall('const NONCE_PLACEHOLDER: &str = "AUTUMN_CSP_NONCE";'),
         [])
    # A script-local shell variable is not application configuration.
    # A test sentinel is not framework configuration, whatever it is named.
    case('a test sentinel binding is not swept in',
         'AUTUMN_TEST_MCP_TOKEN_1970_UNSET' in swept, False)
    case('a bare VAR binding is not an env binding',
         BOUND.findall('const VAR: &str = "AUTUMN_X";'), [])
    case('a bare shell assignment is not swept in',
         'AUTUMN_MANIFEST' in swept, False)
    case('an exported assignment is', ASSIGNED.findall('export AUTUMN_X=1'),
         ['AUTUMN_X'])
    case('a prefix assignment is',
         ASSIGNED_PREFIX.findall('AUTUMN_X=1 cargo run'), ['AUTUMN_X'])
    case('a bare assignment is not',
         (ASSIGNED.findall('AUTUMN_X="path/to"'),
          ASSIGNED_PREFIX.findall('AUTUMN_X="path/to"')), ([], []))

    # Row fixtures live inside a table region, because that is what the checker
    # is responsible for — a row can only leave the check by leaving the table.
    def in_table(row):
        return ('//! | Variable | Config field | Type |\n'
                '//! |----------|-------------|------|\n' + row)

    # A mapping row that loses a backtick must not escape by looking like prose.
    broken = in_table('//! | `AUTUMN_DATABASE__URL` | database.urll | `String` |')
    rows_b, unparsed_b, _ = table_rows(broken)
    case('a malformed mapping row is reported, not called prose',
         (len(rows_b), len(unparsed_b)), (0, 1))
    prose = in_table('//! | `AUTUMN_ENV` | active profile | `String` |')
    rows_p, unparsed_p, _ = table_rows(prose)
    case('the enumerated prose rows still pass',
         (len(rows_p), len(unparsed_p)), (0, 0))
    # A row that loses its OPENING backtick must still be recognised as a row.
    # Requiring the backtick to detect a candidate dropped it from both lists,
    # taking the row count from 142 to 141 with the gate still green.
    no_tick = in_table('//! | AUTUMN_SERVER__PORT` | `server.port` | `u16` |')
    rows_n, unparsed_n, _ = table_rows(no_tick)
    case('a row missing its opening backtick is reported',
         (len(rows_n), len(unparsed_n)), (0, 1))
    # Doc-comment whitespace is not significant, so an indented row must still
    # be PARSED and checked — not merely noticed, and certainly not skipped.
    indented = in_table('//!  |  `AUTUMN_SERVER__HOST`  |  `server.host`  |  `String` |')
    rows_i, unparsed_i, _ = table_rows(indented)
    # A typo BEFORE the underscore must not make the row invisible: candidate
    # rows come from the table region, not from the text being already correct.
    typo = in_table('//! | `AUTMN_SERVER__HOST` | `server.host` | `String` |')
    rows_t, unparsed_t, _ = table_rows(typo)
    case('a misspelled prefix is reported, not skipped',
         (len(rows_t), len(unparsed_t)), (0, 1))
    # A row that loses its LEADING pipe must not end the scan. It did, and every
    # row after it went unchecked in silence: dropping one pipe mid-table left
    # 127 rows parsed — over the floor — and 15 mappings never looked at.
    lost = ('//! | Variable | Config field | Type |\n'
            '//! |----------|-------------|------|\n'
            '//! | `AUTUMN_SERVER__PORT` | `server.port` | `u16` |\n'
            '//! `AUTUMN_SERVER__HOST` | `server.host` | `String` |\n'
            '//! | `AUTUMN_LOG__LEVEL` | `log.level` | `String` |\n')
    rows_l, unparsed_l, _ = table_rows(lost)
    case('a lost leading pipe does not end the scan',
         (len(rows_l), len(unparsed_l)), (2, 1))
    # …while a real end of table is still an end: a markdown table terminates at
    # a blank line, and at the end of the doc block.
    ended = ('//! | Variable | Config field | Type |\n'
             '//! |----------|-------------|------|\n'
             '//! | `AUTUMN_SERVER__PORT` | `server.port` | `u16` |\n'
             '//!\n'
             '//! Prose after the table, mentioning AUTUMN_SERVER__HOST.\n')
    rows_e, unparsed_e, _ = table_rows(ended)
    case('a blank doc line ends the table',
         (len(rows_e), len(unparsed_e)), (1, 0))
    code = ('//! | Variable | Config field | Type |\n'
            '//! |----------|-------------|------|\n'
            '//! | `AUTUMN_SERVER__PORT` | `server.port` | `u16` |\n'
            '\npub struct AutumnConfig { pub server: ServerConfig }\n')
    rows_c, unparsed_c, _ = table_rows(code)
    case('the end of the doc block ends the table',
         (len(rows_c), len(unparsed_c)), (1, 0))
    # …and a line after the table is not a row at all.
    outside = ('//! | Variable | Config field | Type |\n'
               '//! |----------|-------------|------|\n'
               '//! | `AUTUMN_SERVER__PORT` | `server.port` | `u16` |\n'
               '//!\n//! | not a row |\n')
    rows_o, unparsed_o, _ = table_rows(outside)
    # If the header stops being recognised the whole table silently vanishes,
    # so finding no table is itself a defect.
    renamed = ('//! | Variables | Config field | Type |\n'
               '//! |----------|-------------|------|\n'
               '//! | `AUTUMN_SERVER__PORT` | `server.port` | `u16` |\n')
    _, _, found_r = table_rows(renamed)
    case('a renamed header finds no table', found_r, 0)
    _, _, found_ok = table_rows(in_table(
        '//! | `AUTUMN_SERVER__PORT` | `server.port` | `u16` |'))
    case('the real header is found once', found_ok, 1)
    case('the region ends at the first non-row line',
         (len(rows_o), len(unparsed_o)), (1, 0))
    case('an indented row is parsed, not dropped',
         (rows_i, unparsed_i),
         ([(3, 'AUTUMN_SERVER__HOST', 'server.host')], []))

    good = [(1, 'AUTUMN_LOG__LEVEL', 'log.level'),
            (2, 'AUTUMN_SECURITY__SIGNING_SECRET', 'security.signing_secret.secret'),
            (3, 'AUTUMN_DATABASE__SHARDS__{i}__NAME', 'database.shards[i].name')]
    case('sound table rows pass', check_table(good, leaves, built, tokens), [])
    # The two columns answer different questions. A row may name a real config
    # path and still document an override that sets nothing.
    # The shards list is flat: at most one index, between segments.
    case('a doubly-indexed path fails',
         len(check_table([(9, 'AUTUMN_DATABASE__SHARDS__{i}__NAME',
                           'database.shards[i][i].name')],
                         leaves, built, tokens)), 1)
    # An index belongs on the segment that IS a list.
    # Schema descendants do not make a path a list: only a path the runtime
    # addresses positionally is one.
    real_ix = indexable(real_built, real_leaves)
    case('the shard list is indexable', 'database.shards' in real_ix, True)
    case('a struct with children is not',
         {'security.headers', 'server.upgrade'} & real_ix, set())
    case('an open map is not', 'auth.oauth2' in real_ix, False)
    case('an index on a scalar field fails',
         len(check_table([(9, 'AUTUMN_DATABASE__SHARDS__{i}__NAME',
                           'database.shards.name[i]')],
                         leaves, built, tokens)), 1)
    case('a singly-indexed path passes',
         check_table([(9, 'AUTUMN_DATABASE__SHARDS__{i}__NAME',
                       'database.shards[i].name')], leaves, built, tokens), [])
    case('a table row whose variable nothing reads fails',
         len(check_table([(9, 'AUTUMN_OPENAPI__ENABLED', 'openapi.enabled')],
                         leaves | {'openapi.enabled'}, built, tokens)), 1)
    case('table row with a dead path fails',
         len(check_table([(9, 'AUTUMN_LOG__NOPE', 'log.nope')], leaves, built, tokens)), 1)
    case('table row edited on one side fails',
         len(check_table([(9, 'AUTUMN_LOG__LEVEL', 'auth.oauth2')], leaves, built, tokens)), 1)
    # A truncated variable column must NOT pass just because the row's path
    # exists: `AUTUMN_DATABASE` is not how you set `database.shards.name`.
    case('a truncated variable column fails',
         len(check_table([(9, 'AUTUMN_DATABASE', 'database.shards.name')],
                         leaves, built, tokens)), 1)
    # …and the one enumerated untagged-deserializer row still passes.
    case('the signing-secret exception still passes',
         check_table([(9, 'AUTUMN_SECURITY__SIGNING_SECRET',
                       'security.signing_secret.secret')], leaves, built, tokens), [])

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
