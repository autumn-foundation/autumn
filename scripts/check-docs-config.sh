#!/usr/bin/env bash
# Config-key drift gate: every `autumn.toml` key the reader-facing docs tell
# someone to WRITE must exist in the config schema.
#
# WHY THIS EXISTS: the corpus already gates the first two things a reader copies
# off a page. `scripts/check-docs-links.sh` gates its *links*, so nobody is sent
# to a page that does not exist; `scripts/check-docs-cli.sh` gates its
# *commands*, so nobody is handed an `autumn …` line clap will reject. Nothing
# checked the third: the **config key**. The reader-facing corpus carries 168
# `autumn.toml` fences judged against 484 schema leaves, and a renamed or
# never-shipped key leaves behind a line that looks exactly like a working one.
#
# It is the worst of the three, because it is the only one that fails SILENTLY.
# `server.strict_config` is `false` by default (`config.rs`, `ServerConfig`), so
# serde drops an unknown key without a warning: the app boots, the setting the
# reader believed they changed keeps its default, and nothing anywhere says so.
# A bad link 404s and a bad command exits 2 — both are dead ends the reader can
# see and route around. A bad config key is a dead end they cannot see, and they
# take it to production believing the opposite of what is true.
#
# The baseline run found two, both of that shape:
#
#   `docs/guide/wizards.md` — under "For wizards where step data is expensive to
#   re-enter, consider increasing the session TTL in `autumn.toml`", a fence
#   writing `[session] ttl_seconds = 3600`. There is no `ttl_seconds` anywhere in
#   the source: the key is `session.max_age_secs` and its default is `86400`, so
#   the documented "increase" was also a 24x DECREASE if the reader hand-corrected
#   the name. Searching the guide corpus for the asker's own words, "session ttl",
#   returned that one page and no other — it was simultaneously the only page a
#   reader could land on for the question and the wrong answer to it.
#
#   `docs/guide/dev-error-overlay.md` — "If you prefer the plain 500 page without
#   the badge overlay, set the profile to production", over a fence marked
#   `# autumn.toml` writing `[app] profile = "production"`. There is no `[app]`
#   section, and `AutumnConfig::profile` is `#[serde(skip)]` — "Resolved at load
#   time, not deserialized from TOML" — so the profile cannot be set from the file
#   under ANY section name. The line traces to ADR 0006, which proposed exactly
#   that "one-line opt-out in `autumn.toml`"; the implementation resolved the
#   profile from `AUTUMN_ENV`/`--profile`/build mode instead, and the guide
#   shipped the proposal. The reader affected is one already staring at an error
#   page.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   Every table and key path inside an `autumn.toml` fence resolves against the
#   config schema, with the SAME walk semantics as the framework's own
#   unknown-key validator (`AutumnConfig::validate_toml`): a path that has a
#   schema entry has its children checked against that entry, and a path that has
#   NO schema entry is opaque — its children are not descended into. That second
#   rule is not a convenience, it is correctness: `jobs.queues`,
#   `auth.oauth2`, `http.client.base_urls` and
#   `resilience.circuit_breaker.hosts` are flatten/`HashMap` sections whose child
#   keys are arbitrary and valid (`[auth.oauth2.github]`), and the framework
#   deliberately registers no restrictive entry for them so strict config does not
#   reject a correct app. Reporting their children would make this gate reject the
#   corpus's correct pages, which is how a gate gets switched off.
#   `[profile.<name>]` overlays are stripped before the walk, so
#   `[profile.prod.server] port = 9000` is judged as `server.port`, exactly as the
#   loader resolves it.
#
#   ARRAYS OF TABLES are walked item by item against the SAME schema path, as
#   `validate_toml_table` does (there is no `shards.0` in the schema). This is
#   load-bearing, not a corner case: `[[database.shards]]` (`sharding.md`) and
#   `[[security.webhooks.endpoints]]` (`signed-webhooks.md`) are both live in the
#   corpus, and descending only into `dict` values left every key inside them
#   unchecked — a `primary_urll` there read as clean while the framework's own
#   strict validator rejects it.
#
# TRUTH SET — three mechanical sources, no hand-written key list:
#
#   1. `autumn/tests/fixtures/schema_keys.snapshot` (484 leaves) — the compiled
#      `AutumnConfig` schema. Normally a checked-in snapshot is the wrong
#      authority for a drift gate: it is one forgotten regeneration away from
#      gating the docs against a config that no longer exists, which is the very
#      failure this script is for. This one is safe because it does not depend on
#      anyone remembering. `schema_keys_snapshot_guard`
#      (`autumn/tests/integration/schema_drift_guard.rs`) asserts the snapshot
#      BOTH ways against `AutumnConfig::schema_leaf_paths()` on every CI run —
#      keys removed without a `DEPRECATED_CONFIG_KEYS` entry fail, and keys added
#      without regenerating fail too. The file cannot drift from the compiled
#      schema and stay green, so reading it here is equivalent to compiling the
#      config, at zero toolchain cost.
#
#   2. `config_section("<root>")` calls across the workspace — the roots a plugin
#      registers as known-and-opaque (`AppBuilder::config_section`, `app.rs`).
#      `autumn-search` registers `search` and the media plugin registers `media`,
#      so `[search] enabled = false` is a correct line in a correct page even
#      though `search` is not an `AutumnConfig` root. Their children are not
#      validated, matching the app-boot exemption, which treats a declared plugin
#      root as an opaque table — and, like that exemption, the root counts only
#      as a TABLE. `config_section` declares a config *table*, so a registered
#      root written as a scalar or array (`media = "enabled"`) is a malformed
#      section that nothing deserializes, and the app boots on default plugin
#      config; the runtime carries the same `val.is_table()` guard for exactly
#      that reason.
#
#   3. `autumn-cli/src/dev.rs`'s `DevConfig` struct — the `[dev]` keys the CLI
#      reads from `autumn.toml` itself, outside `AutumnConfig`. `watch_dirs` is
#      documented in README.md and is genuinely read (`load_dev_config`), so it is
#      a real key the framework schema cannot see. The FIELDS are parsed from that
#      struct rather than listed here, so renaming one moves the gate with it in
#      the same commit; if the struct is ever renamed or moved the parse yields
#      nothing and the gate fails loudly on README.md rather than silently
#      widening.
#
# WHICH FENCES ARE READ. Of the corpus's 248 ```toml fences, 168 are read. A
# fence is read only on POSITIVE identification — it names `autumn.toml` (or a
# profile overlay / the `.example` template), or it carries a section the config
# surface recognizes.
#
# MARKERS BEAT HEURISTICS — all of them. The rules below are guesses about what a
# fence IS; a marker is the page SAYING so, and a guess must never overrule a
# statement. Getting that order wrong opened three holes at once: a
# `# autumn.toml` fence whose section happened to be `[package]` was written off
# as a Cargo manifest, one with no section at all as a fragment, and a
# `[profile.prod] debug = 1` under the marker as a Cargo profile — each invalid
# Autumn config, each reported clean. So the marker is read first, and only an
# unmarked fence reaches the heuristics. (A marker naming ANOTHER file is read
# before either, so a lead-in naming both — "add this to Cargo.toml next to your
# autumn.toml" — resolves to the other file, the safe direction.)
#
# This corpus is a Rust framework's, so most of its TOML is some other file, and
# several families of it collide with real Autumn section names:
#
#   - **Cargo manifests** (21 unmarked fences, plus 20 more that name
#     `Cargo.toml` outright and are skipped as `other-file` before the
#     root-name heuristic is consulted). Skipped on their root sections
#     (`[package]`, `[dependencies]`, `[features]`, …).
#   - **Cargo profiles**. `[profile.dev] debug = "line-tables-only"` in
#     `platform-support.md` is a Cargo profile, and `[profile.<name>]` is ALSO
#     Autumn's overlay syntax — the same four characters mean two different files.
#     Disambiguated on the leaf keys, not the header: a `[profile.X]` table whose
#     keys are all Cargo profile knobs (`debug`, `opt-level`, `lto`, `panic`,
#     `strip`, …) is Cargo's, and Autumn's overlay carries config SECTIONS, never
#     those names. Header-based skipping would have cost the real overlay fences,
#     which are the production-tuning ones most worth gating.
#   - **`fly.toml`** (4 of the 37 `other-file` fences). Fly's `[deploy]` table takes `release_command`
#     and `kill_timeout`; Autumn's `[deploy]` takes `host`, `app_name`, `app_dir`.
#     Zero key overlap, but the section name is identical, so the fence is read as
#     Fly's only when it SAYS so — a `# fly.toml` marker comment in the fence, or
#     prose naming a `.toml` file other than `autumn.toml` in the three lines
#     above it. That rule is deliberately strict rather than key-set inference,
#     because the marker is what the mid-task reader needs too: someone who
#     ctrl-Fs to `release_command` and lands on a bare `[deploy]` fence, in a
#     corpus where `[deploy]` is also an `autumn.toml` section, cannot tell which
#     file it belongs in either. Where the gate needed the marker, so did they.
#   - **Other Autumn-branded TOML** (10 fences), which the config schema must not
#     be applied to: the `autumn credentials` store
#     (`[active_record_encryption]`, `[acme_dns]`), a capacity contract
#     (`[provenance]`, `[envelope]`, `[calibration]`), a sandboxed-plugin manifest
#     (`[capabilities]`, `[limits]`), `autumn-starter.toml` (`[starter]`),
#     `autumn.generate.toml` (`[scaffold.<Resource>]`), and the data-scrubbing
#     file (`[framework]`). Every one of the 10 was resolved by hand against its
#     page: none is an `autumn.toml` fence, and reading them would have reported
#     24 correct lines across 8 correct pages as drift. A gate that cries wolf on
#     correct pages is one people learn to switch off.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - **VALUES.** Only key existence — with one exception, below. A key's type,
#     range, and whether the value is sensible are the app's to reject; this gate
#     answers the one question the reader cannot answer for themselves, because
#     nothing tells them: does this key exist at all?
#
# THE ONE EXCEPTION — an identified `autumn.toml` fence that does NOT PARSE is a
# defect, not a skip. It is presented as a file to copy, and TOML that does not
# parse fails at boot for whoever copies it; skipping it also leaves every key in
# it unchecked, so the silent-key class this gate exists for can hide behind a
# parse error. The baseline found one: `cloud-native.md`'s read-your-writes block
# set `read_your_writes` twice in one `[database]` table — a duplicate-key error
# — under prose telling the reader to "Add `read_your_writes` in `[database]`".
# It is now two fences, one per option, which is what a reader picking between
# them needed anyway. There is no waiver for this, on the same principle
# `check-docs-cli.sh` applies to fenced commands: a fence is copyable, so the
# remedy is to make it parse — split mutually exclusive options into one fence
# each, or quote an elided placeholder (`"<YOUR_URL>"`) so it is valid TOML.
#   - **Fences with no `[section]` header, no marker, and no dotted key** (12).
#     A bare `key = value` fragment has no root to resolve against, and inferring
#     one from the surrounding prose would invent defects; a marked one IS read
#     (see above), and so is one carrying a DOTTED key whose first segment is a
#     known root — `server.prot = 9000` declares `[server]` without a header, and
#     TOML treats the two spellings as the same document.
#
#     Only the dotted form, though, and that line is drawn on a measurement.
#     `skills/autumn-web/references/api-reference.md` carries a headerless Cargo
#     DEPENDENCY list in which `http` is both a crate everyone depends on and an
#     Autumn schema root, so a "known root among top-level keys" rule admits that
#     fence and then reports its other ~22 crate names as unknown config roots —
#     22 correct lines on a correct page. Inline tables are the same hazard once
#     removed: dependency lists are written `diesel = { version = "2", … }`, and
#     nine schema roots (`cache`, `deploy`, `http`, `jobs`, `log`, `mail`, `role`,
#     `session`, `storage`) are plausible crate names, so that fence's clean
#     result today is luck rather than design. A dotted key carries no such
#     collision: it is a table declaration, not a name. All 12 were resolved by hand: 8 carry no keys at all (empty or
#     comment-only), one is a daemon state file (`pid`, `started_at`), one is a
#     dependency version list, and one — `operator-alerts.md`'s `pagerduty_url`
#     fragment — was a real `[alerts]` key that the fence simply did not name.
#     That was fixed in the DOCS rather than here: the fence now repeats its
#     `[alerts]` header, like its sibling six lines below, which is what a reader
#     arriving mid-page by ctrl-F needs in order to know where the key goes.
#   - Fences nested in a Markdown BLOCKQUOTE are read like any other: the quote
#     prefix is stripped from every line first. `docs/guide/tls.md` documents the
#     ACME `directory` and `ca_root_path` keys in two `[server.tls.acme]` fences
#     inside an aside, and a `^`-anchored fence regex never saw them — 2 live
#     reader-facing fences, silently uncovered. Stripping cannot move a reported
#     line number (removing a prefix removes no newline, asserted in --self-test)
#     and cannot invent a fence (no TOML line starts with `>`).
#     `check-docs-cli.sh` learned this the same way: a live `autumn migrate run`
#     hid behind a `\n> ` break in migrations.md.
#
#   - **`examples/<app>/content/`.** Seed content for an example app, rendered by
#     that app's own routes — the same exclusion, for the same reason,
#     `check-docs-links.sh` makes.
#   - **The CHILDREN of a plugin root** (`[media] …`, `[search] …`). Opacity
#     mirrors the runtime, whose contract is that the plugin owns validation of
#     its own subtree, and `SearchConfig` holds up that end —
#     `#[serde(deny_unknown_fields)]`, so a typo'd `queu` is a boot error, loudly.
#     `MediaConfig` does NOT: it is `#[serde(default)]` without
#     `deny_unknown_fields`, so a typo'd `[media]` key is silently dropped and the
#     default stands — the very class this gate exists for, in the one subtree it
#     cannot see. That gap is REAL and is deliberately not closed here.
#
#     All 35 `[media]` keys across the corpus's three `[media]` fences were
#     resolved by hand and every one is correct, so nothing is broken today.
#     Extracting a schema for them is also not the cheap fix it looks like: the
#     `[media]` subtree is owned by TWO structs in two crates — the runtime
#     `autumn-media-plugin::MediaConfig` and the deploy-side host config in
#     `autumn-cli/src/deploy/media.rs` — and a naive single-file extraction
#     reported 13 correct lines in `deployment.md` (`api_port`, `unit_name`,
#     `binary_path`, …) as drift, because they live in the other struct. The
#     real fix is one line in the plugin (`deny_unknown_fields` on `MediaConfig`,
#     as `SearchConfig` already has), which makes the runtime enforce the
#     contract the opacity rule already assumes — a plugin change, not a docs
#     one.
#   - **The archive trees** — `docs/plans/`, `docs/adr/`, `docs/design/`,
#     `docs/stories/`, `docs/reports/`, `docs/releases/`, `docs/migrations/`,
#     `CHANGELOG.md`, `RELEASE_NOTES.md`, `benchmarks/`, `bmad/`, `agents/`, and
#     the dated top-level design documents. These are records of what was proposed
#     or shipped at a point in time, not instructions a reader follows today: a
#     migration guide names the OLD key on purpose, and a 2026-03 design document
#     naming `[logging]` (the section that shipped as `[log]`) is an accurate
#     record of the proposal. Gating them would either freeze history or bury the
#     gate in permanent noise, and a gate people learn to ignore has stopped
#     working. The baseline sweep of those trees found 13 out-of-schema keys, and
#     every one is a superseded proposal, not a live instruction: `[logging]` and
#     `database.pool` (shipped as `[log]` and `pool_size`), `[harvest]` and
#     `[static]` (designs with no config surface yet), three superseded
#     `[actuator]` keys, and ADR 0006's `[app] profile` — the proposal the
#     dev-error-overlay guide page shipped verbatim, which is exactly why the
#     gate reads the guide and not the ADR.
#
# USAGE:
#   scripts/check-docs-config.sh              # gate the corpus
#   scripts/check-docs-config.sh --list       # every fence read, and its verdict
#   scripts/check-docs-config.sh --self-test  # synthetic-corpus tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

# The checker reads a repo root and prints one line per defect. Kept in Python
# for `tomllib` (standard library since 3.11): hand-rolling a TOML parser in
# bash to decide whether a documented key exists is exactly the sort of
# approximate parsing that produces confident nonsense. python3 is already a
# dependency of scripts/check-plugin-freshness.sh and scripts/check-docs-cli.sh.
run_py() {
  python3 - "$@" <<'PYEOF'
import collections
import os
import re
import subprocess
import sys
import tomllib

MODE = sys.argv[1]
ROOT = sys.argv[2]

# ── Truth set 1: the compiled config schema ───────────────────────────────────

SNAPSHOT = os.path.join(ROOT, "autumn", "tests", "fixtures", "schema_keys.snapshot")


def load_schema():
    """Parent path -> allowed child keys, rebuilt from the leaf-path snapshot.

    The framework's validator holds `HashMap<String, HashSet<String>>` (parent ->
    children) and the snapshot holds the leaf paths that map produces. Rebuilding
    one from the other is exact, INCLUDING the property the walk depends on: a
    path that is a leaf and never a parent (`jobs.queues`, `auth.oauth2`) gets no
    entry here, which is precisely how the framework marks a dynamic-key section
    it must not validate the children of.
    """
    with open(SNAPSHOT, encoding="utf-8") as fh:
        leaves = [line.strip() for line in fh if line.strip()]
    if not leaves:
        sys.exit(f"FAIL: {SNAPSHOT} is empty; the gate has no truth set to read.")
    schema = collections.defaultdict(set)
    for leaf in leaves:
        segments = leaf.split(".")
        for i, seg in enumerate(segments):
            schema[".".join(segments[:i])].add(seg)
    return schema, len(leaves)


# ── Truth set 2: plugin-declared config roots ─────────────────────────────────

# `AppBuilder::config_section("media")` registers `[media]` as known-and-opaque
# under strict config. Read from the source rather than listed, so a new plugin
# root needs no edit here. Test-only placeholders (`my.plugin`) come along; they
# only ever EXEMPT a root, and no page writes them as a real section.
CONFIG_SECTION_CALL = re.compile(r'(?<!has_)config_section\(\s*"([A-Za-z0-9_.]+)"')


def plugin_roots():
    files = subprocess.run(
        ["git", "ls-files", "-z", "*.rs"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split("\0")
    roots = set()
    for rel in filter(None, files):
        with open(os.path.join(ROOT, rel), encoding="utf-8", errors="ignore") as fh:
            roots.update(CONFIG_SECTION_CALL.findall(fh.read()))
    return roots


# ── Truth set 3: `[dev]` keys owned by the CLI, not by AutumnConfig ───────────

DEV_CONFIG_SOURCE = os.path.join(ROOT, "autumn-cli", "src", "dev.rs")
DEV_CONFIG_STRUCT = re.compile(r"struct\s+DevConfig\s*\{(.*?)\n\}", re.S)
STRUCT_FIELD = re.compile(r"^\s*(?:pub\s+)?([a-z_][a-z0-9_]*)\s*:", re.M)


def cli_dev_keys():
    """Field names of `autumn-cli`'s `DevConfig`, which deserializes `[dev]`.

    Returns an empty set if the struct cannot be found, which makes the gate
    report README.md's `dev.watch_dirs` rather than silently keep exempting a
    section the CLI may have stopped reading. A loud failure on a rename is the
    behaviour we want from a truth set.
    """
    try:
        with open(DEV_CONFIG_SOURCE, encoding="utf-8") as fh:
            src = fh.read()
    except OSError:
        return set()
    match = DEV_CONFIG_STRUCT.search(src)
    if not match:
        return set()
    return set(STRUCT_FIELD.findall(match.group(1)))


# ── Corpus ────────────────────────────────────────────────────────────────────

# Records of what was proposed or shipped at a point in time, not instructions a
# reader follows today. See the header for why gating these is worse than not.
ARCHIVE_PREFIXES = (
    "docs/plans/",
    "docs/adr/",
    "docs/design/",
    "docs/stories/",
    "docs/reports/",
    "docs/releases/",
    "docs/migrations/",
    "docs/schemas/",
    "docs/perf/",
    "benchmarks/",
    "bmad/",
    "agents/",
)
ARCHIVE_FILES = (
    "CHANGELOG.md",
    "RELEASE_NOTES.md",
    "docs/architecture-autumn-2026-03-20.md",
    "docs/autumn-workflow-architecture.md",
    "docs/brainstorming-hybrid-rendering-2026-03-26.md",
    "docs/brainstorming-technical-challenges-2026-03-20.md",
    "docs/echo-dx-audit.md",
    "docs/prd-autumn-2026-03-20.md",
    "docs/product-brief-autumn-2026-03-20.md",
    "docs/research-competitive-technical-2026-03-20.md",
    "docs/sprint-plan-autumn-2026-03-20.md",
    "dx_audit_report.md",
    "eris_advisories.md",
)


# `examples/<app>/content/…` is seed content for an example app, rendered by
# that app's own routes — it documents the example's subject matter, not
# Autumn's config. Anchored to `examples/<app>/content/` rather than any path
# containing `/content/`, for the reason check-docs-links.sh gives: a substring
# test would silently drop a real docs tree named `content`.
EXAMPLE_CONTENT = re.compile(r"^examples/[^/]+/content/")


def corpus():
    # NUL-delimited for the same reason check-docs-links.sh uses it: a path with
    # whitespace would otherwise split into fragments and be silently skipped.
    files = subprocess.run(
        ["git", "ls-files", "-z", "*.md"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split("\0")
    return [
        f
        for f in filter(None, files)
        if not f.startswith(ARCHIVE_PREFIXES)
        and f not in ARCHIVE_FILES
        and not EXAMPLE_CONTENT.match(f)
    ]


# ── Fence extraction and classification ───────────────────────────────────────

TOML_FENCE = re.compile(r"^([ \t]*)```toml[ \t]*\n(.*?)^\1```", re.S | re.M)

# A Markdown blockquote prefix (`> `, or nested `> > `), at the start of a line.
#
# Config fences live inside blockquotes in this corpus — `docs/guide/tls.md`
# documents the ACME `directory` and `ca_root_path` keys in two `[server.tls.acme]`
# fences nested in an aside — and every line of those begins with `> `, so a fence
# regex anchored at `^` never sees them. Stripping the prefix from every line
# before extracting makes them ordinary fences. It cannot shift a reported line
# number, because removing a prefix never removes a newline (asserted in
# --self-test), and it cannot invent a fence, because TOML has no line that
# starts with `>`.
#
# `check-docs-cli.sh` learned the same lesson the same way: a live
# `autumn migrate run` hid behind a `\n> ` break in migrations.md.
BLOCKQUOTE_PREFIX = re.compile(r"^[ \t]*(?:>[ \t]?)+", re.M)
SECTION_HEADER = re.compile(r'^\s*\[\[?([A-Za-z0-9_.\-"]+)', re.M)

# A top-level DOTTED assignment: `server.prot = 9000` declares `[server]` without
# a header, and TOML treats the two spellings as the same document. Captures only
# the first segment, which is the root being declared.
DOTTED_ROOT = re.compile(r'^[ \t]*([A-Za-z0-9_-]+)[ \t]*\.[ \t]*[A-Za-z0-9_."\']+[ \t]*=', re.M)

# Root tables that make a fence a Cargo manifest.
CARGO_ROOTS = {
    "package",
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "workspace",
    "features",
    "lib",
    "bin",
    "patch",
    "target",
    "bench",
    "lints",
    "badges",
    "replace",
}

# Keys Cargo's `[profile.<name>]` takes. Autumn's `[profile.<name>]` overlay
# carries config SECTIONS, so the two never look alike below the header.
CARGO_PROFILE_KEYS = {
    "codegen-units",
    "debug",
    "debug-assertions",
    "incremental",
    "inherits",
    "lto",
    "opt-level",
    "overflow-checks",
    "panic",
    "rpath",
    "split-debuginfo",
    "strip",
}

# A marker naming a TOML file, inside the fence or in the prose just above it.
# `autumn.toml` and its profile siblings (`autumn-prod.toml`) identify the fence
# as one this gate reads; any other name identifies it as one it must not.
# `.example` is captured because the container scaffolding ships the production
# config as `autumn.production.toml.example` and copies it to `/app/autumn.toml`
# at build time — same schema, so `deployment.md`'s "Customising the production
# config" fence must be read, not skipped as some other file.
TOML_FILE_NAME = re.compile(r"\b([A-Za-z0-9_.\-]+\.toml(?:\.example)?)\b")

# The overlay form is literally `format!("autumn-{profile_name}.toml")`
# (`config.rs`), and the corpus names a dozen profiles that way — `autumn-dev`,
# `autumn-docker`, `autumn-capsules`, `autumn-split-web`. So the NAME cannot tell
# an overlay from the two other Autumn-branded TOML formats, which are different
# schemas entirely and must not be judged against the config:
#   `autumn-starter.toml`  — the starter manifest (`autumn-cli/src/starters/manifest.rs`)
#   `autumn.generate.toml` — the scaffold intent file (`docs/guide/generators.md`)
NOT_CONFIG_TOML = {"autumn-starter.toml", "autumn.generate.toml"}
# Both separators: the loader reads `autumn-<profile>.toml`, and the container
# template ships as `autumn.production.toml.example`.
AUTUMN_TOML_NAME = re.compile(r"^autumn([-.][A-Za-z0-9_.\-]+?)?\.toml(\.example)?$")

# How many lines above a fence count as its introduction. Three covers the
# corpus's "…in `fly.toml`:" lead-ins, including one that wraps across a line,
# without reaching back into an unrelated paragraph.
LEAD_IN_LINES = 3


def marker_files(body, lead_in):
    """(autumn-ish names, other names) marking this fence, fence first then prose."""
    leading_comments = "\n".join(
        line for line in body.split("\n")[:3] if line.strip().startswith("#")
    )
    ours, theirs = named_toml_files(leading_comments)
    if not (ours or theirs):
        ours, theirs = named_toml_files(lead_in)
    return ours, theirs


def named_toml_files(text):
    """(names of autumn.toml-ish files, names of other .toml files) in `text`."""
    ours, theirs = [], []
    for name in TOML_FILE_NAME.findall(text):
        base = name[: -len(".example")] if name.endswith(".example") else name
        mine = AUTUMN_TOML_NAME.match(name) and base not in NOT_CONFIG_TOML
        (ours if mine else theirs).append(name)
    return ours, theirs


def classify(body, lead_in, known_roots):
    """Why this fence is not read as `autumn.toml`, or None if it is.

    A fence is read only on POSITIVE identification — it says it is
    `autumn.toml`, or it demonstrably contains `autumn.toml` sections. Negative
    identification alone ("nothing marks it as another file") is not enough,
    because most TOML in this corpus is not `autumn.toml` and carries no marker:
    the `autumn credentials` store (`[active_record_encryption]`, `[acme_dns]`),
    a capacity contract (`[provenance]`, `[envelope]`), a sandboxed-plugin
    manifest (`[capabilities]`, `[limits]`), `autumn-starter.toml`
    (`[starter]`), `autumn.generate.toml` (`[scaffold.<Resource>]`), and the
    data-scrubbing file (`[framework]`). Reading those would report 24 correct
    lines across 8 correct pages as drift — and a gate that cries wolf on
    correct pages is one people learn to switch off.

    The cost of requiring positive identification is stated in the header: an
    `autumn.toml` fence whose sections are ALL unrecognized and which names no
    file is not read. The corpus was swept for that shape and has none — the one
    fence that came close, `docs/guide/dev-error-overlay.md`'s impossible
    `[app] profile`, carries a `# autumn.toml` marker and IS read, which is how
    the baseline caught it.
    """
    headers = SECTION_HEADER.findall(body)
    roots = {h.split(".")[0].strip('"') for h in headers}

    # MARKERS BEAT HEURISTICS — all of them. The Cargo-root and headerless rules
    # below are guesses about what a fence IS; a marker is the page SAYING so, and
    # a guess must never overrule a statement. Ordering this the other way round
    # made three separate holes, each found the same way: a `# autumn.toml` fence
    # whose section happened to be `[package]` was written off as a Cargo
    # manifest, one with no section at all was written off as a fragment, and a
    # `[profile.prod] debug = 1` under the marker was written off as a Cargo
    # profile — all three invalid Autumn config, reported as clean.
    ours, theirs = marker_files(body, lead_in)
    # A marker naming another file settles it, even alongside a recognized root:
    # `fly.toml`'s `[deploy]` is spelled exactly like Autumn's. Checked before
    # `ours` so a lead-in naming BOTH files ("add this to Cargo.toml next to your
    # autumn.toml") is read as the other file, which is the safe direction.
    if theirs:
        return f"other-file:{theirs[0]}"
    if ours:
        return None
    if roots & CARGO_ROOTS:
        return "cargo-manifest"
    # A DOTTED key declares its table without a header: `server.prot = 9000` is
    # `[server] prot`, and TOML treats the two as identical. Admitting on that
    # signal costs nothing and closes a hole a bare-key rule cannot.
    #
    # Only the dotted form, deliberately — NOT a bare key or an inline table
    # whose name merely collides with a root. That distinction is measured, not
    # cautious-by-default: `skills/autumn-web/references/api-reference.md` carries
    # a headerless Cargo DEPENDENCY list, and `http` is both a crate everyone
    # depends on and an Autumn schema root. A "known root among top-level keys"
    # rule admits that fence and then reports its other ~22 crate names as unknown
    # config roots — 22 correct lines on a correct page. Inline tables are the
    # same hazard one step removed: dependency lists are written as
    # `diesel = { version = "2", features = [...] }`, and nine schema roots
    # (`cache`, `deploy`, `http`, `jobs`, `log`, `mail`, `role`, `session`,
    # `storage`) are plausible crate names, so today's clean result there is luck
    # rather than design. A dotted key has no such collision: it is a table
    # declaration, not a name.
    if any(seg in known_roots for seg in DOTTED_ROOT.findall(body)):
        return None
    if not headers:
        return "no-section-header"
    # `profile` counts as a recognized root even though it is `#[serde(skip)]`
    # and so absent from the schema: a fence that is ONLY overlays
    # (`[profile.prod.server]`, and nothing else) would otherwise go unread, and
    # those are exactly the production-tuning fences worth gating. A Cargo
    # `[profile.dev]` fence reaches the walk this way too and is neutralized
    # there by `is_cargo_profile`, which reads the leaf keys rather than guessing
    # from the header.
    if roots & (known_roots | {"profile"}):
        return None
    return "no-autumn-section"


def is_cargo_profile(name, table):
    """`[profile.<name>]` whose keys are all Cargo profile knobs."""
    scalars = {k for k, v in table.items() if not isinstance(v, dict)}
    return bool(scalars) and scalars <= CARGO_PROFILE_KEYS


# ── The walk (mirrors AutumnConfig::validate_toml_table) ──────────────────────


def walk(table, path, schema, out, plugin_roots=frozenset()):
    for key, value in table.items():
        full = f"{path}.{key}" if path else key
        if path not in schema:
            # No schema entry: a dynamic-key or plugin-owned section. Its children
            # are arbitrary and valid, exactly as the framework treats them.
            continue
        if not path and key in plugin_roots:
            # A plugin-declared root is known and OPAQUE — but only as a TABLE.
            # The runtime's exemption carries the same `val.is_table()` guard
            # (`config.rs`), because `config_section` declares a config *table*:
            # `media = "enabled"` or `media = ["a"]` is a malformed section that
            # nothing deserializes, so the app boots on default plugin config.
            # Accepting a scalar here would document exactly that.
            if isinstance(value, dict):
                continue
            out.append(full)
            continue
        if key not in schema[path]:
            out.append(full)
            continue
        descend(value, full, schema, out, plugin_roots)


def descend(value, path, schema, out, plugin_roots=frozenset()):
    """Recurse into a value, following ARRAYS OF TABLES as the framework does.

    `[[database.shards]]` and `[[security.webhooks.endpoints]]` reach `tomllib`
    as a list of dicts, and `validate_toml_table` walks each table item against
    the SAME schema path (not an indexed one — there is no `shards.0` in the
    schema). Treating only `dict` as descendable left every key inside an array
    entry unchecked: `[[database.shards]] primary_urll = …` in `sharding.md`
    would have passed this gate while the framework's own strict validator
    rejects it. Both arrays are live in the corpus this gate reads.
    """
    if isinstance(value, dict):
        walk(value, path, schema, out, plugin_roots)
    elif isinstance(value, list):
        for item in value:
            if isinstance(item, dict):
                walk(item, path, schema, out, plugin_roots)


# Marks a finding as a parse defect rather than an unknown key, so the report
# can phrase it correctly — the two have different remedies.
PARSE_DEFECT = "<parse> "


def check_fence(body, schema, plugin_roots=frozenset(), marked=False):
    """Out-of-schema key paths in one fence.

    A fence that has already been IDENTIFIED as `autumn.toml` and does not parse
    is itself a defect, not something to skip: it is presented as a file to copy,
    and TOML that does not parse fails at boot for whoever copies it. The
    baseline found one — `cloud-native.md`'s read-your-writes block set
    `read_your_writes` twice in one `[database]` table (a duplicate-key error) —
    and skipping it also left every other key in that fence unchecked, so the
    silent-key class this gate exists for could hide behind a parse error.

    There is no waiver for this, deliberately, on the same principle
    `check-docs-cli.sh` applies to fenced commands: a fence is copyable, so the
    remedy is to make it parse — split mutually exclusive options into one fence
    each (which is what a reader needs anyway), or quote an elided placeholder
    (`"<YOUR_URL>"`) so it is valid TOML.
    """
    try:
        table = tomllib.loads(body)
    except (tomllib.TOMLDecodeError, ValueError) as exc:
        return [f"{PARSE_DEFECT}does not parse as TOML: {exc}"]
    findings = []
    for key, value in table.items():
        if key == "profile" and isinstance(value, dict):
            # `[profile.prod.server] port = …` resolves as `server.port`.
            for name, overlay in value.items():
                # `[[profile.x]]` reaches here as a list, which the framework
                # also walks item by item.
                entries = overlay if isinstance(overlay, list) else [overlay]
                for entry in entries:
                    if not isinstance(entry, dict):
                        # `[profile] prod = "x"` — a profile name whose value is
                        # not a table. It parses, and a non-strict app ignores it
                        # silently, so it is the gate's to report;
                        # `validate_toml_table` reports `profile.prod` too.
                        findings.append(f"profile.{name}")
                        continue
                    # The Cargo-profile exemption is a HEURISTIC about which file
                    # a `[profile.x]` belongs to, so it may only override an
                    # unmarked fence. Under an explicit `# autumn.toml` marker the
                    # page has already said which file this is, and `debug = 1`
                    # there is an unknown Autumn root after profile resolution,
                    # not a Cargo knob.
                    if not marked and is_cargo_profile(name, entry):
                        continue
                    walk(entry, "", schema, findings, plugin_roots)
        else:
            walk({key: value}, "", schema, findings, plugin_roots)
    return findings


# ── Driver ────────────────────────────────────────────────────────────────────


def build_schema():
    schema, leaf_count = load_schema()
    # Plugin roots and CLI-owned `[dev]` keys join the root entry; plugin roots
    # get no entry of their own, which makes them opaque — children unchecked.
    # They are ALSO returned separately, so `walk` can enforce the runtime's
    # table-only guard on them rather than accepting any value.
    extra_roots = plugin_roots()
    schema[""].update(extra_roots)
    dev_keys = cli_dev_keys()
    if dev_keys:
        schema[""].add("dev")
        schema["dev"].update(dev_keys)
    return schema, leaf_count, extra_roots, dev_keys


def scan(schema, plugin_root_set=frozenset()):
    findings = []
    fences = []
    known_roots = schema[""]
    for rel in corpus():
        with open(os.path.join(ROOT, rel), encoding="utf-8", errors="ignore") as fh:
            text = BLOCKQUOTE_PREFIX.sub("", fh.read())
        for match in TOML_FENCE.finditer(text):
            body = match.group(2)
            line = text[: match.start()].count("\n") + 1
            lead_in = "\n".join(text[: match.start()].split("\n")[-(LEAD_IN_LINES + 1) : -1])
            skip = classify(body, lead_in, known_roots)
            if skip:
                fences.append((rel, line, skip, []))
                continue
            marked = bool(marker_files(body, lead_in)[0])
            bad = check_fence(body, schema, plugin_root_set, marked)
            fences.append((rel, line, "checked", bad))
            for path in bad:
                findings.append((rel, line, path))
    return findings, fences


def suggest(path, schema):
    """Closest sibling key, for the 'did you mean' line."""
    import difflib

    parent, _, leaf = path.rpartition(".")
    siblings = schema.get(parent, set())
    close = difflib.get_close_matches(leaf, sorted(siblings), n=1, cutoff=0.6)
    if not close:
        return None
    return f"{parent}.{close[0]}" if parent else close[0]


def main():
    schema, leaf_count, extra_roots, dev_keys = build_schema()
    findings, fences = scan(schema, extra_roots)

    if MODE == "--list":
        by_verdict = collections.Counter(v.split(":")[0] for _, _, v, _ in fences)
        print(f"schema leaves: {leaf_count}")
        print(f"plugin config roots: {' '.join(sorted(extra_roots)) or '(none)'}")
        print(f"cli-owned [dev] keys: {' '.join(sorted(dev_keys)) or '(none)'}")
        print(f"corpus files: {len(corpus())}")
        print(f"toml fences: {len(fences)}  " + "  ".join(f"{k}={v}" for k, v in sorted(by_verdict.items())))
        for rel, line, verdict, bad in fences:
            flag = f"  -> {' '.join(bad)}" if bad else ""
            print(f"  {rel}:{line}  {verdict}{flag}")
        return 0

    for rel, line, path in findings:
        if path.startswith(PARSE_DEFECT):
            # A parse defect is not an unknown key and must not be phrased as
            # one: the remedy is different (make the fence parse), so it gets
            # its own message rather than being wrapped in "unknown config key".
            print(f"{rel}:{line}: this autumn.toml fence {path[len(PARSE_DEFECT):]}")
            continue
        hint = suggest(path, schema)
        tail = f" (did you mean `{hint}`?)" if hint else ""
        print(f"{rel}:{line}: unknown config key `{path}`{tail}")

    checked = sum(1 for _, _, v, _ in fences if v == "checked")
    print(
        f"Checked {checked} autumn.toml fences against {leaf_count} schema leaves "
        f"({len(fences) - checked} fences skipped as not autumn.toml)."
    )
    return 1 if findings else 0


# ── Self-test ─────────────────────────────────────────────────────────────────


def self_test():
    schema, _ = load_schema()
    schema[""].update({"search", "media"})
    schema[""].add("dev")
    schema["dev"].add("watch_dirs")

    cases = [
        # (name, fence body, expected findings)
        ("known key", "[server]\nport = 3000\n", []),
        ("unknown key", "[session]\nttl_seconds = 3600\n", ["session.ttl_seconds"]),
        ("unknown root", "[sesion]\nmax_age_secs = 1\n", ["sesion"]),
        (
            "nested known",
            "[session.redis]\nurl = \"redis://x\"\nkey_prefix = \"p\"\n",
            [],
        ),
        ("nested unknown", "[session.redis]\nhost = \"x\"\n", ["session.redis.host"]),
        # Dynamic-key sections: arbitrary children are valid and must not be
        # reported, or the gate rejects correct pages.
        ("flatten oauth2 provider", '[auth.oauth2.github]\nclient_id = "x"\n', []),
        ("dynamic job queue", "[jobs.queues.critical]\nworkers = 4\n", []),
        (
            "dynamic circuit-breaker host",
            "[resilience.circuit_breaker.hosts.api]\nopen_duration_secs = 5\n",
            [],
        ),
        ("dynamic base_urls map", '[http.client.base_urls]\nstripe = "https://x"\n', []),
        # Arrays of tables are walked item by item against the SAME schema path,
        # as `validate_toml_table` does. Both of these are live in the corpus.
        (
            "array of tables, valid",
            '[[database.shards]]\nname = "s0"\nprimary_url = "postgres://x"\n',
            [],
        ),
        (
            "array of tables, typo",
            '[[database.shards]]\nname = "s0"\nprimary_urll = "postgres://x"\n',
            ["database.shards.primary_urll"],
        ),
        (
            "array of tables, second entry typo",
            '[[security.webhooks.endpoints]]\nsecret_env = "A"\n'
            '\n[[security.webhooks.endpoints]]\nsecret_enb = "B"\n',
            ["security.webhooks.endpoints.secret_enb"],
        ),
        (
            "array of tables under a profile overlay",
            '[[profile.prod.database.shards]]\nprimary_urll = "postgres://x"\n',
            ["database.shards.primary_urll"],
        ),
        # A scalar array is not a table array and must not be descended into.
        ("scalar array", '[dev]\nwatch_dirs = ["views", "locales"]\n', []),
        # Profile overlays resolve to the base path.
        ("profile overlay ok", "[profile.prod.server]\nport = 9000\n", []),
        ("profile overlay bad", "[profile.prod.server]\nprot = 9000\n", ["server.prot"]),
        # Plugin-declared roots are known and opaque.
        ("plugin root", "[search]\nenabled = false\n", []),
        ("plugin root child", "[search]\nanything_at_all = 1\n", []),
        ("plugin root under profile", "[profile.prod.search]\nenabled = false\n", []),
        # CLI-owned [dev] keys.
        ("cli dev key", '[dev]\nwatch_dirs = ["views"]\n', []),
        ("cli dev unknown", "[dev]\nwatch_dir = 1\n", ["dev.watch_dir"]),
    ]

    known_roots = schema[""]
    classify_cases = [
        ("cargo manifest", '[package]\nname = "x"\n', "", "cargo-manifest"),
        (
            "cargo dependency table",
            '[dependencies]\nautumn-web = "0.1"\n',
            "",
            "cargo-manifest",
        ),
        ("no section header", 'port = 3000\n', "", "no-section-header"),
        (
            "fly.toml by in-fence marker",
            '# fly.toml\n[deploy]\n  kill_timeout = 45\n',
            "",
            "other-file:fly.toml",
        ),
        (
            "fly.toml by lead-in prose",
            "[deploy]\n  release_command = \"autumn migrate\"\n",
            "Uncomment the `release_command` line in `fly.toml` before deploying:",
            "other-file:fly.toml",
        ),
        # A marker naming another file wins over a recognized root: `[deploy]` is
        # a real Autumn section AND a real Fly one.
        (
            "other-file marker beats recognized root",
            "# fly.toml\n[deploy]\n  release_command = \"autumn migrate\"\n",
            "",
            "other-file:fly.toml",
        ),
        (
            "autumn.toml lead-in reads the fence",
            "[server]\nport = 3000\n",
            "Set the port in `autumn.toml`:",
            None,
        ),
        (
            "autumn-prod.toml lead-in reads the fence",
            "[server]\nport = 3000\n",
            "In `autumn-prod.toml`:",
            None,
        ),
        ("recognized root reads the fence", "[server]\nport = 3000\n", "", None),
        # Positive identification: an unrecognized-root fence with no marker is
        # some other TOML file, and must not be read.
        (
            "credentials store is not read",
            '[active_record_encryption]\nprimary_key = "abc"\n',
            "",
            "no-autumn-section",
        ),
        (
            "plugin manifest is not read",
            '[capabilities]\nnet = false\n\n[limits]\nfuel = 1\n',
            "",
            "no-autumn-section",
        ),
        (
            "starter manifest is not read",
            '[starter]\nname = "saas"\n',
            "",
            "no-autumn-section",
        ),
        # `autumn-starter.toml` matches the profile-overlay NAME shape but is a
        # different schema; naming it must not admit the fence.
        (
            "autumn-starter.toml is another file",
            '[starter]\nname = "saas"\n',
            "Each starter carries an `autumn-starter.toml` manifest:",
            "other-file:autumn-starter.toml",
        ),
        (
            "autumn.generate.toml is another file",
            '[scaffold.Bookmark]\nfields = ["url:String"]\n',
            "Create `autumn.generate.toml` with one section per resource:",
            "other-file:autumn.generate.toml",
        ),
        # ...while a genuine profile overlay with the same name shape does.
        (
            "autumn-capsules.toml is a config overlay",
            "[failure_capture]\nenabled = true\n",
            "In `autumn-capsules.toml`:",
            None,
        ),
        (
            "autumn.production.toml.example is a config template",
            "[server]\nhost = \"0.0.0.0\"\n",
            "edit `autumn.production.toml.example` before building:",
            None,
        ),
        # A pure profile-overlay fence names no root section but is autumn.toml.
        ("profile-only overlay is read", "[profile.prod.server]\nport = 9000\n", "", None),
        # Cargo's `[profile.dev]` reaches the walk the same way and is settled
        # there on its leaf keys, not here.
        ("cargo profile fence is read", '[profile.dev]\ndebug = 1\n', "", None),
        (
            "generate file is not read",
            '[scaffold.Bookmark]\nfields = ["url:String"]\n',
            "",
            "no-autumn-section",
        ),
        # ...but an `# autumn.toml` marker overrides that, which is how the
        # baseline caught an impossible `[app] profile` fence.
        (
            "autumn.toml marker beats unrecognized root",
            '# autumn.toml\n[app]\nprofile = "production"\n',
            "",
            None,
        ),
    ]

    # `[profile.dev] debug = …` is Cargo's, not an Autumn overlay.
    profile_cases = [
        ("cargo profile", "dev", {"debug": "line-tables-only"}, True),
        ("cargo profile multi", "release", {"lto": True, "strip": "symbols"}, True),
        ("autumn overlay", "prod", {"server": {"port": 1}}, False),
        ("autumn overlay scalar-free", "prod", {}, False),
    ]

    passed = failed = 0

    for name, body, expected in cases:
        got = check_fence(body, schema)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{name}]: expected {expected}, got {got}")

    for name, body, lead_in, expected in classify_cases:
        got = classify(body, lead_in, known_roots)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{name}]: expected {expected!r}, got {got!r}")

    for name, pname, table, expected in profile_cases:
        got = is_cargo_profile(pname, table)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{name}]: expected {expected}, got {got}")

    # An identified fence that does not parse is a defect, not a skip: it is
    # presented as a file to copy and would fail at boot for whoever copies it.
    for label, fence in (
        ("unparseable: bare placeholder", "[server]\nport = <PORT>\n"),
        # The exact shape the baseline found in cloud-native.md.
        (
            "unparseable: duplicate key",
            '[database]\nread_your_writes = "request"\nread_your_writes = "session"\n',
        ),
    ):
        got = check_fence(fence, schema)
        if len(got) == 1 and got[0].startswith(PARSE_DEFECT):
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]: expected a parse defect, got {got}")

    # A quoted placeholder IS valid TOML — the documented remedy must work.
    if check_fence('[server]\nport = "<PORT>"\n', schema) == []:
        passed += 1
    else:
        failed += 1
        print("  FAIL [quoted placeholder parses clean]")

    # Plugin roots are known-and-opaque only as TABLES, matching the runtime's
    # `val.is_table()` guard; a scalar or array is a malformed section.
    plugins = frozenset({"search", "media"})
    for label, fence, expected in (
        ("plugin root as table", "[media]\nanything = 1\n", []),
        ("plugin root as scalar", 'media = "enabled"\n', ["media"]),
        ("plugin root as array", 'media = ["a", "b"]\n', ["media"]),
    ):
        got = check_fence(fence, schema, plugins)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]: expected {expected}, got {got}")

    # A headerless fence carrying an `# autumn.toml` marker is read, so a
    # top-level scalar key (`role`) is checked rather than skipped.
    for label, body, expected in (
        ("marked headerless, valid root scalar", '# autumn.toml\nrole = "worker"\n', None),
        ("headerless without a marker", 'role = "worker"\n', "no-section-header"),
        # A DOTTED key declares its table, so it identifies the fence.
        ("dotted root is read", "server.port = 9000\n", None),
        ("dotted root, indented", "  database.pool_size = 10\n", None),
        ("dotted non-root is not read", "widget.colour = 1\n", "no-section-header"),
        # ...but a bare key or an inline table whose NAME collides with a root is
        # not enough: a headerless Cargo dependency list carries `http` and could
        # carry `cache`/`mail`/`storage`, and admitting it would report every
        # other crate name as an unknown config root.
        ("bare colliding key is not read", 'http = "1.0"\n', "no-section-header"),
        (
            "inline table colliding with a root is not read",
            'http = { version = "1.0" }\ndiesel = { version = "2" }\n',
            "no-section-header",
        ),
    ):
        got = classify(body, "", known_roots)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]: expected {expected!r}, got {got!r}")
    if check_fence('# autumn.toml\nrol = "worker"\n', schema) == ["rol"]:
        passed += 1
    else:
        failed += 1
        print("  FAIL [marked headerless typo is caught]")

    # MARKERS BEAT HEURISTICS. Each of these was a hole where a guess about the
    # fence's identity overruled the page saying what it is.
    for label, body, lead_in, expected in (
        # A Cargo-shaped root under an explicit autumn marker is invalid Autumn
        # config, not a Cargo manifest.
        ("marker beats cargo-root heuristic", '# autumn.toml\n[package]\nname = "x"\n', "", None),
        # ...but an unmarked one is still a Cargo manifest.
        ("unmarked cargo root still skipped", '[package]\nname = "x"\n', "", "cargo-manifest"),
        # A lead-in naming BOTH files resolves to the other file — the safe way.
        (
            "other-file wins when both are named",
            '[dependencies]\nautumn-web = "0.1"\n',
            "add this to Cargo.toml next to your autumn.toml:",
            "other-file:Cargo.toml",
        ),
    ):
        got = classify(body, lead_in, known_roots)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]: expected {expected!r}, got {got!r}")

    if check_fence('# autumn.toml\n[package]\nname = "x"\n', schema, marked=True) == ["package"]:
        passed += 1
    else:
        failed += 1
        print("  FAIL [marked cargo-root fence reports its keys]")

    # The Cargo-profile exemption is a heuristic too: it may not overrule a marker.
    for label, body, marked, expected in (
        ("unmarked cargo profile stays exempt", "[profile.dev]\ndebug = 1\n", False, []),
        ("marked cargo profile is judged", "# autumn.toml\n[profile.dev]\ndebug = 1\n", True, ["debug"]),
    ):
        got = check_fence(body, schema, marked=marked)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]: expected {expected}, got {got}")

    # A profile name whose value is not a table is reported, as the runtime does.
    for label, body, expected in (
        ("scalar profile entry", '[profile]\nprod = "x"\n', ["profile.prod"]),
        ("array-of-scalars profile entry", '[profile]\nprod = ["x"]\n', ["profile.prod"]),
        ("table profile entry stays clean", "[profile.prod.server]\nport = 1\n", []),
    ):
        got = check_fence(body, schema)
        if got == expected:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]: expected {expected}, got {got}")

    # Blockquoted fences are extracted, and stripping the quote prefix must not
    # move a line number — that is what keeps a reported location clickable.
    quoted = (
        "> Some aside:\n>\n> ```toml\n> [server.tls.acme]\n"
        '> ca_root_path = "x"\n> ```\n'
    )
    stripped = BLOCKQUOTE_PREFIX.sub("", quoted)
    for label, ok in (
        ("blockquote strip preserves line count", stripped.count("\n") == quoted.count("\n")),
        ("blockquoted fence becomes extractable", bool(TOML_FENCE.search(stripped))),
        (
            "blockquoted fence body is clean TOML",
            check_fence(TOML_FENCE.search(stripped).group(2), schema) == [],
        ),
        (
            "typo inside a blockquoted fence is caught",
            check_fence(
                TOML_FENCE.search(
                    BLOCKQUOTE_PREFIX.sub(
                        "", quoted.replace("ca_root_path", "ca_root_pathh")
                    )
                ).group(2),
                schema,
            )
            == ["server.tls.acme.ca_root_pathh"],
        ),
        # Nested quotes and an unquoted fence must both still work.
        (
            "nested blockquote strips too",
            bool(
                TOML_FENCE.search(
                    BLOCKQUOTE_PREFIX.sub(
                        "", "> > ```toml\n> > [server]\n> > port = 1\n> > ```\n"
                    )
                )
            ),
        ),
        (
            "unquoted fence is unaffected",
            bool(
                TOML_FENCE.search(
                    BLOCKQUOTE_PREFIX.sub("", "```toml\n[server]\nport = 1\n```\n")
                )
            ),
        ),
    ):
        if ok:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]")

    # The truth sets must be non-empty against the real tree, or the gate would
    # pass by knowing nothing.
    live_schema, leaves, roots, dev = build_schema()
    for label, ok in (
        ("schema snapshot has leaves", leaves > 100),
        ("plugin roots found", bool(roots)),
        ("cli dev keys found", bool(dev)),
        ("server.port is known", "port" in live_schema.get("server", set())),
        ("session.max_age_secs is known", "max_age_secs" in live_schema.get("session", set())),
    ):
        if ok:
            passed += 1
        else:
            failed += 1
            print(f"  FAIL [{label}]")

    print(f"self-test: {passed}/{passed + failed} passed")
    return 0 if failed == 0 else 1


sys.exit(self_test() if MODE == "--self-test" else main())
PYEOF
}

case "${1-}" in
  --self-test)
    run_py --self-test "$root"
    ;;
  --list)
    run_py --list "$root"
    ;;
  "")
    echo "Checking autumn.toml config keys in the docs corpus..."
    if run_py --check "$root"; then
      echo "Config-key drift gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the docs tell a reader to write config keys that do not exist (above).

An unknown key in `autumn.toml` is dropped silently — `server.strict_config` is
off by default — so a reader who copies one gets no error and no effect, and
believes a setting changed that did not.

Fix each one where it lives:
  - renamed key    -> use the current name (the `did you mean` hint is the
                      closest key in the same section)
  - key that never existed -> drop the line, or document the key that does the
                      job (`autumn/tests/fixtures/schema_keys.snapshot` is the
                      full list of 484)
  - not an autumn.toml fence -> say which file it IS, with a `# fly.toml`
                      marker comment in the fence or a lead-in sentence naming
                      the file. A reader arriving mid-page needs that as much as
                      this gate does.
  - genuinely new key -> land the config change first; the snapshot regenerates
                      with `UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p autumn-web
                      schema_keys_snapshot_guard`

Inspect what the gate read:  scripts/check-docs-config.sh --list
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--list|--self-test]" >&2
    exit 2
    ;;
esac
