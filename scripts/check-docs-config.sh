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
#     suffix". Enumerate the exception; do not widen the rule. Folding a `run:
#     >` scalar is the same shape: YAML folds a break WITHIN a paragraph, and
#     joining every non-empty line instead invented a prefix assignment out of
#     `AUTUMN_X=value`, a blank line, and the next command. A rule taken from
#     one example is a guess about the examples you did not look at.
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
#     It recurs one rung deeper than the last fix reached: "Rust block comments
#     nest" was true in `_rust_classes` and not in the generated-code reader
#     beside it, and "a `#[cfg(test)] mod x;` makes that file test code" was
#     true in the token sweep and not in `built_patterns`. Two readers of the
#     same tree asking the same question need one predicate, not two — and two
#     VIEWS of one file must stay the same length, since they are indexed
#     together: compose's assignment view is built by a second pass, and a
#     blanked last line vanishes from `splitlines`. The positional rule
#     written for prefix assignments was not carried to `export` beside it,
#     so `printf %s export AUTUMN_X=v` still counted a round later.
#     A rung's INPUTS count as the rung: the accessor scan masked comments,
#     tests and generated data, while the pass that derived its helper names
#     from the tree read none of them — so one signature in a comment named an
#     accessor for every file. Mask where the reading happens, not only where
#     the matching does.
#   * A construct spelled in a narrower grammar than its language's. A heredoc
#     delimiter written as `\w+` when Bash takes a word, so `<<'END-CONFIG'` was
#     rejected and `<<\END-CONFIG` matched the prefix `END` — under- and
#     over-blanking from the same missing rule. An assignment value written as
#     `\S*` when a shell word holds `$( … )` and its spaces. A quoting rule
#     stated for "the shell family" and applied to `.sh` alone, while
#     `install.ps1` quotes the same way. One scalar terminator where a line may
#     open several heredocs (`cat <<'ONE' <<'TWO'`), and a per-LINE quote rule
#     for a quote that spans lines — which recurred in YAML, where a quoted
#     `run:` scalar may also run onto later lines. A terminator set that names
#     `;`, `&`, `|` and `)` but not `>` and `<`, so a redirection-only null
#     command (`AUTUMN_X=1 > out`, which starts no process) read as a command.
#     Ask what the language's own grammar says the thing is, then match that.
#     `check-docs-cli.sh` reads the same two languages and had already been
#     through most of these rounds: its `_heredoc_openers` / `_open_quote` are
#     where the heredoc and quote rules here come from. Read the sibling gate
#     before re-deriving one of its answers, and take all of it.
#     The same shape shows up as a container read as OPAQUE where the language
#     re-enters itself: a double-quoted string is not one context, because
#     `"$(printf '%s' '…')"` starts fresh quoting inside the substitution; a
#     PowerShell here-string is held by `@'` and a line beginning `'@`, not by
#     the apostrophes in its body; and a heredoc's openers are collected from
#     the LOGICAL command line, so a `\` continuation still opens both. That
#     rule was written into `_blank_literals` and not into `_mask_inert` beside
#     it, so `probe="$(cat <<EOF"` still hid its own heredoc opener and the
#     body read as commands — the re-entry rule and the fix-it-everywhere rule
#     failing together, one rung apart.
#     And as a DEFAULT standing in for a language: `#` covers most of this tree,
#     so the 171 `.sql` migrations were "commented" by a rule that strips
#     nothing SQL contains. A default is a guess about files nobody listed;
#     check what it is guessing about. The same shape as a KEY NAME standing in
#     for a schema: `run:` was executed wherever it appeared in any non-compose
#     YAML, so a `run` field in a data file, or one under a workflow's `env:`,
#     was scanned as shell. A name means something in a schema, not everywhere.
#     And the FILE SUFFIX is the same guess one level up: `.yml` said Bourne,
#     while `defaults.run.shell: pwsh` says the block is PowerShell, where
#     `$NAME` is a local and only `$env:NAME` reads the environment. The suffix
#     does not know what the consumer was told to run.
#   * A rule believed because it sounds like how the language works, when the
#     language is right there to ask. "An unfinished pipeline keeps bash
#     parsing, so it collects the next line's heredoc delimiters first" is a
#     plausible sentence and it is false — a body begins after the next
#     newline, whatever the line ends in. It shipped for several rounds on the
#     strength of the reasoning. Run the language. The pass-through list for
#     assignment words repeated it one round later: `exec`, `command` and
#     `nohup` sound like they forward an assignment, and all three exit 127
#     trying to run a program named `AUTUMN_X=value`. Six words, three wrong,
#     and bash answers each of them in one line.
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
#
# The namespace is matched case-INSENSITIVELY, because `AUTUMn_LOG__LEVEL` is
# zero edits from `AUTUMN` and still a name the runtime never reads; an
# uppercase-only class made it invisible in exactly the way the misspelt
# namespace was. What follows must be uppercase, though, and that is what keeps
# the crate name out of it: `autumn_web` and `autumn-macros` are prose about a
# package, not a claim about an environment variable.
# The head may be as short as ONE character, because the inserted separator can
# land anywhere: `AUT_UMN` leaves `AUT`, `AU_TUMN` leaves `AU`, `A_UTUMN` leaves
# `A`. Each minimum in turn — four, then three — made the typos below it match
# nothing at all, which is invisible rather than unresolved, and each was set by
# the example in front of me instead of by the range the edit can take. A length
# floor was never what kept this net tight: `near_miss` is, and it judges the
# head only after the first tail segment is joined back on, so `RUST_LOG` and
# `AWS_REGION` are rejected on being nobody's misspelling of `AUTUMN` rather
# than on being the wrong shape. Measured at every floor: 0 defects either way.
NEAR = re.compile(r'\b([A-Za-z][A-Za-z0-9]{0,8})_'
                  r'((?:[A-Z0-9]+|\{[a-z_]+\})(?:_+(?:[A-Z0-9]+|\{[a-z_]+\}))*)')

# The missing edit can be the SEPARATOR. `AUTUMNLOG__LEVEL` has no `_` after the
# namespace at all, so nothing above sees it: `VAR` wants the exact prefix and
# `NEAR` wants a separator where this token has none. The namespace is still
# there, fused to the first segment.
#
# The head here cannot contain `_` — a token that has one before its `__` was
# already a candidate for `NEAR` — so the two patterns never claim the same
# token, and `AUTUMN_LOG__LEVEL` matches neither.
FUSED = re.compile(r'\b([A-Za-z][A-Za-z0-9]*)__'
                   r'((?:[A-Z0-9]+|\{[a-z_]+\})(?:_+(?:[A-Z0-9]+|\{[a-z_]+\}))*)')


def fused_namespace(head):
    """`AUTUMNLOG` — the namespace, or one edit from it, then more letters.

    The namespace spelled correctly is never this: `AUTUMN` reads as `AUTUM`
    plus an `N` under a one-edit prefix rule, so it is excluded up front rather
    than left to a length coincidence.
    """
    up = head.upper()
    if up == 'AUTUMN':
        return False
    return any(len(up) > n and (up[:n] == 'AUTUMN' or near_miss(up[:n]))
               for n in range(5, 9))


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
#
# `#` is now the DEFAULT, and the lists below are the exceptions. An allow-list
# was the first shape and it failed three rounds running, once per file type
# nobody had thought of: Dockerfiles (named, not suffixed), then Terraform
# templates, each with explanatory `#` comments that read as runtime truth until
# someone noticed. The error directions are not symmetric — not stripping hides
# a wrong page for ever, while stripping where `#` is not a comment reports a
# correct page, visibly, once — so the default belongs on the side that fails
# loudly. Measured over every tracked non-markdown file that mentions
# `AUTUMN_`: the C-family and the fixture types below are the whole of the
# exception list.
COMMENT_LEADER = {
    # `#` opens an attribute, not a comment.
    '.rs': '//', '.ts': '//', '.js': '//', '.go': '//',
    # SQL is a third comment family — `--` to the line end and `/* … */` —
    # and the 171 tracked migrations were falling through to the `#` default,
    # which strips nothing an SQL file actually contains. Auditing the rest of
    # what the default was guessing about turned up seven more languages in the
    # same position, none of which a `#` rule can see a comment in: a name
    # mentioned in one of their comments counted as a use. `.lua` shares SQL's
    # `--`; the markup family is block-only and listed by its opening
    # delimiter. None of these files carries an `AUTUMN_` name today, so this
    # closes latent holes and moves no counter.
    '.sql': '--', '.lua': 'lua', '.java': '//',
    '.css': '/*', '.html': '<!--', '.xml': '<!--', '.svg': '<!--',
    '.heex': '<!--', '.erb': '<%#', '.ftl': '<#--',
    # Program output captured as a fixture: nothing in it is a comment, and a
    # line may legitimately begin with `#`.
    '.golden': None, '.stderr': None, '.snap': None, '.json': None,
    '.md': None,
}
COMMENT_LEADER_NAMED = {'Dockerfile': '#', 'Makefile': '#', 'Justfile': '#'}


def effective_suffix(rel):
    """`Cargo.toml.tmpl` is TOML; `README.md.tmpl` is markdown."""
    p = pathlib.PurePath(rel)
    return pathlib.PurePath(p.stem).suffix if p.suffix == '.tmpl' else p.suffix


def comment_leader(rel):
    p = pathlib.PurePath(rel)
    if p.suffix == '.tmpl':
        p = pathlib.PurePath(p.stem)
    named = COMMENT_LEADER_NAMED.get(p.name.split('.')[0])
    if named:
        return named
    return COMMENT_LEADER.get(p.suffix, '#')


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
                # A char literal is CLASSIFIED, not merely skipped. Skipping it
                # fixed the desync `b'"'` caused and left `'}'` counted as a
                # brace, which ended a test mask early — the same defect one
                # question further on: this scan answers "which text is code",
                # and a literal is not.
                #
                # An escape is short and ends at the next quote; a plain one is
                # three characters; anything else is a lifetime, which needs no
                # handling.
                if body[i + 1:i + 2] == '\\':
                    j = body.find("'", i + 2)
                    if 0 <= j - i <= 12:
                        cls[i:j + 1] = 's' * (j + 1 - i)
                        i = j + 1
                        continue
                elif body[i + 2:i + 3] == "'":
                    cls[i:i + 3] = 'sss'
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


# A string region is generated CODE when it carries an inner string literal or
# spans lines. `"https://example.com/x"` is neither — it is a URL, and reading
# its `//` as a comment blanked the rest of the literal; `let p = "/*";` is a
# string whose whole content is a marker. A template that emits Rust always has
# one or the other, because emitted code quotes things and has lines.
GENERATED_CODE = re.compile(r'\\"|\n|(?<=[^\\])"')


def _strip_generated_comments(body, cls):
    """Blank comments in the generated code held INSIDE string literals.

    String contents are kept on purpose — `generate/admin.rs` writes a real
    `std::env::var(\\"AUTUMN_TEST_ADMIN_SESSION\\")` into the code it emits, and
    nine names in the truth set are carried only by accessors like it. But that
    generated code has comments of its own, and a commented accessor inside a
    template is no more a read than one outside it.

    Inside a code region, Rust's rule applies unchanged: `//` outside an inner
    literal opens a comment wherever it appears, including straight after a `;`.
    Requiring whitespace in front was a way of protecting URLs, and it protected
    `();// env::var(…)` too. What actually separates the two is whether the
    region is code at all, plus tracking the generated code's OWN literals —
    delimited by `\\"` in an escaped template and by `"` in a raw one, so either
    toggles, after the outer delimiter, which is skipped on entering the region.

    A `//` stops at the line end; a `/* … */` runs to its terminator across
    generated lines, and both stop at the region end — so a block that never
    closes costs that one literal and nothing after it.

    Length-preserving, so the caller's classification stays valid.
    """
    out, i, n, instr, code = list(body), 0, len(body), False, False
    while i < n:
        if cls[i] != 's':
            instr, code, i = False, False, i + 1
            continue
        if i == 0 or cls[i - 1] != 's':
            # Entering a literal: step over its opening delimiter, `"` or `r#"`,
            # then decide whether what follows is code at all.
            j = body.find('"', i)
            i = (n if j < 0 else j + 1)
            k = i
            while k < n and cls[k] == 's':
                k += 1
            # The region's own closing delimiter is not evidence of anything,
            # so it comes off before the test — with it, every string looked
            # like code because every string ends in a quote.
            instr = False
            code = bool(GENERATED_CODE.search(body[i:k].rstrip('#').rstrip('"')))
            continue
        c = body[i]
        if c == '\\':
            if body[i + 1:i + 2] == '"':
                instr = not instr
            i += 2
            continue
        if c == '"':
            instr, i = not instr, i + 1
            continue
        if code and not instr and c == '/' and body[i + 1:i + 2] == '/':
            while i < n and cls[i] == 's' and body[i] != '\n':
                out[i] = ' '
                i += 1
            continue
        if code and not instr and c == '/' and body[i + 1:i + 2] == '*':
            # A block comment runs to its terminator, ACROSS generated lines.
            # Stopping at the newline left everything after the first line of a
            # multi-line `/* … */` reading as executable generated code.
            #
            # And it NESTS, in generated Rust exactly as in ordinary Rust:
            # `/* a /* b */ std::env::var("AUTUMN_X"); */` closes at the LAST
            # terminator, so stopping at the first left that accessor reading
            # as live generated code. Same depth counter `_rust_classes`
            # already keeps for the enclosing file — the rule belongs to the
            # language, not to one of its two readers here.
            depth = 1
            out[i] = out[i + 1] = ' '
            i += 2
            while i < n and cls[i] == 's':
                pair = body[i:i + 2]
                if pair in ('/*', '*/'):
                    depth += 1 if pair == '/*' else -1
                    out[i] = ' '
                    if i + 1 < n and cls[i + 1] == 's':
                        out[i + 1] = ' '
                    i += 2
                    if depth == 0:
                        break
                    continue
                if body[i] != '\n':
                    out[i] = ' '
                i += 1
            continue
        i += 1
    return ''.join(out)


# What makes a string a Rust PROGRAM rather than prose that quotes one: an
# emitted `.rs` file declares things. Anchored to a statement boundary so
# `perfect` and `constant` inside prose do not match.
STRING_OPEN = re.compile(r'r#*"|"')
RUST_ITEM = re.compile(r'(?:^|[\n;{}])\s*(?:pub\s+)?'
                       r'(?:fn|use|mod|impl|struct|enum|const|static|let|type'
                       r'|trait)\s')


def _generated_data(body):
    """A mask of the string content that is NOT generated code.

    Two kinds of data live inside Rust strings, and neither is a call.

    A NESTED literal inside a template is data one level further in:
    `const T: &str = \"std::env::var(\\\"AUTUMN_X\\\")\";` inside an emitted
    program defines a string that merely looks like a call. So the question is
    where the accessor SITS, not where the name does — the name is always
    inside a literal, because it is the argument.

    And a string that is not a Rust PROGRAM is not generated code. Line count
    was the first cut at this and it was a proxy, not evidence: a multi-line
    help constant that happens to quote one `std::env::var(\"…\")` line passed
    it. The test now asks whether the string contains Rust ITEMS — `fn`, `use`,
    `impl`, `let`, `const` and the rest — because an emitted `.rs` file has
    them and prose quoting a call does not.

    Measured: 30 of the 32 name-carrying regions in this tree match on items,
    the truth set is 430 with the rule and 430 without it, and the 55 `var(`
    hits in single-line CSS strings match nothing.

    This is still a property of the string rather than proof that something
    writes it to disk. Dataflow would be needed for that, and the emission
    signal that looked promising — does the file call `fs::write` — does not
    discriminate at all: `dotenv.rs` and `config.rs` do too. The residual is a
    string that contains Rust items AND an accessor naming an `AUTUMN_*` and is
    never emitted; a generator emitting an item-free snippet fails CLOSED,
    which is visible.
    """
    cls = _rust_classes(body)
    data, i, n = bytearray(len(body)), 0, len(body)
    while i < n:
        if cls[i] != 's':
            i += 1
            continue
        start = i
        while i < n and cls[i] == 's':
            i += 1
        # The region includes its own opening delimiter, and a template may
        # begin with its first item straight after it, so the delimiter is
        # removed rather than left to break the boundary match.
        region = body[start:i]
        opener = STRING_OPEN.match(region)
        content = region[opener.end():] if opener else region
        # An emitted `.rs` file is BOTH: it spans lines, and it declares
        # things. Each clause alone lets one shape through — a one-line
        # `r#"const X: &str = "…";"#` declares but is a fragment, and a
        # multi-line help constant spans lines but declares nothing. Measured
        # separately and together: 430 either way, so neither costs anything.
        if not (('\n' in content or '\\n' in content)
                and RUST_ITEM.search(content)):
            for k in range(start, i):
                data[k] = 1
            continue
        j = body.find('"', start)          # step over the region's own opener
        j = i if j < 0 or j >= i else j + 1
        instr = False
        while j < i:
            c = body[j]
            if c == '\\':
                if body[j + 1:j + 2] == '"':
                    instr = not instr
                elif instr:
                    data[j] = 1
                    if j + 1 < i:
                        data[j + 1] = 1
                j += 2
                continue
            if c == '"':
                instr = not instr
                j += 1
                continue
            if instr:
                data[j] = 1
            j += 1
    return data


def _rust_uncommented(body):
    """Drop `//` and `/* … */` comments, keeping strings and line numbering."""
    cls = _rust_classes(body)
    body = _strip_generated_comments(body, cls)
    return ''.join(c for c, k in zip(body, cls) if k != 'm' or c == '\n')


def _rust_skeleton(body):
    """The same text with comments AND string contents blanked to spaces.

    Length-preserving, so an offset in the skeleton is the same offset in the
    body — which is what makes brace matching on it usable for masking.
    """
    return ''.join(c if k == 'c' or c == '\n' else ' '
                   for c, k in zip(body, _rust_classes(body)))


# Where a `#` needs whitespace in front of it to open a comment. In the shell
# family it does — `${VAR#prefix}` and `$#` are parameter expansion, not
# comments — and in YAML an unquoted `a#b` is a literal scalar. Everywhere else
# a `#` opens a comment wherever it appears outside a string, which is why
# `x = 1# std::env::var("AUTUMN_LOG__LEVL")` in a Python file was surviving a
# rule written for shell and reading as a real accessor.
HASH_NEEDS_SPACE = ('.sh', '.bash', '.zsh', '.yml', '.yaml', '.env', '.example')

# Where a quoted scalar may legitimately continue onto the next line, so the
# closing quote is not read as a new opener. YAML only, and gated further on the
# quote having opened at a value position — see `_hash_uncommented`.
YAML_SCALARS = ('.yml', '.yaml')

# A BLOCK scalar (`key: |` or `key: >`) is a string as far as YAML is concerned,
# so `description: |` holding `AUTUMN_LOG__LEVL=x cmd` is prose — and it was
# reading as a prefix assignment. Whether such a string is ever executed is the
# CONSUMER's rule, not YAML's, so the executed keys are enumerated rather than
# guessed: `run` (GitHub Actions, GitLab), `command` and `entrypoint` (compose,
# Kubernetes), `script` (GitLab). Measured before narrowing: every name a block
# scalar carries in this tree — all 31 occurrences, 10 names — is under `run`.
YAML_BLOCK = re.compile(r'^(\s*)(?:-\s+)?([A-Za-z0-9_.-]+):\s*[|>][-+0-9]*\s*$')
# An executed key does not need a block scalar: `- run: echo "${AUTUMN_X}"` is
# one line and just as real. Blanking every non-block line discarded it.
YAML_INLINE = re.compile(r'^\s*(?:-\s+)?([A-Za-z0-9_.-]+):[^\S\n]+(?![|>]\s*$)\S')
YAML_EXECUTED = ('run', 'command', 'entrypoint', 'script')
# Every key line, value or not, so the nesting can be tracked. `steps:` opens a
# mapping and carries no value, so neither pattern above sees it — and it is
# exactly the ancestor that decides whether a `run` below it is a command.
YAML_KEY = re.compile(r'^(\s*)(-\s+)?([A-Za-z0-9_.-]+):(?=\s|$)')

# …and the key NAME alone does not make a value executable. `run` was accepted
# wherever it appeared in any non-compose YAML, so a `run:` field in a plain
# data file like `bmad/config.yaml`, or one nested under a workflow's `env:`,
# was scanned as shell although no consumer runs either. That is the same
# mistake as reading `#` as every language's comment leader: a name that means
# something in one schema does not mean it everywhere.
#
# So the file says which consumer it has, and the consumer says which POSITION
# it executes. A workflow is recognised by its shape rather than its path —
# `autumn-cli/src/templates/release/*-deploy.yml.tmpl` are real GitHub Actions
# workflows generated into the reader's repo, and 16 of the names here come
# from them, so a `.github/workflows/` path test would have dropped them.
# Measured before narrowing, which is what turned that up.
YAML_WORKFLOW = re.compile(r'^(?:jobs|runs):\s*$', re.M)


def _yaml_consumer(rel, body):
    """Which consumer executes this file's scalars, if any.

    None is the safe answer for a file nothing recognises: its names are
    dropped, so a page naming one is REPORTED rather than passed.
    """
    if _yaml_interpolated(rel):
        return 'compose'
    return 'actions' if YAML_WORKFLOW.search(body) else None


def _yaml_runs(key, stack, consumer, value=''):
    """Whether a scalar under `key`, nested as `stack` says, is executed.

    Compose does NOT put `command:` or `entrypoint:` through a shell — it
    splits the value and execs it — so `command: AUTUMN_X=v echo` tries to run
    a program called `AUTUMN_X=v` and exports nothing. Only a value that names
    a shell as its executable is shell, which is the same first-element rule a
    Dockerfile's exec form needs; a value this cannot read is not one.
    """
    if consumer == 'compose':
        return key in YAML_EXECUTED and _runs_shell(value)
    if consumer == 'actions':
        return key == 'run' and bool(stack) and stack[-1][1] == 'steps'
    return False


# Compose keeps EVERY value, because compose interpolates every value — but
# that is an answer to "where can `${…}` be expanded", and the assignment rungs
# ask a different question. `x-note: AUTUMN_X=v cmd` is an extension field
# nothing runs, and reading it as a prefix assignment put its name in the truth
# set. Two questions, two views — the same split the shell files already have,
# arrived at again one format later.
#
# What DOES assign in a compose file is `environment:`, in either spelling
# (`- AUTUMN_X=v` as a list, `AUTUMN_X: v` as a map), plus the fields that
# invoke a shell. Nothing else in the file names a variable to a process.
COMPOSE_ASSIGNS = 'environment'
# The three spellings a compose `environment:` entry takes. Only the first has
# an `=`, which is why the assignment rungs could not see the other two.
SEQ_ITEM = re.compile(r'^(\s*)-\s+(.*)$')
# …and YAML may quote either spelling: `"AUTUMN_X": v` and `- "AUTUMN_X=v"` are
# both valid and both were invisible, because the name was required to sit
# immediately after the indent or the dash.
COMPOSE_DECLARED = re.compile(
    r'^\s*(?:-\s+)?[\'"]?(AUTUMN_[A-Z0-9_]+)[\'"]?\s*(?:[:=]|$)', re.M)

# A workflow chooses the SHELL its `run:` blocks are written in, and the two
# grammars share almost nothing: in PowerShell `$NAME` is an ordinary local and
# only `$env:NAME` reaches the environment, so reading a `pwsh` block with the
# Bourne rules counts a local as a read AND misses every real one. `.ps1` has
# been read correctly for several rounds; a `pwsh` block inside a `.yml` was
# still read as Bourne because the decision was made from the FILE SUFFIX, and
# the suffix does not know what GitHub Actions was told to run.
#
# The choice has three sources, narrowest first: the step's own `shell:`, the
# job's `defaults.run.shell`, the workflow's. Resolved in a pre-pass because a
# step may declare its shell AFTER its `run:`, which a single forward walk
# cannot see.
# Docker has two forms, and only one of them is shell. `RUN echo $AUTUMN_X`
# goes through `/bin/sh -c`; `RUN ["echo", "$AUTUMN_X"]` execs the binary
# directly, so the dollar is literal text handed to `echo` and no variable is
# read. The scan applied the shell rule to every Dockerfile line because the
# FILE is shell-shaped — the same suffix-for-grammar mistake as `.yml`, now
# inside a single file, where the form changes line by line.
#
# The exception matters and is in this tree: `CMD ["sh", "-c", "…"]` names a
# shell as the executable, so its arguments ARE expanded. The first array
# element decides, which is exactly what Docker does with it.
DOCKER_EXEC = re.compile(r'^\s*(?:RUN|CMD|ENTRYPOINT)\s*\[')
DOCKER_FIRST = re.compile(r'\[\s*"([^"]*)"')
DOCKER_SHELLS = ('sh', 'bash', 'zsh', 'ash', 'dash', 'busybox',
                 'pwsh', 'powershell')


def _docker_literal(body):
    """Line indices where a Dockerfile expands nothing — the exec form.

    A form that continues with `\\` carries on; a first element this cannot
    read is treated as non-shell, which drops names rather than admitting them.
    """
    out, active = set(), False
    for index, l in enumerate(body.splitlines()):
        if active:
            out.add(index)
            active = l.rstrip().endswith('\\')
            continue
        if not DOCKER_EXEC.match(l):
            continue
        first = DOCKER_FIRST.search(l)
        exe = pathlib.PurePath(first.group(1)).name if first else ''
        if exe in DOCKER_SHELLS:
            continue
        out.add(index)
        active = l.rstrip().endswith('\\')
    return out


POWERSHELL = ('pwsh', 'powershell')
# The whole scalar, not one token: Actions takes a CUSTOM shell line —
# `shell: pwsh -NoProfile -Command ". '{0}'"` — and a single-token pattern
# rejected it, so the block silently fell back to Bourne. What names the
# grammar is the EXECUTABLE, so the value is read and its first word taken.
YAML_VALUE = re.compile(r'^\s*(?:-\s+)?[A-Za-z0-9_.-]+:\s*(\S.*?)\s*$')


def _command_option(word):
    """Whether `word` is the option that makes a shell run a command STRING.

    `bash <text>` runs a FILE of that name; only `-c` (and its clustered
    spellings, and PowerShell's `-Command`) makes the next argument a script to
    execute. Naming the shell is half the claim — this is the other half.
    """
    if not word.startswith('-'):
        return False
    opt = word.lstrip('-')
    if opt.lower() in ('c', 'command', 'encodedcommand'):
        return True
    return (not word.startswith('--') and opt.islower()
            and 'c' in opt and len(opt) <= 4)


def _payload_span(text):
    """Where the command STRING sits in `text`, or None.

    `command: sh -c 'AUTUMN_X=1 exec app'` executes the quoted argument, and
    scanning the outer line instead sees `sh` as the command word and the
    assignment as one of its arguments — so the assignment never counted. The
    sequence form already extracts this argument; the inline form must too.
    """
    i, n, seen, first = 0, len(text), False, True
    while i < n:
        while i < n and text[i] in ' \t':
            i += 1
        if i >= n:
            return None
        start, quote = i, ''
        if text[i] in '\'"':
            quote = text[i]
            i += 1
            while i < n and text[i] != quote:
                i += 1
            end, i = i, i + 1
            start += 1
        else:
            while i < n and text[i] not in ' \t':
                i += 1
            end = i
        word = text[start:end]
        if first:
            first = False
            if pathlib.PurePath(word.strip()).name.lower().rstrip('.exe') \
                    not in DOCKER_SHELLS and _shell_named(word) not in DOCKER_SHELLS:
                return None
            continue
        if seen:
            return (start, end)
        if _command_option(word):
            seen = True
    return None


def _runs_shell(value):
    """Whether a command value runs a shell OVER a command string."""
    words = _command_words(value)
    return (bool(words)
            and pathlib.PurePath(words[0]).name in DOCKER_SHELLS
            and any(_command_option(w) for w in words[1:]))


def _command_words(value):
    """The words of a command value, in either the list or the bare form."""
    text = value.strip()
    if text.startswith('['):
        parts = text[1:].split(']')[0].split(',')
    else:
        parts = text.split()
    return [p.strip().strip('\'"') for p in parts if p.strip()]


def _shell_named(value):
    """The executable a `shell:` value or a command names.

    Both spellings, because both appear: a bare command line splits on
    whitespace, and the JSON list form (`["sh", "-lc", "…"]`, which this tree
    uses) splits on the comma — taking the first word of the list form gives
    `["sh","-lc","autumn` and recognises nothing.
    """
    text = value.strip()
    if text.startswith('['):
        first = text[1:].split(',')[0]
    else:
        first = text.split()[0] if text.split() else ''
    # Normalised for comparison: Windows spells it `pwsh.exe`, and a `shell:`
    # value is not case-sensitive. Returning the raw basename meant `pwsh.exe`
    # matched nothing and its block fell back to Bourne.
    name = pathlib.PurePath(first.strip().strip('\'"[] ')).name.lower()
    return name[:-4] if name.endswith('.exe') else name


def _yaml_shells(body):
    """Line indices whose `run:` block is written in PowerShell.

    Only the `run` blocks are considered, since nothing else is executed; a
    file that names no PowerShell anywhere returns an empty set, which is the
    common case and costs one scan.
    """
    if not any(s in body for s in POWERSHELL):
        return set()
    src, stack = body.splitlines(), []
    default, jobs, steps = None, {}, {}
    job, step = None, None
    # Pass one: where each shell is declared, and which step each line is in.
    for index, l in enumerate(src):
        at = YAML_KEY.match(l)
        if not at:
            continue
        column = len(at.group(1)) + len(at.group(2) or '')
        while stack and stack[-1][0] >= column:
            stack.pop()
        path = [k for _, k in stack]
        key = at.group(3)
        if at.group(2) and path[-1:] == ['steps']:
            step = index
        if path[:1] == ['jobs'] and len(path) == 2:
            job = path[1]
        value = YAML_VALUE.match(l)
        if key == 'shell' and value:
            shell = _shell_named(value.group(1))
            if path[-2:] == ['defaults', 'run']:
                if path[:1] == ['jobs']:
                    jobs[job] = shell
                else:
                    default = shell
            elif 'steps' in path:
                steps[step] = shell
        stack.append((column, key))
    # Pass two: the lines each `run:` block owns, under its effective shell.
    out, stack, job, step = set(), [], None, None
    key, indent, block = None, 0, False
    for index, l in enumerate(src):
        if key is not None:
            if l.strip() and (len(l) - len(l.lstrip())) <= indent:
                key = None
            elif block:
                out.add(index)
                continue
            else:
                continue
        at = YAML_KEY.match(l)
        if not at:
            continue
        column = len(at.group(1)) + len(at.group(2) or '')
        while stack and stack[-1][0] >= column:
            stack.pop()
        path = [k for _, k in stack]
        if at.group(2) and path[-1:] == ['steps']:
            step = index
        if path[:1] == ['jobs'] and len(path) == 2:
            job = path[1]
        stack.append((column, at.group(3)))
        if at.group(3) == 'run' and path[-1:] == ['steps']:
            shell = steps.get(step, jobs.get(job, default))
            if shell in POWERSHELL:
                out.add(index)
                block = True
            else:
                block = False
            if YAML_BLOCK.match(l):
                key, indent = 'run', len(at.group(1))
    return out

# Blanking only the non-executed BLOCK scalars was half the rule. An ordinary
# scalar is just as inert: `name: "${AUTUMN_LOG__LEVL}"` in a workflow is text
# GitHub Actions never interpolates, and it was reading as an expansion.
#
# The consumers differ, so the file decides which rule applies. Compose
# interpolates `${…}` in EVERY value — that is what
# `AUTUMN_SECURITY__SIGNING_SECRET: "${AUTUMN_SECURITY__SIGNING_SECRET:?err}"`
# relies on — and Docker identifies a compose file by name, so that is the test
# rather than a guess. Everywhere else the shell syntax only means something
# inside a block a consumer executes.
#
# Measured: all 7 expansions in compose files are values, and all 3 in workflows
# are inside `run:`. Both populations survive.
COMPOSE_NAMES = ('compose.yml', 'compose.yaml',
                 'docker-compose.yml', 'docker-compose.yaml')


def _yaml_interpolated(rel):
    """Whether this YAML file's consumer expands `${…}` outside a `run:`."""
    p = pathlib.PurePath(rel)
    if p.suffix == '.tmpl':
        p = pathlib.PurePath(p.stem)
    return p.name in COMPOSE_NAMES


# What a backslash means in a YAML scalar depends on which quote holds it, and
# the two are opposites: a SINGLE-quoted scalar has no escapes at all — a
# backslash is a backslash and `''` is the only sequence — while a
# DOUBLE-quoted one has C-style escapes that YAML resolves before the value
# exists. A newline escape becomes `;`, which is the shell's own spelling of the
# boundary a newline is: it keeps two commands two, without costing the physical
# line the rest of this function counts on.
#
# An escape YAML does not define (`\$`) is left ALONE, backslash included,
# rather than resolved to its second character. That is the safe direction and
# not the tidy one: dropping the backslash would turn `\${AUTUMN_X}` into an
# expansion and put a name into the truth set, which is how a typo'd page
# passes silently. Keeping it can only cost a name, and a missing name is
# reported rather than hidden.
YAML_ESCAPE = {'0': ' ', 'a': ' ', 'b': ' ', 't': '\t', 'v': ' ', 'f': ' ',
               'e': ' ', '_': ' ', 'n': ';', 'r': ';', 'N': ';', 'L': ';',
               'P': ';', '\\': '\\', '"': '"', '/': '/', ' ': ' '}
YAML_HEX = {'x': 2, 'u': 4, 'U': 8}


def _flow_close(seg, quote):
    """Where `seg` closes a flow scalar already open in `quote`, else None.

    The escape that hides a closing quote differs by style, the same way the
    decoding does: `''` inside a single-quoted scalar, a backslash inside a
    double-quoted one.
    """
    j, n = 0, len(seg)
    while j < n:
        if quote == '"' and seg[j] == '\\':
            j += 2
            continue
        if seg[j] == quote:
            if quote == "'" and seg[j + 1:j + 2] == "'":
                j += 2
                continue
            return j
        j += 1
    return None


def _fold_flow(parts):
    """The folded value of a multi-line flow scalar, one line per part.

    Folding is YAML's, so it matches `flush()`: a break between two lines of a
    paragraph becomes a space, and a blank line is a paragraph break that
    survives — spelled `;` here, the shell's own name for the boundary a
    newline is, so two commands stay two.
    """
    runs, cur = [], []
    for part in parts:
        text = part.strip()
        if text:
            cur.append(text)
            continue
        if cur:
            runs.append(' '.join(cur))
        cur = []
    if cur:
        runs.append(' '.join(cur))
    return ' ; '.join(runs)


def _yaml_decode(scalar):
    """The value a quoted YAML scalar actually has, quotes included on input.

    Never longer than what it decodes, so the caller can pad back to the
    original width and keep every offset on the line usable.
    """
    quote, inner = scalar[0], scalar[1:-1]
    if quote == "'":
        return inner.replace("''", "'")
    out, i, n = [], 0, len(inner)
    while i < n:
        if inner[i] == '\\' and i + 1 < n:
            esc = inner[i + 1]
            width = YAML_HEX.get(esc)
            digits = inner[i + 2:i + 2 + (width or 0)]
            if width and len(digits) == width and all(
                    c in '0123456789abcdefABCDEF' for c in digits):
                char = chr(int(digits, 16))
                out.append(char if char.isprintable() else ' ')
                i += 2 + width
                continue
            out.append(YAML_ESCAPE.get(esc, '\\' + esc))
            i += 2
            continue
        out.append(inner[i])
        i += 1
    return ''.join(out)


def _yaml_blocks(body, interpolated=False, consumer='actions',
                 assignments=False):
    """Blank what the consumer never executes, keeping line numbers intact.

    In a compose file only the non-executed BLOCK scalars go, since every value
    is interpolated. Anywhere else every line outside an executed field goes —
    and which fields those are is `consumer`'s to say, by POSITION and not by
    key name alone: a `run:` under a workflow's `env:`, or in a file no
    consumer executes at all, is data like any other value.

    A FOLDED executed scalar (`run: >`) is joined before the shell ever sees
    it — YAML turns the physical newline into a space — so `AUTUMN_X=value` and
    `command` on consecutive lines really are one command, and reading them as
    two made the first a bare local assignment. Its lines are joined onto the
    first of them and the rest blanked, which keeps the line count.

    Folding is not "join everything", though, and the first cut at it was:
    YAML folds a line break between two lines of the SAME paragraph and keeps
    every other one. A BLANK line is a paragraph break that survives as a
    newline, and a MORE-INDENTED line keeps its breaks literally. Joining
    across either invented a command that never runs — `AUTUMN_X=value`, a
    blank line, then `printf …` is a bare assignment and a separate command,
    and folding them made the assignment a prefix on the printf. The failure
    is the reverse of the one that motivated the fold, which is why it needed
    a rule about paragraphs rather than a wider or narrower join.
    """
    out, key, indent, fold, buf = [], None, 0, False, []
    runs, stack = False, []

    def flush():
        if buf:
            # The block's own indentation is set by its first non-empty line;
            # anything deeper is a more-indented line, not part of the fold.
            base = next((len(out[i]) - len(out[i].lstrip())
                         for i in buf if out[i].strip()), 0)
            runs, cur = [], []
            for i in buf:
                if not out[i].strip():
                    if cur:
                        runs.append(cur)
                    cur = []
                elif len(out[i]) - len(out[i].lstrip()) > base:
                    if cur:
                        runs.append(cur)
                    runs.append([i])
                    cur = []
                else:
                    cur.append(i)
            if cur:
                runs.append(cur)
            heads = set()
            for r in runs:
                out[r[0]] = ' '.join(out[i].strip() for i in r)
                heads.add(r[0])
            for i in buf:
                if i not in heads:
                    out[i] = ''
        del buf[:]

    seq, seq_indent, seq_shell, seq_dashc = None, 0, None, None
    src, consumed = body.splitlines(), set()
    for index, l in enumerate(src):
        if index in consumed:
            # A continuation of a flow scalar already folded onto its first
            # line. Emitted empty so the line count holds.
            out.append('')
            continue
        if key is not None:
            if l.strip() and (len(l) - len(l.lstrip())) <= indent:
                flush()
                key = None
            else:
                out.append(l if runs else '')
                if fold and runs:
                    buf.append(len(out) - 1)
                continue
        # The nesting, kept for every key line — including the valueless ones
        # neither pattern below matches, since those are the ancestors that
        # decide what a key means.
        at = YAML_KEY.match(l)
        if at:
            column = len(at.group(1)) + len(at.group(2) or '')
            while stack and stack[-1][0] >= column:
                stack.pop()
        # A compose command is often a SEQUENCE, not a scalar:
        #
        #     command:
        #       - bash
        #       - -c
        #       - |
        #         …the payload…
        #
        # which is a real `bash -c` and is how this tree writes its longest
        # ones. Only a same-line value was read, so the payload was blanked
        # and every assignment in it was invisible. The first item names the
        # executable, exactly as it does in the inline and JSON forms.
        if consumer == 'compose':
            item = SEQ_ITEM.match(l)
            if at and at.group(3) in YAML_EXECUTED and not YAML_INLINE.match(l):
                seq, seq_indent = at.group(3), column
                seq_shell, seq_dashc = None, None
            elif seq is not None and item and len(item.group(1)) > seq_indent:
                text = item.group(2).strip()
                bare = text.strip('\'"')
                if seq_shell is None:
                    # The first item names the executable; a LATER item has to
                    # ask it to run a command string. `- bash` then a block is
                    # bash running a file of that name, not a script.
                    seq_shell = pathlib.PurePath(bare).name in DOCKER_SHELLS
                elif seq_shell and seq_dashc is None:
                    if _command_option(bare):
                        seq_dashc = True
                elif seq_shell and seq_dashc:
                    # Only the argument IMMEDIATELY after `-c` is the command
                    # string; everything after it is `$0`, `$1`, … So the flag
                    # is three-valued — unseen, expecting, spent — and this
                    # item spends it either way. Leaving it true made every
                    # later block in the sequence executable.
                    seq_dashc = False
                    if text.rstrip('-+0123456789') in ('|', '>'):
                        out.append(l)
                        key, indent = seq, len(item.group(1))
                        runs, fold = True, text.startswith('>')
                        continue
                    # An inline command string is executed too; the `- ` is
                    # YAML, so it is blanked rather than left as a word.
                    out.append(' ' * len(item.group(0).split(text)[0]) + text)
                    continue
            elif l.strip() and (len(l) - len(l.lstrip())) <= seq_indent:
                seq = None
        m = YAML_BLOCK.match(l)
        inline = YAML_INLINE.match(l)
        executed = (inline is not None
                    and _yaml_runs(inline.group(1), stack, consumer,
                                   l.partition(':')[2]))
        # The ASSIGNMENT view of a compose file keeps only what can name a
        # variable to a process; the expansion view keeps everything.
        keep = ((executed or any(k == COMPOSE_ASSIGNS for _, k in stack))
                if (interpolated and assignments)
                else (interpolated or executed))
        if executed:
            # The quotes around an inline scalar are YAML's, and YAML removes
            # them before the command reaches the shell — so
            # `run: 'echo ${AUTUMN_X}'` really does expand. Passing them to the
            # shell pass read them as shell quotes and erased the expansion.
            #
            # Removing the quotes was only half of it: YAML DECODES a quoted
            # scalar, it does not merely unwrap it. `run: "echo \\${AUTUMN_X}"`
            # reaches the shell as `echo \${AUTUMN_X}` — one backslash, an
            # escaped dollar, no expansion at all — and leaving both backslashes
            # let the shell pass pair them off and read a real one. Same shape
            # as the quotes themselves: the consumer's decoding happens before
            # the shell sees anything, so it has to happen here too.
            #
            # And a flow scalar is not bounded by the line it starts on. A
            # quoted `run:` value may run onto later lines, which YAML folds
            # and unquotes exactly as it does the one-line form — while this
            # read the opener as an unterminated scalar and blanked every
            # continuation as an unrelated line, dropping the reads on them.
            # That direction costs coverage rather than admitting a name, so
            # it reported correct pages instead of passing wrong ones; both
            # are wrong, and only one is loud.
            head, _, value = l.partition(':')
            body_ = value.strip()
            if body_ and body_[0] in '\'"' and _flow_close(body_[1:],
                                                           body_[0]) is None:
                quote, parts, j = body_[0], [body_[1:]], index + 1
                while j < len(src):
                    seg = src[j].strip()
                    shut = _flow_close(seg, quote)
                    parts.append(seg if shut is None else seg[:shut])
                    if shut is not None:
                        break
                    j += 1
                # An unterminated scalar is left exactly as it was: guessing
                # where it ends would invent a command nobody wrote.
                if j < len(src):
                    consumed.update(range(index + 1, j + 1))
                    body_ = quote + _fold_flow(parts) + quote
                    value = ' ' + body_
                    l = head + ':' + value
            if len(body_) > 1 and body_[0] == body_[-1] and body_[0] in '\'"':
                pad = len(value) - len(value.lstrip())
                l = head + ':' + (' ' * (pad + 1)
                                  + _yaml_decode(body_)).ljust(len(value))
            # The KEY is not part of the command. `command: AUTUMN_X=1 serve`
            # runs `AUTUMN_X=1 serve`, and leaving `command:` on the line made
            # it read as the command's name — which matters now that an
            # assignment must come before that name. Blanked rather than cut,
            # so every offset on the line still lines up.
            l = ' ' * (len(head) + 1) + l[len(head) + 1:]
            # …and when the command runs a SHELL, only its command-string
            # argument is executed. `sh -c 'AUTUMN_X=1 exec app'` runs the
            # quoted text; leaving the whole line made `sh` the command word,
            # so the assignment read as one of its arguments. Everything but
            # the payload is blanked, which keeps the offsets.
            span = _payload_span(l) if consumer == 'compose' else None
            if span:
                l = (' ' * span[0] + l[span[0]:span[1]]
                     + ' ' * (len(l) - span[1]))
        # A block-opening line carries no value of its own, so it is kept only
        # to preserve the line count, never as evidence.
        out.append(l if (keep or m) else '')
        if m:
            key, indent = m.group(2), len(m.group(1))
            # A block-opening `command:` carries no value on its own line, so
            # nothing here names a shell — which is the answer that drops
            # names rather than admitting them.
            runs = _yaml_runs(key, stack, consumer)
            fold = '>' in l.split(':', 1)[1]
        if at:
            stack.append((column, at.group(3)))
    flush()
    return '\n'.join(out)

# HCL accepts THREE comment forms — `#`, `//` and `/* … */` — so Terraform files
# get the C-style scanner as well as the hash one. It is a superset rather than
# a swap: a `//` in a shell or YAML file is a path, not a comment, which is why
# this is a named set and not the default.
HASH_AND_SLASH = ('.tf', '.tfvars', '.hcl')

# PowerShell has a block comment the line stripper cannot see: `<# … #>` spans
# lines, and `scripts/install.ps1` opens with a 26-line one documenting the very
# `AUTUMN_*` overrides this gate checks. Blanked before anything else runs, with
# newlines kept so line numbering holds.
PS_BLOCK = re.compile(r'<#.*?#>', re.S)
HASH_BLOCK = ('.ps1', '.psm1')

# Bash expands nothing inside single quotes, so `'${AUTUMN_LOG__LEVL}'` names no
# variable — it is a string that happens to contain the syntax. PowerShell has
# the same rule and the same reason to be here: `'$env:AUTUMN_X'` and
# `'${AUTUMN_X}'` are literal text in a `.ps1`, and `scripts/install.ps1` is
# where this project's PowerShell lives. Not applied to YAML, which
# single-quotes freely and whose `${…}` is still interpolated by whatever reads
# the file, docker-compose among them.
SHELL_QUOTED = ('.sh', '.bash', '.zsh', '.ps1', '.psm1')

# The escape character inside a DOUBLE-quoted string, per language: a backslash
# in the Bourne family, a backtick in PowerShell, where `"C:\dir\"` ends at the
# second quote and honouring the backslash would run the string past it.
QUOTE_ESCAPE = {'.ps1': '`', '.psm1': '`'}

# Heredocs are Bourne syntax; PowerShell's here-strings are `@' … '@` and open
# no `<<`. Kept as its own set so adding a language to one rule does not
# silently enrol it in the other — the two passes ran off one tuple until this
# round, and that tuple then had to mean two different things.
HAS_HEREDOC = ('.sh', '.bash', '.zsh')

# …and PowerShell's here-string is the other half of that same split: `@' … '@`
# is held by its own delimiters, not by quotes, so its body may contain the
# apostrophes an ordinary quoted span would end on.
HAS_HERE_STRING = ('.ps1', '.psm1')


def _shell_code(body, escape='\\', here=False):
    """The same body with BOTH quote kinds blanked — executable shell only.

    The two views exist because the rungs mean different things.
    `"${AUTUMN_X}"` inside double quotes IS an expansion, so `_shell_literals`
    keeps it; `echo " AUTUMN_LOG__LEVL=x cmd"` inside the same quotes is a
    string being printed, and reading it as a prefix assignment put the typo
    into the truth set. An assignment is only an assignment where the shell
    would run it, so the assignment rungs get this view and the expansion rung
    keeps the other.

    A substitution inside double quotes is still code — `x="$(FOO=1 cmd)"` runs
    one — and the scan re-enters it, so those assignments survive.
    """
    out = list(body)
    _blank_literals(body, out, 0, len(body), escape, here, also_double=True)
    return ''.join(out)


def _shell_literals(body, escape='\\', here=False):
    """Blank single-quoted spans, tracking quote state ACROSS lines.

    A single-quoted string spans physical lines — `printf '%s\\n' '` opens one
    the next line continues — and its interior lines are string data, not
    commands. Reading each line on its own left every interior line intact, so
    a `${AUTUMN_X}` written inside a multi-line literal read as an expansion.

    Doing it across lines is only safe because the scan is quote-AWARE rather
    than a search for pairs: an apostrophe inside `"don't"` no longer opens a
    literal, which is what the per-line bound was standing in for. The one
    remaining hazard is an apostrophe that opens nothing and never closes, and
    that costs itself and nothing after it — an unterminated span is skipped
    rather than blanked to the end of the file, since blanking there would hide
    real uses and report correct pages.

    `here` adds PowerShell's here-strings, whose bodies are held by their own
    delimiters rather than by quotes.

    Length-preserving, newlines kept, so line numbering holds.
    """
    out = list(body)
    _blank_literals(body, out, 0, len(body), escape, here)
    return ''.join(out)


def _blank_literals(body, out, i, n, escape, here=False, dq=False,
                    also_double=False):
    """Blank the single-quoted spans in `body[i:n]`; return where it stopped.

    A double-quoted string is NOT one opaque context: `"$(printf '%s' '…')"`
    re-enters shell parsing inside the substitution, where an apostrophe opens
    a literal again. Consuming the outer span whole left that inner literal
    readable, so a `${AUTUMN_X}` inside it counted as an expansion. `dq` says
    we are inside double quotes, where an apostrophe is ordinary text and a `"`
    ends the span; a substitution recurses with `dq` false, because quoting
    starts fresh inside one.
    """
    while i < n:
        c = body[i]
        if c == escape:
            # An ESCAPED `$` is a literal one: `"\${AUTUMN_X}"` prints the
            # syntax and reads nothing. Blanked here rather than taught to
            # `EXPANDED`, because that pattern also runs over Rust strings and
            # YAML, where a backslash escapes nothing of the sort.
            if body[i + 1:i + 2] == '$':
                out[i + 1] = ' '
            i += 2
            continue
        if c == '$' and body[i + 1:i + 2] == '(':
            end = min(_group_end(body, i + 1, '(', ')'), n)
            _blank_literals(body, out, i + 2, max(end - 1, i + 2), escape,
                            here, also_double=also_double)
            i = end
            continue
        if c == '`':
            # A backtick substitution, in the shells where a backtick is not
            # the escape character — the `escape` test above has already taken
            # it in PowerShell.
            j = body.find('`', i + 1)
            if j < 0 or j >= n:
                i += 1
                continue
            _blank_literals(body, out, i + 1, j, escape, here,
                            also_double=also_double)
            i = j + 1
            continue
        if dq:
            if c == '"':
                return i + 1
            if also_double and c != '\n':
                out[i] = ' '
            i += 1
            continue
        # A PowerShell here-string is held by `@'` and a line beginning `'@`,
        # NOT by its quotes: its body may contain apostrophes, so reading the
        # first one as the terminator left the rest of the body visible.
        if here and body[i:i + 2] in ("@'", '@"') and body[i + 2:i + 3] in '\r\n':
            quote, end = body[i + 1], body.find('\n' + body[i + 1] + '@', i + 2)
            if end < 0 or end >= n:
                i += 2                      # unterminated: costs the opener
                continue
            if quote == "'":                # `@" … "@` interpolates; keep it
                for k in range(i + 2, end):
                    if body[k] != '\n':
                        out[k] = ' '
            i = end + 3
            continue
        if c == '"':
            i = _blank_literals(body, out, i + 1, n, escape, here, True,
                                also_double)
            continue
        if c == "'":
            j = body.find("'", i + 1)
            if j < 0 or j >= n:
                i += 1
                continue
            for k in range(i + 1, j):
                if body[k] != '\n':
                    out[k] = ' '
            i = j + 1
            continue
        i += 1
    return i


# A QUOTED heredoc — `<<'EOF'` — is literal data handed to a program or written
# to a file. The shell neither runs it nor expands anything in it, so
# `AUTUMN_LOG__LEVL=x cmd` inside one is not a command this shell runs, exactly
# as `'${AUTUMN_X}'` is not an expansion. That matters because the house pattern
# for these gates embeds their self-tests and synthetic corpora in the
# production script: `check-docs-cli.sh` carries 17 such lines, and this script
# 303 of them, which is why it excludes itself by name.
#
# An UNQUOTED heredoc is a different thing entirely and is left alone: `<<EOF`
# does expand `${AUTUMN_X}`, so that IS a reference.
# Every form that quotes the delimiter suppresses expansion, not just the
# single-quoted one: `<<"EOF"` and `<<\EOF` do too, and each was letting a
# fixture body read as commands.
#
# The delimiter is a WORD, not an identifier, and one command line may open
# SEVERAL heredocs. Both of those, and the inertness of an operator inside
# quotes or arithmetic, are questions `check-docs-cli.sh` has already been
# through several rounds of review on — so the rules here are its rules,
# restated for this gate's need (blank a body, keep the line count) rather than
# re-derived. Copying half of a sibling's answer is how the previous two rounds
# of this went.
HEREDOC = re.compile(r'(?<!<)<<-?[ \t]*')


def _escaped(text, i):
    """True when the character at `i` is escaped — PARITY, not presence.

    A run of backslashes pairs off, so `\\\\<<EOF` is a literal backslash and a
    real operator while `\\<<EOF` is a literal `<`.
    """
    run = 0
    while i - run - 1 >= 0 and text[i - run - 1] == '\\':
        run += 1
    return run % 2 == 1


def _mask_inert(text):
    """`text` with quoted and arithmetic spans filled, length preserved.

    A `<<` inside either opens nothing: `printf '%s' "<<EOF"` is an argument
    and `$((1 << 2))` is a left shift. Length is kept so the decision stays
    positional — the operator is FOUND in the real text and TESTED here.

    A double-quoted span is not opaque, though. `probe="$(cat <<EOF)"` opens a
    real heredoc: the shell re-enters its own parsing inside `$( … )`, so the
    `<<` there is an operator and its body is data. Masking the outer span
    whole hid the opener, the body was never consumed, and its lines read as
    commands — the same re-entry rule `_blank_literals` already applies one
    rung over, which is where this one comes from rather than being re-derived.
    """
    out = list(text)
    _mask_from(text, out, 0, len(text), False)
    return ''.join(out)


def _mask_from(text, out, i, n, dq):
    """Mask the inert spans of `text[i:n]`; return where it stopped.

    `dq` says we are inside a double-quoted span, which ends at the next
    unescaped `"` and where an apostrophe is ordinary text rather than an
    opener. A substitution recurses with `dq` false: quoting starts fresh
    inside one, and so does everything else the shell reads.
    """
    while i < n:
        ch = text[i]
        if dq and ch == '\\':
            out[i] = 'x'
            if i + 1 < n:
                out[i + 1] = 'x'
            i += 2
            continue
        if dq and ch == '"':
            return i + 1
        if text[i:i + 3] == '$((':
            # Arithmetic is inert wherever it sits, quoted or not.
            depth, j = 0, i + 1
            while j < n:
                if text[j] == '(':
                    depth += 1
                elif text[j] == ')':
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            for k in range(i, min(j + 1, n)):
                out[k] = 'x'
            i = min(j + 1, n)
            continue
        if text[i:i + 2] == '$(':
            end = min(_group_end(text, i + 1, '(', ')'), n)
            _mask_from(text, out, i + 2, max(end - 1, i + 2), False)
            i = end
            continue
        if ch == '"':
            i = _mask_from(text, out, i + 1, n, True)
            continue
        if ch == "'" and not dq:
            end = text.find("'", i + 1)
            if end < 0 or end > n:
                end = n
            for k in range(i + 1, end):
                out[k] = 'x'
            i = end + 1
            continue
        if dq:
            out[i] = 'x'
        i += 1
    return i


def _heredoc_delim(text, i):
    """The delimiter word at `i` as `(word, quoted)`, or None if there is none.

    A delimiter is a shell WORD, and a word may be spelled in PIECES:
    `<<'END'.JSON` quotes the first half only, and quote removal joins the two.
    Any quoted piece suppresses expansion in the body, which is the whole
    question this answers. Returns None for the here-string `<<<EOF`, whose
    next character is an operator and so ends the word before it starts.
    """
    out, n, quoted = [], len(text), False
    while i < n:
        ch = text[i]
        if text[i:i + 2] == '$(':
            # A substitution is PART of the word: bash does not expand a
            # delimiter, so `<<EOF$(printf x)` waits for that literal spelling.
            depth, j = 0, i + 1
            while j < n:
                if text[j] == '(':
                    depth += 1
                elif text[j] == ')':
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            end = min(j, n - 1)
            out.append(text[i:end + 1])
            i = end + 1
            continue
        if ch in '\'"':
            j = text.find(ch, i + 1)
            if j < 0:
                break
            out.append(text[i + 1:j])
            quoted, i = True, j + 1
            continue
        if ch == '\\':
            if i + 1 < n:
                out.append(text[i + 1])
                quoted = True
            i += 2
            continue
        if ch.isspace() or ch in '<>|&;()':
            break
        out.append(ch)
        i += 1
    word = ''.join(out)
    return (word, quoted) if word else None


def _open_command(text):
    """True when `text` leaves a construct open that keeps bash reading.

    Runs on the masked copy, so a parenthesis or quote inside a string is
    already filled and cannot hold the command open by itself.
    """
    i, n, depth = 0, len(text), 0
    while i < n:
        c = text[i]
        if c == '\\':
            i += 2
            continue
        if c in '\'"':
            j = text.find(c, i + 1)
            if j < 0:
                return True
            i = j + 1
            continue
        if text[i:i + 2] == '$(':
            depth += 1
            i += 2
            continue
        if c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
        i += 1
    return depth > 0


def _heredoc_openers(line):
    """The `(delimiter, strips_tabs, expands)` triples a line opens, in order.

    ALL of them: `cat <<'ONE' <<'TWO'` opens two, bash consumes their bodies in
    that order, and keeping one scalar terminator resumed scanning inside the
    second body — so its data read as commands.
    """
    masked, found = _mask_inert(line), []
    for op in HEREDOC.finditer(line):
        if masked[op.start():op.start() + 2] != '<<' or _escaped(line, op.start()):
            continue
        delim = _heredoc_delim(line, op.end())
        if delim is None:
            continue
        found.append((delim[0], line[op.start():op.start() + 3] == '<<-',
                      not delim[1]))
    return found


def _shell_heredocs(body, code=False):
    """Blank the bodies of quoted heredocs, keeping line numbering intact.

    An UNQUOTED body is not blanked in the expansion view — `<<EOF` expands
    `${AUTUMN_X}`, so that is a real reference — but it IS consumed, because its
    length is what puts the next heredoc's body in the right place.

    `code` asks for the ASSIGNMENT view, where an unquoted body goes too: its
    expansions run, but its lines are data being written, not commands the
    shell executes, so `AUTUMN_X=v cmd` in one is not an assignment.
    """
    out, queue, logical = [], [], ''
    for line in body.splitlines():
        if not queue:
            out.append(line)
            # Bash collects EVERY delimiter on the logical command line before
            # it consumes any body, so `cat <<'ONE' \` + `<<'TWO'` opens two.
            # Reading the physical line took the continuation for the first
            # body and never queued `TWO`, after which its data read as
            # commands. The continuation test runs on the masked copy, so a
            # trailing backslash inside a quoted string does not join lines.
            masked = _mask_inert(line)
            if masked.endswith('\\') and not _escaped(masked, len(masked) - 1):
                logical += line[:-1]
                continue
            # A BACKSLASH is the only continuation that matters here, and that
            # is a fact about bash rather than a simplification. An earlier
            # round added `|`, `&&` and `|&` on the reasoning that an
            # unfinished pipeline keeps bash parsing, so it would collect the
            # next line's delimiters before consuming any body. Checked against
            # bash rather than reasoned about, that is false: a here-document
            # body begins after the next NEWLINE, and `cat <<'ONE' |` followed
            # by a body line pipes that body — the operator defers nothing. A
            # backslash is different because it splices the two physical lines
            # into one BEFORE tokenising, so both `<<`s really are on one line.
            #
            # The rule was inert on this tree — no tracked file opens a heredoc
            # on a line ending in an operator — but it left body text readable
            # as code, which is the direction that admits names. Removed rather
            # than extended, and the extension I had written for `$( … )` on
            # the same reasoning went with it.
            openers = _heredoc_openers(logical + line)
            # When the line leaves a substitution or quote OPEN, this cannot
            # say where the bodies belong: `cat <<OUTER $(` + `cat <<'INNER'`
            # is accepted by bash, and the inner text is demonstrably not
            # expanded (checked with the variable exported — it printed
            # literally), but which body owns which line is not something the
            # rules here derive. So the uncertainty is resolved toward
            # REPORTING: an opener on an unfinished line is treated as
            # non-expanding, its body blanked in both views. That can only drop
            # names, which shows up as a page reported — never as one passed.
            if _open_command(_mask_inert(logical + line)):
                openers = [(d, t, False) for d, t, _ in openers]
            queue.extend(openers)
            logical = ''
            continue
        delim, tabs, expands = queue[0]
        candidate = line.lstrip('\t') if tabs else line
        if candidate.rstrip('\r') == delim:
            out.append(line)
            queue.pop(0)
        else:
            out.append(line if expands and not code else '')
    return '\n'.join(out)


def _ps_uncommented(body):
    return PS_BLOCK.sub(lambda m: re.sub(r'[^\n]', ' ', m.group(0)), body)


# The block-only families, keyed by the opening delimiter `comment_leader`
# returns. `//` is deliberately NOT a CSS comment — it appears in every `url(…)`
# — and treating one as a comment would have hidden real text rather than
# prose.
BLOCK_CLOSER = {'/*': '*/', '<!--': '-->', '<%#': '%>', '<#--': '-->'}


def _block_uncommented(body, opener, closer):
    """Drop `opener … closer` comments, keeping line numbering intact.

    Length-preserving. An unterminated opener costs the rest of the file, which
    is what the language itself does with one.
    """
    out, i, n = list(body), 0, len(body)
    while True:
        start = body.find(opener, i)
        if start < 0:
            return ''.join(out)
        end = body.find(closer, start + len(opener))
        end = n if end < 0 else end + len(closer)
        for k in range(start, min(end, n)):
            if body[k] != '\n':
                out[k] = ' '
        i = end


LUA_LONG = re.compile(r'--\[(=*)\[')


def _lua_uncommented(body):
    """Drop Lua's `--` line comments AND its long comments.

    `--[[ … ]]` and the equals-delimited `--[==[ … ]==]` are the forms the SQL
    scanner cannot see: it stops a `--` at the line end, so everything after
    the first line of a long comment survived as code. The delimiter length is
    part of the syntax — `]]` does not close `--[==[` — so it is matched, not
    assumed.

    Length-preserving, newlines kept, so line numbering holds.
    """
    out, i, n = list(body), 0, len(body)
    while i < n:
        m = LUA_LONG.match(body, i)
        if m:
            close = ']' + m.group(1) + ']'
            end = body.find(close, m.end())
            end = n if end < 0 else end + len(close)
            for k in range(i, min(end, n)):
                if body[k] != '\n':
                    out[k] = ' '
            i = end
            continue
        if body[i:i + 2] == '--':
            while i < n and body[i] != '\n':
                out[i] = ' '
                i += 1
            continue
        if body[i] in '\'"':
            q, j = body[i], i + 1
            while j < n and body[j] != q:
                j += 2 if body[j] == '\\' else 1
            i = min(j, n) + 1
            continue
        i += 1
    return ''.join(out)


def _sql_uncommented(body):
    """Drop SQL's `--` line comments and `/* … */` blocks, keeping strings.

    A string is single-quoted and escapes its own quote by DOUBLING it, so
    `'it''s'` is one string and not two; an identifier is double-quoted. Neither
    holds a comment, which is the whole reason to track them here.

    Length-preserving, newlines kept, so line numbering holds.
    """
    out, i, n = list(body), 0, len(body)
    while i < n:
        c = body[i]
        if c in '\'"':
            j = i + 1
            while j < n:
                if body[j] == c:
                    if body[j + 1:j + 2] == c:
                        j += 2
                        continue
                    break
                j += 1
            i = min(j, n) + 1
            continue
        if body[i:i + 2] == '--':
            while i < n and body[i] != '\n':
                out[i] = ' '
                i += 1
            continue
        if body[i:i + 2] == '/*':
            j = body.find('*/', i + 2)
            j = n if j < 0 else j + 2
            for k in range(i, min(j, n)):
                if body[k] != '\n':
                    out[k] = ' '
            i = j
            continue
        i += 1
    return ''.join(out)


def _hash_uncommented(body, shell_like=False, carry_quotes=False,
                      escape='\\', raw_single=None):
    """Drop `#` comments, respecting the language's rule for where one starts.

    `shell_like` says a `#` opens a comment only at the start of a WORD, which
    holds for the shell family and for YAML but not for Python, TOML or HCL.
    `escape` is the language's escape character — a BACKTICK in PowerShell,
    where reading a backslash instead ended `"quote: `""` at the wrong quote and
    left the trailing comment standing as code. `raw_single` says single quotes
    are literal, so the escape is inert inside them; that travels with the
    shell family and with PowerShell, and defaults to `shell_like`.

    A word starts after whitespace and after a control operator — `true;# …` is
    a comment to bash, and requiring whitespace kept it. `${VAR#prefix}` and
    `$#` still survive, because `{` and `$` are neither.

    Escapes matter because without them `"a\\"b"` reads as a string that ENDS at
    the escaped quote and reopens at the real one, so a trailing comment lands
    inside an imaginary string and survives.

    Quote state is tracked per line rather than across the file: an unbalanced
    quote then costs one line, not the rest of the file. That bound is doing
    real work and is not a shortcut — the sibling gates embed their Python in
    `<<'PYEOF'` heredocs, and this pass runs before the heredoc one, so a single
    apostrophe inside that Python opens a span. Carrying quote state across
    lines unconditionally left **1192 comment lines** in the tree surviving as
    code, every one of them able to bless a name. Measured before, not after.

    `carry_quotes` is the one place a quote may legitimately continue: a YAML
    quoted scalar. It is carried only when the open quote began at a VALUE
    position — straight after a `:` or `-`, or as the whole of the value —
    because that is the only place YAML lets a quoted scalar start. An
    apostrophe inside an unquoted scalar (`name: it's fine`) opens nothing,
    which is what stops one stray quote swallowing every comment below it.
    """
    if raw_single is None:
        raw_single = shell_like
    out, carry, qpos = [], None, 0
    for l in body.splitlines():
        q, cut, esc, start = carry, None, False, 0
        if carry is not None:
            # Inside a scalar that opened on an earlier line: everything up to
            # its closing quote is string, and the line is code again after it.
            end = l.find(carry)
            if end < 0:
                out.append(l)
                continue
            q, start = None, end + 1
        for i in range(start, len(l)):
            c = l[i]
            if esc:
                esc = False
                continue
            if c == escape and not (raw_single and q == "'"):
                esc = True
                continue
            if q:
                if c == q:
                    q = None
            elif c in '"\'':
                q, qpos = c, i
            elif c == '#' and (not shell_like or i == 0
                               or l[i - 1].isspace() or l[i - 1] in ';&|()<>'):
                cut = i
                break
        # An UNTERMINATED quote does not protect a `#` later on the same line.
        # A multi-line string's CLOSING quote reads as a new opener here, and
        # the trailing comment after it then survived as code — the assignment
        # rungs took `# AUTUMN_X=v cmd` out of it. Carrying quote state across
        # lines is the principled repair and is not affordable: measured, it
        # leaves 1192 comment lines surviving as code, and a heredoc-aware
        # version 3448, because the sibling gates embed their Python in
        # heredocs full of apostrophes. This is stateless instead, and fails
        # toward STRIPPING — three lines in the tree lose comment text they
        # were keeping, none of which names a variable.
        if cut is None and q is not None:
            for i in range(qpos + 1, len(l)):
                if l[i] == '#' and (l[i - 1].isspace() or l[i - 1] in ';&|()<>'):
                    cut = i
                    break
        out.append(l if cut is None else l[:cut])
        before = l[:qpos].rstrip() if q else ''
        carry = (q if carry_quotes and q and cut is None
                 and (before == '' or before[-1] in ':-') else None)
    return '\n'.join(out)


def hash_needs_space(rel):
    """Whether this file type needs whitespace before a `#` to open a comment."""
    return effective_suffix(rel) in HASH_NEEDS_SPACE


def uncommented(body, leader='//', needs_space=False, also_slash=False,
                also_block=False, carry_quotes=False):
    """Drop comments, keeping string literals and line numbering intact."""
    if leader == '//':
        return _rust_uncommented(body)
    if leader == '--':
        return _sql_uncommented(body)
    if leader == 'lua':
        return _lua_uncommented(body)
    if leader in BLOCK_CLOSER:
        return _block_uncommented(body, leader, BLOCK_CLOSER[leader])
    if leader == '#':
        if also_block:
            body = _ps_uncommented(body)
        if also_slash:
            body = _rust_uncommented(body)
        # PowerShell's escape character is a BACKTICK, not a backslash:
        # `"quote: `""` ends where the backtick says it does. Reading a
        # backslash as the escape ended the string early, made the real
        # terminator look like a new opener, and left the trailing comment —
        # and its `getenv("AUTUMN_…")` — standing as code. `also_block` is
        # already the flag that says this file is PowerShell.
        return _hash_uncommented(body, needs_space, carry_quotes,
                                 '`' if also_block else '\\',
                                 needs_space or also_block)
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
    test_files = test_module_files(root)
    pats = {}
    for rel in out.split('\0'):
        # Same exclusions as the token sweep, for the same reason and in the
        # same order: a comment is not code, and a name a test builds is not a
        # name the runtime builds. Applied here as well because splitting a rule
        # across the two readers of the tree is how the last two rounds' defects
        # got in — and the split had survived one rung deeper than that fix
        # reached. A module is test-only when a `#[cfg(test)] mod x;` says so
        # and nothing in the FILE does: `autumn/src/cluster/tests.rs` is read as
        # production here while the token sweep skips it, so a `format!` in it
        # would bless documentation globally. Measured: the five templates are
        # unchanged by both exclusions.
        if not rel or test_code(rel, test_files):
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
        # The generated-data mask again — third rung to consult it, after the
        # accessor and the binding scans. A one-line `const HELP: &str =
        # r#"format!("AUTUMN_…{upper}…")"#;` is data, and it was defining a
        # runtime-built NAME PATTERN, which blesses every documented name that
        # matches it rather than just one.
        masked = untested(uncommented(body))
        data = _generated_data(masked)
        for tpl in (m.group(1) for m in TEMPLATE.finditer(masked)
                    if not data[m.start()]):
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
#
# `export` has to BE the command word, for the same reason an assignment does.
# `printf %s export AUTUMN_X=v` passes two arguments to `printf` and exports
# nothing — and this rung matched at every whitespace boundary, so it counted.
# The positional rule was written for prefix assignments one round earlier and
# not carried to the neighbour asking the same question, which is this script's
# most repeated mistake and now has a second entry in its own header.
class _ExportAssignment:
    """`export NAME=…`, where `export` is the command being run."""

    _start = re.compile(r'(?:^|[;&|(\s])(export)\s+(AUTUMN_[A-Z0-9_]+)=')

    def findall(self, body):
        return [m.group(2) for m in self._start.finditer(body)
                if _at_command_word(body, m.start(1),
                                    _PrefixAssignment._word_end)]


ASSIGNED = _ExportAssignment()
def _dquote_end(body, i):
    """Index just past the `"` closing the double quote that opens at `i`."""
    n, i = len(body), i + 1
    while i < n:
        if body[i] == '\\':
            i += 2
            continue
        if body[i] == '"':
            return i + 1
        i += 1
    return n


def _group_end(body, i, opener, closer):
    """Index just past the `closer` matching the `opener` at `i`.

    Quotes inside are consumed whole, so `$(echo ")")` closes where the shell
    closes it and not at the parenthesis in the string.
    """
    n, depth = len(body), 0
    while i < n:
        c = body[i]
        if c == '\\':
            i += 2
            continue
        if c == "'":
            j = body.find("'", i + 1)
            i = n if j < 0 else j + 1
            continue
        if c == '"':
            i = _dquote_end(body, i)
            continue
        if c == opener:
            depth += 1
        elif c == closer:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


# Words that may precede an assignment without being the command name. Each was
# RUN under bash rather than reasoned about, and the first list — written from
# intuition — was wrong in three places: `exec`, `command` and `nohup` do NOT
# consume an assignment word. Each of those tries to execute a program named
# `AUTUMN_X=value` and exits 127, so an assignment after them reaches nothing.
#
#   env  sudo  time            exported the variable (verified, rc=0)
#   if  then  else  do  while  until  !   a new command starts after them
#   exec  command  nohup       "command not found", exit 127 — removed
#
# `elif` is not separately tested; it is the same reserved-word position as
# `then` and `else`, both of which are.
ASSIGN_WORD = re.compile(r'[A-Za-z_][A-Za-z0-9_]*=')
PASS_THROUGH = ('env', 'sudo', 'time',
                'if', 'then', 'elif', 'else', 'do', 'while', 'until', '!')


def _at_command_word(body, at, word_end):
    """Whether `at` is the command-word position of its simple command.

    Everything between the command boundary and `at` must be an assignment
    word or one of the few words that do not consume one.
    """
    i = max((body.rfind(c, 0, at) for c in ';&|()\n'), default=-1) + 1
    while i < at:
        while i < at and body[i] in ' \t':
            i += 1
        if i >= at:
            break
        word = ASSIGN_WORD.match(body, i)
        if word:
            i = word_end(body, word.end())
            continue
        j = i
        while j < len(body) and body[j] not in ' \t\n;&|()':
            j += 1
        if body[i:j] not in PASS_THROUGH:
            return False
        i = j
    return i == at


class _PrefixAssignment:
    """`NAME=value cmd` — where the VALUE is one shell word that may hold
    spaces, and only a word after it makes this an assignment reaching a
    process.

    Spelled as a scanner rather than a regex because a regex can hold quotes
    but not substitutions. In `AUTUMN_LOG__LEVL=$(printf '%s %s' a b)` the
    value spans four apparent words and there is no following command at all;
    a pattern that stopped at the first space read `a b)` as one, and so read
    a script-local variable as one handed to a process. Command substitutions,
    backticks, `${…}`, both quote forms and backslash escapes are consumed as
    part of the word, so the test for a following command asks about the real
    next word.

    Named `findall` because it stands among regexes at every call site and the
    callers should not care which it is.
    """

    # The separator before the name is a shell one; the separator AFTER the
    # value is explicitly not a newline — `\s` spans line breaks, so applied to
    # a whole file this matched a bare assignment against the first word of the
    # NEXT line and read it as a prefix form.
    _start = re.compile(r'(?:^|[;&|(\s])(AUTUMN_[A-Z0-9_]+)=')
    # What follows the value without being a command: a list or pipeline
    # operator, the end of the line, or a comment. `AUTUMN_X=1 ; cmd` sets a
    # variable for the shell, not for `cmd`.
    _not_a_command = ';&|)\n#'
    # A REDIRECTION is not a command either, and it was the one non-command
    # this set could not name: `AUTUMN_X=1 > /tmp/out` is a null command — bash
    # opens the file, assigns in the CURRENT shell and starts no process, so
    # nothing is exported. `>` and `<` were simply absent, so the operator read
    # as the following command's first word.
    #
    # They cannot just join the set, because a redirection may also PRECEDE a
    # real command: `AUTUMN_X=1 >out cmd` does run `cmd` with the variable in
    # its environment. So the operator and its target word are consumed and the
    # question is asked again after them — which is what the shell does.
    _redirect = re.compile(r'[0-9]*(?:&>>?|>>|>\||>&|<&|<>|>|<)')

    # An assignment word only assigns where the shell reads one: BEFORE the
    # command name. After it, `NAME=value` is an ordinary argument —
    # `printf '%s' AUTUMN_X=v cmd` prints the text and exports nothing. A
    # preceding space was standing in for "at the front of a simple command",
    # which is a claim about position that whitespace cannot make.
    #
    # A few words are stepped over rather than treated as the command name —
    # see `PASS_THROUGH`. Anything else ends the assignment prefix, which drops
    # names rather than admitting them.
    def _prefixes_a_command(self, body, name_at):
        """Whether the name at `name_at` sits before its command's name."""
        return _at_command_word(body, name_at, self._word_end)

    def findall(self, body):
        out, n = [], len(body)
        for m in self._start.finditer(body):
            if not self._prefixes_a_command(body, m.start(1)):
                continue
            end = self._word_end(body, m.end())
            gap = end
            while gap < n and body[gap] in ' \t':
                gap += 1
            if gap == end:
                continue
            while True:
                red = self._redirect.match(body, gap)
                if not red:
                    break
                gap = self._word_end(body, self._skip_blanks(body, red.end()))
                gap = self._skip_blanks(body, gap)
            if gap < n and body[gap] not in self._not_a_command:
                out.append(m.group(1))
        return out

    @staticmethod
    def _skip_blanks(body, i):
        while i < len(body) and body[i] in ' \t':
            i += 1
        return i

    @staticmethod
    def _word_end(body, i):
        """Index just past the one shell word starting at `i` (possibly empty)."""
        n = len(body)
        while i < n:
            c = body[i]
            if c in ' \t\n' or c in ';&|)':
                break
            if c == '\\':
                i += 2
                continue
            if c == "'":
                j = body.find("'", i + 1)
                i = n if j < 0 else j + 1
                continue
            if c == '"':
                i = _dquote_end(body, i)
                continue
            if c == '`':
                j = body.find('`', i + 1)
                i = n if j < 0 else j + 1
                continue
            if c == '$' and body[i + 1:i + 2] == '(':
                i = _group_end(body, i + 1, '(', ')')
                continue
            if c == '$' and body[i + 1:i + 2] == '{':
                i = _group_end(body, i + 1, '{', '}')
                continue
            i += 1
        return i


ASSIGNED_PREFIX = _PrefixAssignment()

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
# A single `ENV` may declare SEVERAL variables across a line continuation —
# `ENV AUTUMN_ONE=1 \` then `AUTUMN_TWO=2` — and anchoring to the start of the
# line saw only the first. `DECLARED_CONT` reads the rest, and the caller
# supplies the continuation state, because whether a line is a continuation is
# a property of the line ABOVE it.
DECLARED = re.compile(r'^\s*(?:ARG|ENV)\s+(AUTUMN_[A-Z0-9_]+)')
DECLARED_CONT = re.compile(r'(?:^|\s)(AUTUMN_[A-Z0-9_]+)=')

# A dotenv file's ENTIRE grammar is `NAME=value`: there is no command for a
# prefix assignment to reach and nothing script-local to confuse it with, so the
# bare form is a declaration exactly as `ENV` is in a Dockerfile. Without this a
# name defined only in `.env.example` could not be documented without a waiver.
DOTENV = ('.env', '.example')
# …and the `$` that starts it must not already belong to something else. `$$` is
# the PID, so `$$AUTUMN_X` is a number followed by literal text and reads no
# variable at all — but the scan could start at the SECOND dollar and take the
# name. The lookbehind is the whole fix, and it points at a general hazard in a
# pattern that begins mid-token: a match is only evidence if it starts where the
# construct does. (Compose spells an escaped literal `$` the same way, so the
# exclusion is right for both consumers.)
EXPANDED = re.compile(r'(?<!\$)\$\{?(AUTUMN_[A-Z0-9_]+)\}?')

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
# `set_var` is here and `remove_var` is not, which is a distinction about what
# the act means. Setting a variable PUBLISHES the name into the environment for
# something else to read — the same act as `export` in a shell script, which
# this rung has always counted — and the one name that reaches the truth set
# only that way is real: the generated Tauri shell exports
# `AUTUMN_SYNC__DB_PATH` for the app's own routes, and
# `tauri-mobile-offline-sync.md` shows the read it is exported for
# (`SyncStore::open(std::env::var("AUTUMN_SYNC__DB_PATH")?)`) in application
# code, which by definition does not live in this repository. Removing a
# variable publishes nothing, and costs nothing to drop: measured, no name in
# the tree depends on it.
#
# The residual exposure is real and worth stating: a MISSPELLED `set_var` would
# bless that spelling, exactly as a misspelled `export` in a tracked shell
# script would. That is a property of counting a publish as evidence, not an
# oversight, and closing it would mean dropping the four correct
# `AUTUMN_SYNC__DB_PATH` occurrences above.
# The std accessors are QUALIFIED, because `var` on its own is not an
# environment API — it is any Rust function named `var`, and this tree has
# hundreds of them: `var(--primary)` in the CSS the admin plugin emits from a
# Rust string. Nothing was resolved through those today, but a bare-name rule
# was one plausible call from blessing an argument that is nobody's variable.
#
# What it needs is a RECEIVER or a path, not the std path specifically. The
# first cut required `std::env::` and dropped 27 real names, because this
# tree's own `Env` trait is called as a method — `env.var("AUTUMN_MASTER_KEY")`,
# and in tests `off.var(…)` / `unset.var(…)` on differently-named bindings. So
# the rule is that something owns the call: `env::var(…)` or `‹receiver›.var(…)`
# reads the environment, and a bare `var(…)` reads whatever function is in
# scope. Measured at each step, which is the only reason the 27 came back.
#
# …and a RECEIVER is not automatically an environment. `.var(` on anything at
# all was the next cut and it is still too wide: `css.var("AUTUMN_X")` is a
# method on a stylesheet builder. The receivers are read out of the tree the
# same way the helper names are — `impl Env for T` gives the types, and a
# declaration whose type mentions `Env` gives the bindings, which is what
# `denv: Box<dyn Env>` and the `inner` field of a wrapper need. The static
# pattern below keeps an `env`-named receiver as its FLOOR, so a derivation
# that finds nothing still reads the common spelling.
ACCESSOR = re.compile(r'\b(?:std::)?env::(var|var_os|set_var)\s*\('
                      r'|\b(env\w*)\s*\.\s*(?:var|var_os|set_var)\s*\('
                      r'|\b(env_trimmed|parse_env\w*|env_bool\w*|getenv)\s*\('
                      r'|\b(env\w*)\.get\s*\(')

# `impl Env for OsEnv` — the types that ARE an environment.
ENV_IMPL = re.compile(r'\bimpl\s*(?:<[^<>]*>)?\s*Env\b[^{]*?\bfor\s+([A-Za-z_]\w*)')
# …and any binding, parameter or field whose type mentions one.
ENV_BOUND = re.compile(r'\b([a-z_]\w*)\s*:\s*[^,;{}()=]*\bEnv\b')
# A GENERIC parameter bounded by `Env` is an environment too, and a field
# declared with it mentions no `Env` at all: `struct ForcedProfileEnv<E: Env>`
# has `inner: E`, whose `self.inner.var(key)` is a real read. Derived per file,
# since a type parameter is as local as a binding.
# What a file imports by name, so a module-scoped derivation can be resolved
# where it is USED and not only where it is written.
USE_TREE = re.compile(r'\buse\s+([^;]+);')
USE_NAME = re.compile(r'[A-Za-z_]\w*')
USE_ALIAS = re.compile(r'\b([A-Za-z_]\w*)\s+as\s+([A-Za-z_]\w*)')
ENV_GENERIC = re.compile(r'[<,]\s*([A-Z]\w*)\s*:\s*[^,>]*\bEnv\b'
                         r'|\bwhere\s+([A-Z]\w*)\s*:\s*[^,;{]*\bEnv\b')

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
# The generic parameter list is optional and must be skipped: the house
# helpers are written `fn parse_env<T: std::str::FromStr>(env: &dyn Env, key:
# &str, …)`, and requiring `(` straight after the name derived none of them —
# which silently moved their key to position 0 and cost 85 names.
ENV_HELPER = re.compile(r'\bfn\s+(\w+)\s*(?:<(?:[^<>]|<[^<>]*>)*>)?\s*'
                        r'\(([^)]*\benv\s*:\s*&[^)]*\bkey\s*:\s*&str)', re.S)


def accessor(root, test_files=frozenset()):
    """The accessor pattern, and where each one takes its KEY.

    The key is an argument POSITION, not "the first string literal in the
    call": `set_var(key, "AUTUMN_LOG__LEVL")` passes its key in a variable and
    its value as a literal, and reading the literal blessed a name that is
    nobody's variable. The std accessors take the key first; the helpers take it
    wherever their signature says, which is read out of the tree along with
    their names — `env_trimmed(env, key)` second, `override_string(target, env,
    key)` third.
    """
    out = subprocess.run(['git', 'ls-files', '-z', '*.rs'], cwd=root,
                         capture_output=True, text=True).stdout
    index, types, per_file, masked_all = {}, set(), {}, {}
    helpers, declared, imports, aliases = {}, {}, {}, {}
    for rel in out.split('\0'):
        # Test code is skipped outright, as the scan that consumes this index
        # already does — a receiver bound only in a test is used only in a
        # test, and that call is masked before it is ever read.
        if not rel or test_code(rel, test_files):
            continue
        try:
            body = (root / rel).read_text(encoding='utf-8', errors='replace')
        except OSError:
            continue
        # Masked exactly as the scan that CONSUMES this index is: a signature
        # inside a comment, inside test-only code, or inside generated DATA
        # defines nothing the runtime calls. Unmasked, one commented-out `fn
        # label_lookup(env: &FakeEnv, key: &str)` registered `label_lookup` as
        # an accessor for the WHOLE TREE, so an ordinary two-argument call
        # anywhere blessed its second argument. A derivation that reads more
        # of the file than the rung it feeds is the same shape as reading a
        # `format!` lookalike out of a comment — the fourth rung to need this
        # mask, and the first where the leak was global rather than local.
        masked = untested(_rust_uncommented(body))
        data = _generated_data(masked)
        for m in ENV_HELPER.finditer(masked):
            if data[m.start()]:
                continue
            index[m.group(1)] = len(_split_args(m.group(2))) - 1
            helpers.setdefault(rel, set()).add(m.group(1))
        masked_all[rel] = (masked, data)
        own = {m.group(1) for m in ENV_IMPL.finditer(masked)
               if not data[m.start()]}
        types.update(own)
        declared[rel] = own
        use_text = ' '.join(USE_TREE.findall(masked))
        imports[rel] = set(USE_NAME.findall(use_text))
        # `use …::OsEnv as AliasedEnv` puts BOTH names in the identifier set,
        # and only the original is a derived type — so the local name, which is
        # the one actually written at the call site, resolved to nothing.
        aliases[rel] = {local: orig
                        for orig, local in USE_ALIAS.findall(use_text)}
        names = per_file.setdefault(rel, set())
        names.update(m.group(1) for m in ENV_BOUND.finditer(masked)
                     if not data[m.start()])
        params = {g for m in ENV_GENERIC.finditer(masked) if not data[m.start()]
                  for g in m.groups() if g}
        if params:
            names.update(re.findall(
                r'\b([a-z_]\w*)\s*:\s*&?\s*(?:'
                + '|'.join(sorted(map(re.escape, params))) + r')\s*[,;})]',
                masked))
    # A type that IS an environment can be the receiver itself (`OsEnv.var(…)`),
    # and a binding of one is too — `let denv = DotenvEnv::new(…)` has no
    # annotation to read, so the types are matched on the right of a `let` as
    # well. Every receiver this tree actually uses is derived; `css` is not.
    #
    # This second pass reads the SAME masked text as the first. Reading the raw
    # file here — which is what it did first — let a `let css = OsEnv::new()`
    # written in a comment, a string, or a test add `css` to the accessor regex
    # for the whole tree. That is the fourth time a derivation has been wider
    # than the rung it feeds, and the second time in this one function: the
    # inputs of a rung are the rung, however many passes they take.
    # A TYPE name is module-scoped too. Saying otherwise was my mistake one
    # round ago: two modules may each define a `RootedEnv`, and only one of
    # them is an environment. A file may use a type it declares, or one it
    # imports by name — nothing else.
    def visible(rel, names):
        here = {n for n in names
                if n in declared.get(rel, ()) or n in imports.get(rel, ())}
        return here | {local for local, orig in aliases.get(rel, {}).items()
                       if orig in names}

    for rel, (masked, data) in masked_all.items():
        here = visible(rel, types)
        if not here:
            continue
        bound = re.compile(
            r'\blet\s+(?:mut\s+)?([a-z_]\w*)\s*(?::[^=;]*)?=\s*&?\s*(?:'
            + '|'.join(sorted(map(re.escape, here))) + r')\b')
        per_file.setdefault(rel, set()).update(
            m.group(1) for m in bound.finditer(masked)
            if not data[m.start()])
    # A RECEIVER NAME is file-local, and the pattern was global: `base` is
    # derived from `let base = OsEnv` in one module, and an unrelated module's
    # `base.var(…)` on something else then read as an environment call. A type
    # name is global (it is the same type wherever it is written); a binding is
    # not. So the receiver alternative is built per file, cached by the set of
    # names, and every file that derives none falls back to the `env`-named
    # floor in `ACCESSOR`.
    # Everything derived from the tree is MODULE-SCOPED — a helper function, a
    # type, a binding. Rust says so and I did not: each was registered
    # tree-wide, so `override_string` defined in the media plugin decided what
    # a same-named three-argument function meant in every other crate. A file
    # sees what it declares and what it imports by name; the static floor in
    # `ACCESSOR` is what every file always has.
    # An aliased helper keeps the ORIGINAL's key position: `use … as f` renames
    # the call, not the signature.
    for rel, alias in aliases.items():
        for local, orig in alias.items():
            if orig in index:
                index.setdefault(local, index[orig])
    cache = {}

    def compiled(rel):
        here = frozenset(visible(rel, set(index)) | set(helpers.get(rel, ())))
        recv = frozenset(set(per_file.get(rel, ())) | visible(rel, types))
        if (here, recv) not in cache:
            pattern = ACCESSOR.pattern
            if here:
                pattern += (r'|\b(' + '|'.join(sorted(map(re.escape, here)))
                            + r')\s*\(')
            if recv:
                pattern += (r'|\b(?:' + '|'.join(sorted(map(re.escape, recv)))
                            + r')\s*\.\s*(var|var_os|set_var)\s*\(')
            cache[(here, recv)] = re.compile(pattern)
        return cache[(here, recv)]

    return compiled, index


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

# A module can be test-only without any of its own contents saying so:
# `cluster/mod.rs` declares `#[cfg(test)] mod tests;` and the whole of
# `cluster/tests.rs` is then test code, though nothing in that FILE is marked.
# Masking its `#[test]` functions left every helper beside them reading as
# production. The declarations are read out of the tree rather than matched by
# filename, so a module named anything is covered and a `tests.rs` that is NOT
# declared test-only keeps being read.
TEST_MOD = re.compile(r'#\[cfg\(([^\]]*)\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;')


_TEST_MODULE_FILES = {}


def test_module_files(root):
    """Files a `#[cfg(…test…)] mod x;` declaration makes test-only.

    Cached: two rungs ask for this now, and answering costs a skeleton pass
    over every `#[cfg(`-bearing Rust file in the tree.
    """
    key = str(root)
    if key in _TEST_MODULE_FILES:
        return _TEST_MODULE_FILES[key]
    out = subprocess.run(['git', 'ls-files', '-z', '*.rs'], cwd=root,
                         capture_output=True, text=True).stdout
    files = set()
    for rel in out.split('\0'):
        if not rel:
            continue
        try:
            body = (root / rel).read_text(encoding='utf-8', errors='replace')
        except OSError:
            continue
        if '#[cfg(' not in body:
            continue
        skel = _rust_skeleton(body)
        parent = pathlib.PurePath(rel).parent
        for pred, name in TEST_MOD.findall(skel):
            if _cfg_truth(pred)[0]:
                continue
            files.add(str(parent / f'{name}.rs'))
            files.add(str(parent / name / 'mod.rs'))
    _TEST_MODULE_FILES[key] = files
    return files


def test_code(rel, test_files):
    """Whether a path's contents are test code — by location or declaration.

    One predicate because two rungs ask the question, and a rule that lives in
    two places is a rule that gets fixed in one of them. That is exactly what
    happened: the token sweep learned about `#[cfg(test)] mod x;` modules and
    `built_patterns` did not, leaving `autumn/src/cluster/tests.rs` able to
    define a name pattern that blesses documentation everywhere.
    """
    # Through `.tmpl`, like every other path rule here: `TEST_PATH`'s directory
    # arm already catches `templates/tests/integration_test.rs.tmpl`, but its
    # `_test.rs$` arm would not have seen a `foo_test.rs.tmpl`. Nothing in the
    # tree is spelled that way today; this is the same latent shape as the two
    # `rel.endswith('.rs')` tests, closed at the same time rather than waiting
    # for a file to arrive that exercises it.
    p = pathlib.PurePath(rel)
    bare = str(pathlib.PurePath(p.parent) / p.stem) if p.suffix == '.tmpl' else rel
    return bool(TEST_PATH.search(bare)) or rel in test_files


# A test function does not need a `cfg` to be test code: `#[test]` marks one on
# its own, and 692 of them sit outside any `#[cfg(test)]` region here — the
# whole of `cluster/tests.rs`, and the test blocks in `release.rs`,
# `generate/auth.rs`, `new.rs` and the two macro crates. Masking only the cfg
# regions left every one of those reading as production code.
#
# `#[cfg(test)]` cannot match this: the name must be `test` itself, optionally
# behind a path (`tokio::test`) and optionally taking arguments
# (`tokio::test(flavor = "multi_thread")`).
TEST_ATTR = re.compile(r'#\[(?:[a-z_]+::)*(?:sim_)?test(?:\([^\]]*\))?\]')


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


# How far an argument list may run before this stops trying to find its end. A
# parenthesis inside a string literal — `env::var("AUTUMN_X(")` — is counted
# like any other here, because the accessor itself may live inside a string:
# `generate/admin.rs` emits `std::env::var(\"AUTUMN_TEST_ADMIN_SESSION\")` into
# generated code, and that is a real read by the code it writes. The limit
# bounds what a miscount can swallow.
ARG_SPAN_LIMIT = 500

# Only one argument in a call is the KEY, and taking every quoted name in the
# list made the VALUE one too: `set_var("RUST_LOG", "AUTUMN_LOG__LEVL")` blessed
# a name that is a value assigned to somebody else's variable.
#
# The key is the first argument that is ENTIRELY a string literal. That is a
# rule about the calls rather than a table of arities, and it reads every shape
# here: `var("KEY")`, `set_var("KEY", value)`, `env.get("KEY")`,
# `parse_env(env, "KEY", &mut target)` and
# `override_string(&mut target, env, "KEY")` — the helpers put the key second or
# third, and in none of them does a bare string literal come before it.
# A RAW string is a string literal too. `std::env::var(r"AUTUMN_X")` is an
# ordinary read, and accepting only the escaped spelling made its key invisible,
# so a variable implemented solely that way had its correct page REPORTED. That
# direction is loud rather than silent, which is why it survived this long — but
# a rung that cannot see a valid spelling is the same defect either way.
STRING_ARG = re.compile(r'^(?:"(?:[^"\\]|\\.)*"|r(#*)"[\s\S]*?"\1)$')


def _split_args(text):
    """Top-level arguments of a call: commas outside brackets and strings."""
    parts, depth, quote, esc, start = [], 0, False, False, 0
    for i, c in enumerate(text):
        if esc:
            esc = False
        elif c == '\\':
            esc = True
        elif quote:
            quote = c != '"'
        elif c == '"':
            quote = True
        elif c in '([{<':
            depth += 1
        elif c in ')]}>':
            depth -= 1
        elif c == ',' and depth == 0:
            parts.append(text[start:i])
            start = i + 1
    parts.append(text[start:])
    return parts


def key_argument(args, position=0):
    """The argument at the accessor's KEY position, if it is a literal.

    Escaped quotes are normalised first: `generate/admin.rs` writes a real
    `std::env::var(\\"AUTUMN_TEST_ADMIN_SESSION\\")` into generated code, where
    the whole literal reaches this scan backslash-escaped.

    A position rather than "the first literal anywhere in the call", because
    `set_var(key, \"AUTUMN_LOG__LEVL\")` passes its key in a variable and its
    value as a literal — and the value is nobody's variable name.
    """
    parts = _split_args(args)
    if position >= len(parts):
        return ''
    a = parts[position].strip().replace('\\"', '"')
    return a if STRING_ARG.match(a) else ''


def _balanced(s, i, limit=None):
    """The text inside the parenthesis at `s[i]`, and the index after it."""
    depth = 0
    for j in range(i, min(len(s), i + limit) if limit else len(s)):
        if s[j] == '(':
            depth += 1
        elif s[j] == ')':
            depth -= 1
            if depth == 0:
                return s[i + 1:j], j + 1
    return None, len(s)


def _test_items(skel):
    """Where each test item starts: a test-only `cfg`, or a test function."""
    for m in CFG_ATTR.finditer(skel):
        pred, after = _balanced(skel, m.end() - 1)
        if pred is not None and not _cfg_truth(pred)[0]:
            yield m.start(), after
    for m in TEST_ATTR.finditer(skel):
        yield m.start(), m.end()


def untested(body):
    """Blank every test item, keeping line numbering intact."""
    if '#[cfg(' not in body and '#[' not in body:
        return body
    skel = _rust_skeleton(body)
    masked = []
    for start, after in _test_items(skel):
        depth, end, group, angle = 0, None, 0, 0
        for i in range(after, len(skel)):
            c = skel[i]
            if c in '([{':
                # A comma INSIDE a group is punctuation — `fn f(a: u8, b: u8)`.
                # Counting the group rather than latching a flag is what lets
                # the comma after a closed group still end the item: a
                # `#[cfg(test)]` match arm is `0 => var("X"),`, and a latched
                # flag ignored that comma and masked the arms after it.
                group += 1
                if c == '{':
                    depth += 1
            elif c in ')]':
                # Clamped, because the scan starts inside the attribute and its
                # own closing `]` would otherwise take the count negative — and
                # a negative count made every following comma look top-level.
                group = max(0, group - 1)
            elif c == '<' and i > after and (skel[i - 1].isalnum()
                                             or skel[i - 1] in '_:'):
                # Generic, not a comparison: `Map<A, B>` opens, `1 < 2` does not.
                angle += 1
            elif c == '>' and angle:
                angle -= 1
            elif c == '}':
                if depth == 0:
                    # The ENCLOSING block closed first, so this was a field or a
                    # variant. Ending here is what bounds the over-mask:
                    # `#[cfg(test)] release_count: Option<Arc<…>>,` in
                    # `scheduler.rs` drove the depth NEGATIVE, and the old rule
                    # — break when the depth returns to zero — then ended the
                    # item two blocks later. Not a runaway to end of file, but
                    # twice the lines it should be: 67 masked in `scheduler.rs`
                    # against 34, and a real read among them is dropped.
                    end = i
                    break
                depth -= 1
                if depth == 0:
                    end = i
                    break
            elif c == ';' and depth == 0:
                # `#[cfg(test)] use uuid::Uuid;` — an item with no block, which
                # a brace search alone would run to the end of the file.
                end = i
                break
            elif c == ',' and depth == 0 and group == 0 and angle == 0:
                # `#[cfg(test)] pub name: String,` — a field, an enum variant or
                # a match arm, each of which ends at its comma.
                end = i
                break
        masked.append((start, len(skel) if end is None else end))
    if not masked:
        return body
    lines, out, pos = body.splitlines(), [], 0
    for l in lines:
        span = (pos, pos + len(l))
        out.append('' if any(a <= span[1] and span[0] <= b for a, b in masked)
                   else l)
        pos += len(l) + 1
    return '\n'.join(out)


# The formats that actually perform shell-style expansion or declare
# environment variables as `NAME=value`. An ALLOW-list, because the shapes are
# grammar: `${AUTUMN_X}` in a JavaScript template literal interpolates a JS
# variable, in a Rust string it interpolates nothing, and in a `.golden` fixture
# it is captured output — none of them is evidence that the runtime reads a
# name. Excluding only Rust left every one of those blessing documentation.
#
#   .sh/.bash/.zsh   the shell itself
#   .env/.example    dotenv, whose whole grammar is `NAME=value`
#   .yml/.yaml       compose `environment: - NAME=v`, `${NAME}`, and CI `run:`
#                    blocks, which are shell
#   Dockerfile*      `ARG`, `ENV`, `${…}` — matched by NAME, because
#                    `Dockerfile.api.tmpl` has the effective suffix `.api`
#   .ps1/.psm1       `$env:NAME`, read below
#   .tf/.tfvars/.hcl HCL interpolates `${…}` in strings
#
# Deliberately NOT extended to `.conf`, `.service` or `.properties` on the
# theory that they might: no such file in this tree names an `AUTUMN_*`
# variable, and guessing at a format's semantics is what this list replaces. If
# one appears, it fails CLOSED — a correct page gets reported, which is visible
# — rather than open.
SHELL_SHAPED = ('.sh', '.bash', '.zsh', '.env', '.example', '.yml', '.yaml',
                '.tf', '.tfvars', '.hcl', '.ps1', '.psm1')
SHELL_SHAPED_NAMED = ('Dockerfile', 'Containerfile', 'Makefile', 'Justfile')

# PowerShell reads and writes the environment as `$env:NAME` — not `$NAME`,
# which is an ordinary variable. `scripts/install.ps1` does it three times in
# its parameter block, so an override implemented only in PowerShell could not
# be documented without a waiver.
# Both spellings reach the environment: `$env:NAME` and the braced
# `${env:NAME}`, which is the form needed whenever the name would otherwise run
# on into the next token.
PS_ENV = re.compile(r'\$\{?[Ee][Nn][Vv]:(AUTUMN_[A-Z0-9_]+)\}?')


def shell_shapes(rel):
    """Whether the SHELL use-shapes apply to this file at all.

    `NAME=…`, `export NAME=…`, `ARG/ENV NAME` and `${NAME}` are shell grammar,
    and a format that does not interpret them is not offering evidence by
    containing them. Read through `.tmpl` for the same reason
    `comment_leader` does — `main.rs.tmpl` is Rust, and the previous
    `rel.endswith('.rs')` test did not see it.
    """
    p = pathlib.PurePath(rel)
    if p.suffix == '.tmpl':
        p = pathlib.PurePath(p.stem)
    return (p.name.split('.')[0] in SHELL_SHAPED_NAMED
            or p.suffix in SHELL_SHAPED)


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
    test_files = test_module_files(root)
    acc_for, key_index = accessor(root, test_files)
    tokens = set()
    for rel in out.split('\0'):
        # Markdown is prose, and `README.md.tmpl` is prose too — the same
        # effective-suffix reading `comment_leader` already does, applied here
        # as well, because a template's example `export AUTUMN_LOG__LEVL=x` was
        # entering the truth set as a shell assignment.
        if not rel or effective_suffix(rel) == '.md' or rel == SELF:
            continue
        # A file under `tests/` or `benches/` is test code whatever it is
        # written in, and so is a module a `#[cfg(test)] mod x;` declares.
        if test_code(rel, test_files):
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
        #
        # WHICH grammar strips them is the same decision as which grammar
        # reads them, and it has to be made FIRST: where a comment starts
        # depends on where the string before it ends, and PowerShell ends a
        # string by its own escape. `Write-Output "q: `""  # $env:AUTUMN_X`
        # closes its string at the unescaped quote, so the rest is a comment —
        # under the Bourne rule it was code. So the effective shell is resolved
        # on the RAW text, before anything is stripped.
        yaml_file = effective_suffix(rel) in YAML_SCALARS
        powershell = set()
        if yaml_file and _yaml_consumer(rel, body) == 'actions':
            powershell = _yaml_shells(body)
        raw = body
        body = uncommented(body, comment_leader(rel),
                          hash_needs_space(rel),
                          effective_suffix(rel) in HASH_AND_SLASH,
                          effective_suffix(rel) in HASH_BLOCK,
                          effective_suffix(rel) in YAML_SCALARS)
        if powershell:
            # `also_block` is already the flag that means "PowerShell" to
            # `uncommented`; each pwsh line is taken from that pass.
            ps_lines = uncommented(raw, '#', hash_needs_space(rel), False,
                                   True, True).splitlines()
            merged = body.splitlines()
            for i in powershell:
                if i < len(merged) and i < len(ps_lines):
                    merged[i] = ps_lines[i]
            body = '\n'.join(merged)
        # `effective_suffix`, not `rel.endswith('.rs')`: `build.rs.tmpl` is
        # Rust, and the suffix test did not see it, so a `#[test]` in a Rust
        # TEMPLATE went unmasked.
        if effective_suffix(rel) == '.rs':
            body = untested(body)
        interpolated = yaml_file and _yaml_interpolated(rel)
        yaml_code = None
        # A Dockerfile's exec-form lines expand nothing — see `_docker_literal`.
        literal = (_docker_literal(body)
                   if pathlib.PurePath(rel).name.split('.')[0] == 'Dockerfile'
                   or pathlib.PurePath(rel).name.split('.')[0] == 'Containerfile'
                   else set())
        if yaml_file:
            consumer = _yaml_consumer(rel, body)
            # Compose needs its own ASSIGNMENT view — see `COMPOSE_ASSIGNS`.
            # Everywhere else the two views coincide, because a non-compose
            # file keeps only what its consumer executes to begin with.
            if interpolated:
                yaml_code = _yaml_blocks(body, interpolated, consumer, True)
            body = _yaml_blocks(body, interpolated, consumer)
        # Heredocs FIRST: blanking single quotes first erases the `'EOF'`
        # marker, and the heredoc body then reads as commands. The self-test for
        # each function passed in isolation while the composition was wrong,
        # which is why the real-tree proof is not optional.
        # A non-compose YAML file has just been reduced to its executed blocks,
        # and those blocks ARE shell — so they take the shell passes, which
        # `.yml` never did: `echo '${AUTUMN_X}'` in a `run:` step is a literal.
        # A compose file does not take them, because compose interpolates every
        # value BEFORE any shell sees it, so the quotes there are YAML's.
        shell = (effective_suffix(rel) in HAS_HEREDOC
                 or (yaml_file and not interpolated))
        quoted = (effective_suffix(rel) in SHELL_QUOTED
                  or (yaml_file and not interpolated))
        # TWO views, because the rungs mean different things inside double
        # quotes: `"${AUTUMN_X}"` is an expansion, while
        # `echo " AUTUMN_X=v cmd"` is a string being printed. The expansion rung
        # reads `body`; the assignment rungs read `code`, which also blanks both
        # quote kinds and every unquoted heredoc body.
        code = body if yaml_code is None else yaml_code
        if shell:
            body, code = _shell_heredocs(body), _shell_heredocs(body, True)
        if quoted:
            esc = QUOTE_ESCAPE.get(effective_suffix(rel), '\\')
            here = effective_suffix(rel) in HAS_HERE_STRING
            plain, plain_code = body, code
            body, code = (_shell_literals(plain, esc, here),
                          _shell_code(plain_code, esc, here))
            # Choosing WHICH RUNG reads a pwsh line was only half of it: the
            # preprocessing under it still used the Bourne grammar, so
            # ``Write-Output "`$env:AUTUMN_X"`` — where PowerShell's backtick
            # escapes the dollar and the name is printed literally — reached
            # `PS_ENV` with its `$` intact. The escape character and the
            # here-string rule belong to the same decision as the rung.
            #
            # Both passes run over the whole text and each line is taken from
            # the one its own shell calls for. The limit, stated rather than
            # hidden: a pass carries quote state ACROSS lines, so state
            # entering a pwsh block was accumulated under the other grammar.
            # Inside a block — which is what a `run:` body is — that state is
            # this block's own, since `_yaml_blocks` has already blanked
            # everything between blocks.
            if powershell:
                ps_body = _shell_literals(plain, '`', True).splitlines()
                ps_code = _shell_code(plain_code, '`', True).splitlines()
                seen, seen_code = body.splitlines(), code.splitlines()
                for i in powershell:
                    if i < len(seen) and i < len(ps_body):
                        seen[i] = ps_body[i]
                    if i < len(seen_code) and i < len(ps_code):
                        seen_code[i] = ps_code[i]
                body, code = '\n'.join(seen), '\n'.join(seen_code)
        lines, code_lines = body.splitlines(), code.splitlines()
        # The two views are indexed together, so they must be the same length.
        # They are by construction when both come from one pass over the same
        # text — but compose builds its assignment view in a SEPARATE pass, and
        # a view whose last line is blanked loses it to `splitlines`. A missing
        # trailing line is an empty one by definition, so pad rather than guess.
        code_lines += [''] * (len(lines) - len(code_lines))
        # Names this file assigns without exporting them, and without handing
        # them to a command: its own variables.
        local = (set(ASSIGNED_ANY.findall(code))
                 - set(ASSIGNED.findall(code))
                 - set(ASSIGNED_PREFIX.findall(code)))
        # An accessor or a binding written inside generated DATA is not
        # something the generated program does — see `_generated_data`. Built
        # before the per-line loop because both rungs consult it, and paired
        # with the offset of each line so a per-line match can be located in
        # the whole-body mask.
        nested = (_generated_data(body)
                  if effective_suffix(rel) == '.rs' else None)
        offsets, at = [], 0
        for l in lines:
            offsets.append(at)
            at += len(l) + 1
        declaring = False
        for n, line in enumerate(lines):
            # `NAME=` is how a SHELL names a variable; in Rust it is just text
            # inside a string, and the text is often not an environment variable
            # at all. `autumn-cli/src/db/retention.rs` frames a line of stdout
            # with the prefix `AUTUMN_DB_RETENTION_REPORT=`, and a test fixture
            # contains that framed line verbatim — neither is an env read, and
            # both were putting the name into the truth set.
            #
            # `${NAME}` is a SHELL rule too, and it used to be applied to Rust
            # as well on the grounds that a template string reaches a shell
            # eventually. That is the mention standard this gate exists to
            # reject: Rust expands nothing, so `${AUTUMN_X}` in a literal is
            # text, and whether anything ever interpolates it is not visible
            # here. Measured before removing it, since a rung that stops
            # carrying something is how a correct page gets reported: it saw
            # exactly two names, `AUTUMN_ADMIN_SECRET` and
            # `AUTUMN_SECURITY__SIGNING_SECRET`, and BOTH are carried by real
            # evidence elsewhere — the first by `std::env::var(…)` twice in the
            # same generated file, the second by five compose files. The truth
            # set is 430 with this rung and 430 without it.
            # PowerShell shares none of the Bourne grammar: `NAME=value` is not
            # an assignment there, `ARG`/`ENV` are Dockerfile words, and
            # `$NAME` / `${NAME}` name an ORDINARY variable — only `$env:NAME`
            # reaches the environment. Running the Bourne rungs over it read a
            # local `$AUTUMN_X` as an environment read. Measured before
            # narrowing: `.ps1` contributes nothing through those rungs today,
            # and three names through `PS_ENV`.
            # …and a `run:` block the workflow runs under `pwsh` is PowerShell
            # wherever it is written. The suffix says `.yml`; what decides is
            # what Actions was told to run.
            if effective_suffix(rel) in HAS_HERE_STRING or n in powershell:
                tokens.update(PS_ENV.findall(line))
            elif shell_shapes(rel):
                tokens.update(ASSIGNED.findall(code_lines[n]))
                tokens.update(ASSIGNED_PREFIX.findall(code_lines[n]))
                if DECLARED.match(code_lines[n]) or declaring:
                    # BOTH: `ARG AUTUMN_X` has no `=` at all, and the legacy
                    # `ENV AUTUMN_X value` form has none either, so
                    # `DECLARED_CONT` alone dropped them. Adding the
                    # continuation reader must not replace the reader it
                    # extends.
                    tokens.update(DECLARED.findall(code_lines[n]))
                    tokens.update(DECLARED_CONT.findall(code_lines[n]))
                    declaring = code_lines[n].rstrip().endswith('\\')
                elif effective_suffix(rel) in DOTENV:
                    tokens.update(DECLARED_CONT.findall(code_lines[n]))
                # Compose declares a variable in three spellings, and only one
                # of them has an `=`: `- NAME=value`, `- NAME` (inherit from
                # the host) and `NAME: value` as a mapping key. The assignment
                # rungs all want `NAME=`, so a variable declared only as a
                # mapping key was invisible and its correct page was REPORTED.
                # The assignment view has already narrowed these lines to the
                # `environment:` section, so the shape is all that is left to
                # read.
                if interpolated:
                    tokens.update(COMPOSE_DECLARED.findall(code_lines[n]))
                if n not in literal:
                    tokens.update(v for v in EXPANDED.findall(line)
                                  if v not in local)
            # The generated-data mask applies to BINDINGS as well as
            # accessors: `r#"const FAKE_ENV: &str = "AUTUMN_X";"#` inside an
            # ordinary Rust string is sample text, not a binding. Same rule,
            # both rungs — applying it to one of the two is this script's most
            # repeated mistake.
            # `const NAME: &str = "AUTUMN_X"` is RUST syntax, so like the
            # accessor rung it belongs to Rust. It was running on every
            # language: the only rung still reading a `.lua` file was this one,
            # and it would have read that declaration out of a JavaScript or
            # Lua string just as happily. Found by checking a claim I had
            # already written on the review thread; measured at 430 either way.
            for at, v in (((mb.start(), mb.group(2))
                           for mb in BOUND.finditer(line))
                          if effective_suffix(rel) == '.rs' else ()):
                if nested is None or not nested[offsets[n] + at]:
                    tokens.update([v])
        # A quoted name counts when it is the accessor's own ARGUMENT, not when
        # it merely shares a neighbourhood with one. The four-line window this
        # replaces read `let unrelated = "AUTUMN_LOG__LEVL";` as an environment
        # name because an unrelated `env::var("RUST_LOG")` sat three lines up.
        # Taking the balanced argument list also covers the house multi-line
        # shape — `parse_env(\n env,\n "AUTUMN_MEDIA__ROOM_NAMESPACE")` — which
        # is what the window existed for.
        # The accessor rung is RUST's. `getenv("AUTUMN_X")` inside a
        # JavaScript string is text, and outside Rust this script has no way to
        # tell a call from a string containing one — `_rust_classes` and
        # `_generated_data` are what make the question answerable, and they are
        # Rust-only. Same shape as `shell_shapes`: a pattern belongs to the
        # languages that give it meaning.
        #
        # Measured before narrowing: the only non-Rust hits were three
        # `std::env::var(…)` lines inside a heredoc in
        # `deploy-real-vps-validate.sh` that writes a Rust file, and all three
        # names are carried elsewhere. Truth set 430 either way.
        for m in (acc_for(rel).finditer(body)
                  if effective_suffix(rel) == '.rs' else ()):
            if nested is not None and nested[m.start()]:
                continue
            head = body.rfind('\n', 0, m.start()) + 1
            tail = body.find('\n', m.start())
            if NEGATED.search(body[head:tail if tail >= 0 else len(body)]):
                continue
            args, _ = _balanced(body, m.end() - 1, ARG_SPAN_LIMIT)
            if args:
                called = next((g for g in m.groups() if g), '')
                tokens.update(QUOTED.findall(
                    key_argument(args, key_index.get(called, 0))))
    return tokens


def corpus(root):
    """The reader-facing pages, including the ones written into a new project.

    A markdown TEMPLATE is documentation a reader will hold: `new.rs`
    `include_str!`s `templates/README.md.tmpl` and writes it as every scaffolded
    application's `README.md`, so its config keys reach more readers than most
    guide pages. Excluding it from the truth set was right — it is prose, not
    source — but excluding it from the CORPUS as well left it unchecked in both
    directions, which is how a page ends up with no owner at all.
    """
    # NUL-delimited so a path containing whitespace is not split into fragments.
    out = subprocess.run(['git', 'ls-files', '-z', '*.md', '*.md.tmpl'],
                         cwd=root, capture_output=True, text=True).stdout
    return [f for f in out.split('\0')
            if f and (f.startswith(INCLUDE_DIRS) or f in INCLUDE_FILES
                      or f.endswith('.md.tmpl'))]


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
                head = m.group(1)
                # An inserted separator splits the namespace across the boundary
                # this pattern uses — `AUTU_MN_LOG__LEVEL` leaves `AUTU` as the
                # head — so the first segment of the tail is joined back on and
                # judged too. `RUST_LOG` and `DATABASE_URL` survive that, being
                # nowhere near either way.
                joined = head + '_' + m.group(2).split('_')[0]
                # A casing typo of the namespace is zero edits away and still
                # unreadable by the runtime; anything else must be one edit off.
                if head == 'AUTUMN' or not (head.upper() == 'AUTUMN'
                                            or near_miss(head.upper())
                                            or near_miss(joined.upper())):
                    continue
                if at[i] in waived.get(m.group(0), ()):
                    stats['waived'] += 1
                else:
                    defects.append((rel, i, m.group(0), line.strip()))
            for m in FUSED.finditer(line):
                if not fused_namespace(m.group(1)):
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

    Fourth: no variable and no path may appear twice. A count alone is a weak
    ratchet — replacing the `AUTUMN_SERVER__HOST` row with a second, perfectly
    valid `AUTUMN_SERVER__PORT` row holds the count at 142 and every row still
    passes, while the host mapping quietly leaves the published reference. A
    duplicate is also never right on its own terms: this table maps each name to
    one key.
    """
    out = []
    # Compared CANONICALLY, because two spellings of one mapping are still one
    # mapping: `…SHARDS__0__NAME | database.shards[0].name` and
    # `…SHARDS__{i}__NAME | database.shards[i].name` are the same row written
    # twice, and the raw comparison let the indexed spelling stand in for a
    # deleted reference entry while the count held at 142.
    for column, canon, label in ((1, to_path, 'variable'),
                                 (2, lambda p: re.sub(r'\[\w+\]', '', p),
                                  'config path')):
        seen = {}
        for row in rows:
            key = canon(row[column])
            first = seen.setdefault(key, row[0])
            if first != row[0]:
                out.append((row[0], row[1], row[2],
                            f'this {label} is already documented on line '
                            f'{first} — a duplicate row can stand in for one '
                            f'that was removed'))
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
    # The inserted separator can land anywhere in the namespace, so the head
    # can be one character. Each length floor in turn hid the typos below it.
    case('a separator inserted anywhere in the namespace is scanned',
         [[m.group(0) for m in NEAR.finditer(f'export {w}_LOG__LEVEL=debug')
           if near_miss(m.group(1).upper())
           or near_miss((m.group(1) + '_' + m.group(2).split('_')[0]).upper())]
          for w in ('AUT_UMN', 'AU_TUMN', 'A_UTUMN', 'RUST', 'AWS_REGION')],
         [['AUT_UMN_LOG__LEVEL'], ['AU_TUMN_LOG__LEVEL'], ['A_UTUMN_LOG__LEVEL'],
          [], []])
    case('somebody else\'s variable is not a near miss',
         (near_miss('DATABASE'), near_miss('RUST'), near_miss('CARGO')),
         (False, False, False))
    _, dn = scan(['d.md'], lambda _: 'export AUTMN_LOG__LEVEL=debug\n',
                 leaves, built, tokens)
    case('a misspelt namespace is reported', len(dn), 1)
    # A CASING typo of the namespace is zero edits away and equally unreadable.
    case('a mixed-case namespace is scanned',
         [m.group(0) for m in NEAR.finditer('export AUTUMn_LOG__LEVEL=debug')],
         ['AUTUMn_LOG__LEVEL'])
    _, dc = scan(['d.md'], lambda _: 'export AUTUMn_LOG__LEVEL=debug\n',
                 leaves, built, tokens)
    case('a mixed-case namespace is reported', len(dc), 1)
    # …while the crate name is prose about a package. What follows the namespace
    # must be uppercase, which is what keeps `autumn_web` out of this rung.
    _, dcrate = scan(['d.md'],
                     lambda _: 'Add `autumn_web` and `autumn-macros` to Cargo.toml.\n',
                     leaves, built, tokens)
    case('the crate name is not a config key claim', len(dcrate), 0)
    # …and the scan has to cross a `{i}` placeholder, or the seven documented
    # shard families are the one place a namespace typo stays invisible.
    case('a misspelt namespace is scanned through a placeholder',
         [m.group(0) for m in
          NEAR.finditer('| `AUTMN_DATABASE__SHARDS__{i}__NAME` |')],
         ['AUTMN_DATABASE__SHARDS__{i}__NAME'])
    _, dp = scan(['d.md'],
                 lambda _: '| `AUTMN_DATABASE__SHARDS__{i}__NAME` | x |\n',
                 leaves, built, tokens)
    case('and reported', len(dp), 1)
    # The missing edit can be the separator itself: `AUTUMNLOG__LEVEL`.
    # An INSERTED separator splits the namespace across the pattern's own
    # boundary, leaving `AUTU` as the head — so the first tail segment is joined
    # back on and judged too.
    case('an underscore inside the namespace is scanned',
         [m.group(0) for m in NEAR.finditer('export AUTU_MN_LOG__LEVEL=debug')
          if near_miss((m.group(1) + '_' + m.group(2).split('_')[0]).upper())],
         ['AUTU_MN_LOG__LEVEL'])
    _, du = scan(['d.md'], lambda _: 'export AUTU_MN_LOG__LEVEL=debug\n',
                 leaves, built, tokens)
    case('an underscore inside the namespace is reported', len(du), 1)
    _, dun = scan(['d.md'], lambda _: 'export RUST_LOG=debug\nDATABASE_URL=x y\n',
                  leaves, built, tokens)
    case('and a joined head that is nobody\'s typo is not', len(dun), 0)
    # Bash expands nothing inside single quotes.
    case('a single-quoted expansion names no variable',
         EXPANDED.findall(_shell_literals("printf '%s' '${AUTUMN_LOG__LEVL}'")),
         [])
    case('…while an unquoted one still does',
         EXPANDED.findall(_shell_literals('echo "${AUTUMN_LOG__LEVEL}"')),
         ['AUTUMN_LOG__LEVEL'])
    # A QUOTED heredoc is data the shell neither runs nor expands. These gates
    # embed their self-tests in the production script — `check-docs-cli.sh`
    # carries 17 such lines — so its fixtures were reading as commands.
    case('a quoted heredoc body is data',
         ASSIGNED_PREFIX.findall(_shell_heredocs(
             "cat <<'EOF'\nAUTUMN_LOG__LEVL=x ignored-command\nEOF\n")),
         [])
    # …and an UNQUOTED heredoc is a different thing: `<<EOF` does expand.
    case('an unquoted heredoc still expands',
         EXPANDED.findall(_shell_heredocs(
             'cat <<EOF\n${AUTUMN_LOG__LEVEL}\nEOF\n')),
         ['AUTUMN_LOG__LEVEL'])
    case('the terminator line survives, and line numbering with it',
         _shell_heredocs("a\ncat <<'EOF'\nx\nEOF\nb").splitlines(),
         ['a', "cat <<'EOF'", '', 'EOF', 'b'])
    # The ORDER of the two shell passes is load-bearing: blanking single quotes
    # first erases the `'EOF'` marker, after which the heredoc body reads as
    # commands. Both functions passed their own tests while the composition was
    # wrong, and only the real-tree proof caught it.
    case('heredocs are blanked before single quotes',
         ASSIGNED_PREFIX.findall(_shell_literals(_shell_heredocs(
             "cat <<'EOF'\nAUTUMN_LOG__LEVL=x cmd\nEOF\n"))),
         [])
    # The delimiter is a shell WORD, not an identifier. `<<'END-CONFIG'` was
    # rejected outright and its body read as commands; `<<\END-CONFIG` matched
    # the prefix `END` and set a terminator no line equals, blanking the rest of
    # the file. Both from spelling a word as `\w+`.
    case('a hyphenated heredoc delimiter is one word',
         (_shell_heredocs("cat <<'END-CONFIG'\nAUTUMN_LOG__LEVL=x cmd\n"
                          'END-CONFIG\nkeep\n').splitlines(),
          _shell_heredocs('cat <<"END-CONFIG"\nx\nEND-CONFIG\nkeep\n'
                          ).splitlines()[-1],
          _shell_heredocs('cat <<\\END-CONFIG\nx\nEND-CONFIG\nkeep\n'
                          ).splitlines()[-1]),
         (["cat <<'END-CONFIG'", '', 'END-CONFIG', 'keep'], 'keep', 'keep'))
    # PowerShell suppresses interpolation inside single quotes exactly as Bash
    # does, and `scripts/install.ps1` is where this project's PowerShell lives.
    # It gets the literal pass and NOT the heredoc one: its here-strings are
    # `@' … '@` and open no `<<`.
    case('a PowerShell single-quoted expansion names no variable',
         (effective_suffix('scripts/install.ps1') in SHELL_QUOTED,
          effective_suffix('scripts/install.ps1') in HAS_HEREDOC,
          EXPANDED.findall(_shell_literals(
              "Write-Output '${AUTUMN_LOG__LEVL}'"))),
         (True, False, []))
    # A value is ONE shell word, and a shell word can hold spaces without being
    # two. A command substitution is the case a regex cannot reach.
    case('a substituted value is not a following command',
         (ASSIGNED_PREFIX.findall("AUTUMN_LOG__LEVL=$(printf '%s %s' a b)"),
          ASSIGNED_PREFIX.findall('AUTUMN_LOG__LEVL=`echo a b`'),
          ASSIGNED_PREFIX.findall('AUTUMN_LOG__LEVL=${OTHER:-a b}')),
         ([], [], []))
    case('…and a real command after one still is',
         ASSIGNED_PREFIX.findall('AUTUMN_LOG__LEVEL=$(id -u) cargo run'),
         ['AUTUMN_LOG__LEVEL'])
    # What follows the value has to be a command, not a separator.
    case('a list operator is not a command',
         (ASSIGNED_PREFIX.findall('AUTUMN_X=1 ; cargo run'),
          ASSIGNED_PREFIX.findall('AUTUMN_X=1 && cargo run')), ([], []))
    # Rust block comments NEST, in generated code as much as in ordinary code.
    case('a nested block comment in generated Rust closes at the last end',
         QUOTED.findall(_rust_uncommented(
             'let s = "/* a /* b */ std::env::var(\\"AUTUMN_LOG__LEVL\\"); */";'
         )), [])
    case('…and the generated code after one is still read',
         QUOTED.findall(_rust_uncommented(
             'let s = "/* a /* b */ */ std::env::var(\\"AUTUMN_LOG__LEVEL\\");";'
         )), ['AUTUMN_LOG__LEVEL'])
    # Test code is test code for BOTH readers of the tree. `built_patterns` was
    # asking only about the path, so a `format!` in a `#[cfg(test)] mod tests;`
    # module could have blessed documentation globally.
    case('a cfg-only test module is test code by declaration',
         (test_code('autumn/src/cluster/tests.rs', test_module_files(ROOT)),
          test_code('autumn/src/config.rs', test_module_files(ROOT)),
          test_code('autumn/tests/integration/a11y.rs', set())),
         (True, False, True))
    # …and a path rule reads through `.tmpl` like every other one here.
    case('a test path is a test path through .tmpl too',
         (test_code('a/foo_test.rs.tmpl', set()),
          test_code('autumn-cli/src/templates/tests/x.rs.tmpl', set()),
          test_code('autumn-cli/src/templates/main.rs.tmpl', set())),
         (True, True, False))
    case('a fused namespace is scanned',
         [m.group(0) for m in FUSED.finditer('export AUTUMNLOG__LEVEL=debug')
          if fused_namespace(m.group(1))], ['AUTUMNLOG__LEVEL'])
    _, df2 = scan(['d.md'], lambda _: 'export AUTUMNLOG__LEVEL=debug\n',
                  leaves, built, tokens)
    case('a fused namespace is reported', len(df2), 1)
    # The correct spelling has its separator, so neither pattern claims it —
    # which is also what keeps the two from reporting the same token twice.
    case('the correct spelling is claimed by neither',
         ([m.group(0) for m in FUSED.finditer('export AUTUMN_LOG__LEVEL=x')],
          [m.group(0) for m in NEAR.finditer('export AUTUMN_LOG__LEVEL=x')
           if fused_namespace(m.group(1))]), ([], []))
    case('another project\'s fused name is not a near miss',
         (fused_namespace('DATABASEURL'), fused_namespace('SERVERPORT'),
          fused_namespace('LOG')), (False, False, False))
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
    # This case asserted the OPPOSITE for ten rounds, on the belief that blanking
    # would drop a real read. It does not. `tauri_mobile.rs` asserts that the
    # generated code contains that accessor COMMENTED OUT — evidence of a
    # non-read — and `AUTUMN_SYNC__TOKEN` is real, reaching the truth set
    # through the uncommented `set_var` elsewhere in the same template.
    case('a commented line inside a raw string is stripped',
         uncommented('assert!(x.contains(r#"// set_var("AUTUMN_SYNC__TOKEN");"#));'),
         'assert!(x.contains(r#"                                   ));')
    case('…and the name it names is still read',
         'AUTUMN_SYNC__TOKEN' in swept, True)
    case('an escaped quote does not end a string',
         uncommented(r'let s = "a\" // b"; let c = 1;'),
         r'let s = "a\" // b"; let c = 1;')
    # Generated code held in a string has comments of its own, and a commented
    # accessor inside it is no more a read than one outside.
    # The blanking runs to the real newline, so the generated line's own `\n\`
    # terminator goes with it. Nothing re-emits this text — it is only scanned
    # for names — so losing the marker costs nothing.
    case('a comment opening a generated line is stripped',
         uncommented('let t = "\\n\\\n  // std::env::var(\\"AUTUMN_LOG__LEVL\\");\\n\\\n";'),
         'let t = "\\n\\\n                                            \n";')
    # …but it must OPEN the line, or a `//` inside a URL eats the rest of the
    # literal — and a `/*` that is the whole point of a string is not a comment.
    # A block comment in generated code runs to its terminator ACROSS lines.
    # Stopping at the first newline left everything after it reading as code.
    case('a multi-line generated block comment is stripped whole',
         uncommented('let t = r#"/*\nstd::env::var("AUTUMN_LOG__LEVL");\n*/\n'
                     'let keep = 1;"#;'),
         'let t = r#"  \n                                  \n  \n'
         'let keep = 1;"#;')
    # PowerShell's block comment spans lines and the line stripper cannot see
    # it: `install.ps1` opens with a 26-line one documenting these very
    # variables.
    case('a PowerShell block comment is blanked, newlines kept',
         uncommented('<#\n${AUTUMN_LOG__LEVL}\n#>\n$x = 1', '#',
                     also_block=True),
         '  \n                   \n  \n$x = 1')
    case('…and its line count is preserved',
         uncommented('<#\na\n#>\nx', '#', also_block=True).count('\n'), 3)
    case('a URL inside a generated string survives',
         uncommented('let u = "\\n\\\n  let b = \\"https://x\\"; env::var(\\"AUTUMN_ENV\\");\\n\\\n";'),
         'let u = "\\n\\\n  let b = \\"https://x\\"; env::var(\\"AUTUMN_ENV\\");\\n\\\n";')
    # `#` opens a comment at the start of a WORD, so shell parameter expansion
    # survives.
    case('a trailing shell comment is stripped',
         uncommented('export AUTUMN_X=1 # AUTUMN_LOG__LEVL', '#'),
         'export AUTUMN_X=1 ')
    # Where a `#` opens a comment is per language. In the shell family it needs
    # whitespace in front — `$#` is parameter expansion — and in YAML an
    # unquoted `a#b` is a literal scalar. Everywhere else a `#` opens one
    # wherever it appears outside a string, which is Python's rule and the
    # default: `x = 1# …` was surviving a rule written for shell.
    case('a parameter expansion is not a comment',
         uncommented('echo "${AUTUMN_X#prefix}" $#', '#', needs_space=True),
         'echo "${AUTUMN_X#prefix}" $#')
    case('an inline comment with no space before it is one',
         uncommented('x = 1# std::env::var("AUTUMN_LOG__LEVL")', '#'),
         'x = 1')
    case('…but not in the shell family',
         uncommented('x=1# not a comment here', '#', needs_space=True),
         'x=1# not a comment here')
    case('the boundary follows the file type',
         (hash_needs_space('x.sh'), hash_needs_space('x.yml'),
          hash_needs_space('x.py'), hash_needs_space('main.tf.tmpl')),
         (True, True, False, False))
    case('a `#` inside a string is not a comment either',
         uncommented('url = "http://x/#frag"', '#'), 'url = "http://x/#frag"')
    # An escaped quote does not end a string. Without this, `"a\"b"` reads as a
    # string that ENDS at the escaped quote and reopens at the real one, so a
    # trailing comment lands inside an imaginary string and survives.
    case('an escaped quote does not end a shell string',
         uncommented(r'''printf "a\"b" # env::var("AUTUMN_LOG__LEVL")''',
                     '#', needs_space=True),
         r'printf "a\"b" ')
    # …but a backslash is literal inside shell single quotes, so the quote after
    # it really does close the string.
    case('a backslash is literal in shell single quotes',
         uncommented(r"""echo 'a\' # note""", '#', needs_space=True),
         r"echo 'a\' ")
    case('an escape works in both quotes elsewhere',
         uncommented(r'''x = "a\"b"# note''', '#'), r'x = "a\"b"')
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
    # A struct field or enum variant ends at its COMMA. Running past it drove
    # the brace depth negative and masked to end of file — 717 of 1950 lines in
    # `live_events.rs`, dropping every real read after the field.
    case('a cfg(test) field ends at its comma',
         untested('struct S {\n  #[cfg(test)]\n  probe: bool,\n  real: u8,\n}\n'
                  'fn f() { }'),
         'struct S {\n\n\n  real: u8,\n}\nfn f() { }')
    # …and a field whose type carries a comma inside generics ends at its OWN
    # comma: the generic is counted, so the inner comma is punctuation. This
    # used to fall back to the enclosing brace and swallow the rest of the
    # struct — bounded, but twice the lines it needed.
    case('a generic field ends at its own comma',
         untested('struct S {\n  #[cfg(test)]\n  probe: Map<A, B>,\n}\n'
                  'fn f() { }'),
         'struct S {\n\n\n}\nfn f() { }')
    # A match arm ends at its comma too, and the call before it must not latch
    # the item open: `0 => var("X"),` followed by a production arm had the arm
    # AND the arms after it masked.
    case('a cfg(test) match arm ends at its comma',
         untested('match n {\n  #[cfg(test)]\n  0 => var("A"),\n'
                  '  1 => var("B"),\n}'),
         'match n {\n\n\n  1 => var("B"),\n}')
    case('a comparison is not a generic',
         untested('#[cfg(test)]\nconst X: bool = 1 < 2;\nfn p() { }'),
         '\n\nfn p() { }')
    case('a fn signature comma is not the end of the item',
         untested('#[cfg(test)]\nfn f(a: u8, b: u8) {\n  let x = 1;\n}\n'
                  'fn p() { }'),
         '\n\n\n\nfn p() { }')
    # A test function needs no `cfg` to be test code, and 692 `#[test]`s here sit
    # outside any `#[cfg(test)]` region — a whole file of them in `cluster/tests.rs`.
    case('a bare #[test] function is masked',
         untested('fn prod() { }\n#[test]\nfn t() {\n  let a = 1;\n}\nfn more() { }'),
         'fn prod() { }\n\n\n\n\nfn more() { }')
    case('an async test attribute with arguments is too',
         untested('#[tokio::test(flavor = "multi_thread")]\nasync fn t() {\n}\nfn p() { }'),
         '\n\n\nfn p() { }')
    case('a #[test] inside a string is not',
         untested('let t = "#[test]\\nfn x() {}";\nfn p() { }'),
         'let t = "#[test]\\nfn x() {}";\nfn p() { }')
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
    # A char literal is blanked like any other literal, so neither the `"` in
    # `b'"'` nor the `}` in `'}'` is read as syntax — the braces around it still
    # are.
    case('a char literal holding a quote does not open a string',
         _rust_skeleton('if q == b\'"\' { let s = "x"; }'),
         'if q == b    { let s =    ; }')
    case('a char literal holding a brace is not a brace',
         _rust_skeleton("fn t() { let _ = '}'; let s = 1; }"),
         'fn t() { let _ =    ; let s = 1; }')
    case('a test item is not ended by a brace in a char literal',
         untested("#[test]\nfn t() {\n  let _ = '}';\n  let a = 1;\n}\nfn p() { }"),
         '\n\n\n\n\nfn p() { }')
    case('a lifetime is not a char literal',
         _rust_skeleton("fn f<'a>(s: &'a str) { }"),
         "fn f<'a>(s: &'a str) { }")
    case('a test-only name is not swept in', 'AUTUMN_DEV' in swept, False)
    case('a test path is not swept',
         (bool(TEST_PATH.search('autumn/tests/integration/a11y.rs')),
          bool(TEST_PATH.search('autumn/src/config.rs'))), (True, False))
    # A module can be test-only without anything in its own FILE saying so:
    # `cluster/mod.rs` declares `#[cfg(test)] mod tests;`, and helpers in
    # `cluster/tests.rs` beside the `#[test]` functions were reading as
    # production. Read out of the tree, so a module named anything is covered.
    case('an externally declared test module is not swept',
         'autumn/src/cluster/tests.rs' in test_module_files(ROOT), True)
    case('the declaration is matched by cfg, not by name',
         (TEST_MOD.findall('#[cfg(test)]\nmod tests;'),
          TEST_MOD.findall('#[cfg(feature = "db")]\nmod tests;')),
         ([('test', 'tests')], [('feature = "db"', 'tests')]))
    case('…and only a test-only cfg excludes the file',
         (_cfg_truth('test')[0], _cfg_truth('feature = "db"')[0]),
         (False, True))
    # The crates' own env helpers are read out of the tree, not listed here: the
    # media plugin reads 17 real variables through `override_string`, and
    # `AUTUMN_MEDIA__FFMPEG__BIN` was in the truth set only through a `${…}`
    # expansion inside a test — so masking tests without this would have
    # reported a correct page.
    case('an env helper declaration is recognised',
         [n for n, _ in ENV_HELPER.findall(
             'fn override_string(target: &mut String, '
             'env: &HashMap<String, String>, key: &str) {')],
         ['override_string'])
    # …and derived from EXECUTABLE Rust only. This index is global — one
    # signature read out of a comment names an accessor for every file in the
    # tree — so it is masked exactly like the scan it feeds.
    case('a helper signature that is not code derives nothing',
         [[n for n, _ in ENV_HELPER.findall(
              untested(_rust_uncommented(s)))
           if not _generated_data(untested(_rust_uncommented(s)))[
               untested(_rust_uncommented(s)).index('fn ' + n)]]
          for s in
          ('// fn label_lookup(env: &FakeEnv, key: &str) -> String { }',
           '#[cfg(test)]\nmod t {\n'
           '    fn label_lookup(env: &FakeEnv, key: &str) -> String { }\n}',
           'fn override_string(env: &Env, key: &str) {}')],
         [[], [], ['override_string']])
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
    # `#` is the DEFAULT, and the map is the exception list. An allow-list
    # failed three rounds running, once per file type nobody had listed —
    # Dockerfiles, then Terraform templates.
    case('an unlisted type gets the default leader',
         (comment_leader('main.tf.tmpl'), comment_leader('x.py'),
          comment_leader('x.hcl')), ('#', '#', '#'))
    case('the C family does not',
         (comment_leader('x.rs'), comment_leader('x.ts')), ('//', '//'))
    # Program output captured as a fixture has no comments, and a line in it may
    # legitimately begin with `#`.
    # A markdown TEMPLATE is prose too. Reading it as source made its example
    # `AUTUMN_DATABASE__URL` a shell assignment in the truth set.
    case('a markdown template is markdown',
         (effective_suffix('autumn-cli/src/templates/README.md.tmpl'),
          effective_suffix('Cargo.toml.tmpl'), effective_suffix('x.rs')),
         ('.md', '.toml', '.rs'))
    # …and it is a page a reader holds: `new.rs` writes it as every scaffolded
    # app's `README.md`, so it belongs in the CORPUS rather than merely being
    # skipped as source.
    case('a markdown template is a checked page',
         'autumn-cli/src/templates/README.md.tmpl' in corpus(ROOT), True)
    # HCL takes all three comment forms; a `//` in shell or YAML is a path.
    case('Terraform strips its slash forms too',
         uncommented('// export AUTUMN_LOG__LEVL=x\nreal = 1 # note', '#',
                     also_slash=True),
         '\nreal = 1 ')
    case('a shell path is not a comment',
         uncommented('cp //server/share /tmp # note', '#', needs_space=True),
         'cp //server/share /tmp ')
    # Setting a variable publishes it, as `export` does; removing one does not.
    case('a write that publishes is an accessor',
         bool(ACCESSOR.search('std::env::set_var("AUTUMN_SYNC__DB_PATH", p)')),
         True)
    case('removing a variable is not',
         bool(ACCESSOR.search('std::env::remove_var("AUTUMN_LOG__LEVL")')),
         False)
    case('a fixture keeps every line',
         (comment_leader('x.golden'), comment_leader('x.stderr'),
          comment_leader('README.md.tmpl')), (None, None, None))
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
    # A quoted name counts as its accessor's ARGUMENT, not as its neighbour. The
    # window this replaced took a name off any line within three of a call.
    _acc_for, real_index = accessor(ROOT, test_module_files(ROOT))
    real_acc = _acc_for('autumn/src/config.rs')

    def args_of(text, acc=ACCESSOR, index=None):
        m = acc.search(text)
        called = next((x for x in m.groups() if x), '')
        pos = (index or {}).get(called, 0)
        return QUOTED.findall(
            key_argument(_balanced(text, m.end() - 1, ARG_SPAN_LIMIT)[0], pos))
    case('a name outside the call is not its argument',
         args_of('std::env::var(\n  "RUST_LOG",\n);\nlet u = "AUTUMN_LOG__LEVL";'),
         [])
    # …while the house multi-line shape, which the window existed for, still
    # reads through the balanced argument list.
    case('a multi-line argument list still reads',
         args_of('parse_env(\n    env,\n    "AUTUMN_MEDIA__ROOM_NAMESPACE",\n)',
                 real_acc, real_index),
         ['AUTUMN_MEDIA__ROOM_NAMESPACE'])
    # Only ONE argument is the key. Taking every quoted name in the list made
    # the VALUE one too: a name assigned to somebody else's variable.
    case('a value argument is not the key',
         args_of('std::env::set_var("RUST_LOG", "AUTUMN_LOG__LEVL")'), [])
    case('…and the key still is',
         args_of('std::env::set_var("AUTUMN_SYNC__DB_PATH", path)'),
         ['AUTUMN_SYNC__DB_PATH'])
    # The helpers put the key second or third, behind arguments that are not
    # string literals — which is what makes "first whole string literal" a rule
    # about the calls rather than a table of arities.
    # …read through the accessor of the module that DEFINES the helper, since
    # `override_string` is the media plugin's function and means nothing in
    # `config.rs`. That scoping is the point, so the test asks the right file.
    _media_acc = _acc_for('autumn-media-plugin/src/config.rs')
    case('a helper key behind other arguments reads',
         args_of('override_string(&mut self.ffmpeg.bin, env, '
                 '"AUTUMN_MEDIA__FFMPEG__BIN")', _media_acc, real_index),
         ['AUTUMN_MEDIA__FFMPEG__BIN'])
    # The key is a POSITION: `set_var(key, "…")` passes its key in a variable
    # and its value as a literal, and the value is nobody's variable name.
    case('a literal value with a variable key is not the key',
         args_of('std::env::set_var(key, "AUTUMN_LOG__LEVL")'), [])
    # The helpers are written with a generic parameter list, and requiring `(`
    # straight after the name derived none of them — moving every helper key to
    # position 0 and costing 85 names. Measured, not assumed.
    case('a generic helper signature is derived',
         (real_index.get('parse_env'), real_index.get('parse_env_option'),
          real_index.get('override_string')), (1, 1, 2))
    case('a helper key at its derived position reads',
         args_of('parse_env(env, "AUTUMN_MEDIA__ROOM_NAMESPACE", &mut t)',
                 real_acc, real_index),
         ['AUTUMN_MEDIA__ROOM_NAMESPACE'])
    # A raw string is a string literal: `var(r"AUTUMN_X")` is an ordinary read,
    # and not seeing it REPORTED a correct page.
    case('a raw-string key argument reads',
         (args_of('std::env::var(r"AUTUMN_RAW_ONLY")'),
          args_of('std::env::var(r#"AUTUMN_RAW_HASH"#)'),
          STRING_ARG.match('key') is None),
         (['AUTUMN_RAW_ONLY'], ['AUTUMN_RAW_HASH'], True))
    # `$$` is the PID, so the second dollar does not start an expansion. A
    # pattern that can begin mid-token has to say where the construct starts.
    case('a dollar already consumed by $$ starts no expansion',
         (EXPANDED.findall('echo $$AUTUMN_X'),
          EXPANDED.findall('echo $${AUTUMN_X}'),
          EXPANDED.findall('echo $AUTUMN_X'),
          EXPANDED.findall('echo ${AUTUMN_X}')),
         ([], [], ['AUTUMN_X'], ['AUTUMN_X']))
    case('an escaped literal in generated code still reads',
         args_of(r'std::env::var(\"AUTUMN_TEST_ADMIN_SESSION\")'),
         ['AUTUMN_TEST_ADMIN_SESSION'])
    case('a comma inside a string does not split the arguments',
         _split_args('"a,b", c'), ['"a,b"', ' c'])
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
    # An assignment word assigns only BEFORE the command name; after it, it is
    # an ordinary argument. A few words pass assignments through, or open a
    # command without being one.
    case('an assignment after the command name is an argument',
         (ASSIGNED_PREFIX.findall("printf '%s' AUTUMN_X=v ignored-command"),
          ASSIGNED_PREFIX.findall('cmd AUTUMN_X=1 arg'),
          ASSIGNED_PREFIX.findall('AUTUMN_X=1 cargo run'),
          ASSIGNED_PREFIX.findall('env AUTUMN_X=1 cmd'),
          ASSIGNED_PREFIX.findall('AUTUMN_A=1 AUTUMN_X=2 cmd'),
          ASSIGNED_PREFIX.findall('echo hi; AUTUMN_X=1 cmd')),
         ([], [], ['AUTUMN_X'], ['AUTUMN_X'], ['AUTUMN_A', 'AUTUMN_X'],
          ['AUTUMN_X']))
    # `export` must BE the command word, exactly as an assignment must sit
    # before one — the same question, asked of the neighbouring rung.
    case('export counts only as the command word',
         (ASSIGNED.findall('export AUTUMN_X=1'),
          ASSIGNED.findall('printf %s export AUTUMN_X=x'),
          ASSIGNED.findall('foo && export AUTUMN_X=1'),
          ASSIGNED.findall('env export AUTUMN_X=1')),
         (['AUTUMN_X'], [], ['AUTUMN_X'], ['AUTUMN_X']))
    # The pass-through list is what bash DOES, not what sounds right: `exec`,
    # `command` and `nohup` each try to run a program named `AUTUMN_X=v` and
    # exit 127, so an assignment after them reaches nothing.
    case('only words that really pass an assignment through are stepped over',
         [ASSIGNED_PREFIX.findall(f'{w} AUTUMN_X=1 cmd') for w in
          ('env', 'sudo', 'time', 'exec', 'command', 'nohup')],
         [['AUTUMN_X'], ['AUTUMN_X'], ['AUTUMN_X'], [], [], []])
    # Where a comment STARTS depends on where the string before it ends, and
    # PowerShell ends a string by its own escape — so the comment grammar is
    # the same decision as the rung, and has to be made before stripping.
    _psline = 'Write-Output "quote: `"" # $env:AUTUMN_X "'
    case('a pwsh comment is stripped with PowerShell quoting',
         (PS_ENV.findall(uncommented(_psline, '#', True, False, True, True)),
          PS_ENV.findall(uncommented(_psline, '#', True, False, False, True))),
         ([], ['AUTUMN_X']))
    # Docker's exec form runs no shell, so its dollars are literal — unless the
    # executable IS a shell, which this tree does use.
    case('a Docker exec form expands nothing unless it names a shell',
         sorted(_docker_literal(
             'RUN ["echo", "$AUTUMN_A"]\nRUN echo $AUTUMN_B\n'
             'CMD ["sh", "-c", "echo $AUTUMN_C"]\n'
             'CMD ["/app/x", \\\n  "$AUTUMN_D"]\nENV AUTUMN_E=1\n')),
         [0, 3, 4])
    # A bare `var(` is any function named `var`; the environment API has a
    # receiver or a path. This tree's own `Env` trait is called as a method.
    # …and the receiver must be an ENVIRONMENT one, derived from the tree.
    # `off` is bound only inside a test module, so it is not derived — and
    # costs nothing, because the calls that use it are test code too and are
    # masked before this pattern ever reads them.
    # …and the receiver must be an ENVIRONMENT one, derived from the tree and
    # SCOPED to the file that declares it: a binding name is file-local, while
    # a type name is not. `inner` is a field of a wrapper in `deploy.rs`, so it
    # is an accessor there and an ordinary method name in `config.rs`. `off` is
    # bound only inside a test module, so it is derived nowhere — which costs
    # nothing, since the calls using it are test code and already masked.
    case('an accessor needs an environment receiver, scoped to its file',
         (bool(real_acc.search('var("AUTUMN_X")')),
          bool(real_acc.search('background: var(--primary)')),
          bool(real_acc.search('css.var("AUTUMN_X")')),
          bool(real_acc.search('off.var("AUTUMN_X")')),
          bool(real_acc.search('std::env::var("AUTUMN_X")')),
          bool(real_acc.search('env::var("AUTUMN_X")')),
          bool(real_acc.search('env.var("AUTUMN_X")')),
          bool(real_acc.search('OsEnv.var("AUTUMN_X")')),
          bool(_acc_for('autumn-cli/src/deploy.rs')
               .search('self.inner.var("AUTUMN_X")')),
          bool(real_acc.search('self.inner.var("AUTUMN_X")'))),
         (False, False, False, False, True, True, True, True, True, False))
    # A redirection is not a command — `AUTUMN_X=1 > out` is a null command
    # that starts no process — but it may PRECEDE one, so it is consumed and
    # the question asked again rather than added to the terminator set.
    case('a redirection is not the command a prefix assignment needs',
         (ASSIGNED_PREFIX.findall('AUTUMN_X=1 > /tmp/out'),
          ASSIGNED_PREFIX.findall('AUTUMN_X=1 >> log'),
          ASSIGNED_PREFIX.findall('AUTUMN_X=1 &> f'),
          ASSIGNED_PREFIX.findall('AUTUMN_X=1 >out cmd'),
          ASSIGNED_PREFIX.findall('AUTUMN_X=1 2>&1 cmd'),
          ASSIGNED_PREFIX.findall('AUTUMN_X=1 < in cmd')),
         ([], [], [], ['AUTUMN_X'], ['AUTUMN_X'], ['AUTUMN_X']))
    # The value is one shell WORD, quotes included. Reading it as `\S*` made the
    # second word of a quoted value look like the command that follows a prefix
    # assignment, so a script-local variable read as one handed to a process.
    case('a quoted value with a space is not a command',
         (ASSIGNED_PREFIX.findall('AUTUMN_LOG__LEVL="some value"'),
          ASSIGNED_PREFIX.findall("AUTUMN_LOG__LEVL='some value'")), ([], []))
    case('…while a real prefix assignment still is',
         (ASSIGNED_PREFIX.findall('AUTUMN_X="a b" cargo run'),
          ASSIGNED_PREFIX.findall('AUTUMN_X= cmd')),
         (['AUTUMN_X'], ['AUTUMN_X']))
    # Every form that quotes the delimiter suppresses expansion.
    case('all quoted heredoc delimiters are recognised',
         [_heredoc_openers(h)
          for h in ("cat <<'EOF'", 'cat <<"EOF"', 'cat <<\\EOF')],
         [[('EOF', False, False)]] * 3)
    # An unquoted one is still an opener — its body has to be CONSUMED so the
    # next heredoc's body starts in the right place — but it expands, so it is
    # read rather than blanked.
    case('an unquoted delimiter opens an expanding body',
         _heredoc_openers('cat <<EOF'), [('EOF', False, True)])
    # One command line may open SEVERAL, and bash consumes them in order.
    # Keeping one scalar terminator resumed scanning inside the second body.
    case('every heredoc on a line is queued',
         (_heredoc_openers("cat <<'ONE' <<'TWO'"),
          _shell_heredocs("cat <<'ONE' <<'TWO'\nAUTUMN_LOG__LEVL=x cmd\nONE\n"
                          'AUTUMN_LOG__LEVL=y cmd\nTWO\nkeep\n').splitlines()),
         ([('ONE', False, False), ('TWO', False, False)],
          ["cat <<'ONE' <<'TWO'", '', 'ONE', '', 'TWO', 'keep']))
    # An operator inside quotes or arithmetic opens nothing, and a here-string
    # has no delimiter word at all.
    case('an inert `<<` is not an opener',
         (_heredoc_openers('printf \'%s\' "<<EOF"'),
          _heredoc_openers('echo $((1 << 2))'),
          _heredoc_openers('cat <<<EOF')), ([], [], []))
    # …but a double-quoted span is not opaque: the shell re-enters inside
    # `$( … )`, so a `<<` there is a real operator whose body is data. The
    # negatives above must still hold, since both directions come from the
    # same mask.
    case('a heredoc opened inside a quoted substitution is found',
         (_heredoc_openers('probe="$(cat <<EOF'),
          _heredoc_openers('probe="$(cat <<\'EOF\''),
          _heredoc_openers('probe="literal <<EOF"'),
          _mask_inert('probe="$(cat <<EOF')),
         ([('EOF', False, True)], [('EOF', False, False)], [],
          'probe="$(cat <<EOF'))
    # A `<<-` terminator may be indented with TABS, and only tabs.
    case('a tab-stripping heredoc ends on an indented terminator',
         _shell_heredocs("cat <<-'EOF'\nx\n\tEOF\nkeep\n").splitlines(),
         ["cat <<-'EOF'", '', '\tEOF', 'keep'])
    # A single-quoted string spans lines, and its interior lines are data.
    case('a multi-line literal is blanked throughout',
         EXPANDED.findall(_shell_literals(
             "printf '%s\n${AUTUMN_LOG__LEVL}\n%s' a\n")), [])
    # …which is only safe because the scan is quote-aware: an apostrophe inside
    # a double-quoted string opens no literal.
    case('an apostrophe inside double quotes opens nothing',
         EXPANDED.findall(_shell_literals(
             'echo "don\'t ${AUTUMN_LOG__LEVEL}"')), ['AUTUMN_LOG__LEVEL'])
    # An unterminated quote costs itself and nothing after it — blanking to the
    # end of the file would hide real uses and report correct pages.
    case('an unterminated quote does not blank the rest',
         EXPANDED.findall(_shell_literals(
             "echo it's\nexport AUTUMN_LOG__LEVEL=x\necho \"${AUTUMN_ENV}\"\n")),
         ['AUTUMN_ENV'])
    # A double-quoted string is not one opaque context: a substitution inside it
    # re-enters shell parsing, where an apostrophe opens a literal again.
    case('a literal inside a substitution inside double quotes is blanked',
         (EXPANDED.findall(_shell_literals(
             """probe="$(printf '%s' '${AUTUMN_LOG__LEVL}')" """)),
          EXPANDED.findall(_shell_literals(
              'probe="$(id -u) ${AUTUMN_LOG__LEVEL}"'))),
         ([], ['AUTUMN_LOG__LEVEL']))
    # A PowerShell here-string is held by `@'` and a line beginning `'@`, so its
    # body may contain the apostrophes an ordinary quoted span would end on.
    case('a PowerShell here-string body is literal to its own terminator',
         (EXPANDED.findall(_shell_literals(
             "@'\ndon't\n${AUTUMN_LOG__LEVL}\n'@\n", '`', True)),
          EXPANDED.findall(_shell_literals(
              '@"\ndon\'t\n${AUTUMN_LOG__LEVEL}\n"@\n', '`', True))),
         ([], ['AUTUMN_LOG__LEVEL']))
    # Bash collects every delimiter on the LOGICAL command line before it
    # consumes a body, so a continuation still opens both.
    case('a continued command opens both its heredocs',
         _shell_heredocs("cat <<'ONE' \\\n<<'TWO' >/dev/null\nfirst\nONE\n"
                         'AUTUMN_LOG__LEVL=x cmd\nTWO\nkeep\n').splitlines(),
         ["cat <<'ONE' \\", "<<'TWO' >/dev/null", '', 'ONE', '', 'TWO', 'keep'])
    # …but an OPERATOR is not a continuation, however unfinished it leaves the
    # pipeline: a here-document body begins after the next newline, so the
    # line after `cat <<'ONE' |` is ONE's body and the `<<'TWO'` written there
    # is body text. Verified by running bash on this exact input, which is why
    # the expectation changed: an earlier round asserted the opposite from
    # reasoning alone.
    case('an operator does not defer a heredoc body',
         [_shell_heredocs(f"cat <<'ONE' {op}\ncat <<'TWO'\nfirst\nONE\n"
                          'AUTUMN_LOG__LEVL=x cmd\nTWO\nkeep\n').splitlines()
          for op in ('|', '&&')],
         [[f"cat <<'ONE' {op}", '', '', 'ONE',
           'AUTUMN_LOG__LEVL=x cmd', 'TWO', 'keep']
          for op in ('|', '&&')])
    case('…while a backgrounding `&` ends the command',
         _shell_heredocs("cat <<'ONE' &\nfirst\nONE\nkeep\n").splitlines(),
         ["cat <<'ONE' &", '', 'ONE', 'keep'])
    # `${NAME}` is a shell rule, and Rust expands nothing. Measured before
    # removing it: the rung saw exactly two names, both carried by real
    # evidence elsewhere, and the truth set is 430 either way.
    # Two views, because the rungs mean different things inside double quotes:
    # `"${AUTUMN_X}"` is an expansion, `echo " AUTUMN_X=v cmd"` is a string being
    # printed. An assignment is only one where the shell would run it.
    case('a printed string is not an assignment, but is still an expansion',
         (ASSIGNED_PREFIX.findall(_shell_code('echo " AUTUMN_LOG__LEVL=x cmd"')),
          ASSIGNED_PREFIX.findall(_shell_code('AUTUMN_LOG__LEVEL=debug cargo run')),
          EXPANDED.findall(_shell_literals('echo "${AUTUMN_LOG__LEVEL}"'))),
         ([], ['AUTUMN_LOG__LEVEL'], ['AUTUMN_LOG__LEVEL']))
    # …and a substitution inside double quotes is still code, so an assignment
    # written there survives the code view.
    case('an assignment inside a substitution survives',
         ASSIGNED_PREFIX.findall(_shell_code('x="$(AUTUMN_LOG__LEVEL=debug cmd)"')),
         ['AUTUMN_LOG__LEVEL'])
    # PowerShell's escape is a BACKTICK. Reading a backslash ended the string at
    # the wrong quote, made the real terminator look like a new opener, and left
    # the trailing comment — and its accessor — standing as code.
    case('a PowerShell backtick escape ends the string where it says',
         uncommented('Write-Output "quote: `"" # getenv("AUTUMN_LOG__LEVL")\n',
                     comment_leader('a.ps1'), hash_needs_space('a.ps1'),
                     False, True).rstrip(),
         'Write-Output "quote: `""')
    # The shell shapes are grammar, so they apply to the formats that interpret
    # them and to nothing else. An ALLOW-list: excluding only Rust left
    # `${AUTUMN_X}` in a JS template literal, a `.toml`, a `.golden` fixture —
    # and, because the old test was `rel.endswith('.rs')`, a Rust `.tmpl` —
    # all reading as evidence.
    case('the shell shapes apply only to formats that interpret them',
         [shell_shapes(f) for f in
          ('scripts/x.sh', 'docker-compose.yml', '.env.example',
           'autumn-cli/src/templates/Dockerfile.api.tmpl', 'scripts/install.ps1',
           'autumn/src/config.rs', 'autumn-cli/src/templates/main.rs.tmpl',
           'autumn-admin-plugin/src/admin.js', 'autumn/Cargo.toml', 'a.golden')],
         [True] * 5 + [False] * 5)
    # A YAML block scalar is a string; whether it is ever executed is the
    # CONSUMER's rule, so the executed keys are enumerated. Every name a block
    # scalar carries in this tree is under `run`, which really is shell.
    case('a data block scalar is not commands, but a `run:` block is',
         (ASSIGNED_PREFIX.findall(_yaml_blocks(
             'description: |\n  AUTUMN_LOG__LEVL=x cmd\n  more\n')),
          ASSIGNED_PREFIX.findall(_yaml_blocks(
              'steps:\n  - run: |\n      AUTUMN_LOG__LEVEL=debug cargo run\n'))),
         ([], ['AUTUMN_LOG__LEVEL']))
    # An ordinary scalar is just as inert as a block one — but only where the
    # consumer says so. Compose interpolates every value; Actions interpolates
    # none, so there the shell syntax means something only inside `run:`.
    case('the consumer decides which YAML values interpolate',
         (EXPANDED.findall(_yaml_blocks(
             'name: "${AUTUMN_LOG__LEVL}"\nsteps:\n  - run: |\n'
             '      echo "${AUTUMN_LOG__LEVEL}"\n',
             _yaml_interpolated('.github/workflows/ci.yml'))),
          EXPANDED.findall(_yaml_blocks(
              'services:\n  app:\n    environment:\n'
              '      AUTUMN_LOG__LEVEL: "${AUTUMN_LOG__LEVEL:?err}"\n',
              _yaml_interpolated('examples/x/docker-compose.yml'))),
          _yaml_interpolated(
              'autumn-cli/src/templates/release/docker-compose.yml.tmpl')),
         (['AUTUMN_LOG__LEVEL'], ['AUTUMN_LOG__LEVEL'], True))
    # A declaration need not carry an `=` at all: `ARG AUTUMN_X` and the legacy
    # `ENV AUTUMN_X value` are both valid, and the continuation reader must
    # EXTEND the declaration reader rather than replace it.
    case('a declaration without an equals sign is kept',
         (DECLARED.findall('ARG AUTUMN_DOCKER_ONLY'),
          DECLARED.findall('ENV AUTUMN_DOCKER_ONLY value')),
         (['AUTUMN_DOCKER_ONLY'], ['AUTUMN_DOCKER_ONLY']))
    # One `ENV` may declare several variables across a continuation, and
    # anchoring to the line start saw only the first.
    case('a continued ENV declares every name on it',
         [DECLARED_CONT.findall(l) for l in
          'ENV AUTUMN_ONE=1 \\\n    AUTUMN_TWO=2'.splitlines()],
         [['AUTUMN_ONE'], ['AUTUMN_TWO']])
    # A dotenv file's whole grammar is `NAME=value`: no command follows, and
    # there is nothing script-local to confuse it with.
    case('a bare dotenv assignment is a declaration',
         ('.example' in DOTENV, effective_suffix('.env.example') in DOTENV,
          DECLARED_CONT.findall('AUTUMN_LOG__LEVEL="debug"')),
         (True, True, ['AUTUMN_LOG__LEVEL']))
    # A retained `run:` block IS shell, so it takes the shell passes — which
    # `.yml` never did. A compose value does not: compose interpolates `${…}`
    # before any shell sees it, so the quotes there are YAML's.
    case('a retained run: block is parsed as shell, a compose value is not',
         (EXPANDED.findall(_shell_literals(_yaml_blocks(
             "steps:\n  - run: |\n      echo '${AUTUMN_LOG__LEVL}'\n", False))),
          EXPANDED.findall(_yaml_blocks(
              'services:\n  a:\n    environment:\n'
              '      X: "${AUTUMN_LOG__LEVEL:?e}"\n', True))),
         ([], ['AUTUMN_LOG__LEVEL']))
    # An unquoted heredoc EXPANDS but does not RUN: its expansions are real
    # references, its lines are data being written rather than commands.
    case('an unquoted heredoc body expands but assigns nothing',
         (EXPANDED.findall(_shell_heredocs(
             'cat <<EOF\nAUTUMN_LOG__LEVL=x cmd\n${AUTUMN_LOG__LEVEL}\nEOF\n')),
          ASSIGNED_PREFIX.findall(_shell_heredocs(
              'cat <<EOF\nAUTUMN_LOG__LEVL=x cmd\n${AUTUMN_LOG__LEVEL}\nEOF\n',
              True))),
         (['AUTUMN_LOG__LEVEL'], []))
    # A Rust TEMPLATE is Rust: `build.rs.tmpl` needs the test mask too, and
    # `rel.endswith('.rs')` did not see it.
    case('a Rust template is Rust for the test mask as well',
         (effective_suffix('autumn-cli/src/templates/build.rs.tmpl'),
          'autumn-cli/src/templates/build.rs.tmpl'.endswith('.rs')),
         ('.rs', False))
    # Line count was a proxy; the test is whether the string is a Rust
    # PROGRAM. A multi-line help constant that quotes one accessor line is not
    # one, and passed the proxy.
    case('a multi-line string is not automatically generated code',
         [[m.group(0) for m in ACCESSOR.finditer(_rust_uncommented(s))
           if not _generated_data(_rust_uncommented(s))[m.start()]]
          for s in
          ('const HELP: &str = "Usage:\\n  set it, then\\n'
           '  std::env::var(\\\\"AUTUMN_X\\\\")\\n  and restart.";',
           'fn t() -> &\'static str { r#"use std::env;\nfn main() {\n'
           '    std::env::var("AUTUMN_X");\n}"# }')],
         [[], ['std::env::var(']])
    # The same mask applies to BINDINGS, not only to accessors — applying a
    # rule to one of the two rungs that ask it is this script's most repeated
    # mistake.
    case('a binding inside Rust string data is not a binding',
         ([m.group(2) for m in BOUND.finditer(_rust_uncommented(
             'let t = r#"const FAKE_ENV: &str = "AUTUMN_X";"#;'))
           if not _generated_data(_rust_uncommented(
               'let t = r#"const FAKE_ENV: &str = "AUTUMN_X";"#;'))[m.start()]],
          [m.group(2) for m in BOUND.finditer(_rust_uncommented(
              'const CANARY_ENV: &str = "AUTUMN_CANARY";'))]),
         ([], ['AUTUMN_CANARY']))
    # An executed key does not need a block scalar to be executed.
    case('an inline executed value is kept, an inline inert one is not',
         [_yaml_blocks(y, False).splitlines()[-1].strip() for y in
          ('steps:\n  - run: echo "${AUTUMN_X}"\n',
           'steps:\n  - name: echo "${AUTUMN_X}"\n',
           'steps:\n  - run: |\n      echo x\n')],
         # The key is blanked on an executed line: `run:` is YAML, not shell.
         ['echo "${AUTUMN_X}"', '', 'echo x'])
    # A FOLDED executed scalar is joined before the shell sees it, so an
    # assignment and its command on consecutive lines are one command. A
    # LITERAL `|` scalar keeps its newlines and must not be joined.
    case('a folded executed scalar is joined, a literal one is not',
         (ASSIGNED_PREFIX.findall(_shell_code(_yaml_blocks(
             'steps:\n  - run: >\n      AUTUMN_X=value\n      some-command\n',
             False))),
          _yaml_blocks('steps:\n  - run: |\n      AUTUMN_A=1\n      echo hi\n',
                       False).splitlines()[-2:]),
         (['AUTUMN_X'], ['      AUTUMN_A=1', '      echo hi']))
    # …but folding is per PARAGRAPH. A blank line survives as a newline and a
    # more-indented line keeps its breaks, so neither may be joined across —
    # the joined form invents a prefix assignment the workflow never runs.
    case('a folded scalar does not join across a blank or indented line',
         (ASSIGNED_PREFIX.findall(_shell_code(_yaml_blocks(
             'steps:\n  - run: >\n      AUTUMN_X=value\n\n      printf x\n',
             False))),
          ASSIGNED_PREFIX.findall(_shell_code(_yaml_blocks(
              'steps:\n  - run: >\n      AUTUMN_X=value\n        printf x\n',
              False))),
          [l.strip() for l in _yaml_blocks(
              'steps:\n  - run: >\n      a one\n      a two\n\n      b one\n',
              False).splitlines()[-4:]]),
         ([], [], ['a one a two', '', '', 'b one']))
    # PowerShell shares none of the Bourne grammar — `$NAME` is an ordinary
    # variable there, and only `$env:NAME` reaches the environment.
    case('an ordinary PowerShell variable is not an environment read',
         (EXPANDED.findall('$AUTUMN_LOG__LEVL'),
          PS_ENV.findall('$AUTUMN_LOG__LEVL'),
          PS_ENV.findall('$env:AUTUMN_LOG__LEVEL')),
         (['AUTUMN_LOG__LEVL'], [], ['AUTUMN_LOG__LEVEL']))
    # A multi-line string's CLOSING quote reads as an opener here, so an
    # unterminated quote must not protect a `#` later on the same line.
    case('an unterminated quote does not protect a trailing comment',
         (ASSIGNED_PREFIX.findall(_shell_code(uncommented(
             'printf "first\nsecond" # AUTUMN_X=v cmd\n', '#', True))),
          uncommented('echo "ok"  # note\n', '#', True).rstrip()),
         ([], 'echo "ok"'))
    # A YAML inline scalar's quotes are YAML's: removed before the command
    # reaches the shell, so the expansion inside really does happen.
    case('an inline YAML scalar is decoded before the shell pass',
         EXPANDED.findall(_shell_literals(_yaml_blocks(
             "steps:\n  - run: 'echo ${AUTUMN_X}'\n", False))),
         ['AUTUMN_X'])
    # …DECODED, not just unquoted. YAML resolves `\\` to one backslash before
    # the command exists, so the shell gets an escaped `$` and reads nothing —
    # while an escape YAML does not define keeps its backslash, because
    # resolving one that isn't there would invent a name.
    case('a double-quoted YAML scalar is decoded, not merely unwrapped',
         (EXPANDED.findall(_shell_literals(_yaml_blocks(
             'steps:\n  - run: "echo \\\\${AUTUMN_X}"\n', False))),
          EXPANDED.findall(_shell_literals(_yaml_blocks(
              'steps:\n  - run: "echo ${AUTUMN_X}"\n', False))),
          _yaml_decode(r'"say \"hi\""'), _yaml_decode(r"'it''s'"),
          _yaml_decode(r'"a\nb"'), _yaml_decode(r'"\x41"'),
          _yaml_decode(r'"\${AUTUMN_X}"')),
         ([], ['AUTUMN_X'], 'say "hi"', "it's", 'a;b', 'A',
          r'\${AUTUMN_X}'))
    # …and a flow scalar is not bounded by its first line. Both quote styles
    # fold and decode the same way across lines; an UNTERMINATED one is left
    # alone, since guessing its end would invent a command. Line count holds
    # either way — the continuations are blanked, not dropped.
    def _yb(y):
        return EXPANDED.findall(_shell_literals(_yaml_blocks(y, False)))
    _tail = '  - run: echo ${AUTUMN_TAIL}\n'
    case('a multi-line flow scalar is folded, decoded and kept',
         (_yb("steps:\n  - run: 'printf x\n      ${AUTUMN_X}'\n" + _tail),
          _yb('steps:\n  - run: "printf x\n      ${AUTUMN_X}"\n' + _tail),
          _yb("steps:\n  - run: 'unterminated ${AUTUMN_X}\n" + _tail),
          _yb("steps:\n  - name: 'printf x\n      ${AUTUMN_X}'\n" + _tail),
          _fold_flow(['a one', 'a two', '', 'b one']),
          (_flow_close("it''s x' rest", "'"), _flow_close(r'a\"b" rest', '"'),
           _flow_close('no close here', "'")),
          len(_yaml_blocks("steps:\n  - run: 'printf x\n      ${AUTUMN_X}'\n"
                           + _tail, False).splitlines())),
         (['AUTUMN_X', 'AUTUMN_TAIL'], ['AUTUMN_X', 'AUTUMN_TAIL'],
          ['AUTUMN_X', 'AUTUMN_TAIL'], ['AUTUMN_TAIL'],
          'a one a two ; b one', (7, 4, None), 4))
    # …and the key NAME does not decide it: a consumer executes a POSITION.
    # `run` under a workflow's `env:`, or in a file no consumer runs at all, is
    # data. The consumer is read off the file's SHAPE rather than its path,
    # because the generated `*-deploy.yml.tmpl` workflows are real workflows
    # and carry 16 of the names here.
    _wf = 'name: x\non: push\njobs:\n  build:\n    steps:\n'
    def _yr(y, consumer='actions'):
        return EXPANDED.findall(_shell_literals(_yaml_blocks(y, False,
                                                             consumer)))
    case('an executed YAML key is one the consumer runs, in position',
         (_yr(_wf + '      - run: echo ${AUTUMN_A}\n'),
          _yr(_wf + '      - name: s\n        env:\n'
                    '          run: "${AUTUMN_B}"\n'),
          _yr(_wf + '      - env:\n          run: |\n'
                    '            echo ${AUTUMN_C}\n'),
          _yr('project: x\nrun: "${AUTUMN_D}"\n', None),
          (_yaml_consumer('x.yml', _wf), _yaml_consumer('x.yml', 'a: 1\n'),
           _yaml_consumer('docker-compose.yml', 'a: 1\n'))),
         (['AUTUMN_A'], [], [], [], ('actions', None, 'compose')))
    # Compose keeps every value for EXPANSION and only the assigning ones for
    # ASSIGNMENT: `x-note:` is an extension field nothing runs, and `command:`
    # is exec'd rather than put through a shell unless it names one. Compose
    # also declares in three spellings and only one has an `=`.
    #
    # The assignment view can be SHORTER than the expansion view — blanking
    # the last line loses it to `splitlines` — which is what the padding in
    # `source_tokens` is for. What must hold here is that it is never longer.
    _comp = ('services:\n  app:\n'
             '    x-note: AUTUMN_NOTE=x ignored-command\n'
             '    environment:\n      - AUTUMN_ENVLIST=1\n'
             '      - AUTUMN_BARE\n      AUTUMN_MAP: value\n'
             '    command: AUTUMN_CMD=3 serve\n'
             '    entrypoint: ["sh", "-lc", "AUTUMN_SH=1 run"]\n')
    _cav, _cev = (_yaml_blocks(_comp, True, 'compose', True),
                  _yaml_blocks(_comp, True, 'compose'))
    case('a compose file has an expansion view and an assignment view',
         (ASSIGNED_PREFIX.findall(_cav), ASSIGNED_ANY.findall(_cav),
          COMPOSE_DECLARED.findall(_cav),
          ASSIGNED_PREFIX.findall(_cev),
          len(_cev.splitlines()) >= len(_cav.splitlines())),
         ([], ['AUTUMN_ENVLIST'],
          ['AUTUMN_ENVLIST', 'AUTUMN_BARE', 'AUTUMN_MAP'], [], True))
    # A compose command written as a SEQUENCE is a real command: the first
    # item names the executable and the block item is the payload.
    _seqc = ('services:\n  app:\n    command:\n      - bash\n      - -c\n'
             '      - |\n        AUTUMN_SEQ=1 true\n    x-after: done\n')
    _seqx = ('services:\n  app:\n    command:\n      - /app/x\n      - -c\n'
             '      - |\n        AUTUMN_NOSEQ=1 true\n')
    _seqn = ('services:\n  app:\n    command:\n      - bash\n'
             '      - |\n        AUTUMN_NOC=1 true\n')
    case('a sequence-form compose command is read when it runs a shell',
         (ASSIGNED_PREFIX.findall(_yaml_blocks(_seqc, True, 'compose', True)),
          ASSIGNED_PREFIX.findall(_yaml_blocks(_seqx, True, 'compose', True)),
          ASSIGNED_PREFIX.findall(_yaml_blocks(_seqn, True, 'compose', True))),
         (['AUTUMN_SEQ'], [], []))
    # Only the argument IMMEDIATELY after `-c` is the command string; a
    # later one is `$0`. An inline command string counts as well as a block.
    _seql = ('services:\n  a:\n    command:\n      - bash\n      - -c\n'
             '      - echo safe\n      - |\n        AUTUMN_LATE=1 true\n')
    _seqi = ('services:\n  a:\n    command:\n      - bash\n      - -c\n'
             '      - AUTUMN_INLINE=1 true\n')
    case('only the argument after -c is the command string',
         (ASSIGNED_PREFIX.findall(_yaml_blocks(_seql, True, 'compose', True)),
          ASSIGNED_PREFIX.findall(_yaml_blocks(_seqi, True, 'compose', True))),
         ([], ['AUTUMN_INLINE']))
    # Compose may quote either declaration spelling.
    case('a quoted compose declaration is still a declaration',
         COMPOSE_DECLARED.findall('      - AUTUMN_A=1\n      "AUTUMN_B": v\n'
                                  '      - "AUTUMN_C=1"\n'
                                  "      'AUTUMN_D': v\n      - AUTUMN_E\n"),
         ['AUTUMN_A', 'AUTUMN_B', 'AUTUMN_C', 'AUTUMN_D', 'AUTUMN_E'])
    # Naming the shell is half the claim; `-c` is the other half. `bash <text>`
    # runs a FILE of that name.
    case('a shell runs a command string only when asked to',
         [_runs_shell(v) for v in
          ('["sh", "-lc", "autumn migrate"]', '[bash, script]',
           'sh -c "echo"', 'pwsh -NoProfile -Command "."',
           'bash', '["/app/x"]', 'AUTUMN_X=v echo')],
         [True, False, True, True, False, False, False])
    # A custom Actions shell line is still a shell: what names the grammar is
    # the EXECUTABLE, and the JSON list form splits on the comma, not on space.
    case('a shell is named by its executable, in either spelling',
         [_shell_named(v) for v in
          ('pwsh -NoProfile -Command ". \'{0}\'"', '["sh","-lc","autumn x"]',
           '["/app/x"]', 'AUTUMN_X=v echo', '/bin/sh -c x', 'bash')],
         ['pwsh', 'sh', 'x', 'autumn_x=v', 'sh', 'bash'])
    # …normalised, because Windows spells it `pwsh.exe` and a `shell:` value is
    # not case-sensitive.
    case('an executable name is normalised before it is matched',
         [_shell_named(v) for v in ('pwsh.exe', 'PWSH', 'bash.exe', 'sh')],
         ['pwsh', 'pwsh', 'bash', 'sh'])
    # Only the command-STRING argument of an inline shell command is executed.
    case('an inline -c payload is the part that runs',
         [(lambda sp, t: t[sp[0]:sp[1]] if sp else None)(_payload_span(t), t)
          for t in ("sh -c 'AUTUMN_X=1 exec app'", 'sh -c "AUTUMN_Y=1 app"',
                    '/app/x -c "y"', 'sh "no dash c"')],
         ['AUTUMN_X=1 exec app', 'AUTUMN_Y=1 app', None, None])
    case('a custom pwsh shell line still selects PowerShell',
         sorted(_yaml_shells('on: push\njobs:\n  a:\n    defaults:\n'
                             '      run:\n'
                             '        shell: pwsh -NoProfile -Command ". \'{0}\'"\n'
                             '    steps:\n      - run: echo $AUTUMN_X\n')),
         [7])
    # …and which GRAMMAR a `run:` block is in is the workflow's to say, not the
    # file suffix's. All three declaration sites, including a step whose
    # `shell:` comes AFTER its `run:` — which is why the resolution is a
    # pre-pass rather than a forward walk.
    _ps = ('on: push\njobs:\n  a:\n    defaults:\n      run:\n'
           '        shell: pwsh\n    steps:\n      - run: |\n'
           '          echo $env:AUTUMN_X\n')
    case('a pwsh run block is read as PowerShell',
         (sorted(_yaml_shells(_ps)),
          sorted(_yaml_shells('on: push\ndefaults:\n  run:\n    shell: pwsh\n'
                              'jobs:\n  a:\n    steps:\n'
                              '      - run: echo $AUTUMN_X\n')),
          sorted(_yaml_shells('on: push\njobs:\n  a:\n    steps:\n'
                              '      - run: echo $AUTUMN_X\n'
                              '        shell: pwsh\n')),
          sorted(_yaml_shells('on: push\njobs:\n  a:\n    steps:\n'
                              '      - run: echo $AUTUMN_X\n')),
          PS_ENV.findall('echo $env:AUTUMN_X'),
          EXPANDED.findall('echo $env:AUTUMN_X')),
         ([7, 8], [7], [4], [], ['AUTUMN_X'], []))
    # …and the grammar under the rung must match it too: PowerShell escapes
    # with a backtick, so ``"`$env:X"`` prints the name and reads nothing,
    # while the Bourne pass leaves that dollar intact.
    case('a pwsh line is preprocessed with PowerShell quoting',
         (PS_ENV.findall(_shell_literals('Write-Output "`$env:AUTUMN_X"',
                                         '`', True)),
          PS_ENV.findall(_shell_literals('Write-Output "`$env:AUTUMN_X"',
                                         '\\', False)),
          PS_ENV.findall(_shell_literals('Write-Output "$env:AUTUMN_X"',
                                         '`', True)),
          PS_ENV.findall(_shell_literals("Write-Output '$env:AUTUMN_X'",
                                         '`', True))),
         ([], ['AUTUMN_X'], ['AUTUMN_X'], []))
    # Both PowerShell spellings reach the environment.
    case('the braced PowerShell environment form is a use',
         (PS_ENV.findall('${env:AUTUMN_X}'), PS_ENV.findall('$env:AUTUMN_X'),
          PS_ENV.findall('${AUTUMN_X}')),
         (['AUTUMN_X'], ['AUTUMN_X'], []))
    # Lua's long comments are the forms the SQL scanner cannot see, and the
    # delimiter length is part of the syntax — `]]` does not close `--[==[`.
    case('a Lua long comment is a comment',
         (ACCESSOR.search(uncommented(
             '--[[\nos.getenv("AUTUMN_X")\n]]\n', comment_leader('a.lua')))
          is None,
          ACCESSOR.search(uncommented(
              '--[==[\nos.getenv("AUTUMN_X")\n]]\nstill\n]==]\n',
              comment_leader('a.lua'))) is None,
          bool(ACCESSOR.search(uncommented(
              'os.getenv("AUTUMN_X")\n', comment_leader('a.lua'))))),
         (True, True, True))
    # The accessor and binding rungs are Rust's: outside Rust this script
    # cannot tell a call — or a declaration — from a string containing one,
    # because `_rust_classes` is what answers that.
    case('the Rust-shaped rungs belong to Rust',
         (effective_suffix('autumn/src/config.rs') == '.rs',
          effective_suffix('autumn-admin-plugin/src/admin.js') == '.rs',
          effective_suffix('autumn/src/security/rate_limit.lua') == '.rs',
          [m.group(2) for m in BOUND.finditer(
              'const FAKE_ENV: &str = "AUTUMN_X";')]),
         (True, False, False, ['AUTUMN_X']))
    # PowerShell reads the environment as `$env:NAME`, not `$NAME`.
    case('a PowerShell environment read is a use',
         (PS_ENV.findall('if ($env:AUTUMN_VERSION) { $env:AUTUMN_VERSION }'),
          PS_ENV.findall('$env:AUTUMN_PS_ONLY = "yes"'),
          PS_ENV.findall('$AUTUMN_NOT_ENV')),
         (['AUTUMN_VERSION', 'AUTUMN_VERSION'], ['AUTUMN_PS_ONLY'], []))
    # A separator inserted at the THIRD character leaves a three-character head,
    # and a four-character minimum made that one-edit typo invisible.
    case('a namespace split at the third character is scanned',
         [[m.group(0) for m in NEAR.finditer(w)
           if near_miss((m.group(1) + '_' + m.group(2).split('_')[0]).upper())]
          for w in ('AUT_UMN_LOG__LEVEL', 'AUTU_MN_LOG__LEVEL',
                    'AUTUMN_LOG__LEVEL', 'RUST_LOG', 'AWS_REGION')],
         [['AUT_UMN_LOG__LEVEL'], ['AUTU_MN_LOG__LEVEL'], [], [], []])
    # SQL is a third comment family, and the 171 tracked migrations were reading
    # a `-- AUTUMN_LOG__LEVL=x cmd` line as a shell assignment.
    case('a SQL comment is not a use',
         (ASSIGNED_PREFIX.findall(uncommented(
             '-- AUTUMN_LOG__LEVL=x cmd\nSELECT 1;\n', comment_leader('a.sql'))),
          ASSIGNED_PREFIX.findall(uncommented(
              '/* AUTUMN_LOG__LEVL=x cmd */\nSELECT 1;\n',
              comment_leader('a.sql')))),
         ([], []))
    # …while a string that merely contains the comment leader is still a string.
    case('a doubled quote keeps one SQL string whole',
         _sql_uncommented("SELECT 'it''s -- fine';\n"),
         "SELECT 'it''s -- fine';\n")
    # Auditing what else the `#` default was guessing about turned up seven more
    # languages whose comments a `#` rule cannot see at all.
    case('every unlisted comment family is stripped',
         [ASSIGNED_PREFIX.findall(
             uncommented(probe, comment_leader(rel), hash_needs_space(rel)))
          for rel, probe in
          (('a.java', '// AUTUMN_LOG__LEVL=x cmd\nint x;\n'),
           ('a.css', '/* AUTUMN_LOG__LEVL=x cmd */\na{color:red}\n'),
           ('a.html', '<!-- AUTUMN_LOG__LEVL=x cmd -->\n<p>hi</p>\n'),
           ('a.erb', '<%# AUTUMN_LOG__LEVL=x cmd %>\n'),
           ('a.ftl', '<#-- AUTUMN_LOG__LEVL=x cmd -->\n'),
           ('a.lua', '-- AUTUMN_LOG__LEVL=x cmd\n'))],
         [[]] * 6)
    # `//` is not a CSS comment: it is in every `url(…)`, and reading one as a
    # comment would hide real text rather than prose.
    case('a CSS url survives its block-only rule',
         uncommented('a{background:url(https://x/y)}\n', comment_leader('a.css')),
         'a{background:url(https://x/y)}\n')
    # A YAML quoted scalar may span lines, so the closing quote on the
    # continuation line was reading as a new opener and the `#` after it
    # survived as code.
    case('a multi-line YAML scalar does not reopen the quote',
         ASSIGNED_PREFIX.findall(uncommented(
             'value: "first\n second" # AUTUMN_LOG__LEVL=x cmd\n',
             comment_leader('a.yml'), hash_needs_space('a.yml'),
             False, False, True)), [])
    # …carried ONLY from a value position. An apostrophe in an unquoted scalar
    # opens nothing — carrying from one left 1192 comment lines in this tree
    # surviving as code, every one of them able to bless a name.
    case('an apostrophe in an unquoted YAML scalar carries nothing',
         ASSIGNED_PREFIX.findall(uncommented(
             "name: it's fine\nother: x # AUTUMN_LOG__LEVL=y cmd\n",
             comment_leader('a.yml'), hash_needs_space('a.yml'),
             False, False, True)), [])
    # …and the bound the carry does NOT relax: this pass runs before the heredoc
    # one, so an apostrophe in a sibling gate's embedded Python must still cost
    # one line rather than every comment below it.
    # An ESCAPED `$` is a literal one, so `"\${AUTUMN_X}"` prints the syntax and
    # reads nothing. Blanked in the shell pass, not taught to `EXPANDED`, which
    # also runs over Rust strings and YAML.
    case('an escaped dollar is not an expansion',
         (EXPANDED.findall(_shell_literals(
             'printf \'%s\' "\\${AUTUMN_LOG__LEVL}"')),
          EXPANDED.findall(_shell_literals('echo "${AUTUMN_LOG__LEVEL}"'))),
         ([], ['AUTUMN_LOG__LEVEL']))
    # A generated read is real; a string INSIDE generated code is data again.
    # The question is where the accessor sits, not where the name does — the
    # name is always inside a literal, because it is the argument.
    # Two kinds of string are DATA. A nested literal inside a template — the
    # reproducing shape is an outer RAW template, which is how a generator
    # naturally writes one — and a single-line string, which is not a program
    # at all. Both spellings of a real multi-line template have to survive.
    case('an accessor inside generated string data is not a call',
         [[m.group(0) for m in ACCESSOR.finditer(_rust_uncommented(s))
           if not _generated_data(_rust_uncommented(s))[m.start()]]
          for s in
          ('fn p() -> &\'static str { r#"const T: &str =\n'
           '"std::env::var(\\"AUTUMN_X\\")";"# }',
           'const HELP: &str = r#"std::env::var("AUTUMN_X")"#;',
           'fn p() -> &\'static str { "fn m() {\\n'
           'std::env::var(\\"AUTUMN_X\\");\\n}" }',
           'fn p() -> &\'static str { r#"fn m() {\n'
           'std::env::var("AUTUMN_X");\n}"# }')],
         [[], [], ['env::var('], ['std::env::var(']])
    case('a stray quote outside YAML still costs one line',
         _hash_uncommented("x = 'a\n# AUTUMN_LOG__LEVL=y cmd\nz = 1\n",
                           True).splitlines(),
         ["x = 'a", '', 'z = 1'])
    case('a double-quoted heredoc body is data',
         ASSIGNED_PREFIX.findall(_shell_literals(_shell_heredocs(
             'cat <<"EOF"\nAUTUMN_LOG__LEVL=x cmd\nEOF\n'))),
         [])

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
    # A duplicate row holds the count while standing in for a removed one, so
    # the ratchet alone does not catch it. Each name maps to one key.
    dup_var = [(1, 'AUTUMN_SERVER__PORT', 'server.port'),
               (2, 'AUTUMN_SERVER__PORT', 'server.port')]
    case('a duplicated variable is reported',
         len([why for _, _, _, why in
              check_table(dup_var, real_leaves, real_built, tokens)
              if 'already documented' in why]), 2)
    dup_path = [(1, 'AUTUMN_SERVER__PORT', 'server.port'),
                (2, 'AUTUMN_SERVER__HOST', 'server.port')]
    case('a duplicated config path is too',
         any('already documented' in why
             for _, _, _, why in check_table(dup_path, real_leaves,
                                             real_built, tokens)), True)
    # Two spellings of one mapping are still one mapping: an indexed shard row
    # duplicates the `{i}` row it is written beside.
    dup_index = [(1, 'AUTUMN_DATABASE__SHARDS__{i}__NAME',
                  'database.shards[i].name'),
                 (2, 'AUTUMN_DATABASE__SHARDS__0__NAME',
                  'database.shards[0].name')]
    case('an indexed spelling duplicates the placeholder row',
         any('already documented' in why
             for _, _, _, why in check_table(dup_index, real_leaves,
                                             real_built, tokens)), True)
    case('distinct rows are not',
         [why for _, _, _, why in
          check_table([(1, 'AUTUMN_SERVER__PORT', 'server.port'),
                       (2, 'AUTUMN_SERVER__HOST', 'server.host')],
                      real_leaves, real_built, tokens)
          if 'already documented' in why], [])
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
