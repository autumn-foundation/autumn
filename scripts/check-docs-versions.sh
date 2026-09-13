#!/usr/bin/env bash
# Release-line drift gate: every `autumn-* = "<version>"` pin the reader-facing
# docs hand someone must name the release line that is actually published.
#
# WHY THIS EXISTS: the corpus already gates the six things a reader copies off a
# page and the one thing they cannot copy at all.
# `scripts/check-docs-links.sh` gates its *links* (a 404 on GitHub),
# `scripts/check-docs-cli.sh` its *commands* (`unrecognized subcommand`),
# `scripts/check-docs-config.sh` the `AUTUMN_*` variables they SET (a silent
# no-op), `scripts/check-docs-toml.sh` the `autumn.toml` keys they WRITE
# (dropped silently), `scripts/check-docs-symbols.sh` the `autumn_web::…` paths
# they IMPORT (E0432 against their own file), `scripts/check-docs-routes.sh` the
# `/actuator/…` URLs they CURL, and `scripts/check-docs-orphans.sh` asserts the
# page can be reached at all. Nothing gated the line the reader copies FIRST and
# that every one of those other checks is implicitly relative to: the dependency
# pin that decides WHICH autumn-web the rest of the page is describing.
#
# It was gated in one place, over eight pages.
# `autumn-cli/tests/integration/repo_hygiene.rs`'s
# `first_run_docs_match_current_release_line` holds a hand-listed `FIRST_RUN_DOCS`
# array — `README.md`, `getting-started.md`, `docs-smoke.md`, `deployment.md`,
# `websockets.md`, `tutorial/01-project-setup.md`, `tutorial/12-whats-next.md`
# and `macro-transparency.md` — to the published pin. The reader-facing corpus
# is 212 pages and carries 74 pins. The other 204 pages were ungated, and the
# CHANGELOG records the class being swept by hand twice already: "aligned the
# `autumn-cli` install pin to the workspace `0.6.0` across …" (0.6.0, five
# pages) and the getting-started rewrite that found a guide "announcing the
# '0.4 release line' while pinning 0.6 commands".
#
# The existing test is also EXISTENTIAL rather than per-occurrence: it asks
# whether a page `contains` the right pin anywhere, so a page carrying a correct
# pin and a stale one passes. This gate checks every pin.
#
# THE BASELINE RUN FOUND ONE, and it is the expensive kind:
#
#   skills/autumn-patterns/SKILL.md:40
#     autumn-web = { version = "0.5", features = ["test-support"] }
#
# That is a `[dev-dependencies]` block in the "Testing with TestApp and
# TestClient" section — the line a reader adds the first time they try to write
# a test — two release lines behind the `autumn-web = "0.7"` the same corpus
# pins in 47 other places. It entered the tree already stale (added whole in
# 10ce912; it has never been bumped), which is the reason a release-time sweep
# would not have caught it and a gate does.
#
# WHAT THE READER GETS is why this class is worth its own gate rather than a
# style note. `autumn-web` 0.5 and 0.7 both pull `libsqlite3-sys`, which carries
# a `links = "sqlite3"` key, and Cargo permits exactly one package per `links`
# value in a graph. So the pin does not fail as a version complaint the reader
# could trace back to this page. It fails as:
#
#     error: failed to select a version for `libsqlite3-sys`.
#         ... required by package `autumn-web v0.7.0`
#     package `libsqlite3-sys` links to the native library `sqlite3`, but it
#     conflicts with a previous package which links to `sqlite3` as well:
#     package `libsqlite3-sys v0.36.0`
#         ... which satisfies dependency `autumn-web = "^0.5"`
#
# Neither `autumn-web 0.5` nor the page is named as the cause. The reader is
# handed a native-library linking conflict, at the first test they ever write,
# and the obvious reading of it — something is wrong with my sqlite setup — is
# wrong. Verified in both directions against crates.io: the `0.5` pin exits 101
# at resolve time, the `0.7` pin resolves clean.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Every dependency-position pin on a PUBLISHED workspace `autumn*` crate —
#      `autumn-web = "0.7"`, `autumn-web = { version = "0.7", … }`, and the
#      inline-code spelling prose uses (`autumn-storage-s3 = "0.7"`) — names the
#      published release line, as `x.y` or the exact `x.y.z`.
#
#      HOW A PIN IS READ: a ```toml fence is a TOML document, so it is PARSED
#      (`tomllib`) rather than matched; everything else — prose, a `text` fence
#      reproducing a panic, an unlabelled block — has no document to parse and
#      goes through the pattern. The two paths partition the page, so no pin is
#      read twice, and a fence that does not parse is a fragment rather than a
#      defect and falls back to the pattern rather than being skipped.
#
#      Parsing is not the obvious first choice for a docs gate, and it was not
#      the first choice here. It is the answer to five review rounds in a row,
#      three of which were the same finding wearing a different hat: the
#      pattern did not know the `[dependencies.x]` subtable, then did not know
#      TOML literal (single-quoted) strings, then did not know a
#      `package = "…"` rename on a key not starting with `autumn`. Each was a
#      silent hole, each had a fourth waiting behind it, and
#      `check-docs-scope.sh` already records this repo shipping the identical
#      mistake once — a manifest reader that took only double-quoted
#      `readme = "…"`, so a single-quoted path resolved to nothing in all four
#      gates that shared it. A TOML parser knows every spelling at once; a
#      pattern learns them one review round at a time.
#
#      Two spellings still matter on the PATTERN path, since prose carries pins
#      too, and the first version of this gate missed both:
#
#        - A Cargo COMPARISON OPERATOR. `autumn-web = "=0.6.0"` is the opening
#          instruction of every migration guide ("Pin your current dependency …
#          and commit"), four of them here, and a pattern anchored at a digit
#          reaches none. `=`, `^` and `~` all name a line and are checked; `>`,
#          `<`, `*` and comma-joined ranges name BOUNDS, which "is this stale?"
#          cannot be asked of, and are skipped deliberately.
#        - A MULTILINE inline table. `skills/autumn-web/SKILL.md` opens
#          `autumn-web = { version = "0.7", features = [` and closes it nine
#          lines later; read one line at a time it does not exist. Pins are
#          extracted from the whole comment-blanked page, with line numbers
#          recovered from the match offset.
#        - Cargo's SUBTABLE form, `[dependencies.autumn-web]` with a plain
#          `version = "…"` some lines below. Neither half looks like a pin on
#          its own. The parser handles this inside a `toml` fence; the pattern
#          keeps its own reader for the fragment fallback and for a subtable
#          written outside one.
#
#      One rule spans both paths. Cargo does NOT treat `-` and `_` as
#      interchangeable in a dependency table key, so `[dependencies.autumn_web]`
#      names a crate called `autumn_web`, which does not exist, unless it
#      carries an explicit `package = "autumn-web"`. Following Cargo rather
#      than guessing is what keeps a correct page from being reported.
#
#   2. The published line is read from README.md's quickstart
#      (`cargo install autumn-cli --version <x.y.z>`) — the same single source
#      of truth `scripts/check-quickstart.sh` and
#      `first_run_docs_match_current_release_line` already use, so this gate
#      cannot disagree with them about what "current" means.
#   3. That pin is itself validated against `[workspace.package] version`: it
#      must parse as `x.y.z` and must not be NEWER than the workspace, which is
#      the one direction the corpus cannot be right about.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - Third-party pins (`diesel = "2"`, `tokio = "1"`). Those track upstream
#     releases this repo does not cut, so "current" is not a question this
#     workspace can answer. `check-msrv.sh` and `cargo-deny` own that surface.
#   - A pin inside a `docs/migrations/<version>.md` page that names that
#     page's line or an OLDER one. A migration guide to 0.4.0 exists to show
#     `autumn-web = "0.3"` above `autumn-web = "0.4"`; holding those to the
#     current line would report the five correct pins in `0.4.0.md` and make
#     the gate useless on the four pages where version text matters most. A
#     pin NEWER than the page's own version is still a defect — that is a
#     guide describing a release it predates.
#   - A `[patch.crates-io]` (or any `[patch.<registry>]`) entry. This one is a
#     deliberate NO, not an oversight, and it is written down because it looks
#     like an oversight: `_dependency_tables` walks `dependencies`,
#     `dev-dependencies`, `build-dependencies`, `target.*` and `workspace.*`,
#     and stops there.
#
#     A patch entry is not a release-line pin. It exists to redirect a
#     dependency AWAY from the published registry — this workspace's own root
#     manifest says `autumn-web = { path = "autumn" }`, with no version at all
#     — so a `version` beside it constrains the replacement source rather than
#     declaring which release a reader should install. A page demonstrating a
#     patch onto an older fork is CORRECT, and holding it to the published line
#     would report it. That is the false-positive shape `check-docs-cli.sh`
#     warns about in its own header: a gate people learn to ignore has stopped
#     working, and one wrong report on a right page buys that faster than ten
#     missed ones.
#
#     Known asymmetry, stated rather than papered over: the parser skips a
#     patch table because it never descends into `patch`, while the PATTERN has
#     no notion of TOML sections, so the same declaration written in prose IS
#     reported. Making the pattern section-aware is the complexity that
#     motivated parsing in the first place, and no tracked markdown in this
#     repo contains a `[patch.…]` table at all, so the inconsistency is
#     documented and left rather than built around.
#   - Whether an EXACT pin names the newest patch. `autumn-web = "=0.7.0"` is
#     accepted while `0.7.1` is published, and that is deliberate: this gate
#     asks which release LINE a page names, and both are the 0.7 line. A
#     `0.7.2` pin against a published `0.7.1` IS refused, because a page may
#     not promise a patch nobody can install.
#
#     The case for tightening it was that an old exact pin could reproduce the
#     duplicate-framework failure this gate exists to catch. Measured, it does
#     not: `^1.0` beside `=1.0.200` resolves to ONE version (1.0.200) — cargo
#     unifies them, so there is no second copy and no `links` conflict. The
#     older patch also remains installable, since crates.io does not remove
#     published versions; a test against a local PATH package at the newer
#     patch fails only because no other version exists there, which is not the
#     situation a reader is in.
#     What is left is mild staleness at the patch level, and gating it would
#     refuse `=0.7.0` on the day `0.7.1` lands — on pages that still work.
#   - An inline table on the PATTERN path whose strings contain a BRACE. The
#     `\{[^{}]*\}` branch stops at any brace, so such a table is not matched at
#     all, and on the fallback its pin is missed.
#
#     Left as a known limit rather than fixed, on two grounds. The shape that
#     motivated it is not valid Cargo: `features = ["x{y}"]` fails to resolve
#     (exit 101 — a feature name takes `[A-Za-z0-9_+.-]` only), so a manifest
#     written that way does not build, and no inline table in this corpus
#     carries a brace inside a string. And the fix is a string-aware
#     brace-matcher — MORE hand-written TOML in the fallback, which is the one
#     component in this file that has produced a defect on nearly every commit
#     touching it, in both directions. Adding parser surface there is the
#     opposite of where that evidence points; the fallback's shape is a
#     question for a maintainer (see the PR), not something to answer by
#     growing it further.
#
# MIGRATION PAGES, precisely: the rule is `pin <= page version`, not
# `pin == page version`, because the "before" block is the whole point of the
# page. `0.4.0.md` legitimately carries 0.3 and 0.4; it may not carry 0.5.
#
# WAIVERS: a reader-facing page sometimes has to SHOW a stale pin. The panic
# text in `docs/plugins.md` reproduces what Autumn prints when a plugin's
# declared range excludes the framework in the build, and the `autumn-web =
# "0.6"` inside it is the remediation line that message prints — the page would
# be wrong with any other number in it. Waive it with a marker directly below
# the passage:
#
#     <!-- version-pin-allow: autumn-web = "0.6" — inside the reproduced
#          plugin-contract panic text; the message's own remediation line, not
#          a pin the reader adds -->
#
# The marker sits in the page beside the claim, so it is deleted by the same
# commit that deletes the passage. A central allowlist would outlive it and
# silently re-admit the defect. Every waiver must carry a reason after the pin,
# separated by `—` or `:`.
#
# A waiver covers only its own blank-line-separated block and the one directly
# above it — the passage it was written for — anchored at the line the marker
# OPENS on, so a reason long enough to wrap is scoped by where it was written.
# The same stale pin further down the page is still reported.
#
# HTML comments are blanked before pins are extracted, on the same
# visible-vs-invisible line `check-docs-orphans.sh` and `check-docs-routes.sh`
# draw: a comment renders as nothing, so it is not a pin a reader can see or
# paste. That rule is also what lets a waiver name the pin it exempts without
# the gate re-reporting its own marker one line down.
#
# THE CORPUS IS THE SIBLINGS'. This file's `INCLUDE_DIRS`/`INCLUDE_FILES`/
# `package_readmes` block is copied verbatim from `check-docs-cli.sh`, and this
# gate is registered in `scripts/check-docs-scope.sh`'s `SIBLINGS` in the same
# commit. #2709 exists because four gates each spelled their own corpus and three
# of them drifted; a fifth gate spelling a fifth copy, unwatched, is how that
# recurs. The scope gate asks this one what it reads, so a divergence fails
# loudly instead of leaving pages with no owner.
#
# Run locally with:
#
#     ./scripts/check-docs-versions.sh
#     ./scripts/check-docs-versions.sh --list        # every pin the gate read
#     ./scripts/check-docs-versions.sh --corpus      # the pages it reads
#     ./scripts/check-docs-versions.sh --self-test   # synthetic-page tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

read -r -d '' PYSRC <<'PYEOF' || true
import os
import pathlib
import re
import subprocess
import sys
import tomllib

MODE = sys.argv[1]
ROOT = sys.argv[2]


INCLUDE_DIRS = ('docs/guide/', 'docs/migrations/', 'skills/', 'agents/',
                '.claude/skills/')
# `docs/plugins.md` is a live product guide sitting at the `docs/` root rather
# than under `docs/guide/`, linked from seven corpus pages as *the* plugin
# guide. It joined the sibling `check-docs-config.sh` list in the same commit:
# the two definitions of "reader-facing" are kept identical on purpose, since
# a page covered by one gate and not the other is how a page ends up with no
# owner. Corpus 175 -> 176 here, and this gate stays green over it.
INCLUDE_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md',
                 'docs/plugins.md')
# A `README.md` under `examples/` is the page a reader LANDS on: the root
# `README.md` table links thirteen examples by directory and `EXAMPLES.md`
# eleven more, and a directory link renders that directory's `README.md`. They
# carry copyable `autumn …` commands and `AUTUMN_*` exports alike, so they join
# both gates in the same commit, for the reason `docs/plugins.md` did. Corpus
# 176 -> 192 here, and this gate stays green over it.
INCLUDE_README_DIRS = ('examples/',)


def in_scope(path):
    return (path.startswith(INCLUDE_DIRS) or path in INCLUDE_FILES
            or (path.startswith(INCLUDE_README_DIRS)
                and pathlib.PurePath(path).name == 'README.md'))


# `readme = "…"` in a crate manifest names that crate's crates.io landing page.
# It is reader-facing by PUBLICATION rather than by where it sits in the tree,
# which is why a directory-shaped rule cannot reach it — `check-docs-routes.sh`
# reads the manifests for exactly this reason, and its argument carries here
# unchanged: these pages carry `autumn_web::…` paths and `AUTUMN_*` variables
# the same way they carry `/actuator/…` URLs.
# TOML has two string forms and a manifest may use either, so both are read. The
# double-quote-only spelling missed `readme = 'README.md'` — valid TOML that
# every gate sharing this parser would have skipped in step, which an agreement
# check between them cannot see.
#
# Cargo's IMPLICIT discovery (no `readme` key, a `README.md` beside the
# manifest) is deliberately not modelled: `scripts/check-crate-metadata.sh`
# lists `readme` among REQUIRED_FIELDS for every publishable crate, so a
# published landing page always has an explicit key to find. The crates that
# rely on discovery here are the `examples/*`, all `publish = false` and so not
# published at all, and their READMEs are already corpus by directory.
README_CANDIDATES = ('README.md', 'README.txt', 'README')


def tracked_files(root):
    """Every tracked path, for the published READMEs the markdown glob misses."""
    out = subprocess.run(
        ['git', 'ls-files', '-z'],
        cwd=root, capture_output=True, text=True, check=True,
    ).stdout
    return {f for f in out.split('\0') if f}


def _inherited(value):
    """Whether a manifest value defers to `[workspace.package]`."""
    return isinstance(value, dict) and value.get('workspace') is True


def _published(pkg, workspace):
    """Cargo's `publish`: absent means yes, `false` and `[]` mean no."""
    value = pkg.get('publish')
    if _inherited(value):
        value = workspace.get('publish')
    if value is None:
        return True
    if value is False:
        return False
    if isinstance(value, list):
        return bool(value)
    return True


def _workspace_of(rel, tracked, parsed):
    """The manifest whose `[workspace.package]` this one inherits from.

    Cargo walks UP from the package directory to the nearest ancestor manifest
    carrying a `[workspace]` table, and `package.workspace = "…"` names one
    explicitly. A manifest with its own `[workspace]` table is its own root,
    which is how five standalone workspaces sit inside this repository without
    belonging to the root one: `fuzz/`, `examples/island-flock/`,
    `examples/reddit-clone/src-tauri/` and the two benchmark harnesses.

    Always reading the repository-root manifest instead would resolve an
    inherited value from a workspace the package is not in — the right answer
    only by coincidence, and only for packages in the root workspace.
    """
    data = parsed(rel)
    named = (data.get('package') or {}).get('workspace')
    if isinstance(named, str):
        here = str(pathlib.PurePosixPath(rel).parent)
        for suffix in (named, os.path.join(named, 'Cargo.toml')):
            cand = os.path.normpath(os.path.join(here, suffix))
            cand = cand.replace(os.sep, '/')
            if cand in tracked and 'workspace' in parsed(cand):
                return cand
    if 'workspace' in data:
        return rel
    parts = rel.split('/')[:-1]
    while parts:
        parts.pop()
        cand = '/'.join(parts + ['Cargo.toml'])
        if cand in tracked and 'workspace' in parsed(cand):
            return cand
    return None


def package_readmes(root):
    """Every file a `Cargo.toml` publishes as its crate's README.

    PARSED AS TOML, not matched with a regex, and that is the point. Review
    found four ways a hand-rolled matcher misread a manifest: it took only
    double-quoted values, then only explicit keys, then ignored `publish`, then
    missed the inline-table spelling of the inheritance it did match. Each fix
    was correct and each left the next corner of the same grammar uncovered,
    because the thing being approximated is a TOML parser. `tomllib` is
    standard library and already used by `check-docs-toml.sh` and by
    `check-example-bin-names.sh`, the latter on `Cargo.toml` exactly like this.

    `cargo metadata` would be more authoritative still, and is deliberately not
    used: every docs gate shares a CI job that carries no Rust toolchain and no
    cache, on purpose, so that it reports in seconds and cannot be blocked by a
    compile failure elsewhere. Reading the manifests keeps that property.

    What Cargo does, and so does this: `readme = false` means none; a string is
    a path relative to the manifest; `workspace = true` takes the
    `[workspace.package]` value of the package's OWN workspace, relative to
    that workspace's root; and an ABSENT key discovers `README.md`,
    `README.txt` or `README` beside the manifest, in that order. A package that
    does not publish is skipped — it has no landing page to keep true, and
    enrolling its working notes made the drift gates fail on the illustrative
    commands such a page may contain.
    """
    tracked = tracked_files(root)
    root_path = pathlib.Path(root)
    cache = {}

    def parsed(rel):
        if rel not in cache:
            cache[rel] = tomllib.loads(
                (root_path / rel).read_text(encoding='utf-8'))
        return cache[rel]

    out = set()
    for rel in sorted(f for f in tracked
                      if f == 'Cargo.toml' or f.endswith('/Cargo.toml')):
        manifest = pathlib.PurePosixPath(rel)
        parent = str(manifest.parent)
        parent = '' if parent == '.' else parent + '/'
        pkg = parsed(rel).get('package')
        if not isinstance(pkg, dict):
            continue

        ws_manifest = _workspace_of(rel, tracked, parsed)
        workspace, ws_dir = {}, ''
        if ws_manifest:
            workspace = parsed(ws_manifest).get('workspace', {}).get(
                'package', {})
            ws_dir = str(pathlib.PurePosixPath(ws_manifest).parent)
            ws_dir = '' if ws_dir == '.' else ws_dir

        if not _published(pkg, workspace):
            continue

        named = pkg.get('readme')
        if _inherited(named):
            # An inherited path is relative to ITS workspace's root.
            named = workspace.get('readme')
            if isinstance(named, str):
                resolved = os.path.normpath(os.path.join(ws_dir, named))
                out.add(resolved.replace(os.sep, '/'))
            continue
        if named is False:
            continue
        if isinstance(named, str):
            # `readme = "../README.md"` points at the workspace root's page.
            resolved = os.path.normpath(str(manifest.parent / named))
            out.add(resolved.replace(os.sep, '/'))
            continue
        for candidate in README_CANDIDATES:
            if parent + candidate in tracked:
                out.add(parent + candidate)
                break
    return out


def corpus(root):
    # NUL-delimited so a path containing whitespace is not split into
    # fragments, and so git does not quote unusual paths.
    out = subprocess.run(['git', 'ls-files', '-z', '*.md', '*.md.tmpl'], cwd=root,
                         capture_output=True, text=True).stdout
    published = package_readmes(root)
    files = [f for f in out.split('\0')
             if f and (in_scope(f) or f.endswith('.md.tmpl')
                       or f in published)]
    # A published landing page is corpus whatever it is NAMED. Using
    # `published` only to filter the markdown glob meant a crate that names a
    # `README.rst` or `README.txt` — valid, and unrestricted by
    # `check-crate-metadata.sh` — resolved to a path the glob never produced, so
    # the clause above could not add it and the page had no owner in any gate.
    # All four filtered identically, so they agreed and the scope gate stayed
    # green over it. Unioned in instead, and only when tracked.
    seen = set(files)
    tracked = tracked_files(root)
    return files + sorted(p for p in published
                          if p in tracked and p not in seen)

# ---------------------------------------------------------------------------
# The published release line
# ---------------------------------------------------------------------------

# README.md's quickstart pins the latest PUBLISHED autumn-cli, which can lag the
# (unreleased) workspace version between releases — PR #1622. It is the source
# of truth `scripts/check-quickstart.sh` installs from and the one
# `first_run_docs_match_current_release_line` holds the first-run docs to, so
# reading it here keeps all three from disagreeing about what "current" means.
INSTALL_PIN = 'cargo install autumn-cli --version '


def published_version(root):
    readme = pathlib.Path(root, 'README.md').read_text(encoding='utf-8')
    for line in readme.splitlines():
        stripped = line.strip()
        if stripped.startswith(INSTALL_PIN):
            rest = stripped[len(INSTALL_PIN):].split()
            if rest:
                return rest[0]
    raise SystemExit(
        'README.md must pin the published CLI install command '
        '`cargo install autumn-cli --version <x.y.z>`')


def workspace_version(root):
    """`[workspace.package] version`, parsed rather than matched.

    The last manifest reader in this file to still use a pattern, and it had
    the same double-quote-only assumption the others were converted out of: a
    valid `version = '0.7.0'` read as MISSING and failed the docs job over a
    manifest Cargo accepts.
    """
    try:
        manifest = tomllib.loads(
            pathlib.Path(root, 'Cargo.toml').read_text(encoding='utf-8'))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as err:
        raise SystemExit(f'Cargo.toml could not be read as TOML: {err}')
    workspace = manifest.get('workspace')
    package = workspace.get('package') if isinstance(workspace, dict) else None
    version = package.get('version') if isinstance(package, dict) else None
    if isinstance(version, str):
        return version
    raise SystemExit('Cargo.toml must set [workspace.package] version')


def core(version):
    """A semver string without its prerelease or build metadata.

    `0.6.0-beta.1` -> `0.6.0`, `0.7.0+build.5` -> `0.7.0`.

    A prerelease suffix does not change which RELEASE LINE a requirement names,
    and the line is the only question this gate asks. Refusing these was not a
    harmless omission: `QUICKSTART_CLI_VERSION` exists to gate "a release
    candidate that was just published to crates.io" (`check-quickstart.sh`,
    wired into `quickstart-gate.yml`), so during an RC cycle README pins
    `0.8.0-rc.1` — and this gate answered with "README.md pins a malformed
    autumn-cli version", checked ZERO pins, and failed CI at the exact moment a
    release was being cut. A gate that breaks the release process it exists to
    protect is worse than one that does not run.
    """
    for separator in ('-', '+'):
        head, found, _ = version.partition(separator)
        if found:
            version = head
    return version


def triple(text):
    """`x.y.z` as a tuple, or None. Anything else is not a release version.

    Prerelease and build metadata are stripped first, so `0.8.0-rc.1` is the
    0.8 line exactly as `0.8.0` is.
    """
    parts = core(text).split('.')
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        return None
    return tuple(int(p) for p in parts)


def series(version):
    """`0.7.0` -> `0.7`. The release LINE, which is what a pin usually names.

    Metadata is stripped first, so an RC reports its own line (`0.8.0-rc.1` ->
    `0.8`) rather than a suffix fragment.
    """
    major, minor = core(version).split('.')[:2]
    return f'{major}.{minor}'


# ---------------------------------------------------------------------------
# The crates whose pins this workspace can answer for
# ---------------------------------------------------------------------------

def published_crates(root):
    """Every workspace member that is actually published to crates.io.

    A pin is only checkable when this repo cuts the release it names. The
    `examples/*` members are `publish = false`, so a hypothetical
    `todo-app = "0.1"` in a page is not this gate's business; and a third-party
    pin never is. Membership is read from the manifests rather than hard-coded
    so a new plugin crate is covered the day it is published.

    The manifests are PARSED, not matched, for the same reason the `toml`
    fences are: a pattern reading only `name = "…"` misses the equally valid
    `name = 'autumn-foo'`, and the crate would then be absent from this set, so
    every stale pin on it is dropped at `crate not in crates` — a silent pass
    reached through the truth set rather than through the corpus, which is the
    quietest place for one to hide. `package_readmes()` in the shared corpus
    block above already parses for exactly this reason.

    `publish` inheritance is resolved against the package's OWN workspace, via
    the `_workspace_of` helper already in the block above. Reading
    `[workspace.package]` off the repository root instead is right only by
    coincidence, and only for packages in the root workspace: five standalone
    workspaces sit inside this repository (`fuzz/`, `examples/island-flock/`,
    `examples/reddit-clone/src-tauri/` and the two benchmark harnesses), and
    two of them hold `autumn-*` crates — `autumn-fuzz` and
    `autumn-island-flock`. `_workspace_of`'s own docstring warns against the
    root-only read; the first version of this function did it anyway.
    """
    tracked = tracked_files(root)
    root_path = pathlib.Path(root)
    cache = {}

    def parsed(rel):
        if rel not in cache:
            cache[rel] = tomllib.loads(
                (root_path / rel).read_text(encoding='utf-8'))
        return cache[rel]

    out = set()
    for rel in sorted(f for f in tracked
                      if f == 'Cargo.toml' or f.endswith('/Cargo.toml')):
        try:
            manifest = parsed(rel)
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError):
            # A manifest that will not parse is Cargo's problem, not this
            # gate's; `check-crate-metadata.sh` owns manifest well-formedness.
            continue
        package = manifest.get('package')
        if not isinstance(package, dict):
            continue
        name = package.get('name')
        if not isinstance(name, str) or not name.startswith('autumn'):
            continue
        try:
            ws_manifest = _workspace_of(rel, tracked, parsed)
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError):
            ws_manifest = None
        inherited = None
        if ws_manifest:
            ws_package = parsed(ws_manifest).get('workspace', {}).get(
                'package', {})
            if isinstance(ws_package, dict):
                inherited = ws_package.get('publish')
        # `publish = false` keeps a member off crates.io, and a LIST form
        # publishes only to the registries it names. Absent means published,
        # which is Cargo's own default. `_publish_state` holds that rule —
        # shared with `--self-test` rather than restated there, so the tested
        # logic is the logic that runs.
        if _publish_state(package.get('publish'), inherited) == 'excluded':
            continue
        out.add(name)
    return out


def _publish_state(publish, inherited):
    """The membership verdict `published_crates` reaches for one `publish`.

    Factored out so `--self-test` can exercise the inheritance rule without a
    synthetic workspace on disk; `published_crates` applies these same three
    steps in this same order.
    """
    if isinstance(publish, dict) and publish.get('workspace') is True:
        publish = inherited
    if publish is False:
        return 'excluded'
    if isinstance(publish, list) and 'crates-io' not in publish:
        return 'excluded'
    return 'included'


# ---------------------------------------------------------------------------
# Reading pins out of a page
# ---------------------------------------------------------------------------

# `autumn-web = "0.7"` and `autumn-web = { version = "0.7", features = [...] }`,
# in a fence or in the inline-code spelling prose uses. The crate name is
# anchored at a word boundary so `bench-autumn-web = "…"` is not read as a pin
# on `autumn-web`.
#
# The quoted branch takes ANY string and lets `requirement()` below decide
# whether it names a release. Anchoring it at a digit instead skipped every
# operator form, and `autumn-web = "=0.6.0"` is the first instruction in every
# migration guide ("Pin your current dependency … and commit") — four of them
# in this corpus, none of which reached the check.
#
# The inline-table branch forbids braces inside itself rather than merely
# stopping at the first `}`. That is what lets the pattern run across NEWLINES
# safely, which it must: a dependency table is routinely opened on one line and
# closed several later, and `\{[^{}]*\}` still cannot swallow a neighbouring
# block the way `[^}]*` spanning lines would.
#
# BOTH TOML string forms, here as well as in the parser. A basic `"0.5"` and a
# literal `'0.5'` are the same declaration, and the pattern path is not only
# prose — it is also the fallback for a `toml` fence that does not parse, so a
# double-quote-only pattern left the literal-string fix working on parseable
# fences alone. That is the same mistake `check-docs-scope.sh` records this repo
# shipping once, one layer down.
PIN = re.compile(
    r'(?<![\w.-])["\']?(?P<crate>autumn[a-z0-9-]*)["\']?\s*=\s*'
    r'(?:"(?P<basic>[^"\n]*)"'
    r"|'(?P<literal>[^'\n]*)'"
    r'|\{(?P<table>[^{}]*)\})')

# A `key = <string>` in either TOML string form, for the keys read out of an
# inline table or a subtable body.
VERSION_KEY = re.compile(r'version\s*=\s*(?:"([^"\n]*)"|\'([^\'\n]*)\')')
# Line-anchored, for a SUBTABLE body where `package` is a key of its own.
PACKAGE_KEY = re.compile(
    r'(?m)^[^\S\n]*package\s*=\s*(?:"([^"\n]*)"|\'([^\'\n]*)\')')
# Unanchored, for an INLINE table where it sits mid-line among the other keys.
PACKAGE_KEY_INLINE = re.compile(
    r'package\s*=\s*(?:"([^"\n]*)"|\'([^\'\n]*)\')')


def _string(match):
    """The one non-None alternative of a two-form quoted capture."""
    if match is None:
        return None
    return next((g for g in match.groups() if g is not None), None)


def without_toml_comments(text):
    """TOML comments blanked, preserving length so offsets still line up.

    Only ever applied to text already known to be TOML — the inside of an
    inline table, or a subtable's body — never to a whole markdown page, where
    `#` opens a heading rather than a comment.

    This exists because the fallback searches a table for `version = "…"` with
    an unanchored pattern, and a COMMENTED version sits earlier in the text
    than the live one:

        autumn-web = {
            # version = "0.7"
            version = "0.5", features = ["db"]
        }

    `tomllib` rejects a newline directly inside the braces (TOML 1.0 forbids
    it, Cargo accepts it), so this shape reaches the fallback — which then read
    `0.7` off the comment and passed a page really pinning `0.5`. That is worse
    than the misses found elsewhere in this path: not a pin overlooked, but the
    WRONG TEXT read and a false pass reported.

    A `#` inside a string is data, so quoting is tracked rather than stripped
    line-wise.
    """
    out = []
    quote = None
    index = 0
    while index < len(text):
        char = text[index]
        if quote:
            out.append(char)
            # Only a BASIC string honours backslash escapes; in a literal
            # string a backslash is an ordinary character.
            if char == '\\' and quote == '"' and index + 1 < len(text):
                out.append(text[index + 1])
                index += 2
                continue
            if char == quote:
                quote = None
            index += 1
            continue
        if char in '"\'':
            quote = char
            out.append(char)
            index += 1
            continue
        if char == '#':
            while index < len(text) and text[index] != '\n':
                out.append(' ')
                index += 1
            continue
        out.append(char)
        index += 1
    return ''.join(out)

# Cargo comparison operators that still name ONE release line, against the ones
# that name a RANGE instead.
#
# `=0.6.0` is as concrete an instruction as a bare pin, and `^`/`~` are the
# default caret and the tilde — all three say "this release line", so all three
# are checked. A `>=`, `>`, `<`, `<=` or `*` requirement names BOUNDS rather
# than a version, and a comma joins several of them into one range; "is this
# stale?" is not a question those can be asked, and reporting them would put
# noise on a correct page. They are skipped deliberately, not missed.
PINNING_OPS = ('=', '^', '~')
RANGE_OPS = ('>', '<', '*')


def requirement(spec):
    """The release version a Cargo requirement names, or None if it names none.

    None covers three populations, all of them correct pages: a range rather
    than a pin (`>=0.5`), a placeholder (`{X.Y.Z}` in `docs/migrations/next.md`,
    `<declared>` in the skill), and anything else that is not a version.

    Whatever this DOES return is a bare `x.y` or `x.y.z` that `acceptable()` can
    judge. That is an invariant, not a coincidence, and `--self-test` asserts it:
    `check()` reads a None verdict from `acceptable()` as "not this gate's
    business" and passes it, so any spec this function admits but that one
    cannot parse becomes a silent hole. A `0.5.*` did exactly that.
    """
    spec = spec.strip()
    if not spec or ',' in spec:
        return None
    if spec.startswith(RANGE_OPS):
        return None
    for op in PINNING_OPS:
        if spec.startswith(op):
            spec = spec[len(op):].strip()
            break
    # A wildcard in a LATER position has a floor and names a line: `0.5.*` is
    # `0.5.0`, the same as `0.5`. That is not this gate's invention —
    # `docs/guide/upgrading.md` documents `autumn upgrade` reading it that way,
    # so a gate that skipped it would disagree with the tool the same corpus
    # tells the reader to run. A bare `*` (already a range above) and a `0.*`
    # have no single floor: the first spans everything, the second a whole
    # major, and `upgrading.md` lists both among the forms with no floor.
    # `x` and `X` are wildcards too, and Cargo canonicalizes `0.5.x` to `0.5.*`
    # — verified against cargo itself, which resolves both spellings. Handling
    # only the literal `.*` left the x-form falling through to the numeric check
    # below and out as None, which is a silent pass.
    if spec[-1:] in ('*', 'x', 'X') and spec[-2:-1] == '.':
        head = spec[:-2]
        if head.count('.') != 1 or not all(p.isdigit() for p in head.split('.')):
            return None
        spec = head
    if not spec[:1].isdigit():
        return None
    # A PRERELEASE requirement is a real pin: Cargo accepts `0.6.0-beta.1`, and
    # this repo's release machinery cuts release candidates
    # (`QUICKSTART_CLI_VERSION`). Its line is its core, so the metadata is
    # dropped for the comparison while `spec` keeps the page's own spelling for
    # the report and for waiver matching. Refusing it was the same silent pass
    # the wildcard forms above were: `requirement()` returns None, so the pin is
    # never yielded and nothing is ever judged.
    parts = core(spec).split('.')
    # Anything left that `acceptable()` could not judge would pass silently.
    if len(parts) not in (2, 3) or not all(p.isdigit() for p in parts):
        return None
    return spec

# `<!-- version-pin-allow: autumn-web = "0.6" — reason -->`. The reason is
# required: a waiver without one outlives the passage it was written for.
# Matched over the whole page rather than line by line, because a waiver long
# enough to explain itself wraps.
WAIVER = re.compile(
    r'<!--\s*version-pin-allow:\s*(autumn[a-z0-9-]*)\s*=\s*"([^"]+)"\s*'
    r'(?:—|:)\s*(\S[^>]*?)-->', re.S)

# Any HTML comment, blanked before pins are extracted. A comment renders as
# nothing, so it is not a pin a reader can see, read or paste — and without
# this the gate reads its own waivers and they could never win.
HTML_COMMENT = re.compile(r'<!--.*?-->', re.S)


def blank_comments(text):
    """Replace HTML comment bodies with spaces, preserving every line number."""
    return HTML_COMMENT.sub(lambda m: re.sub(r'[^\n]', ' ', m.group(0)), text)


def waived(text):
    """{(crate, version): {line numbers the waiver covers}} for one page.

    A waiver covers its own blank-line-separated block and the one directly
    above it — the passage it was written for. Anchored at the line the marker
    OPENS on, so a reason that wraps is still scoped by where it was written.
    """
    lines = text.splitlines()
    # Block index per line, where a RUN of blank lines is ONE separator.
    #
    # Counting every blank line as its own separator put a passage in block 0
    # and a waiver two blank lines below it in block 2, so `own - 1` named a
    # block that does not exist and the waiver silently did nothing — the pin
    # it was written for was reported anyway, and CI rejected a page whose
    # author had done everything right. A blank line between a fence and the
    # comment under it is ordinary markdown, so this was reachable by writing
    # the waiver the obvious way.
    block_of = []
    block = 0
    in_blank_run = True      # leading blank lines open no block
    for line in lines:
        if line.strip():
            block_of.append(block)
            in_blank_run = False
        else:
            block_of.append(None)
            if not in_blank_run:
                block += 1
                in_blank_run = True
    out = {}
    for match in WAIVER.finditer(text):
        opens_at = text.count('\n', 0, match.start())
        if opens_at >= len(block_of):
            continue
        own = block_of[opens_at]
        if own is None:
            continue
        covered = {own}
        # The block directly above, skipping the blank run between them.
        above = own - 1
        if above >= 0:
            covered.add(above)
        key = (match.group(1), match.group(2))
        out.setdefault(key, set()).update(
            n for n, b in enumerate(block_of, 1) if b in covered)
    return out


# `[dependencies.autumn-web]`, and its dev/build variants — Cargo's SUBTABLE
# spelling, where the crate name is a section header and the version is a plain
# `version = "…"` key some lines below it. Neither half looks like a pin on its
# own, so the inline pattern above finds nothing and the page passes unchecked.
#
# The corpus has no `autumn*` subtable today (its one subtable is
# `[dependencies.web-sys]`, third-party and not this gate's business), so this
# closes a LATENT hole rather than a live defect. It is worth closing anyway:
# the form is ordinary Cargo that a page could adopt at any time, and
# `autumn-cli`'s `generate auth --mail` patcher already exists because real
# projects write their dependency this way.
SUBTABLE = re.compile(
    r'^[^\S\n]*\['
    # `workspace.` and `target.'cfg(…)'.` are the two prefixes Cargo puts in
    # front of a dependency table, and `_dependency_tables` walks both on the
    # parsed path. The fallback has to know them too: `docs/guide/edge.md` and
    # `autumn-edge/README.md` both pin `autumn-web` under
    # `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`, so this is a
    # shape the corpus really uses — it reaches the parser today only because
    # those fences happen to parse.
    r'(?:workspace\.|target\.(?:\'[^\']*\'|"[^"]*"|[^.\]]+)\.)?'
    r'(?:dev-|build-)?dependencies'
    r'\.["\']?([A-Za-z0-9_-]+)["\']?\][^\S\n]*$', re.M)

# The `version` key inside a subtable body, in either TOML string form and
# anchored at the start of its line so a `version` inside some other value is
# not mistaken for it.
SUBTABLE_VERSION = re.compile(
    r'(?m)^[^\S\n]*version\s*=\s*(?:"([^"\n]*)"|\'([^\'\n]*)\')')

# The start of the next TOML section, which is where a subtable's body ends.
NEXT_SECTION = re.compile(r'^[^\S\n]*\[', re.M)


def subtable_pins(body):
    """Yield (line_no, crate, spec) for each `[dependencies.<crate>]` section.

    The line reported is the `version = "…"` key, not the header: that is the
    line a fix edits.

    The crate is the header key unless the section renames it with
    `package = "…"`. Cargo does NOT treat `-` and `_` as interchangeable in a
    dependency table key, so `[dependencies.autumn_web]` is a dependency on a
    crate called `autumn_web` — which does not exist — UNLESS it carries that
    explicit rename. Following Cargo here rather than guessing is what keeps a
    correct page from being reported.
    """
    for match in SUBTABLE.finditer(body):
        start = match.end()
        following = NEXT_SECTION.search(body, start)
        section = without_toml_comments(
            body[start:following.start() if following else len(body)])
        version = SUBTABLE_VERSION.search(section)
        if not version:
            continue
        renamed = _string(PACKAGE_KEY.search(section))
        crate = renamed if renamed else match.group(1)
        if requirement(_string(version)) is None:
            continue
        lineno = body.count('\n', 0, start + version.start()) + 1
        yield lineno, crate, _string(version)


# `web = { package = "autumn-web", version = "0.5" }` — an inline table on a key
# that does not itself start with `autumn`, renamed to a crate that does. `PIN`
# anchors on the key, so it cannot see these.
#
# The PARSER already resolves a rename from any dependency table, which is why
# this only has to exist for the fragment fallback: a fence that both renames
# and fails to parse would otherwise lose its pin, and the asymmetry between the
# two paths is itself the kind of thing that turns into a finding later.
RENAMED_PIN = re.compile(
    r'(?<![\w.-])["\']?([A-Za-z0-9_-]+)["\']?\s*=\s*\{([^{}]*)\}')


def renamed_pins(body):
    """Yield (line_no, crate, spec) for inline tables renamed to an autumn crate.

    Keys that already start with `autumn` are left to `PIN`, so nothing is
    reported twice.
    """
    for match in RENAMED_PIN.finditer(body):
        table = without_toml_comments(match.group(2))
        package = _string(PACKAGE_KEY_INLINE.search(table))
        if not package or not package.startswith('autumn'):
            continue
        # Suppress only what `PIN` will report as the SAME crate. An earlier
        # version skipped every key starting with `autumn`, which was too
        # broad in both directions: `autumn_web = { package = "autumn-web" }`
        # vanished entirely, because `PIN` stops at the underscore and cannot
        # match that key either, and an `autumn-alias` key renamed to a real
        # crate was dropped unless the alias itself happened to be published.
        if package == match.group(1):
            continue
        spec = _string(VERSION_KEY.search(table))
        if spec is None or requirement(spec) is None:
            continue
        yield body.count('\n', 0, match.start()) + 1, package, spec


def pattern_pins(body):
    """Yield (line_no, crate, spec) for every pin a PATTERN can see.

    Matched over the whole comment-blanked text rather than one line at a time.
    An inline dependency table is routinely opened on one line and closed
    several later — `skills/autumn-web/SKILL.md` spells a nine-feature table
    that way — and a line-at-a-time reading skips those ENTIRELY: a silent hole
    in a gate whose whole purpose is not to have one. Line numbers are
    recovered from the match offset, so a defect still points at the line the
    pin opens on.

    This is the path for pins OUTSIDE a ```toml fence — prose, a `text` fence
    reproducing a panic, an unlabelled block — where there is no TOML document
    to parse. `toml` fences go through `fenced_toml_pins`.
    """
    yield from subtable_pins(body)
    yield from renamed_pins(body)
    for match in PIN.finditer(body):
        spec = match['basic'] if match['basic'] is not None else match['literal']
        if spec is None:
            table = without_toml_comments(match['table'] or '')
            # A `package = "…"` rename means the KEY is a local ALIAS and not
            # the crate, so reporting the key here is a FALSE POSITIVE — the
            # one failure this gate cannot afford. `autumn-edge = { package =
            # "axum", version = "0.5" }` is a dependency on axum, and a page
            # writing it is correct; before this, the pattern path called it a
            # stale `autumn-edge` pin. An alias renamed to a DIFFERENT autumn
            # crate was worse: `renamed_pins` reported the real crate and this
            # pass reported the alias, so one declaration produced two defects.
            #
            # The parsed path never had this, because `_declared` resolves the
            # rename before yielding. This makes the two paths agree, and makes
            # `PIN` and `renamed_pins` exactly complementary: a rename to
            # another crate belongs to `renamed_pins`, everything else here.
            renamed = _string(PACKAGE_KEY_INLINE.search(table))
            if renamed is not None and renamed != match['crate']:
                continue
            inner = _string(VERSION_KEY.search(table))
            if inner is None:
                # `{ path = "../autumn" }` or `{ workspace = true }` pins no
                # version, so there is nothing here to be stale.
                continue
            spec = inner
        if requirement(spec) is None:
            continue
        # The spec is yielded AS WRITTEN, not normalized: a report saying
        # `autumn-web = "0.5.0"` against a page that says `"=0.5.0"` sends the
        # reader looking for text that is not there, and a waiver marker is
        # written by copying the pin off the page. `requirement()` is applied
        # again by the caller to compare it.
        yield body.count('\n', 0, match.start()) + 1, match['crate'], spec


# A fence opener, with its language. Only ```toml blocks are handed to the TOML
# parser; every other block stays on the pattern path.
FENCE_MARK = re.compile(r'^[^\S\n]*(?P<mark>`{3,}|~{3,})[^\S\n]*'
                        r'(?P<info>[^\n]*)$')


def fence_opener(line):
    """A fence opener as (marker run, info string), or None.

    The marker is returned WHOLE rather than as its character, because its
    length is part of the fence: a closer must be at least as long as the
    opener it closes. `fence_closes` needs both.

    CommonMark restricts the info string of a BACKTICK fence only, and only as
    to backticks: ```` ```lang`x` ```` would be ambiguous with inline code. A
    tilde fence's info string may hold anything, backticks and tildes included.

    Excluding both characters from every info string — which is what this
    pattern did — repeats the bug `fence_language` was written to fix, one
    character over. An ordinary annotation such as
    ```` ```toml title="~/Cargo.toml" ```` matched the opener NOT AT ALL, so
    the block was neither parsed nor masked and its whole body went to the
    pattern path; a `[patch.crates-io]` pin under that opener was then reported
    against the exemption this gate documents a few hundred lines up.
    """
    match = FENCE_MARK.match(line)
    if match is None:
        return None
    run, info = match['mark'], match['info']
    if run[0] == '`' and '`' in info:
        return None
    return run, info


def fence_closes(line, run):
    """Does LINE close a fence opened by RUN?

    CommonMark: a closing fence is a run of the SAME character, AT LEAST AS
    LONG as the opener, followed by nothing but whitespace. Both halves matter
    here, and neither was enforced:

      * A shorter run does not close a longer fence. Inside a ```` ````toml ````
        block, a ```` ``` ```` line is content.
      * A run carrying an info string closes nothing. Inside a ```` ```toml ````
        block — a multiline string holding a snippet, say — a ```` ```rust ````
        line is content too.

    Treating either as a closer ends the fence early, and the rest of the TOML
    then goes to the pattern path, where a `[patch.crates-io]` entry below the
    split is reported against the exemption this gate documents: a false CI
    failure on a page doing nothing wrong.

    Indentation is NOT held to CommonMark's three-space limit, deliberately.
    This scanner is flat rather than container-aware, and an opener is already
    matched at any indentation, so a fence nested in a list item must be able
    to close at the same depth its opener sat at.
    """
    stripped = line.strip()
    length = len(stripped) - len(stripped.lstrip(run[0]))
    return length >= len(run) and not stripped[length:].strip()


def fence_language(info):
    """The language of a fence, from its CommonMark INFO STRING.

    The info string is everything after the opening marker, and only its first
    word is the language: ```` ```toml title="Cargo.toml" ```` is TOML, and so
    is ```` ```toml,ignore ```` — a form this repo already uses elsewhere
    (`rust,ignore`, `rust,no_run`).

    Requiring the language to be the whole info string was worse than it looks.
    ```` ```toml title="…" ```` matched the opener pattern not at all, so the
    block was neither parsed NOR masked, and its whole body went to the pattern
    path. An annotated `[patch.crates-io]` block then had its pin reported —
    a false positive, and one that contradicted the patch exemption this gate
    documents a few hundred lines up.
    """
    return info.strip().split()[0].split(',')[0].lower() if info.strip() else ''

# The dependency tables Cargo reads a version out of.
DEP_TABLES = ('dependencies', 'dev-dependencies', 'build-dependencies')


def toml_fences(text):
    """Yield (first_body_line, body) for every ```toml block, 1-based."""
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        opener = fence_opener(lines[i])
        if opener is None:
            i += 1
            continue
        run, info = opener
        start = i + 1
        end = start
        while end < len(lines) and not fence_closes(lines[end], run):
            end += 1
        if fence_language(info) == 'toml':
            yield start + 1, '\n'.join(lines[start:end])
        i = end + 1


def _dependency_tables(doc):
    """Every dependency table in a parsed manifest fragment."""
    out = []
    for name in DEP_TABLES:
        table = doc.get(name)
        if isinstance(table, dict):
            out.append(table)
    target = doc.get('target')
    if isinstance(target, dict):
        for cfg in target.values():
            if isinstance(cfg, dict):
                for name in DEP_TABLES:
                    table = cfg.get(name)
                    if isinstance(table, dict):
                        out.append(table)
    # `[workspace.dependencies]` is where an Autumn workspace declares the
    # framework once for every member to inherit with `workspace = true`, so it
    # is the single most load-bearing pin a multi-crate project has. Appending
    # `doc` below does not reach it: that yields the key `workspace`, whose
    # value is a table with no `version` of its own.
    workspace = doc.get('workspace')
    if isinstance(workspace, dict):
        for name in DEP_TABLES:
            table = workspace.get(name)
            if isinstance(table, dict):
                out.append(table)
    # A fence that IS a dependency table body with the `[dependencies]` header
    # left off — which is how most snippets in this corpus are written.
    out.append(doc)
    # Deduplicate the TRAVERSAL, by table identity, rather than the
    # declarations: if some shape ever makes one table reachable twice, walking
    # it twice would double-report its pins, while dropping declarations that
    # merely have equal values loses real occurrences.
    unique = []
    for table in out:
        if not any(table is already for already in unique):
            unique.append(table)
    return unique


def _declared(table):
    """Yield (key, crate, spec) for each dependency that names a version."""
    for key, value in table.items():
        if isinstance(value, str):
            yield key, key, value
        elif isinstance(value, dict):
            version = value.get('version')
            if isinstance(version, str):
                # `package = "…"` renames the dependency: the table KEY can be
                # anything, and the crate it really pins is the rename. Cargo
                # resolves it this way in an inline table and a subtable alike.
                package = value.get('package')
                yield key, package if isinstance(package, str) else key, version


def _pin_line(body_lines, key, spec, taken):
    """Where inside a fence this pin is written, as a 0-based offset.

    `tomllib` reports no positions, so the line is recovered by looking for the
    dependency's key and then the version literal at or below it. That covers
    the inline form (both on one line), the multiline table (key on the opening
    line) and the subtable (key in the header, version below).

    `taken` holds the offsets already claimed by pins from this same fence, and
    both searches skip them. Without it, the SAME key declared in two tables —
    `[dependencies]` and `[dev-dependencies]` are the ordinary pairing — both
    resolve to the first line, so the two occurrences collapse onto one report.

    The line a repeated key lands on follows the order `_dependency_tables`
    walks, not the order the tables appear in the fence, so a fence that writes
    `[dev-dependencies]` above `[dependencies]` attributes the two the other way
    round. Both are reported and both are judged; only which report carries
    which line number can swap. There is no position information to do better
    with, and being one line out is a far smaller defect than dropping the
    occurrence entirely.
    """
    declared = next((i for i, line in enumerate(body_lines)
                     if key in line and i not in taken), None)
    if declared is None:
        declared = next((i for i, line in enumerate(body_lines) if key in line),
                        0)
    quoted = (f'"{spec}"', f"'{spec}'")
    return next((i for i in range(declared, len(body_lines))
                 if i not in taken and any(q in body_lines[i] for q in quoted)),
                declared)


def fenced_toml_pins(body):
    """Yield (line_no, crate, spec) from every ```toml block, parsed as TOML.

    `tomllib` rather than a pattern, because the thing a pattern approximates
    here IS a TOML parser, and three review rounds in a row each found another
    spelling it did not know: the `[dependencies.x]` subtable, literal
    (single-quoted) strings, and `package = "…"` renames on a key that does not
    start with `autumn`. `check-docs-scope.sh` records this repo shipping the
    same mistake once already — a manifest reader that took only double-quoted
    `readme = "…"`, so a single-quoted path resolved to nothing in all four
    gates that shared it. Parsing ends the class instead of adding a fourth
    special case to a pattern that will have a fifth.

    A fence that does not parse is a FRAGMENT, not a defect — an elided `…`, a
    slice of a larger file, a deliberate syntax error being described — so it
    falls back to the pattern rather than being skipped. Skipping would be a
    silent hole of exactly the kind this gate exists to prevent.
    """
    for first_line, fence in toml_fences(body):
        try:
            doc = tomllib.loads(fence)
        except (tomllib.TOMLDecodeError, ValueError, TypeError):
            # Comments are blanked before the pattern sees the fence, not just
            # inside a table it has already captured. A WHOLE declaration can be
            # commented out — `# autumn-web = "0.5"` above a live pin is how
            # anyone shows a before-and-after — and reporting it is a false
            # positive on a page doing nothing wrong. The fence is known TOML
            # here, which is what makes this safe; the same blanking must never
            # be applied to a whole markdown page, where `#` opens a heading.
            for lineno, crate, spec in pattern_pins(without_toml_comments(fence)):
                yield first_line + lineno - 1, crate, spec
            continue
        fence_lines = fence.splitlines()
        # Offsets already claimed, so two declarations of one key land on two
        # lines. This deduplicates the LINE, never the declaration: an earlier
        # version keyed a `seen` set on (key, crate, spec) and so discarded the
        # second of two identical pins in `[dependencies]` and
        # `[dev-dependencies]` — a real second occurrence, silently dropped,
        # which is the per-occurrence promise this gate makes over the
        # existential `FIRST_RUN_DOCS` test it replaces. It also let a
        # line-scoped waiver on the survivor conceal an unwaived duplicate.
        taken = set()
        for table in _dependency_tables(doc):
            for key, crate, spec in _declared(table):
                if requirement(spec) is None:
                    continue
                offset = _pin_line(fence_lines, key, spec, taken)
                taken.add(offset)
                yield first_line + offset, crate, spec


def mask_toml_fences(text):
    """Blank every ```toml body, preserving line numbers.

    The parser owns those blocks. Leaving them visible to the pattern as well
    would report every pin in them twice.
    """
    lines = text.splitlines(keepends=True)
    for first_line, fence in toml_fences(text):
        for offset in range(len(fence.splitlines())):
            index = first_line - 1 + offset
            if index < len(lines):
                lines[index] = re.sub(r'[^\n]', ' ', lines[index])
    return ''.join(lines)


def pins(text):
    """Yield (line_no, crate, spec) for every pin a reader can see.

    Two paths over one page, partitioned so nothing is read twice: a ```toml
    fence is a TOML document and is parsed as one; everything else — prose, a
    `text` fence, an unlabelled block — has no document to parse and stays on
    the pattern.
    """
    body = blank_comments(text)
    found = list(fenced_toml_pins(body))
    found.extend(pattern_pins(mask_toml_fences(body)))
    return sorted(found)


# A migration guide's own release line, from its filename: `0.4.0.md` -> 0.4.0.
MIGRATION_PAGE = re.compile(r'^docs/migrations/(\d+\.\d+\.\d+)\.md$')


def ceiling(path, published):
    """The newest release line `path` may pin, as a triple.

    Every page is held to the published line. A migration guide is held to its
    OWN line instead: it exists to show the before-and-after, so a pin at or
    below its version is the page working correctly, and only a pin above it —
    a guide describing a release it predates — is a defect.
    """
    match = MIGRATION_PAGE.match(path)
    if match:
        return triple(match.group(1))
    return triple(published)


def acceptable(version, cap, allow_older):
    """Whether a pin naming `version` is allowed under `cap`.

    A pin is usually a LINE (`0.7`), occasionally the exact release (`0.7.0`),
    and Cargo reads both as a caret range — so both spellings are compared on
    the line, and `0.7` and `0.7.0` are the same claim.

    `allow_older` is the migration-page rule and the ONLY place an old pin is
    right. An ordinary page must name the published line exactly: a pin below it
    is the defect this gate exists for, and a pin above it promises a release
    that is not out. A migration guide to 0.4.0 may pin 0.4 or anything older,
    because the before-block is the page's whole purpose, but still not 0.5.

    A `0.7.1` pin against a published `0.7.0` is refused in both modes: it is on
    the right line but claims a patch nobody can install.

    Returns None when `version` is not a release version at all, which is not
    something this gate has an opinion about.
    """
    # Compared on the CORE, so a prerelease is judged by the line it belongs to
    # — `0.7.0-rc.1` is the 0.7 line. Splitting the raw spelling instead left a
    # two-component prerelease (`0.6-beta`) matching neither branch and falling
    # out as None, which `check()` passes.
    parts = core(version).split('.')
    if len(parts) == 2 and all(p.isdigit() for p in parts):
        got = (int(parts[0]), int(parts[1]))
        return got <= cap[:2] if allow_older else got == cap[:2]
    exact = triple(version)
    if exact is None:
        return None      # not a release version; not this gate's business
    if allow_older:
        return exact <= cap
    return exact[:2] == cap[:2] and exact <= cap


def check(root):
    published = published_version(root)
    workspace = workspace_version(root)
    pub_triple = triple(published)
    ws_triple = triple(workspace)
    problems = []
    if pub_triple is None:
        problems.append(
            f'README.md pins a malformed autumn-cli version {published!r}; '
            f'expected x.y.z')
    if ws_triple is None:
        problems.append(
            f'[workspace.package] version {workspace!r} should be x.y.z')
    if problems:
        return problems, 0, 0
    if pub_triple > ws_triple:
        problems.append(
            f'README.md pins autumn-cli {published}, which is newer than the '
            f'workspace version {workspace}; the quickstart must pin the '
            f'latest published release')
        return problems, 0, 0
    # A PRERELEASE published version cannot be compared by release line, and
    # accepting it would be worse than refusing it. Cargo does not resolve an
    # ordinary `^0.8` requirement against a `0.8.0-rc.1` candidate — it reports
    # "candidate versions found which didn't match" and asks for the prerelease
    # to be named explicitly — so every `autumn-web = "0.8"` in the corpus would
    # reduce to the same 0.8 line, pass this gate, and resolve for nobody.
    #
    # This repo does not cut semver prereleases: there are no `-rc`/`-beta`
    # tags, and `docs/release-checklist.md` calls the thing it publishes a
    # "release candidate" while giving its version as an ordinary `0.7.0`. So
    # rather than build prerelease-compatibility logic for a release shape that
    # has never existed here, the gate says plainly that it cannot judge this
    # one. Failing loudly beats a green run over a corpus none of which
    # resolves.
    if core(published) != published:
        problems.append(
            f'README.md pins the prerelease {published}. This gate compares '
            f'release LINES, and Cargo will not resolve a plain `^x.y` '
            f'requirement against a prerelease candidate, so every pin in the '
            f'corpus would be accepted while none of them resolves. Pin a '
            f'published release, or teach this gate to require '
            f'prerelease-compatible requirements before shipping one.')
        return problems, 0, 0

    crates = published_crates(root)
    checked = 0
    waived_count = 0
    for path in corpus(root):
        try:
            text = pathlib.Path(root, path).read_text(encoding='utf-8')
        except (OSError, UnicodeDecodeError):
            continue
        allowed = waived(text)
        cap = ceiling(path, published)
        for lineno, crate, spec in pins(text):
            if crate not in crates:
                continue
            # `spec` is the page's own spelling (`=0.4.0`); `requirement` is
            # what it names (`0.4.0`). Compare the second, report the first.
            verdict = acceptable(requirement(spec), cap,
                                 allow_older=bool(MIGRATION_PAGE.match(path)))
            if verdict is None or verdict:
                checked += 1
                continue
            if lineno in allowed.get((crate, spec), ()):
                waived_count += 1
                continue
            checked += 1
            want = series(published) if not MIGRATION_PAGE.match(path) else \
                series('.'.join(str(p) for p in cap))
            problems.append(
                f'{path}:{lineno}  {crate} = "{spec}"  '
                f'(this corpus publishes {series(published)}; '
                f'this page may pin at most {want})')
    return problems, checked, waived_count


def emit(lines):
    """Print an inspection listing, tolerating a reader that stops early.

    `--list | head` is how the failure message tells someone to read this, and
    `head` closing the pipe is not an error. Without this, that exact command
    answers with a `BrokenPipeError` traceback — and Python re-raises it again
    while flushing at shutdown, so catching alone is not enough; stdout is
    pointed at the null device so the interpreter's own flush stays quiet.
    """
    try:
        for line in lines:
            print(line)
        sys.stdout.flush()
    except BrokenPipeError:
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())


def list_pins(root):
    crates = published_crates(root)

    def rows():
        for path in corpus(root):
            try:
                text = pathlib.Path(root, path).read_text(encoding='utf-8')
            except (OSError, UnicodeDecodeError):
                continue
            for lineno, crate, version in pins(text):
                if crate in crates:
                    yield f'{path}:{lineno}\t{crate}\t{version}'

    emit(rows())


def print_corpus():
    """Print this gate's resolved corpus, one path per line.

    `scripts/check-docs-scope.sh` compares these lists across the gates that
    share a reader-facing corpus. It asks each gate what it reads rather than
    re-deriving it from this file's source, because a corpus is widened in
    several places at once and a checker that models some of those rules
    reports agreement over the rest.
    """
    emit(sorted(corpus(ROOT)))


def self_test():
    """Synthetic pages, so the rules are tested without waiting on the corpus."""
    failures = []

    def expect(name, got, want):
        if got != want:
            failures.append(f'{name}: got {got!r}, want {want!r}')

    expect('series', series('0.7.0'), '0.7')
    expect('triple ok', triple('0.7.0'), (0, 7, 0))
    expect('triple rejects line', triple('0.7'), None)
    expect('triple rejects junk', triple('0.7.x'), None)

    cap = (0, 7, 0)
    expect('line pin ok', acceptable('0.7', cap, allow_older=False), True)
    expect('exact pin ok', acceptable('0.7.0', cap, allow_older=False), True)
    expect('older line stale', acceptable('0.5', cap, allow_older=False), False)
    expect('older exact stale',
           acceptable('0.5.0', cap, allow_older=False), False)
    expect('future line stale', acceptable('0.8', cap, allow_older=False), False)
    expect('unreleased patch',
           acceptable('0.7.1', cap, allow_older=False), False)
    expect('not a version', acceptable('1', cap, allow_older=False), None)

    # A migration guide is held to its own line, and may show anything older.
    expect('migration ceiling', ceiling('docs/migrations/0.4.0.md', '0.7.0'),
           (0, 4, 0))
    expect('guide ceiling', ceiling('docs/guide/storage.md', '0.7.0'),
           (0, 7, 0))
    expect('migration before-block ok',
           acceptable('0.3', (0, 4, 0), allow_older=True), True)
    expect('migration own line ok',
           acceptable('0.4', (0, 4, 0), allow_older=True), True)
    expect('migration future pin stale',
           acceptable('0.5', (0, 4, 0), allow_older=True), False)
    # The same old pin on an ordinary page is the defect this gate is for.
    expect('guide before-block not a thing',
           acceptable('0.3', (0, 7, 0), allow_older=False), False)

    # Pin extraction.
    expect('inline table pin',
           list(pins('autumn-web = { version = "0.7", features = ["ws"] }')),
           [(1, 'autumn-web', '0.7')])
    expect('bare pin', list(pins('autumn-storage-s3 = "0.7"')),
           [(1, 'autumn-storage-s3', '0.7')])
    expect('prose pin', list(pins('add `autumn-cache-redis = "0.7"` to it')),
           [(1, 'autumn-cache-redis', '0.7')])
    expect('path dep has no version',
           list(pins('autumn-web = { path = "../autumn" }')), [])
    expect('workspace dep has no version',
           list(pins('autumn-web = { workspace = true }')), [])

    # Cargo requirement operators. Every migration guide opens with
    # `autumn-web = "=0.6.0"`, so the exact form has to reach the check.
    # The spec is reported AS WRITTEN, so a defect quotes text that is really
    # on the page and a waiver can be written by copying it.
    expect('exact-pin operator', list(pins('autumn-web = "=0.4.0"')),
           [(1, 'autumn-web', '=0.4.0')])
    expect('caret operator', list(pins('autumn-web = "^0.7"')),
           [(1, 'autumn-web', '^0.7')])
    expect('tilde operator', list(pins('autumn-web = "~0.7"')),
           [(1, 'autumn-web', '~0.7')])
    expect('requirement strips the operator', requirement('=0.4.0'), '0.4.0')
    expect('requirement keeps a bare version', requirement('0.7'), '0.7')
    # A range names bounds, not a version; asking whether it is stale is not a
    # question it can answer, so it is skipped rather than reported.
    expect('lower-bound range skipped', list(pins('autumn-web = ">=0.5"')), [])
    expect('comma range skipped',
           list(pins('autumn-web = ">=0.5, <0.8"')), [])
    expect('wildcard skipped', list(pins('autumn-web = "*"')), [])
    # Placeholders in `docs/migrations/next.md` and the skill are not versions.
    expect('brace placeholder skipped',
           list(pins('autumn-web = "={X.Y.Z}"')), [])
    expect('angle placeholder skipped',
           list(pins('autumn-web = "<declared>"')), [])

    # A dependency table opened on one line and closed several later. Read one
    # line at a time this is invisible, which is the hole this covers; the line
    # number reported is the one the pin OPENS on.
    multiline = ('autumn-web = { version = "0.5", features = [\n'
                 '    "mail",\n'
                 '    "ws",\n'
                 '] }\n')
    expect('multiline inline table', list(pins(multiline)),
           [(1, 'autumn-web', '0.5')])
    # …and it must actually be REFUSED, not merely listed.
    expect('multiline stale pin is refused',
           acceptable(requirement('0.5'), (0, 7, 0), allow_older=False), False)
    expect('exact stale pin is refused',
           acceptable(requirement('=0.5.0'), (0, 7, 0), allow_older=False),
           False)
    # Two tables in one page must not be fused into one match across the gap.
    two = ('autumn-web = { version = "0.7" }\n'
           '\n'
           'autumn-edge = { version = "0.7" }\n')
    expect('adjacent tables stay separate', list(pins(two)),
           [(1, 'autumn-web', '0.7'), (3, 'autumn-edge', '0.7')])

    # Cargo's subtable spelling: neither half looks like a pin on its own, and
    # the line reported is the `version` key, which is the line a fix edits.
    sub = ('[dependencies.autumn-web]\n'
           'version = "0.5"\n'
           'features = ["db"]\n')
    expect('subtable pin', list(pins(sub)), [(2, 'autumn-web', '0.5')])
    expect('dev-dependencies subtable',
           list(pins('[dev-dependencies.autumn-web]\nversion = "0.5"\n')),
           [(2, 'autumn-web', '0.5')])
    # `package = "…"` renames the dependency; the rename is the real crate.
    expect('subtable rename resolves the crate',
           list(pins('[dependencies.autumn_web]\n'
                     'package = "autumn-web"\nversion = "0.5"\n')),
           [(3, 'autumn-web', '0.5')])
    # A version key in the NEXT section belongs to that section, not this one.
    expect('subtable stops at the next section',
           list(pins('[dependencies.autumn-web]\n'
                     'features = ["db"]\n'
                     '\n[package]\nversion = "0.5"\n')), [])
    expect('subtable with no version pins nothing',
           list(pins('[dependencies.autumn-web]\npath = "../autumn"\n')), [])

    # ---- the ```toml path, which is parsed rather than matched ----
    def fenced(*body):
        return '```toml\n' + '\n'.join(body) + '\n```\n'

    # TOML literal (single-quoted) strings are as valid as basic ones. A
    # double-quote-only reader shipped in this repo once already; see
    # `check-docs-scope.sh` on `readme = 'README.md'`.
    expect('literal string pin', list(pins(fenced("autumn-web = '0.5'"))),
           [(2, 'autumn-web', '0.5')])
    expect('literal string in an inline table',
           list(pins(fenced("autumn-web = { version = '0.5' }"))),
           [(2, 'autumn-web', '0.5')])
    # A `package = "…"` rename on a key that does not start with `autumn`
    # still pins autumn-web, in an inline table exactly as in a subtable.
    expect('inline table rename resolves the crate',
           list(pins(fenced('web = { package = "autumn-web", '
                            'version = "0.5" }'))),
           [(2, 'autumn-web', '0.5')])
    expect('parsed dev-dependencies table',
           list(pins(fenced('[dev-dependencies]', 'autumn-web = "0.5"'))),
           [(3, 'autumn-web', '0.5')])
    # A pin must be reported ONCE, not by both paths.
    expect('toml fence is not double-read',
           list(pins(fenced('autumn-web = "0.5"'))),
           [(2, 'autumn-web', '0.5')])
    # A fence that is a FRAGMENT does not parse. Falling back to the pattern is
    # what keeps that from becoming a silent hole.
    fragment = fenced('[dependencies]', 'autumn-web = "0.5"', '# …', 'oops =')
    expect('unparseable fence falls back to the pattern',
           list(pins(fragment)), [(3, 'autumn-web', '0.5')])
    # A non-toml fence has no document to parse and stays on the pattern.
    expect('text fence stays on the pattern',
           list(pins('```text\nautumn-web = "0.6"\n```\n')),
           [(2, 'autumn-web', '0.6')])

    # `[workspace.dependencies]` is where a multi-crate project declares the
    # framework once for every member to inherit — the most load-bearing pin
    # such a project has, and not reached by appending the document.
    expect('workspace.dependencies',
           list(pins(fenced('[workspace.dependencies]',
                            'autumn-web = "0.5"'))),
           [(3, 'autumn-web', '0.5')])
    expect('workspace dependency subtable',
           list(pins('[workspace.dependencies.autumn-web]\nversion = "0.5"\n')),
           [(2, 'autumn-web', '0.5')])

    # A wildcard in a LATER position has a floor and names a line, exactly as
    # `docs/guide/upgrading.md` documents `autumn upgrade` reading it.
    expect('later-position wildcard has a floor', requirement('0.5.*'), '0.5')
    expect('wildcard pin is refused when stale',
           acceptable(requirement('0.5.*'), (0, 7, 0), allow_older=False),
           False)
    expect('wildcard pin on the current line passes',
           acceptable(requirement('0.7.*'), (0, 7, 0), allow_older=False), True)
    expect('wildcard pin is read', list(pins(fenced('autumn-web = "0.5.*"'))),
           [(2, 'autumn-web', '0.5.*')])
    # A whole-major or bare wildcard has no single floor; both stay ranges.
    expect('major wildcard skipped', requirement('0.*'), None)
    expect('bare wildcard skipped', requirement('*'), None)
    # Cargo canonicalizes `0.5.x` to `0.5.*` and accepts either case; verified
    # against cargo itself, which resolves both spellings.
    expect('x wildcard has a floor', requirement('0.5.x'), '0.5')
    expect('X wildcard has a floor', requirement('0.5.X'), '0.5')
    expect('x wildcard is read', list(pins(fenced('autumn-web = "0.5.x"'))),
           [(2, 'autumn-web', '0.5.x')])
    expect('major x wildcard skipped', requirement('0.x'), None)

    # A rename on a key that is not itself `autumn*`, in a fence that does NOT
    # parse — so the parser cannot resolve it and the pattern must.
    expect('rename in an unparseable fence',
           list(pins(fenced('web = { package = "autumn-web", '
                            'version = "0.5" }', 'oops ='))),
           [(2, 'autumn-web', '0.5')])
    expect('rename in prose',
           list(pins('use `web = { package = "autumn-web", version = "0.5" }`')),
           [(1, 'autumn-web', '0.5')])
    # A rename to a non-autumn crate is not this gate's business.
    expect('rename to a foreign crate ignored',
           list(pins('web = { package = "axum", version = "0.5" }')), [])

    # A `[patch.…]` entry redirects a dependency AWAY from the registry, so its
    # version is not a release-line pin and a page patching onto an older fork
    # is correct. Asserted so a later contributor does not "fix" the parser into
    # reporting it — the omission is the decision, not a gap.
    expect('patch table is not a release pin',
           list(pins(fenced('[patch.crates-io]',
                            'autumn-web = { path = "../fork", '
                            'version = "0.5" }'))),
           [])
    expect('patch table with no version is inert',
           list(pins(fenced('[patch.crates-io]',
                            'autumn-web = { path = "autumn" }'))),
           [])

    # ---- fence INFO STRINGS: only the first word is the language ----
    expect('bare language', fence_language('toml'), 'toml')
    expect('attribute after the language',
           fence_language('toml title="Cargo.toml"'), 'toml')
    expect('comma-separated attributes', fence_language('toml,ignore'), 'toml')
    expect('empty info string', fence_language(''), '')
    expect('a different language is not toml', fence_language('rust,ignore'),
           'rust')
    # The consequence that made this matter: an ANNOTATED toml fence did not
    # match the opener at all, so it was neither parsed nor masked, and a
    # `[patch.…]` block inside it had its pin reported — contradicting the
    # exemption documented above.
    expect('annotated patch fence stays exempt',
           list(pins('```toml title="Cargo.toml"\n[patch.crates-io]\n'
                     'autumn-web = { path = "../fork", version = "0.5" }\n'
                     '```\n')),
           [])
    expect('annotated toml fence is still parsed',
           list(pins('```toml title="Cargo.toml"\n[dependencies]\n'
                     'autumn-web = "0.5"\n```\n')),
           [(3, 'autumn-web', '0.5')])
    # A BACKTICK fence's info string may hold a tilde — a path annotation is
    # the obvious way to write one. Excluding tildes from every info string
    # reopened the hole above, one character over.
    expect('tilde in a backtick info string is still an opener',
           fence_opener('```toml title="~/Cargo.toml"'),
           ('```', 'toml title="~/Cargo.toml"'))
    expect('tilde-annotated toml fence is parsed',
           list(pins('```toml title="~/Cargo.toml"\n[dependencies]\n'
                     'autumn-web = "0.5"\n```\n')),
           [(3, 'autumn-web', '0.5')])
    expect('tilde-annotated patch fence stays exempt',
           list(pins('```toml title="~/Cargo.toml"\n[patch.crates-io]\n'
                     'autumn-web = { path = "../fork", version = "0.5" }\n'
                     '```\n')),
           [])
    # …and the one restriction CommonMark does place stays enforced: a
    # backtick in a BACKTICK fence's info string is ambiguous with inline code,
    # so that line opens nothing.
    expect('backtick in a backtick info string is not an opener',
           fence_opener('```toml `x`'), None)
    # A TILDE fence carries no such restriction, on either character.
    expect('tilde fence info string takes both characters',
           fence_opener('~~~toml title="`~/Cargo.toml`"'),
           ('~~~', 'toml title="`~/Cargo.toml`"'))
    expect('tilde-fenced toml is parsed',
           list(pins('~~~toml\n[dependencies]\nautumn-web = "0.5"\n~~~\n')),
           [(3, 'autumn-web', '0.5')])

    # ---- fence CLOSERS: same character, no shorter, nothing but whitespace ----
    expect('a plain closer closes', fence_closes('```', '```'), True)
    expect('a longer closer closes', fence_closes('`````', '```'), True)
    expect('a shorter run does not close a longer fence',
           fence_closes('```', '````'), False)
    expect('a closer may not carry an info string',
           fence_closes('```rust', '```'), False)
    expect('a closer may not carry anything else',
           fence_closes('``` and then some prose', '```'), False)
    expect('the other fence character does not close',
           fence_closes('~~~', '```'), False)
    expect('a closer may be indented', fence_closes('    ```', '```'), True)
    # The consequence, and why this is a FALSE FAILURE rather than a miss: a
    # line that is not a closer ended the block anyway, so the TOML below the
    # split went to the pattern path — which does not know the patch exemption.
    expect('a nested fence line does not split a toml block',
           list(pins('```toml\n[deps]\nx = """\n```rust\n"""\n\n'
                     '[patch.crates-io]\n'
                     'autumn-web = { path = "../fork", version = "0.5" }\n'
                     '```\n')),
           [])
    expect('a short run does not split a longer toml block',
           list(pins('````toml\n[patch.crates-io]\n'
                     'autumn-web = { path = "../fork", version = "0.5" }\n'
                     'note = """\n```\n"""\n````\n')),
           [])
    # …and a block that legitimately ends still ends: the pin AFTER a real
    # closer is prose, and must not be swallowed into the fence.
    expect('a real closer still ends the block',
           list(pins('```toml\n[dependencies]\nautumn-web = "0.5"\n```\n\n'
                     'Then in prose, autumn-web = "0.6" is mentioned.\n')),
           [(3, 'autumn-web', '0.5'), (6, 'autumn-web', '0.6')])

    # ---- the surface line survives a README pin that is not a version ----
    # `series()` raises without two numeric components, and this line printed
    # BEFORE the problems did, so the gate answered a malformed pin with a
    # traceback instead of the message it had already prepared.
    expect('surface line tolerates a malformed pin',
           'unreadable' in describe_surface('garbage', '0.7.0'), True)
    expect('surface line is normal for a real pin',
           describe_surface('0.7.0', '0.7.0'),
           'surface: published release line 0.7 (README quickstart), '
           'workspace 0.7.0')

    # PER-OCCURRENCE, which is the whole promise over the existential
    # `FIRST_RUN_DOCS` test: one key declared in two tables is two pins on two
    # lines, not one. Deduplicating equal declarations dropped the second.
    expect('repeated key across tables is two pins',
           list(pins(fenced('[dependencies]', 'autumn-web = "0.5"', '',
                            '[dev-dependencies]', 'autumn-web = "0.5"'))),
           [(3, 'autumn-web', '0.5'), (6, 'autumn-web', '0.5')])
    # …including when the two name DIFFERENT lines, which must not be swapped
    # onto one another.
    expect('repeated key with differing versions',
           list(pins(fenced('[dependencies]', 'autumn-web = "0.5"', '',
                            '[dev-dependencies]', 'autumn-web = "0.6"'))),
           [(3, 'autumn-web', '0.5'), (6, 'autumn-web', '0.6')])
    # A single declaration must still report exactly once.
    expect('single declaration reports once',
           list(pins(fenced('[dependencies]', 'autumn-web = "0.5"'))),
           [(3, 'autumn-web', '0.5')])

    # ---- prereleases, which this repo's release machinery actually cuts ----
    expect('core strips a prerelease', core('0.6.0-beta.1'), '0.6.0')
    expect('core strips build metadata', core('0.7.0+build.5'), '0.7.0')
    expect('core leaves a plain version', core('0.7.0'), '0.7.0')
    expect('triple reads an rc', triple('0.8.0-rc.1'), (0, 8, 0))
    expect('series reads an rc', series('0.8.0-rc.1'), '0.8')
    # The spelling survives for the report and for waiver matching.
    expect('prerelease keeps its spelling',
           requirement('0.6.0-beta.1'), '0.6.0-beta.1')
    expect('stale prerelease is refused',
           acceptable(requirement('0.6.0-beta.1'), (0, 7, 0),
                      allow_older=False), False)
    expect('current-line prerelease passes',
           acceptable(requirement('0.7.0-rc.1'), (0, 7, 0),
                      allow_older=False), True)
    # A two-component prerelease matched neither branch before `core` was
    # applied on both sides, and fell out as an unjudgeable None.
    expect('two-component prerelease is judged',
           acceptable(requirement('0.6-beta'), (0, 7, 0), allow_older=False),
           False)
    expect('prerelease pin is read',
           list(pins(fenced('autumn-web = "0.6.0-beta.1"'))),
           [(2, 'autumn-web', '0.6.0-beta.1')])

    # ---- a waiver separated by MORE than one blank line ----
    # A blank line between a fence and the comment under it is ordinary
    # markdown, so counting each blank as its own separator made the obvious
    # spelling silently do nothing.
    two_blanks = ('autumn-web = "0.6"\n'
                  '\n'
                  '\n'
                  '<!-- version-pin-allow: autumn-web = "0.6" — reason -->\n')
    covered = waived(two_blanks)
    expect('waiver reaches across a blank RUN',
           1 in covered.get(('autumn-web', '0.6'), set()), True)
    # One blank line must still work, and a passage two blocks up must not be
    # swept in.
    one_blank = ('autumn-web = "0.5"\n'
                 '\n'
                 'autumn-web = "0.6"\n'
                 '\n'
                 '<!-- version-pin-allow: autumn-web = "0.6" — reason -->\n')
    covered = waived(one_blank)
    expect('waiver covers the adjacent block',
           3 in covered.get(('autumn-web', '0.6'), set()), True)
    expect('waiver does not reach two blocks up',
           1 in covered.get(('autumn-web', '0.6'), set()), False)

    # ---- an autumn-PREFIXED alias carrying a rename ----
    # `PIN` stops at the underscore and cannot match this key, and skipping
    # every `autumn`-prefixed key made the declaration vanish from both paths.
    expect('underscore alias with a rename',
           list(pins('autumn_web = { package = "autumn-web", '
                     'version = "0.5" }')),
           [(1, 'autumn-web', '0.5')])
    # A key that IS the crate stays with `PIN`, reported once.
    expect('self-named key is not double-read',
           list(pins('autumn-web = { package = "autumn-web", '
                     'version = "0.5" }')),
           [(1, 'autumn-web', '0.5')])

    # ---- quoted dependency keys, which TOML allows and the parser accepts ----
    expect('quoted key on the pattern path',
           list(pins('"autumn-web" = "0.5"')), [(1, 'autumn-web', '0.5')])
    expect('quoted subtable key',
           list(pins('[dependencies."autumn-web"]\nversion = "0.5"\n')),
           [(2, 'autumn-web', '0.5')])

    # ---- target-specific subtables on the fallback path ----
    # `docs/guide/edge.md` and `autumn-edge/README.md` both pin `autumn-web`
    # under a `[target.'cfg(…)'.dependencies]` table, so this is a shape the
    # corpus really uses; it reaches the parser today only because those
    # fences parse. The fallback has to know the same prefixes.
    expect('target subtable, cfg predicate',
           list(pins('[target.\'cfg(not(target_arch = "wasm32"))\''
                     '.dependencies.autumn-web]\nversion = "0.5"\n')),
           [(2, 'autumn-web', '0.5')])
    expect('target subtable, plain triple',
           list(pins('[target.x86_64-pc-windows-msvc.dependencies.autumn-web]'
                     '\nversion = "0.5"\n')),
           [(2, 'autumn-web', '0.5')])
    expect('target dev-dependencies subtable',
           list(pins('[target."cfg(unix)".dev-dependencies.autumn-web]'
                     '\nversion = "0.5"\n')),
           [(2, 'autumn-web', '0.5')])
    # The prefix is optional, not required: a plain subtable still works.
    expect('plain subtable still works',
           list(pins('[dependencies.autumn-web]\nversion = "0.5"\n')),
           [(2, 'autumn-web', '0.5')])

    # ---- an alias renamed AWAY from the crate its key names ----
    # The only FALSE-POSITIVE class found in review: the key is a local alias,
    # not the crate, so reporting it calls a correct page wrong.
    expect('alias renamed to a foreign crate is not a pin',
           list(pins('autumn-edge = { package = "axum", version = "0.5" }')),
           [])
    # Renamed to a DIFFERENT autumn crate: reported ONCE, against the real
    # crate. Reporting the alias too made one declaration into two defects.
    expect('alias renamed to another autumn crate reports once',
           list(pins('autumn-web = { package = "autumn-edge", '
                     'version = "0.5" }')),
           [(1, 'autumn-edge', '0.5')])
    # A rename that agrees with its key is still an ordinary pin.
    expect('rename agreeing with the key still reports',
           list(pins('autumn-web = { package = "autumn-web", '
                     'version = "0.5" }')),
           [(1, 'autumn-web', '0.5')])

    # ---- a COMMENTED version must not shadow the live one ----
    # `tomllib` rejects a newline directly inside the braces, so this reaches
    # the fallback; reading the comment reported a false PASS on a stale pin.
    expect('comment does not shadow the live version',
           list(pins(fenced('[dependencies]', 'autumn-web = {',
                            '    # version = "0.7"',
                            '    version = "0.5", features = ["db"]', '}'))),
           [(3, 'autumn-web', '0.5')])
    expect('commented-out package rename is ignored',
           list(pins('web = {\n  # package = "autumn-web"\n'
                     '  version = "0.5"\n}')),
           [])
    # A table whose ONLY version is commented out pins nothing.
    expect('commented-out version leaves no pin',
           list(pins(fenced('[dependencies]', 'autumn-web = {',
                            '  # version = "0.5"',
                            '  path = "../autumn"', '}'))),
           [])
    # A `#` INSIDE a string is data, not a comment.
    expect('hash inside a string is not a comment',
           list(pins('autumn-web = {\n  features = ["a#b"],\n'
                     '  version = "0.5"\n}')),
           [(1, 'autumn-web', '0.5')])
    expect('comment stripper preserves length',
           len(without_toml_comments('a = 1 # note\nb = 2\n')),
           len('a = 1 # note\nb = 2\n'))
    # A WHOLE declaration commented out, in a fence that does not parse. The
    # live pin beside it is still read; the commented one is not a pin at all.
    expect('commented-out declaration is not a pin',
           list(pins(fenced('[dependencies]', '# autumn-web = "0.5"',
                            'autumn-web = "0.7"', 'oops ='))),
           [(4, 'autumn-web', '0.7')])
    # …and a commented subtable header does not open a section either.
    expect('commented subtable header is inert',
           list(pins(fenced('# [dependencies.autumn-web]', '# version = "0.5"',
                            'oops ='))),
           [])

    # `publish` may be INHERITED from `[workspace.package]`. A member deferring
    # with `publish.workspace = true` must resolve to the workspace's value,
    # not be read as a table and counted as published — which would fail an old
    # pin on a crate that has no published release line at all.
    expect('inherited publish = false excludes',
           _publish_state({'workspace': True}, inherited=False), 'excluded')
    expect('inherited private registry excludes',
           _publish_state({'workspace': True}, inherited=['private']),
           'excluded')
    expect('inherited crates-io includes',
           _publish_state({'workspace': True}, inherited=['crates-io']),
           'included')
    expect('inherited absent includes',
           _publish_state({'workspace': True}, inherited=None), 'included')
    expect('a direct publish = false still excludes',
           _publish_state(False, inherited=None), 'excluded')
    # An `autumn*` key is PIN's; it must not be reported by both readers.
    expect('autumn key is not double-read',
           list(pins('autumn-web = { version = "0.5" }')),
           [(1, 'autumn-web', '0.5')])

    # Literal strings on the PATTERN path, which is prose AND the fallback for
    # a fence that does not parse — where the parser-side fix cannot reach.
    expect('literal string in prose',
           list(pins("add `autumn-web = '0.5'` to it")),
           [(1, 'autumn-web', '0.5')])
    expect('literal string in an unparseable fence',
           list(pins(fenced('[dependencies]', "autumn-web = '0.5'", 'oops ='))),
           [(3, 'autumn-web', '0.5')])
    expect('literal string in a subtable',
           list(pins("[dependencies.autumn-web]\nversion = '0.5'\n")),
           [(2, 'autumn-web', '0.5')])

    # THE INVARIANT behind all of the above: `check()` reads a None verdict as
    # "not this gate's business" and passes it, so anything `requirement()`
    # admits must be something `acceptable()` can actually judge. A `0.5.*`
    # broke this and passed silently; this asserts the class cannot return.
    admitted = ['0.7', '0.7.0', '=0.6.0', '^0.7', '~0.7', '0.5.*', '=0.5.*',
                '0.5', '10.20.30', ' 0.7 ']
    for spec in admitted:
        got = requirement(spec)
        if got is None:
            continue
        if acceptable(got, (0, 7, 0), allow_older=False) is None:
            failures.append(f'requirement({spec!r}) -> {got!r}, which '
                            f'acceptable() cannot judge')
    # A longer crate name must not be read as a pin on a shorter one.
    expect('no prefix bleed', list(pins('bench-autumn-web = "0.1"')), [])
    # A comment renders as nothing, so it carries no pin a reader can paste.
    expect('comment blanked',
           list(pins('<!-- autumn-web = "0.5" -->')), [])

    # Waivers.
    page = ('The panic reads:\n'
            '\n'
            '    pin the framework: autumn-web = "0.6"\n'
            '\n'
            '<!-- version-pin-allow: autumn-web = "0.6" — reproduced panic\n'
            '     text, not a pin the reader adds -->\n'
            '\n'
            'Elsewhere autumn-web = "0.6" is simply stale.\n')
    covered = waived(page)
    expect('waiver keyed by pin', sorted(covered), [('autumn-web', '0.6')])
    expect('waiver covers the passage', 3 in covered[('autumn-web', '0.6')],
           True)
    expect('waiver does not cover the whole page',
           8 in covered[('autumn-web', '0.6')], False)
    expect('waiver needs a reason',
           waived('<!-- version-pin-allow: autumn-web = "0.6" -->'), {})

    for failure in failures:
        print(f'FAIL {failure}')
    print(f'self-test: {len(failures)} failure(s)')
    return 1 if failures else 0


def describe_surface(published, workspace):
    """The `surface:` line, readable even when the pin is not a version.

    `series()` needs two numeric components and raises without them, and this
    line was formatted BEFORE the problems were printed — so a README pinning
    `garbage` produced a traceback instead of the targeted "pins a malformed
    autumn-cli version" message `check()` had already prepared. The gate's own
    diagnostic broke on precisely the input it exists to report, which is the
    worst moment to lose it.
    """
    try:
        line = series(published)
    except ValueError:
        line = f'unreadable ({published!r})'
    return (f'surface: published release line {line} '
            f'(README quickstart), workspace {workspace}')


def main():
    problems, checked, waived_count = check(ROOT)
    pages = len(corpus(ROOT))
    print(f'corpus: {pages} reader-facing markdown files')
    print(describe_surface(published_version(ROOT), workspace_version(ROOT)))
    print(f'checked: {checked} `autumn-* = "…"` pins')
    print()
    if problems:
        print(f'defects: {len(problems)}'
              + (f' ({waived_count} waived)' if waived_count else ''))
        for problem in problems:
            print(f'  {problem}')
        return 1
    print(f'defects: 0' + (f' ({waived_count} waived)' if waived_count else ''))
    print('Release-line pin gate OK.')
    return 0


sys.exit(self_test() if MODE == '--self-test'
         else print_corpus() if MODE == '--corpus'
         else list_pins(ROOT) if MODE == '--list'
         else main())
PYEOF

run_py() {
  python3 -c "$PYSRC" "$@"
}

mode="${1:-}"
case "$mode" in
  --self-test) run_py --self-test "$root" ;;
  --list)      run_py --list "$root" ;;
  --corpus)    run_py --corpus "$root" ;;
  "")
    echo "Checking autumn release-line pins across the reader-facing docs..."
    if ! run_py --check "$root"; then
      cat <<'EOF'

A pin naming an old release line is not a cosmetic staleness. Cargo resolves it
literally, and because `autumn-web` carries a `links = "sqlite3"` dependency,
two lines of it in one graph fail as a NATIVE LIBRARY conflict that names
neither `autumn-web` nor this page — so the reader debugs their sqlite setup
instead of the documentation.

Fix each one where it lives:
  - an install or dependency snippet -> bump it to the published line
  - a migration guide's before-block  -> it is already allowed to pin its own
                        line or older; a pin ABOVE the page's own version means
                        the page is describing a release it predates
  - a pin the page must SHOW rather than offer (reproduced panic or error text)
                     -> waive it beside the passage, with the reason:

      <!-- version-pin-allow: autumn-web = "0.6" — inside the reproduced
           plugin-contract panic text, not a pin the reader adds -->

The published line comes from README.md's quickstart
(`cargo install autumn-cli --version <x.y.z>`), so a release bump moves this
gate's expectation by editing that one line.

Inspect what the gate read:  scripts/check-docs-versions.sh --list
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--list|--corpus|--self-test]" >&2
    exit 2
    ;;
esac
