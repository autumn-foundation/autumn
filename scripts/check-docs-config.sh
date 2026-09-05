#!/usr/bin/env bash
# Config-key drift gate: every `autumn.toml` key the reader-facing docs tell
# someone to WRITE must exist in the config schema.
#
# WHY THIS EXISTS: the corpus already gates the first two things a reader copies
# off a page. `scripts/check-docs-links.sh` gates its *links*, so nobody is sent
# to a page that does not exist; `scripts/check-docs-cli.sh` gates its
# *commands*, so nobody is handed an `autumn …` line clap will reject. Nothing
# checked the third: the **config key**. The reader-facing corpus carries 162
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
#      root as an opaque table.
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
# WHICH FENCES ARE READ. Of the corpus's 246 ```toml fences, 162 are read. A
# fence is read only on POSITIVE identification — it names `autumn.toml` (or a
# profile overlay / the `.example` template), or it carries a section the config
# surface recognizes. This corpus is a Rust framework's, so most of its TOML is
# some other file, and several families of it collide with real Autumn section
# names:
#
#   - **Cargo manifests** (41 fences). Skipped on their root sections
#     (`[package]`, `[dependencies]`, `[features]`, …).
#   - **Cargo profiles**. `[profile.dev] debug = "line-tables-only"` in
#     `platform-support.md` is a Cargo profile, and `[profile.<name>]` is ALSO
#     Autumn's overlay syntax — the same four characters mean two different files.
#     Disambiguated on the leaf keys, not the header: a `[profile.X]` table whose
#     keys are all Cargo profile knobs (`debug`, `opt-level`, `lto`, `panic`,
#     `strip`, …) is Cargo's, and Autumn's overlay carries config SECTIONS, never
#     those names. Header-based skipping would have cost the real overlay fences,
#     which are the production-tuning ones most worth gating.
#   - **`fly.toml`** (4 fences). Fly's `[deploy]` table takes `release_command`
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
#   - **VALUES.** Only key existence. A key's type, range, and whether the value
#     is sensible are the app's to reject; this gate answers the one question the
#     reader cannot answer for themselves, because nothing tells them: does this
#     key exist at all?
#   - **Fences that do not parse as TOML.** Elided snippets (`…`, `<host>`) are
#     illustrations, not files. 1 fence in the corpus is in this state.
#   - **Fences with no `[section]` header** (16). A bare `key = value` fragment
#     has no root to resolve against; guessing one would invent defects.
#   - **`examples/<app>/content/`.** Seed content for an example app, rendered by
#     that app's own routes — the same exclusion, for the same reason,
#     `check-docs-links.sh` makes.
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
SECTION_HEADER = re.compile(r'^\s*\[\[?([A-Za-z0-9_.\-"]+)', re.M)

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
    if not headers:
        return "no-section-header"

    roots = {h.split(".")[0].strip('"') for h in headers}
    if roots & CARGO_ROOTS:
        return "cargo-manifest"

    # The fence's own first comment lines, then the prose that introduces it.
    leading_comments = "\n".join(
        line for line in body.split("\n")[:3] if line.strip().startswith("#")
    )
    ours, theirs = named_toml_files(leading_comments)
    if not (ours or theirs):
        ours, theirs = named_toml_files(lead_in)
    # A marker naming another file settles it, even alongside a recognized root:
    # `fly.toml`'s `[deploy]` is spelled exactly like Autumn's.
    if theirs:
        return f"other-file:{theirs[0]}"
    if ours:
        return None
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


def walk(table, path, schema, out):
    for key, value in table.items():
        full = f"{path}.{key}" if path else key
        if path not in schema:
            # No schema entry: a dynamic-key or plugin-owned section. Its children
            # are arbitrary and valid, exactly as the framework treats them.
            continue
        if key not in schema[path]:
            out.append(full)
            continue
        if isinstance(value, dict):
            walk(value, full, schema, out)


def check_fence(body, schema):
    """Out-of-schema key paths in one fence, or None if it is not read."""
    try:
        table = tomllib.loads(body)
    except (tomllib.TOMLDecodeError, ValueError):
        return None
    findings = []
    for key, value in table.items():
        if key == "profile" and isinstance(value, dict):
            # `[profile.prod.server] port = …` resolves as `server.port`.
            for name, overlay in value.items():
                if not isinstance(overlay, dict):
                    continue
                if is_cargo_profile(name, overlay):
                    continue
                walk(overlay, "", schema, findings)
        else:
            walk({key: value}, "", schema, findings)
    return findings


# ── Driver ────────────────────────────────────────────────────────────────────


def build_schema():
    schema, leaf_count = load_schema()
    # Plugin roots and CLI-owned `[dev]` keys join the root entry; plugin roots
    # get no entry of their own, which makes them opaque — children unchecked.
    extra_roots = plugin_roots()
    schema[""].update(extra_roots)
    dev_keys = cli_dev_keys()
    if dev_keys:
        schema[""].add("dev")
        schema["dev"].update(dev_keys)
    return schema, leaf_count, extra_roots, dev_keys


def scan(schema):
    findings = []
    fences = []
    known_roots = schema[""]
    for rel in corpus():
        with open(os.path.join(ROOT, rel), encoding="utf-8", errors="ignore") as fh:
            text = fh.read()
        for match in TOML_FENCE.finditer(text):
            body = match.group(2)
            line = text[: match.start()].count("\n") + 1
            lead_in = "\n".join(text[: match.start()].split("\n")[-(LEAD_IN_LINES + 1) : -1])
            skip = classify(body, lead_in, known_roots)
            if skip:
                fences.append((rel, line, skip, []))
                continue
            bad = check_fence(body, schema)
            if bad is None:
                fences.append((rel, line, "unparseable", []))
                continue
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
    findings, fences = scan(schema)

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

    # An unparseable fence is skipped, not reported.
    if check_fence("[server]\nport = <PORT>\n", schema) is None:
        passed += 1
    else:
        failed += 1
        print("  FAIL [unparseable fence]: expected None")

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
