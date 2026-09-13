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
# is 212 pages and carries 57 pins. The other 204 pages were ungated, and the
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
    manifest = pathlib.Path(root, 'Cargo.toml').read_text(encoding='utf-8')
    section = re.search(r'(?ms)^\[workspace\.package\]\n(.*?)(?=^\[|\Z)', manifest)
    if section:
        found = re.search(r'(?m)^\s*version\s*=\s*"([^"]+)"', section.group(1))
        if found:
            return found.group(1)
    raise SystemExit('Cargo.toml must set [workspace.package] version')


def triple(text):
    """`x.y.z` as a tuple, or None. Anything else is not a release version."""
    parts = text.split('.')
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        return None
    return tuple(int(p) for p in parts)


def series(version):
    """`0.7.0` -> `0.7`. The release LINE, which is what a pin usually names."""
    major, minor = version.split('.')[:2]
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
    """
    listing = subprocess.run(
        ['git', 'ls-files', '-z', 'Cargo.toml', '*/Cargo.toml'],
        cwd=root, capture_output=True, text=True, check=True).stdout
    out = set()
    for rel in (f for f in listing.split('\0') if f):
        text = pathlib.Path(root, rel).read_text(encoding='utf-8')
        package = re.search(r'(?ms)^\[package\]\n(.*?)(?=^\[|\Z)', text)
        if not package:
            continue
        name = re.search(r'(?m)^\s*name\s*=\s*"([^"]+)"', package.group(1))
        if not name or not name.group(1).startswith('autumn'):
            continue
        # `publish = false` keeps a member off crates.io. Absent means published,
        # which is Cargo's own default.
        if re.search(r'(?m)^\s*publish\s*=\s*false', package.group(1)):
            continue
        out.add(name.group(1))
    return out


# ---------------------------------------------------------------------------
# Reading pins out of a page
# ---------------------------------------------------------------------------

# `autumn-web = "0.7"` and `autumn-web = { version = "0.7", features = [...] }`,
# in a fence or in the inline-code spelling prose uses. The crate name is
# anchored at a word boundary so `bench-autumn-web = "…"` is not read as a pin
# on `autumn-web`.
PIN = re.compile(
    r'(?<![\w.-])(autumn[a-z0-9-]*)\s*=\s*(?:"([0-9][^"]*)"|\{([^}]*)\})')

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
    # Block index per line, where a run of blank lines separates blocks.
    block_of = []
    block = 0
    for line in lines:
        if line.strip():
            block_of.append(block)
        else:
            block_of.append(None)
            block += 1
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


def pins(text):
    """Yield (line_no, crate, version) for every pin a reader can see."""
    for lineno, line in enumerate(blank_comments(text).splitlines(), 1):
        for match in PIN.finditer(line):
            crate = match.group(1)
            version = match.group(2)
            if version is None:
                inner = re.search(r'version\s*=\s*"([^"]+)"', match.group(3) or '')
                if not inner:
                    # `{ path = "../autumn" }` or `{ workspace = true }` pins no
                    # version, so there is nothing here to be stale.
                    continue
                version = inner.group(1)
            yield lineno, crate, version


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
    parts = version.split('.')
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
        for lineno, crate, version in pins(text):
            if crate not in crates:
                continue
            verdict = acceptable(version, cap,
                                 allow_older=bool(MIGRATION_PAGE.match(path)))
            if verdict is None or verdict:
                checked += 1
                continue
            if lineno in allowed.get((crate, version), ()):  
                waived_count += 1
                continue
            checked += 1
            want = series(published) if not MIGRATION_PAGE.match(path) else \
                series('.'.join(str(p) for p in cap))
            problems.append(
                f'{path}:{lineno}  {crate} = "{version}"  '
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


def main():
    problems, checked, waived_count = check(ROOT)
    pages = len(corpus(ROOT))
    print(f'corpus: {pages} reader-facing markdown files')
    print(f'surface: published release line {series(published_version(ROOT))} '
          f'(README quickstart), workspace {workspace_version(ROOT)}')
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
