#!/usr/bin/env bash
# Feature-gate drift gate: every reader-facing page that shows Rust reaching for
# an `autumn_web` item that only exists behind a NON-DEFAULT Cargo feature must
# name that feature on the page.
#
# WHY THIS EXISTS: the corpus already gates the seven things a reader copies off
# a page and the one thing they cannot copy at all.
# `scripts/check-docs-links.sh` gates its *links* (a 404 on GitHub),
# `scripts/check-docs-cli.sh` its *commands* (`unrecognized subcommand`),
# `scripts/check-docs-config.sh` the `AUTUMN_*` variables they SET (a silent
# no-op), `scripts/check-docs-toml.sh` the `autumn.toml` keys they WRITE
# (dropped silently), `scripts/check-docs-symbols.sh` the `autumn_web::…` paths
# they IMPORT (E0432 against their own file), `scripts/check-docs-routes.sh`
# the `/actuator/…` URLs they CURL, `scripts/check-docs-macro-args.sh` the
# keyword arguments they put inside an attribute macro,
# `scripts/check-docs-versions.sh` the dependency pin every one of those is
# relative to, and `scripts/check-docs-orphans.sh` asserts the page can be
# reached at all.
#
# Every one of them checks the page against a crate built with EVERY feature on.
# That is deliberate, and `check-docs-symbols.sh` says so in as many words:
#
#     Feature gates. The surface is read as a superset with every `#[cfg]`
#     ignored, so a path that only exists under `--features ws` still resolves.
#     Gating on the default feature set would report an item a reader can
#     absolutely use as missing; that direction of error is not worth trading
#     for […]
#
# That reasoning is right, and it leaves a hole exactly the shape of its own
# premise. The symbol gate proves `autumn_web::ws::WebSocket` EXISTS. It cannot
# ask the only question the reader has, which is whether it exists **in their
# build** — and `autumn-web`'s default feature set is eight features wide
# (`maud`, `htmx`, `tailwind`, `db`, `cache-moka`, `http-client`, `reporting`,
# `flash`) out of fifty. `ws`, `mail`, `pdf`, `storage`, `i18n`, `presence`,
# `markdown`, `mcp`, `seed`, `tls` and the rest are off unless the reader turns
# them on, and nothing in the corpus was required to tell them so.
#
# So a page could open on:
#
#     use autumn_web::pdf::Pdf;
#
#     #[get("/invoices/{id}/pdf")]
#     async fn invoice_pdf(id: Path<i64>) -> Pdf { … }
#
# — every path resolving, every macro argument valid, every link live — and the
# reader who pastes it into the project the quickstart just scaffolded gets
#
#     error[E0433]: failed to resolve: could not find `pdf` in `autumn_web`
#
# with nothing anywhere naming the word `pdf` as a *feature*. This is the same
# failure class as the env gate's, one layer earlier: the compiler's message is
# about their file, the fix is a line in a file the page never showed them, and
# the page reads as correct to its author because the author's checkout has the
# feature on. `docs/guide/pdf-downloads.md` was in exactly that state at the
# baseline, and made the reason legible by pointing AT the missing line:
# "requires the `maud` feature; enabled together with `pdf` in the quick start
# above" — where the quick start above contains no `Cargo.toml` at all.
#
# THE BASELINE RUN found nine such page/feature pairs across eight pages, out
# of 78 gated uses in the corpus:
#
#   docs/guide/cloud-native.md:929        `#[ws]`                  -> ws
#   docs/guide/daemon.md:194              `autumn_web::managed_pg` -> managed-pg
#   docs/guide/macro-transparency.md:295  `#[ws]`                  -> ws
#   docs/guide/macro-transparency.md:1646 `#[mailer]`              -> mail
#   docs/guide/macro-transparency.md:1684 `#[inbound_mail]`        -> inbound-mail
#   docs/guide/mail-compliance.md:54      `#[mailer]`              -> mail
#   docs/guide/pdf-downloads.md:24        `autumn_web::pdf`        -> pdf
#   docs/guide/testing.md:739             `autumn_web::storage`    -> storage
#   docs/migrations/next.md:136           `autumn_web::tls`        -> tls
#
# Three of them sit under a literal "**You write:**" heading. None is a page
# about an obscure corner: `cloud-native.md` is the deployment guide, and its
# `#[ws]` block is the WebSocket *drain contract* — read by someone wiring up a
# rolling deploy, which is the worst moment to discover a missing feature.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Inside a ```rust fence, a path `autumn_web::<item>` whose first segment
#      is a top-level item of `autumn-web` carrying a `#[cfg(feature = "…")]`
#      for a feature OUTSIDE the default closure.
#   2. Inside a ```rust fence, an attribute `#[<macro>]` naming an attribute
#      macro re-exported under such a `#[cfg]` — `#[ws]`, `#[mailer]`,
#      `#[mailer_preview]`, `#[mail_previews]`, `#[inbound_mail]`,
#      `#[wire_client]`. An attribute is the form the prelude is USED in, so
#      leaving it out would exempt the most-copied construct in the guide.
#   3. The page then has to NAME that feature, in a spelling a reader can act
#      on: a `features = [ … "ws" … ]` array (newlines and comments inside it
#      are fine — the corpus writes them that way), a `--features ws`
#      invocation, the prose forms `` `ws` feature ``/`` `ws` Cargo feature ``/
#      `` feature `ws` ``/`` feature flag `i18n` ``, or a `[features]` table
#      row `ws = [`. All five spellings are live in the corpus today.
#
# NAMING, NOT PLACEMENT. The rule is that the feature is named SOMEWHERE on the
# page, not that it is named before the first fence that needs it. Placement is
# the sharper reader question — three pages name the feature only AFTER the code
# that needs it, worst of them `docs/guide/tauri-mobile-offline-sync.md`, where
# the `offline-sync` line sits 221 lines below the first fence that needs it —
# and it is deliberately NOT gated. A page whose enabling line lives in a
# "Prerequisites" section further down is a legitimate shape, and a gate that
# ordered a restructure of it would trade a reader defect for an author fight.
# Presence is the half that is unarguable: without it there is no line to find
# at all, at any distance. The ordering count is printed on every run and
# itemised by `--list`, so the next pass has it measured rather than felt.
#
# TRUTH SET: parsed from `autumn/src/lib.rs`, `autumn/src/prelude.rs` and
# `autumn/Cargo.toml`, not from a checked-in snapshot — for the reason
# `check-docs-cli.sh` gives: a snapshot is one forgotten regeneration away from
# gating the docs against a crate that no longer exists, which is the very
# failure this script is for. Move an item behind a new feature and the gate
# moves with it in the same commit.
#
#   - The default closure is computed TRANSITIVELY from `[features]`, because
#     `default` names eight features and those name more (`db` implies
#     `autumn-macros/db`, `oauth2` implies `http-client`). Taking the literal
#     `default = [...]` list instead would report `reporting`'s items — pulled
#     in by nothing else — as needing to be enabled, on every page that shows a
#     failure capsule.
#   - Only COLUMN-ZERO declarations count. `lib.rs` carries five inline
#     `pub mod … {` blocks (`include_dir`, `__fuzz`, `__private`, `reexports`,
#     `tests`), and 21 of its `#[cfg(feature = …)] pub use` lines are indented
#     inside them. Those items are not `autumn_web::<item>` — they are
#     `autumn_web::__fuzz::<item>` — and reading them as top-level would put
#     `extract_path_params` (openapi) and four `plugin-sandbox` fuzz seams in
#     the truth set under names nothing documents. The workspace runs
#     `cargo fmt --all`, so top-level is column zero.
#   - Only the exact `#[cfg(feature = "…")]` form is read. `#[cfg(all(…))]` and
#     `#[cfg(any(…))]` leave the item UNGATED here, which under-reports rather
#     than over-reports: a page is never failed for a feature the gate guessed
#     at. `autumn-web` has no such form on a top-level item today.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - Bare identifiers. A fence that writes `use autumn_web::prelude::*;` and
#     then `Mailer` names no path, and the prelude's gated surface is 174 items
#     wide with names like `Format`, `Column`, `Link`, `Client`, `Patch`,
#     `Lock`, `Story` and `Transport`. Matching those as words would report
#     most of the guide. `check-docs-symbols.sh` draws the line in the same
#     place and for the same reason.
#   - Bang macros. `t!("key")` is the `i18n` feature's whole call surface, and
#     `\bt!\(` is one character long: `assert!(`, `insert!(`, `expect!(` and
#     `vec!(` all end in it, and requiring a word boundary still leaves a
#     one-letter token this gate would have to be right about on 160 pages.
#     Names first, and a one-letter name is not one.
#   - Anything past the first path segment. `autumn_web::db::replica` is judged
#     on `db` (default, so not judged at all); a feature gating a nested module
#     is invisible here. Same boundary as the symbol gate's.
#   - Non-`rust` fences and prose. A feature gate is a COMPILE failure, so the
#     only place it can bite is a block a reader compiles. `autumn_web::pdf::Pdf`
#     in a README's capability table is a description of the example, not a
#     line anyone pastes, and gating those made the root `README.md` and
#     `EXAMPLES.md` catalogs report against every feature they mention.
#   - Crates other than `autumn-web`. `autumn-cli`, `autumn-macros` and the
#     plugin crates carry their own features; none of them is reached through
#     an `autumn_web::` path, and the macros a reader writes are re-exported
#     THROUGH `autumn-web`, which is where this reads their gate from.
#
# THE CORPUS IS THE SIBLINGS'. The `INCLUDE_DIRS`/`INCLUDE_FILES`/
# `package_readmes` block below is copied verbatim from
# `check-docs-versions.sh`, and this gate is registered in
# `scripts/check-docs-scope.sh`'s `SIBLINGS` in the same commit. #2709 exists
# because four gates each spelled their own corpus and three of them drifted; a
# sixth gate spelling a sixth copy, unwatched, is how that recurs.
#
# WAIVERS. A page that must SHOW a gated construct rather than offer it — a
# comparison table, a migration note quoting an old snippet — waives it beside
# the passage, with a reason:
#
#     <!-- feature-gate-allow: ws — quoted from the 0.5 release notes, not a
#          snippet this page offers -->
#
# A waiver covers its own blank-line-separated block and the one above it, the
# same scope `check-docs-routes.sh` and `check-docs-versions.sh` give theirs: a
# page-wide waiver silently re-admits the defect the gate exists to catch. The
# baseline needed none — all eight defects were real, including the three under
# "**You write:**" — so the mechanism ships unexercised by the corpus and
# exercised by `--self-test`.
#
# Run locally with:
#
#     ./scripts/check-docs-features.sh
#     ./scripts/check-docs-features.sh --list        # every gated use it read
#     ./scripts/check-docs-features.sh --surface     # the truth set
#     ./scripts/check-docs-features.sh --corpus      # the pages it reads
#     ./scripts/check-docs-features.sh --self-test   # the extractor's own tests

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
# The truth set: which autumn-web items are behind a non-default feature
# ---------------------------------------------------------------------------

MANIFEST = 'autumn/Cargo.toml'
# `prelude.rs` is read alongside `lib.rs` because it re-exports a DIFFERENT set
# under the same gates — `Locale` (i18n), `Multipart` (multipart) and the whole
# `maud` widget surface appear only there. Both files declare at column zero.
SOURCES = ('autumn/src/lib.rs', 'autumn/src/prelude.rs')

CFG_FEATURE = re.compile(r'^#\[cfg\(feature = "([a-z0-9_-]+)"\)\]$')
MOD_DECL = re.compile(r'^(?:pub(?:\([^)]*\))? )?mod ([a-z_0-9]+)\s*[;{]')
USE_ONE_LINE = re.compile(r'^pub use ([a-z_0-9:]+)::\{?([^;{}]+?)\}?;')
USE_BRACE_OPEN = re.compile(r'^pub use ([a-z_0-9:]+)::\{$')
# `pub use autumn_macros::foo;` is the only source of an ATTRIBUTE a reader
# writes. Anything re-exported from a `crate::…` path is a type or a function,
# reachable only as a path or (after a prelude glob) as a bare name.
MACRO_CRATE = 'autumn_macros'


def default_features(root):
    """The default feature set, closed over what those features imply.

    `[features] default = [...]` names eight, and those name more. Reading the
    literal list would leave `reporting`'s items looking optional on every page
    that shows a failure capsule, which is a gate reporting correct pages as
    broken — the one error direction the docs gates refuse to trade for.

    A `dep:foo` entry activates an optional dependency and a `foo/bar` entry
    activates a feature of another crate; neither names a feature of this one,
    so both are dropped before the closure is walked.
    """
    text = (pathlib.Path(root) / MANIFEST).read_text(encoding='utf-8')
    data = tomllib.loads(text)
    table = data.get('features')
    if not isinstance(table, dict) or 'default' not in table:
        sys.exit(
            f'FAIL: {MANIFEST} has no `[features] default = [...]`. The '
            f'manifest was restructured; this gate cannot tell a default '
            f'feature from an optional one and would report every page. Fix '
            f'default_features() in scripts/check-docs-features.sh.')

    def edges(name):
        return [d for d in table.get(name, [])
                if '/' not in d and not d.startswith('dep:')]

    closure = set()
    stack = list(edges('default'))
    while stack:
        feature = stack.pop()
        if feature in closure:
            continue
        closure.add(feature)
        stack.extend(edges(feature))
    return closure, set(table) - {'default'}


def gated_items(root):
    """Map every column-zero item behind a `#[cfg(feature = …)]` to its feature.

    Returns `{name: (feature, kinds)}` where `kinds` is a set drawn from
    `module`, `macro` (an attribute a reader writes) and `item` (a type or
    function). A name can be SEVERAL of those at once and `ws` is: `lib.rs`
    declares `pub mod ws;` and re-exports `autumn_macros::ws` under the same
    gate, so `autumn_web::ws::WebSocket` and `#[ws("/echo")]` are both real.
    Keeping only the first reading made `#[ws]` — the single most-copied gated
    construct in the guide — invisible to this gate, which its self-test now
    holds it to. A name's FEATURE is taken from its first declaration; `lib.rs`
    and `prelude.rs` never disagree about one.

    Column zero is the whole nesting model, and it is enough because the
    workspace runs `cargo fmt --all`. See the header for the five inline
    `pub mod … {` blocks this keeps out.
    """
    found = {}
    for rel in SOURCES:
        lines = (pathlib.Path(root) / rel).read_text(
            encoding='utf-8').splitlines()
        pending = None
        index = 0
        while index < len(lines):
            line = lines[index]
            index += 1
            # Indented or blank: inside something, or between things. Neither
            # can carry a top-level declaration, and neither cancels a pending
            # `#[cfg]` — rustfmt puts no blank line between an attribute and
            # the item it is on, but a doc comment may sit between them.
            if not line or line[:1].isspace():
                continue
            match = CFG_FEATURE.match(line)
            if match:
                pending = match.group(1)
                continue
            # Doc comments and further attributes sit between the `#[cfg]` and
            # the item; they carry the gate forward rather than clearing it.
            if line.startswith(('///', '//!', '//', '#[')):
                continue
            match = MOD_DECL.match(line)
            if match:
                if pending:
                    _record(found, match.group(1), pending, 'module')
                pending = None
                continue
            match = USE_ONE_LINE.match(line)
            if match:
                if pending:
                    kind = 'macro' if match.group(1) == MACRO_CRATE else 'item'
                    for name in _names(match.group(2)):
                        _record(found, name, pending, kind)
                pending = None
                continue
            match = USE_BRACE_OPEN.match(line)
            if match:
                body = []
                while index < len(lines) and not lines[index].startswith('};'):
                    body.append(lines[index])
                    index += 1
                index += 1
                if pending:
                    kind = 'macro' if match.group(1) == MACRO_CRATE else 'item'
                    for name in _names('\n'.join(body)):
                        _record(found, name, pending, kind)
                pending = None
                continue
            pending = None
    if not found:
        sys.exit(
            'FAIL: no `#[cfg(feature = "…")]` declarations found at column '
            'zero in ' + ', '.join(SOURCES) + '. The crate root was '
            'restructured; this gate has no truth set to read and would pass '
            'everything. Fix gated_items() in '
            'scripts/check-docs-features.sh.')
    return found


def _record(found, name, feature, kind):
    """Add one declaration, keeping every kind a name is declared under."""
    if name in found:
        found[name][1].add(kind)
    else:
        found[name] = (feature, {kind})


def _names(blob):
    """The identifiers in a `pub use` list, after `as` renames and comments.

    `pub use crate::x::{a, b as c};` exports `a` and `c` — the name a reader
    writes is the one AFTER `as`, and taking the one before it put four
    `__fuzz_*` internals in the truth set under names nothing documents.
    """
    out = []
    for piece in re.split(r'[,\n]', blob):
        piece = piece.split('//')[0].strip()
        if not piece:
            continue
        piece = piece.split(' as ')[-1].strip()
        if re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*', piece):
            out.append(piece)
    return out


def surface(root):
    """`{name: (feature, kind)}` for the items a DEFAULT build does not have."""
    closure, declared = default_features(root)
    gated = gated_items(root)
    unknown = sorted({f for f, _ in gated.values()} - declared)
    if unknown:
        sys.exit(
            f'FAIL: {", ".join(unknown)} is `#[cfg(feature = …)]`-gated in the '
            f'crate root but is not a feature in {MANIFEST}. Either the '
            f'manifest lost it (the cfg is then dead and the item ships to '
            f'nobody) or this gate mis-parsed one of them. Inspect with '
            f'--surface.')
    return {name: (feature, kinds) for name, (feature, kinds) in gated.items()
            if feature not in closure}


# ---------------------------------------------------------------------------
# What a page shows, and whether it names the feature
# ---------------------------------------------------------------------------

FENCE = re.compile(r'^\s*```([A-Za-z0-9_+-]*)')
# The languages a reader compiles. `rs` and `rust,no_run` are both live in the
# corpus; an info string carries the language up to the first comma or space.
RUST_LANGS = ('rust', 'rs')
PATH_USE = re.compile(r'\bautumn_web::([A-Za-z_][A-Za-z_0-9]*)')
ATTR_USE = re.compile(r'#\[([a-z_][a-z_0-9]*)')
HTML_COMMENT = re.compile(r'<!--.*?-->', re.S)
WAIVER = re.compile(
    r'<!--\s*feature-gate-allow:\s*([a-z0-9_-]+)\s*(?:—|:)\s*(\S[^>]*?)-->',
    re.S)


def blank_comments(text):
    """Replace HTML comment bodies with spaces, preserving every line number.

    A waiver names the feature it waives, so a waiver comment read as page text
    would satisfy the naming rule it exists to bypass — and so would a `<!--
    features = ["ws"] -->` note left behind by an edit. A comment renders as
    nothing, so it offers the reader nothing.
    """
    return HTML_COMMENT.sub(
        lambda m: re.sub(r'[^\n]', ' ', m.group(0)), text)


def rust_fences(text):
    """Yield (line_no, line) for every line inside a ```rust fence.

    Fence tracking is a two-state toggle on the info string, which is how
    `check-docs-macro-args.sh` reads the same corpus: a closing ``` carries no
    language, so the language is remembered from the opener. An unbalanced
    fence therefore ends at the next fence rather than swallowing the page.
    """
    lang = None
    for lineno, line in enumerate(text.splitlines(), 1):
        match = FENCE.match(line)
        if match:
            if lang is None:
                info = match.group(1).lower()
                lang = info.split(',')[0]
            else:
                lang = None
            continue
        if lang in RUST_LANGS:
            yield lineno, line


def uses(text, gated):
    """Yield (line_no, feature, shown) for each gated construct in a rust fence.

    `shown` is what the reader sees, so the report can say `#[ws]` rather than
    `ws` — the difference between a line they can find on the page and a name
    they have to go looking for.
    """
    for lineno, line in rust_fences(blank_comments(text)):
        for match in PATH_USE.finditer(line):
            name = match.group(1)
            if name in gated:
                yield lineno, gated[name][0], f'autumn_web::{name}'
        for match in ATTR_USE.finditer(line):
            name = match.group(1)
            entry = gated.get(name)
            # An attribute is only judged against an attribute MACRO. A module
            # named `storage` is not `#[storage]`, and reading it as one would
            # have this gate guessing at a construct that does not exist.
            if entry and 'macro' in entry[1]:
                yield lineno, entry[0], f'#[{name}]'


def naming_patterns(feature):
    """The spellings that tell a reader how to turn `feature` on.

    All five are live in the corpus. They are kept TIGHT on purpose: an earlier
    version allowed up to 24 characters between the word "feature" and the
    backticked name, and `docs/guide/pdf-downloads.md` passed on "requires the
    `maud` feature; enabled together with `pdf` in the quick start above" —
    a sentence that names no enabling line, and points at a quick start that
    has none. It passed because the filler happened to be exactly 24
    characters long. A rule that a one-character edit flips is not a rule.
    """
    name = re.escape(feature)
    return (
        # `features = ["ws"]`, including the multi-line, commented spelling
        # `skills/autumn-web/SKILL.md` writes.
        rf'features\s*=\s*\[[^\]]*"{name}"',
        rf'--features[^\n]*(?:[",\s=]|^){name}(?:[",\s]|$)',
        rf'`{name}`(?:\s+Cargo)?\s+features?\b',
        rf'\bfeatures?\b(?:\s+flag)?\s+`{name}`',
        rf'^\s*{name}\s*=\s*\[',
    )


def names_feature(text, feature):
    return any(re.search(pattern, text, re.M)
               for pattern in naming_patterns(feature))


def first_naming_line(text, feature):
    """The first line that names `feature`, or None. Reported, never gated."""
    best = None
    for pattern in naming_patterns(feature):
        match = re.search(pattern, text, re.M)
        if match:
            line = text.count('\n', 0, match.start()) + 1
            best = line if best is None else min(best, line)
    return best


def waived_lines(text):
    """Map a waived feature to the line numbers its waivers cover.

    A waiver covers its own blank-line-separated block and the one directly
    above it — the passage it was written for. Anything further down the page
    is still reported, because a page-wide waiver silently re-admits the defect
    the gate exists to catch. Same scope as `check-docs-routes.sh`'s.
    """
    lines = text.splitlines()
    block_of = []
    block = 0
    prev_blank = True
    for line in lines:
        if not line.strip():
            prev_blank = True
            block_of.append(block)
            continue
        if prev_blank:
            block += 1
        prev_blank = False
        block_of.append(block)
    covered = {}
    for match in WAIVER.finditer(text):
        feature = match.group(1)
        lineno = text.count('\n', 0, match.start()) + 1
        scope = {block_of[lineno - 1], block_of[lineno - 1] - 1}
        covered.setdefault(feature, set()).update(
            n for n, b in enumerate(block_of, 1) if b in scope)
    return covered


def check(root):
    """(problems, checked, waived, late) over the whole corpus."""
    gated = surface(root)
    problems = []
    checked = 0
    waived = 0
    late = []
    for rel in corpus(root):
        text = (pathlib.Path(root) / rel).read_text(
            encoding='utf-8', errors='ignore')
        if 'autumn_web' not in text and '#[' not in text:
            continue
        covered = waived_lines(text)
        first = {}
        for lineno, feature, shown in uses(text, gated):
            checked += 1
            if lineno in covered.get(feature, ()):
                waived += 1
                continue
            first.setdefault(feature, (lineno, shown))
        for feature, (lineno, shown) in sorted(first.items()):
            named = first_naming_line(text, feature)
            if named is None:
                problems.append(
                    f'{rel}:{lineno}: {shown} needs the non-default '
                    f'`{feature}` feature, which this page never names')
            elif named > lineno:
                late.append((rel, feature, lineno, shown, named))
    return problems, checked, waived, late


# ---------------------------------------------------------------------------
# Modes
# ---------------------------------------------------------------------------


def print_corpus():
    for path in corpus(ROOT):
        print(path)
    return 0


def print_surface():
    closure, _ = default_features(ROOT)
    gated = surface(ROOT)
    print(f'default feature closure ({len(closure)}): '
          f'{", ".join(sorted(closure))}')
    print(f'items behind a non-default feature: {len(gated)}')
    by_feature = {}
    for name, (feature, kinds) in gated.items():
        by_feature.setdefault(feature, []).append(
            ('+'.join(sorted(kinds)), name))
    for feature in sorted(by_feature):
        print(f'  {feature}')
        for kinds, name in sorted(by_feature[feature]):
            print(f'    {kinds:13s} {name}')
    return 0


def list_uses():
    gated = surface(ROOT)
    total = 0
    for rel in corpus(ROOT):
        text = (pathlib.Path(ROOT) / rel).read_text(
            encoding='utf-8', errors='ignore')
        covered = waived_lines(text)
        rows = []
        for lineno, feature, shown in uses(text, gated):
            named = first_naming_line(text, feature)
            if named is None:
                verdict = 'NOT NAMED'
            elif named > lineno:
                verdict = f'named at {named} (+{named - lineno})'
            else:
                verdict = f'named at {named}'
            if lineno in covered.get(feature, ()):
                verdict += ' [waived]'
            rows.append(f'  {rel}:{lineno}: {shown} -> {feature} — {verdict}')
        total += len(rows)
        for row in rows:
            print(row)
    print(f'{total} gated use(s) in rust fences')
    return 0


def self_test():
    """Tests for the extractor, over inputs the corpus does and does not carry.

    Every one of these was a bug this gate had, or a shape the corpus writes
    that a plausible simpler rule gets wrong.
    """
    failures = []

    def expect(label, got, want):
        if got != want:
            failures.append(f'{label}: got {got!r}, want {want!r}')

    gated = {
        'ws': ('ws', {'module', 'macro'}),
        'channels': ('ws', {'module'}),
        'pdf': ('pdf', {'module'}),
        'storage': ('storage', {'module'}),
        'mailer': ('mail', {'macro'}),
    }

    def found(text):
        return sorted(uses(text, gated))

    # Only rust fences are read.
    expect('rust fence read',
           found('```rust\nuse autumn_web::pdf::Pdf;\n```\n'),
           [(2, 'pdf', 'autumn_web::pdf')])
    expect('rs alias read',
           found('```rs\nuse autumn_web::pdf::Pdf;\n```\n'),
           [(2, 'pdf', 'autumn_web::pdf')])
    expect('info string with attributes read',
           found('```rust,no_run\nuse autumn_web::pdf::Pdf;\n```\n'),
           [(2, 'pdf', 'autumn_web::pdf')])
    expect('toml fence not read',
           found('```toml\nautumn_web::pdf\n```\n'), [])
    expect('prose not read', found('See `autumn_web::pdf::Pdf` for this.\n'), [])
    expect('text after the fence closes not read',
           found('```rust\nlet x = 1;\n```\n\n`autumn_web::pdf`\n'), [])

    # Attributes are judged only against attribute macros.
    expect('attribute macro read',
           found('```rust\n#[ws("/echo")]\nasync fn echo() {}\n```\n'),
           [(2, 'ws', '#[ws]')])
    expect('module name is not an attribute',
           found('```rust\n#[storage]\nstruct S;\n```\n'), [])
    expect('unrelated attribute ignored',
           found('```rust\n#[derive(Debug)]\nstruct S;\n```\n'), [])

    # A comment renders as nothing, so it can neither show nor name.
    expect('comment blanked',
           found('```rust\n<!-- autumn_web::pdf -->\n```\n'), [])

    # The naming rule.
    for spelling in (
            'autumn-web = { version = "0.7", features = ["ws"] }',
            'features = [\n    "mail",  # email\n    "ws",\n]',
            'cargo build --features ws',
            'the `ws` feature',
            'the `ws` Cargo feature',
            'gated behind the feature `ws`',
            'behind the feature flag `ws`',
            'ws = ["dep:tokio-stream"]',
    ):
        if not names_feature(spelling, 'ws'):
            failures.append(f'naming missed: {spelling!r}')
    # The regression that motivated the tight rule: a sentence that mentions
    # the feature name near the word "feature" without offering a line.
    expect('loose mention rejected',
           names_feature(
               'requires the `maud` feature; enabled together with `pdf` in '
               'the quick start above', 'pdf'),
           False)
    expect('a longer feature name is not a shorter one',
           names_feature('features = ["ws-compat"]', 'ws'), False)
    expect('a comment naming the feature does not count',
           names_feature(blank_comments('<!-- features = ["ws"] -->'), 'ws'),
           False)

    # Waivers.
    page = ('```rust\n'
            '#[ws("/echo")]\n'
            '```\n'
            '\n'
            '<!-- feature-gate-allow: ws — quoted from the 0.5 notes, not a\n'
            '     snippet this page offers -->\n'
            '\n'
            '```rust\n'
            '#[ws("/other")]\n'
            '```\n')
    covered = waived_lines(page)
    expect('waiver keyed by feature', sorted(covered), ['ws'])
    expect('waiver covers the passage above it', 2 in covered['ws'], True)
    expect('waiver does not cover the whole page', 9 in covered['ws'], False)
    expect('waiver needs a reason',
           waived_lines('<!-- feature-gate-allow: ws -->'), {})

    # The truth set, against the real crate.
    real = surface(ROOT)
    closure, declared = default_features(ROOT)
    for feature in ('maud', 'htmx', 'tailwind', 'db', 'cache-moka',
                    'http-client', 'reporting', 'flash'):
        if feature not in closure:
            failures.append(f'default closure missing {feature}')
    # `autumn-macros/db` implies nothing here, but `db` reaches `reporting`'s
    # siblings only through the literal list; `oauth2 -> http-client` is the
    # transitive edge that proves the closure is walked rather than read.
    if 'http-client' not in closure:
        failures.append('closure did not walk `oauth2 -> http-client`')
    expect('a default feature is not gated surface',
           any(f == 'db' for f, _ in real.values()), False)
    for name, feature, kinds in (('ws', 'ws', {'module', 'macro'}),
                                 ('pdf', 'pdf', {'module'}),
                                 ('mailer', 'mail', {'macro'}),
                                 ('storage', 'storage', {'module'}),
                                 ('managed_pg', 'managed-pg', {'module'})):
        if real.get(name) != (feature, kinds):
            failures.append(
                f'truth set: {name} -> {real.get(name)!r}, want '
                f'{(feature, kinds)!r}')
    # Items inside `lib.rs`'s inline `pub mod … {` blocks are NOT top-level.
    for name in ('extract_path_params', 'parse_sandbox_manifest'):
        if name in real:
            failures.append(
                f'truth set: {name} is inside an inline module and must not '
                f'be read as a crate-root item')

    for failure in failures:
        print(f'FAIL {failure}')
    print(f'self-test: {len(failures)} failure(s)')
    return 1 if failures else 0


def main():
    problems, checked, waived, late = check(ROOT)
    gated = surface(ROOT)
    features = sorted({feature for feature, _ in gated.values()})
    print(f'corpus: {len(corpus(ROOT))} reader-facing markdown files')
    print(f'surface: {len(gated)} crate-root items behind '
          f'{len(features)} non-default features')
    print(f'checked: {checked} gated use(s) inside rust fences')
    print(f'ordering: {len(late)} page(s) name the feature only AFTER the '
          f'code that needs it (reported, not gated)')
    for rel, feature, lineno, shown, named in sorted(
            late, key=lambda row: row[4] - row[2], reverse=True)[:5]:
        print(f'  {rel}:{lineno}: {shown} -> `{feature}` named at '
              f'{named} (+{named - lineno})')
    print()
    suffix = f' ({waived} waived)' if waived else ''
    if problems:
        print(f'defects: {len(problems)}{suffix}')
        for problem in problems:
            print(f'  {problem}')
        return 1
    print(f'defects: 0{suffix}')
    print('Feature-gate documentation gate OK.')
    return 0


sys.exit(self_test() if MODE == '--self-test'
         else print_corpus() if MODE == '--corpus'
         else print_surface() if MODE == '--surface'
         else list_uses() if MODE == '--list'
         else main())
PYEOF

run_py() {
  python3 -c "$PYSRC" "$@"
}

mode="${1:-}"
case "$mode" in
  --self-test) run_py --self-test "$root" ;;
  --list)      run_py --list "$root" ;;
  --surface)   run_py --surface "$root" ;;
  --corpus)    run_py --corpus "$root" ;;
  "")
    echo "Checking non-default Cargo features across the reader-facing docs..."
    if ! run_py --check "$root"; then
      cat <<'EOF'

A page that hands someone Rust reaching for a feature-gated item, and never
names the feature, fails them at `cargo build` with a message about THEIR file:

    error[E0433]: failed to resolve: could not find `pdf` in `autumn_web`

The item exists, every path on the page resolves, and the missing line is in a
file the page never showed them. Fix it where it lives — add the enabling line
to the page, ideally above the first block that needs it:

    ```toml
    autumn-web = { version = "0.7", features = ["pdf"] }
    ```

Any of these spellings satisfies the gate, so a page that already explains the
feature in prose needs no fence:

    the `pdf` feature          the `pdf` Cargo feature
    feature `pdf`              feature flag `pdf`
    cargo build --features pdf

A construct the page must SHOW rather than offer — a quoted release note, a
comparison against an older API — is waived beside the passage, with a reason:

    <!-- feature-gate-allow: ws — quoted from the 0.5 release notes, not a
         snippet this page offers -->

Inspect what the gate read:  scripts/check-docs-features.sh --list
The truth set it read it against:  scripts/check-docs-features.sh --surface
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--list|--surface|--corpus|--self-test]" >&2
    exit 2
    ;;
esac
