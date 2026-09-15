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
# THE BASELINE RUN found seventeen such page/feature pairs across thirteen
# pages, out of 149 gated uses in the corpus. Nine were visible from the first
# version of the extractor:
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
# and a tenth, reachable only once the gate resolved a second path segment:
#
#   docs/guide/jobs.md:824                `autumn_web::data::csv`  -> csv
#
# and four more once a bare name after a prelude glob was read:
#
#   docs/guide/custom-subsystems.md:202   `LocalChannelsBackend`   -> ws
#   docs/guide/events.md:69               `Mailer`                 -> mail
#   docs/guide/presence.md:48             `Presence`               -> presence
#   docs/guide/transactions.md:322        `Mailer`                 -> mail
#
# and a fifteenth once `t!` was read:
#
#   docs/guide/macro-transparency.md:1727 `t!`                     -> i18n
#
# and two more once a gated ITEM under an ungated module was read:
#
#   docs/guide/generators.md:871          `sse::stream`            -> ws
#   docs/migrations/next.md:259           `openapi::Parameter`     -> openapi
#
# `presence.md` is the one to read twice. It was not missing the feature name —
# it gave the WRONG one, pinning `features = ["ws"]` and saying the extractor
# "is available from `autumn_web::prelude::*` automatically when `ws` is
# enabled". `presence = ["ws"]` runs one way only, so a reader who followed that
# line exactly got an app with no `Presence` in it, from the page whose whole
# subject is `Presence`.
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
#   2. Inside a ```rust fence, an attribute `#[<macro>]` or a bang macro
#      `<name>!` gated under such a `#[cfg]`. WHICH of the two a name is comes
#      from `autumn-macros/src/lib.rs`, where every export carries
#      `#[proc_macro]` or `#[proc_macro_attribute]` one line above it — 12 bang
#      and 35 attribute. This list was once written by hand as "`#[ws]`,
#      `#[mailer]`, `#[mailer_preview]`, `#[mail_previews]`, `#[inbound_mail]`,
#      `#[wire_client]`", and two of those six are not attributes at all:
#      `mail_previews![…]` and `wire_client!` are bang macros. An attribute is
#      the form the prelude is USED in, so leaving either kind out would exempt
#      the most-copied constructs in the guide.
#   3. The page then has to NAME that feature, in a spelling a reader can act
#      on: a `features = [ … "ws" … ]` array TIED TO autumn-web — either the
#      inline table `autumn-web = { …, features = [ … ] }` or a
#      `[dependencies.autumn-web]` section, with newlines and comments inside
#      the array fine, since the corpus writes them that way — a `--features
#      ws` invocation, the prose forms `` `ws` feature ``/`` `ws` Cargo
#      feature ``/`` feature `ws` ``/`` feature flag `i18n` ``, or a
#      `[features]` table row `ws = [`. Every spelling is live in the corpus.
#
#      The TIE matters and was missing at first. An unqualified `features = […]`
#      match let ANY dependency's array satisfy the gate, and
#      `skills/autumn-web/references/api-reference.md:1303` carries
#      `axum = { version = "0.8", features = ["macros", "ws"] }` — which enables
#      axum's websocket support and does nothing for autumn-web's `ws`. `ws`,
#      `mail`, `tls`, `redis`, `openapi`, `markdown` and `csv` are all ordinary
#      feature names in other crates, so that was a standing false-negative
#      channel rather than one page's bad luck. Found by Codex review on #2800;
#      no page in the corpus was relying on it, so the tie cost nothing to add.
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
#   - A CONJUNCTION requires all of its conjuncts, and every non-default one is
#     reported. Eleven column-zero attributes here are `#[cfg(all(…))]`:
#     `presence_badge` is `all(presence, maud)` and `presence_stream` is a
#     line-wrapped `all(presence, ws, maud, htmx)`. The first version of this
#     gate read only the single-feature form — and said so in this paragraph,
#     claiming "autumn-web has no such form on a top-level item today", which
#     was simply false. Both items were missing from the truth set entirely, so
#     a fence naming either would have passed while the reader's build failed
#     for want of `presence` or `ws`. Found by Codex review on #2800; the
#     attribute is now read by PARENTHESIS BALANCE, since `presence_stream`'s
#     wraps across four lines and a line-anchored regex cannot see it.
#     Requirements that are default (`maud`, `htmx`) are dropped rather than
#     demanded, so an item behind default features alone — `live`, behind
#     `all(htmx, maud)` — is not gated surface at all.
#   - A `#[cfg(not(…))]` predicate is a build CONSTRAINT, not a requirement, so
#     it contributes no feature name — reading one out would tell a reader to
#     enable the very feature that removes the item (`lib.rs` has
#     `#[cfg(not(feature = "seed"))]`). It does NOT cancel the positive
#     conjuncts beside it, though: `capsule::build_recording_pool` is
#     `all(feature = "test-support", feature = "db", not(feature = "sqlite"))`
#     and still needs `test-support` in an ordinary default build. Dropping the
#     whole gate on sight of a `not(` hid that. Found by Codex review on #2800.
#   - A declaration made at least once with NO feature requirement removes its
#     gated siblings in the same namespace. `autumn/src/db.rs` defines
#     `RuntimeConnection` under both `not(feature = "sqlite")` and
#     `feature = "sqlite"`; recording only the positive arm put
#     `db::RuntimeConnection` in the surface as needing `sqlite`, which would
#     have told a reader on the ordinary Postgres path to enable the one
#     feature that swaps their database backend. PER NAMESPACE, because
#     stripping by name alone then removed `autumn_web::edge` — the gated crate
#     re-export — on account of `#[edge]`, the ungated attribute macro sharing
#     its name. Both found by Codex review on #2800, the second while fixing
#     the first.
#   - `#[cfg(any(…))]` is dropped too — naming ONE alternative satisfies it and
#     this gate cannot tell which a page meant — with ONE resolved exception:
#     `any(test, feature = "X")`, where every non-feature predicate is one that
#     cannot hold in a reader's build. `cfg(test)` is set for the crate being
#     compiled as a test and NEVER for its dependencies, so a reader writing a
#     test against autumn-web does not get that branch and genuinely needs `X`.
#     `autumn/src/metrics.rs:1991` gates `metrics::testing` exactly so, and
#     `docs/guide/metrics.md:465` hands the reader
#     `autumn_web::metrics::testing::unique_name`. Dropping the whole item hid
#     a real requirement rather than avoiding a guess. Found by Codex review on
#     #2800. A genuine `any(feature = "a", feature = "b")` is still dropped.
#   - NAMING AN IMPLYING FEATURE COUNTS, because Cargo activates what a feature
#     implies. `presence = ["ws"]`, so a page that writes
#     `features = ["presence"]` beside a `presence_stream` snippet is correct
#     and complete — the reader's build gets `ws` too. Subtracting only the
#     default closure and then demanding each conjunct BY NAME reported `ws` as
#     missing on exactly that correct page: the gate telling an author to break
#     something that works, which is the one error direction this whole family
#     refuses to trade for. The check now asks whether anything the page names
#     ACTIVATES the requirement, walking the manifest's implication graph. The
#     same edge runs through `mcp = ["openapi"]`, `constela = ["maud"]`,
#     `offline-sync = ["db", "http-client"]` and `oauth2 = ["http-client"]`, so
#     it is a rule about the manifest, not a special case. Found by Codex
#     review on #2800.
#   - A WHOLE CRATE re-exported under a new name is a module to the reader.
#     `#[cfg(feature = "edge")] pub use autumn_edge as edge;` is how
#     `autumn_web::edge::…` exists at all, and it carries no `::` for either
#     `pub use` pattern to bite on, so `edge` was missing from the truth set
#     and a fence writing `autumn_web::edge::EdgeRoute` would have passed
#     ungated. Found by Codex review on #2800. No page uses it in a rust fence
#     today, so this closed a hole rather than fixing a page.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - A bare identifier in a fence with NO prelude glob. One WITH a glob is
#     read, and this bullet used to refuse that outright, arguing from a
#     prelude surface "174 items wide with names like `Format`, `Column`,
#     `Link`, `Client`, `Patch`, `Lock`, `Story` and `Transport`" that
#     "matching those as words would report most of the guide". That was the
#     wrong number: every ambiguous name in it is behind `maud` or `db`, both
#     DEFAULT, so none is gated surface at all. The set actually at risk is 31
#     uppercase names, and measuring the corpus found 27 occurrences over 14
#     page/name pairs — of which FOUR were live defects, including
#     `docs/guide/presence.md`, which told readers to enable `ws` when the
#     feature is `presence` (`presence = ["ws"]` runs one way only, so that
#     app has no `Presence` at all). Found by Codex review on #2800. Two rules
#     keep it safe: the fence must write the glob, and the name must start
#     UPPERCASE — which excludes `t`, every module and every macro by
#     construction. `check-docs-symbols.sh` still stops at bare identifiers,
#     because it is resolving PATHS; this is asking a narrower question.
#   - A name inside a `use autumn_web::{…}`
#     GROUP is not a bare identifier and IS read — the group names the path, so
#     nothing has to be inferred — including a nested `{pdf::Pdf}`. Single-line
#     groups only: the corpus writes no multi-line one, and this reads a line at
#     a time. Found by Codex review on #2800, over 16 grouped imports in the
#     corpus's rust fences.
#   - (Nothing about bang macros. This bullet twice claimed `t!` could not be
#     read — "`\bt!\(` is one character long: `assert!(`, `insert!(`,
#     `expect!(` and `vec!(` all end in it" — and that is simply false. A word
#     boundary requires a NON-word character before the `t`, and in `assert!`
#     the `t` follows `r`. `\bt!` matches none of them, which one `re` call
#     would have shown either time. The claim survived two rounds because it
#     was reasoned about rather than run. `t!` is read now, with no length
#     floor: the boundary plus membership in the parsed bang set is the whole
#     filter. It had a live defect behind it —
#     `docs/guide/macro-transparency.md:1727` offers `t!(locale, …)` under a
#     "**You write:**" heading on a page that names `i18n` nowhere. Found by
#     Codex review on #2800.)
#   - Anything past the SECOND path segment. One level down IS resolved, for
#     both MODULES and ITEMS, and has to be: a gate below an unconditional
#     module is invisible from the head, and worst when the head is a DEFAULT
#     feature or no feature at all. `autumn_web::db::sqlite_types` needs
#     `sqlite` while `db` is on by default; `autumn_web::openapi::Parameter`
#     needs `openapi` while `pub mod openapi;` carries no `#[cfg]` at all, so
#     the path resolved to nothing and the gate vouched for it
#     (`docs/migrations/next.md:259`, a live defect; `sse::stream` under `ws`
#     was a second). Both found by Codex review on #2800; surface 71 -> 197,
#     checked uses 137 -> 149. For modules only `pub mod` counts — 11 of the 40
#     gated child modules are a private `mod tests`, which is not a path any
#     reader can write. A THIRD segment is an item inside a module inside a
#     module; resolving it needs type resolution, which is where
#     `check-docs-symbols.sh` stops and where this stops too.
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
import tempfile
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

CFG_OPEN = re.compile(r'^#\[cfg\(')
CFG_FEATURE_NAME = re.compile(r'feature\s*=\s*"([a-z0-9_.+-]+)"')
# Every visibility, because the line has to be CONSUMED either way: a `#[cfg]`
# sitting above `pub(crate) mod session_redis;` is that module's gate, and a
# matcher that skipped the line would carry the gate forward onto the next
# declaration instead. Group 1 is the visibility prefix, and only a bare `pub `
# makes the module a path a reader outside the crate can write — see
# `_is_public`.
MOD_DECL = re.compile(r'^(pub(?:\([^)]*\))? )?mod ([a-z_0-9]+)\s*[;{]')
# Strictly `pub`, for the nested pass: a `pub(crate)` or private `mod tests`
# gated by a feature is not a path a reader can write.
PUB_MOD_DECL = re.compile(r'^pub mod ([a-z_0-9]+)\s*[;{]')


def _is_public(visibility):
    """True for `pub`, false for `pub(…)` and for no visibility at all.

    `pub(crate)`, `pub(super)` and a bare `mod` are all invisible downstream, so
    a reader cannot write the path however many features they enable — which
    makes reporting one exactly backwards: the gate would demand `redis` for
    `autumn_web::session_redis` (`lib.rs:632`) and enabling it would not make
    the path resolve. The nested pass has required strict `pub` since round 8
    and said why; the root pass did not, which is the fifth time on this PR a
    rule landed on one of two code paths. Found by Codex review on #2800.
    """
    return visibility == 'pub '
# A public item declared or re-exported at column zero INSIDE a root module.
# `pub mod openapi;` is unconditional; `openapi::Parameter` is not.
PUB_ITEM_DECL = re.compile(
    r'^pub (?:struct|enum|trait|fn|async fn|type|const|static) '
    r'([A-Za-z_][A-Za-z_0-9]*)')
PUB_USE_ONE_LINE = re.compile(r'^pub use [A-Za-z_0-9:]+::\{?([^;{}]+?)\}?;')
PUB_USE_BRACE_OPEN = re.compile(r'^pub use [A-Za-z_0-9:]+::\{$')
USE_ONE_LINE = re.compile(r'^pub use ([a-z_0-9:]+)::\{?([^;{}]+?)\}?;')
USE_BRACE_OPEN = re.compile(r'^pub use ([a-z_0-9:]+)::\{$')
# A whole CRATE re-exported under a new name, with no `::` anywhere:
# `#[cfg(feature = "edge")] pub use autumn_edge as edge;` is how
# `autumn_web::edge::…` exists at all. The two patterns above both require a
# `::`, so this declaration parsed as nothing and `edge` was missing from the
# truth set — a fence writing `autumn_web::edge::EdgeRoute` would have passed
# while the reader's build failed for want of the non-default `edge` feature.
# Found by Codex review on #2800. It is a module to a reader, so it is recorded
# as one.
USE_CRATE_AS = re.compile(r'^pub use ([a-z_0-9]+) as ([a-z_0-9]+);')
# `#[cfg(all(feature = "embed-assets", feature = "i18n"))] #[macro_export]
# macro_rules! embed_locales { … }` — a BANG macro, exported from the crate
# root, and the only way to reach it is `autumn_web::embed_locales!()` or a
# bare `embed_locales!()`. Neither the module nor the `pub use` patterns match a
# `macro_rules!` line, so the pending gate was discarded and the macro was
# absent from the surface: a fence calling only `embed_static!()` passed.
# Found by Codex review on #2800.
MACRO_RULES_DECL = re.compile(r'^macro_rules!\s+([a-z_][a-z_0-9]*)')
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
    graph = {name: edges(name) for name in table if name != 'default'}
    return closure, set(graph), graph


PROC_MACRO_SRC = 'autumn-macros/src/lib.rs'
PROC_BANG = re.compile(r'^#\[proc_macro\]$')
PROC_ATTR = re.compile(r'^#\[proc_macro_attribute\]$')
PROC_FN = re.compile(r'^pub fn ([a-z_][a-z_0-9]*)')


def proc_macro_kinds(root):
    """`{name: 'bang' | 'attribute'}` for every macro `autumn-macros` exports.

    PARSED, because guessing was wrong. The header used to list `#[ws]`,
    `#[mailer]`, `#[mailer_preview]`, `#[mail_previews]`, `#[inbound_mail]` and
    `#[wire_client]` as the attribute macros this gate reads — and two of those
    six are not attributes at all: `mail_previews` and `wire_client` are
    `#[proc_macro]`, called as `mail_previews![…]`. The crate declares 12 bang
    macros and 35 attributes, and which is which is written down one line above
    each `pub fn`. Reading it costs nothing and cannot drift.

    This is also what lets `t` be read. It reaches the crate root as
    `pub use crate::i18n::t` — a plain re-export this gate recorded as an
    `item`, so a bare `t!("key")` matched nothing. Found by Codex review on
    #2800, with a live defect behind it
    (`docs/guide/macro-transparency.md:1726`).
    """
    out = {}
    lines = (pathlib.Path(root) / PROC_MACRO_SRC).read_text(
        encoding='utf-8').splitlines()
    pending = None
    for line in lines:
        if PROC_BANG.match(line):
            pending = 'bang'
            continue
        if PROC_ATTR.match(line):
            pending = 'attribute'
            continue
        if line.startswith(('///', '//!', '//', '#[')):
            continue
        match = PROC_FN.match(line)
        if match and pending:
            out[match.group(1)] = pending
        pending = None
    if not out:
        sys.exit(
            f'FAIL: no `#[proc_macro]`/`#[proc_macro_attribute]` declarations '
            f'found in {PROC_MACRO_SRC}. The macro crate was restructured; '
            f'this gate cannot tell `#[mailer]` from `mail_previews![…]` and '
            f'would judge both wrongly. Fix proc_macro_kinds() in '
            f'scripts/check-docs-features.sh.')
    return out


def _strip_not(text):
    """Remove every balanced `not( … )` subexpression from a cfg predicate."""
    out = []
    index = 0
    while True:
        found = text.find('not(', index)
        if found < 0:
            out.append(text[index:])
            return ''.join(out)
        out.append(text[index:found])
        depth = 0
        cursor = found + 3
        while cursor < len(text):
            if text[cursor] == '(':
                depth += 1
            elif text[cursor] == ')':
                depth -= 1
                if depth == 0:
                    cursor += 1
                    break
            cursor += 1
        index = cursor


def _cfg_requirement(lines, index):
    """Read one column-zero `#[cfg(…)]` and return (required_features, next_i).

    `required_features` is None when the attribute is not a plain conjunction of
    `feature = "…"` predicates — which is the whole reason this is a function
    rather than a regex. Three shapes are live at column zero in this crate and
    each needs its own answer:

      - `#[cfg(feature = "ws")]` — one required feature.
      - `#[cfg(all(feature = "presence", feature = "maud"))]` and its
        line-wrapped four-predicate sibling on `presence_stream` — EVERY
        conjunct is required, so every non-default one has to be named. An
        earlier version read only the single-feature form and declared in its
        header that "autumn-web has no such form on a top-level item today".
        That was simply false: eleven column-zero attributes are `all(…)`, and
        `autumn_web::presence_badge` (presence + maud) and
        `autumn_web::presence_stream` (presence + ws + maud + htmx) were
        therefore missing from the truth set entirely, so a fence could use
        either without naming `presence` or `ws` and this gate would pass it.
        Caught by Codex review on #2800.
      - `#[cfg(not(feature = "seed"))]` — the item exists when the feature is
        OFF. Reading the name out of it would demand the reader enable the one
        feature that REMOVES the item, so `not(` yields None, as does `any(`
        (naming one alternative is already enough, and this gate cannot tell
        which one a page meant). Both under-report rather than over-report: a
        page is never failed for a feature the gate guessed at.

    The attribute may wrap across lines — `presence_stream`'s does — so the
    text is gathered by PARENTHESIS BALANCE rather than by line. A single-line
    regex silently skipped it, which is the same "invisible because it spans a
    line" failure `check-docs-cli.sh` records for its own span reader.
    """
    depth = 0
    parts = []
    while index < len(lines):
        line = lines[index]
        parts.append(line)
        depth += line.count('(') - line.count(')')
        index += 1
        if depth <= 0:
            break
    text = ' '.join(parts)
    if 'not(' in text:
        # A `not(…)` predicate is a build CONSTRAINT, not a requirement — but
        # it does not cancel the positive conjuncts beside it.
        # `autumn/src/capsule/mod.rs:71` gates `build_recording_pool` on
        # `all(feature = "test-support", feature = "db", not(feature =
        # "sqlite"))`, and in an ordinary default (non-SQLite) build that item
        # still needs `test-support`. Dropping the whole gate the moment a
        # `not(` appeared hid that. Found by Codex review on #2800.
        #
        # The negative subexpressions are removed and whatever positive
        # features remain are required. A gate that is ONLY `not(…)` —
        # `#[cfg(not(feature = "seed"))]` in `lib.rs` — leaves nothing, which
        # is the right answer: it marks an item that exists when the feature is
        # off, so there is no feature to name.
        stripped = _strip_not(text)
        if 'any(' in stripped:
            return None, index
        names = CFG_FEATURE_NAME.findall(stripped)
        return (set(names) if names else None), index
    names = CFG_FEATURE_NAME.findall(text)
    if 'any(' in text:
        # `#[cfg(any(test, feature = "test-support"))]` — the shape
        # `autumn/src/metrics.rs:1991` uses for `metrics::testing`, which
        # `docs/guide/metrics.md:465` hands the reader as
        # `autumn_web::metrics::testing::unique_name`.
        #
        # `cfg(test)` is set for the crate being compiled as a test, NEVER for
        # its dependencies — so a reader writing a test against autumn-web does
        # not get that branch and genuinely needs `test-support`. Dropping the
        # whole item (the old behaviour for every `any(`) therefore hid a real
        # requirement rather than avoiding a guess. Found by Codex review on
        # #2800.
        #
        # Only this shape is resolved: every non-feature predicate must be one
        # that cannot hold in a reader's build, and exactly one feature may
        # remain. A genuine `any(feature = "a", feature = "b")` is still
        # dropped — naming either satisfies it and this gate cannot tell which
        # the page meant, which is the original `any(` argument and still
        # right.
        others = re.findall(r'\b(test|doc|doctest|miri)\b', text)
        if len(names) == 1 and others:
            return {names[0]}, index
        return None, index
    return (set(names) if names else None), index


def gated_items(root, macro_kinds):
    """Map every column-zero item behind a `#[cfg(…)]` to the features it needs.

    Returns `{name: (features, kinds)}`. `features` is the set the item
    REQUIRES — several, for an `all(…)` conjunction. `kinds` is a set drawn
    from `module`, `macro` (an attribute a reader writes) and `item` (a type or
    function): a name can be several of those at once, and `ws` is, since
    `lib.rs` declares `pub mod ws;` and re-exports `autumn_macros::ws` under
    the same gate, so `autumn_web::ws::WebSocket` and `#[ws("/echo")]` are both
    real. Keeping only the first reading made `#[ws]` — the most-copied gated
    construct in the guide — invisible, which the self-test now holds it to.

    Column zero is the whole nesting model, and it is enough because the
    workspace runs `cargo fmt --all`. See the header for the five inline
    `pub mod … {` blocks this keeps out.
    """
    found = {}
    unconditional = set()
    for rel in SOURCES:
        lines = (pathlib.Path(root) / rel).read_text(
            encoding='utf-8').splitlines()
        pending = None
        index = 0
        while index < len(lines):
            line = lines[index]
            # Indented or blank: inside something, or between things. Neither
            # can carry a top-level declaration, and neither cancels a pending
            # `#[cfg]` — rustfmt puts no blank line between an attribute and
            # the item it is on, but a doc comment may sit between them.
            if not line or line[:1].isspace():
                index += 1
                continue
            if CFG_OPEN.match(line):
                required, index = _cfg_requirement(lines, index)
                if required:
                    pending = required
                continue
            index += 1
            # Doc comments and further attributes sit between the `#[cfg]` and
            # the item; they carry the gate forward rather than clearing it.
            if line.startswith(('///', '//!', '//', '#[')):
                continue
            match = MOD_DECL.match(line)
            if match:
                # A non-`pub` module is neither gated surface nor an
                # unconditional definition that could strip one: it is not a
                # path a reader can write under ANY feature set, so it should
                # neither be demanded nor vouch for a public namesake. The line
                # still clears `pending` — its `#[cfg]` belongs to it.
                if _is_public(match.group(1)):
                    if pending:
                        _record(found, match.group(2), pending, 'module')
                    else:
                        unconditional.add((match.group(2), 'type'))
                pending = None
                continue
            match = MACRO_RULES_DECL.match(line)
            if match:
                name = match.group(1)
                # `__autumn_register_fake_seeder` is plumbing the seed macro
                # expands into, not a call any reader writes.
                if pending and not name.startswith('__'):
                    _record(found, name, pending, 'bang')
                elif not pending:
                    unconditional.add((name, 'macro'))
                pending = None
                continue
            match = USE_CRATE_AS.match(line)
            if match:
                if pending:
                    _record(found, match.group(2), pending, 'module')
                else:
                    unconditional.add((match.group(2), 'type'))
                pending = None
                continue
            match = USE_ONE_LINE.match(line)
            if match:
                if pending:
                    for name in _names(match.group(2)):
                        _record(found, name, pending,
                                macro_kinds.get(name, 'item'))
                else:
                    unconditional.update(_ns_pairs(
                        _names(match.group(2)), macro_kinds))
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
                    for name in _names('\n'.join(body)):
                        _record(found, name, pending,
                                macro_kinds.get(name, 'item'))
                else:
                    unconditional.update(_ns_pairs(
                        _names('\n'.join(body)), macro_kinds))
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
    return _strip_unconditional(found, unconditional)


# Rust resolves a name per NAMESPACE, and this gate has to as well: `module`
# and `item` live in the type namespace, `attribute` and `bang` in the macro
# one. `lib.rs` declares `edge` in both — `#[cfg(feature = "edge")] pub use
# autumn_edge as edge;` (the module, gated) and `pub use autumn_macros::edge;`
# (the `#[edge]` attribute, NOT gated) — which is legal precisely because they
# do not collide.
_NAMESPACE = {'module': 'type', 'item': 'type',
              'attribute': 'macro', 'bang': 'macro'}


def _ns_pairs(names, macro_kinds):
    """`(name, namespace)` for each name, using the parsed macro kinds."""
    return {(n, _NAMESPACE.get(macro_kinds.get(n, 'item'), 'type'))
            for n in names}


def _strip_unconditional(found, unconditional):
    """Drop every name that is ALSO declared without a feature requirement.

    `autumn/src/db.rs` defines `RuntimeConnection` twice — once under
    `#[cfg(not(feature = "sqlite"))]` and once under `#[cfg(feature =
    "sqlite")]`. Reading the `not(` arm as "no requirement" and then recording
    the positive arm left `db::RuntimeConnection` in the surface as needing
    `sqlite`, when it exists in a DEFAULT build and always has.

    That is a false positive, and the worst-shaped one this gate could produce:
    it would have told a reader on the ordinary Postgres path to enable the one
    feature that swaps their database backend. Every other boundary here was
    drawn to avoid exactly this. Found by Codex review on #2800.

    The rule is general rather than a `not(`-specific patch: a declaration made
    at least once with no feature requirement — a complementary `cfg` arm, a
    bare declaration beside a gated re-export — is available in a default build.

    PER NAMESPACE, though, and that distinction is not academic. Stripping by
    name alone removed `autumn_web::edge` (the gated crate re-export) because
    `#[edge]` (the ungated attribute macro) shares its name — trading the false
    positive above for a false negative on a whole module. `unconditional`
    carries `(name, namespace)` pairs, and only the kinds in that namespace are
    dropped; a name keeps its entry as long as any kind survives.
    """
    for name, namespace in unconditional:
        entry = found.get(name)
        if not entry:
            continue
        kinds = {k for k in entry[1] if _NAMESPACE.get(k) != namespace}
        if kinds:
            found[name] = (entry[0], kinds)
        else:
            del found[name]
    return found


def _record(found, name, features, kind):
    """Add one declaration, unioning both the features and the kinds.

    A name declared twice — `ws` as a module in `lib.rs` and as a macro in
    `prelude.rs` — keeps every kind. Its requirements are unioned rather than
    replaced: the reader needs whatever ANY of its declarations needs, and
    taking only the first would let the narrower gate vouch for the wider one.
    """
    if name in found:
        found[name][0].update(features)
        found[name][1].add(kind)
    else:
        found[name] = (set(features), {kind})


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


def root_modules(root):
    """`{name: features}` for every column-zero `pub mod` in `lib.rs`.

    Features is the module's OWN requirement set, empty for an unconditional
    one — and the empty ones are the point. `nested_modules()` was first seeded
    from `gated_items()`, which records only declarations carrying a `#[cfg]`,
    so it never descended into an unconditional parent. That skipped exactly
    the case the nested pass exists for: `pub mod data;` is unconditional,
    `data::csv` is behind the non-default `csv` feature, and
    `docs/guide/jobs.md:824` imports `autumn_web::data::csv::export_csv` in a
    rust fence without naming it — a live defect the gate reported as clean.
    Found by Codex review on #2800, and the comment on that seeding claimed it
    read "every top-level module whatever its own gate" while the code did not.
    """
    out = {}
    lines = (pathlib.Path(root) / 'autumn/src/lib.rs').read_text(
        encoding='utf-8').splitlines()
    pending = None
    index = 0
    while index < len(lines):
        line = lines[index]
        if not line or line[:1].isspace():
            index += 1
            continue
        if CFG_OPEN.match(line):
            required, index = _cfg_requirement(lines, index)
            if required:
                pending = required
            continue
        index += 1
        if line.startswith(('///', '//!', '//', '#[')):
            continue
        match = PUB_MOD_DECL.match(line)
        if match:
            out[match.group(1)] = set(pending or ())
        pending = None
    if not out:
        sys.exit(
            'FAIL: no column-zero `pub mod` declarations found in '
            'autumn/src/lib.rs. The crate root was restructured; the nested '
            'pass has nothing to descend into and would silently check only '
            'the head segment. Fix root_modules() in '
            'scripts/check-docs-features.sh.')
    return out


def nested_modules(root, parents):
    """`{"parent::child": features}` for one level below the crate root.

    A feature gate below an UNCONDITIONAL module is invisible from the first
    path segment, and the worst case is the one where the parent is a DEFAULT
    feature: `autumn_web::db::sqlite_types` needs `sqlite`, `db` is on by
    default, so reading only the head drops the whole path out of the surface
    and the gate vouches for a line that does not build. `storage::variant`
    (`variants`) is the same shape with a non-default parent. Found by Codex
    review on #2800.

    Only `pub mod` counts: a private `mod tests` gated by a feature is not a
    path any reader can write, and 11 of the 40 gated child modules are that.
    Requirements are the UNION of the parent's and the child's, because a
    reader needs both — `storage::variant` wants `storage` and `variants`,
    though the manifest's `variants = ["storage", …]` means naming the second
    already brings the first (see `enablers()`).

    ONE level, not arbitrary depth. `check-docs-symbols.sh` stops at the first
    item segment because resolving further needs type resolution; a module path
    is the part that does not, and one level is where the corpus's gated
    modules actually live. A third segment is an item inside a module, and
    guessing at those is how a gate starts reporting confident nonsense.
    """
    out = {}
    for name, parent_features in parents.items():
        unconditional = set()
        for candidate in (f'autumn/src/{name}.rs', f'autumn/src/{name}/mod.rs'):
            path = pathlib.Path(root) / candidate
            if path.exists():
                break
        else:
            # An inline `pub mod x { … }` in lib.rs has no file of its own.
            continue
        lines = path.read_text(encoding='utf-8').splitlines()
        pending = None
        index = 0
        while index < len(lines):
            line = lines[index]
            if not line or line[:1].isspace():
                index += 1
                continue
            if CFG_OPEN.match(line):
                required, index = _cfg_requirement(lines, index)
                if required:
                    pending = required
                continue
            index += 1
            if line.startswith(('///', '//!', '//', '#[')):
                continue
            match = PUB_MOD_DECL.match(line)
            if match:
                if pending:
                    out[f'{name}::{match.group(1)}'] = (
                        set(parent_features) | pending, {'module'})
                else:
                    unconditional.add(
                        (f'{name}::{match.group(1)}', 'type'))
                pending = None
                continue
            # A gated public ITEM inside an ungated module is the same hole one
            # rung down: `pub mod openapi;` is unconditional while
            # `openapi::Parameter` is `#[cfg(feature = "openapi")]`, so a page
            # writing `use autumn_web::openapi::Parameter;` resolved to nothing
            # and the gate vouched for it. `docs/migrations/next.md:259` did
            # exactly that. Found by Codex review on #2800.
            match = PUB_ITEM_DECL.match(line)
            if match:
                if pending:
                    out[f'{name}::{match.group(1)}'] = (
                        set(parent_features) | pending, {'item'})
                else:
                    # A complementary arm: `#[cfg(not(feature = "sqlite"))]
                    # pub type RuntimeConnection = …` beside the gated one. The
                    # item exists in a default build, so nothing about it is
                    # worth telling a reader. See `_strip_unconditional`.
                    unconditional.add(
                        (f'{name}::{match.group(1)}', 'type'))
                pending = None
                continue
            match = PUB_USE_ONE_LINE.match(line)
            if match:
                if pending:
                    for item in _names(match.group(1)):
                        out[f'{name}::{item}'] = (
                            set(parent_features) | pending, {'item'})
                else:
                    unconditional.update(
                        (f'{name}::{i}', 'type')
                        for i in _names(match.group(1)))
                pending = None
                continue
            match = PUB_USE_BRACE_OPEN.match(line)
            if match:
                inner = []
                while index < len(lines) and not lines[index].startswith('};'):
                    inner.append(lines[index])
                    index += 1
                index += 1
                if pending:
                    for item in _names('\n'.join(inner)):
                        out[f'{name}::{item}'] = (
                            set(parent_features) | pending, {'item'})
                else:
                    unconditional.update(
                        (f'{name}::{i}', 'type')
                        for i in _names('\n'.join(inner)))
                pending = None
                continue
            pending = None
        _strip_unconditional(out, unconditional)
    return out


def surface(root):
    """`{name: (features, kinds)}` for what a DEFAULT build does not have.

    `features` is narrowed to the NON-DEFAULT requirements. An item behind
    `all(feature = "presence", feature = "maud")` keeps `presence` alone:
    `maud` is on by default, so telling a reader to enable it is noise on a
    page that is otherwise correct. An item whose every requirement is default
    — `live`, behind `htmx` + `maud` — drops out entirely.
    """
    closure, declared, _graph = default_features(root)
    gated = gated_items(root, proc_macro_kinds(root))
    unknown = sorted(set().union(*(f for f, _ in gated.values())) - declared)
    if unknown:
        sys.exit(
            f'FAIL: {", ".join(unknown)} is `#[cfg(feature = …)]`-gated in the '
            f'crate root but is not a feature in {MANIFEST}. Either the '
            f'manifest lost it (the cfg is then dead and the item ships to '
            f'nobody) or this gate mis-parsed one of them. Inspect with '
            f'--surface.')
    out = {}
    for name, (features, kinds) in gated.items():
        needed = features - closure
        if needed:
            out[name] = (needed, kinds)
    # One level down, keyed `parent::child`, resolved BEFORE the head in
    # `uses()`. Seeded from `root_modules()` — EVERY column-zero `pub mod`,
    # gated or not — because the interesting case is a gated child under an
    # UNCONDITIONAL parent, and seeding from the gated set alone skipped it.
    # `data::csv` is that case, and it was a live defect. See `root_modules()`.
    for name, (features, kinds) in nested_modules(
            root, root_modules(root)).items():
        needed = features - closure
        if needed:
            out[name] = (needed, kinds)
    return out


# ---------------------------------------------------------------------------
# What a page shows, and whether it names the feature
# ---------------------------------------------------------------------------


def enablers(graph):
    """feature -> every feature whose activation also activates it.

    Cargo features imply one another, and a page that names the IMPLYING one has
    already told the reader everything they need. `presence = ["ws"]`, so a page
    that writes `features = ["presence"]` beside an `autumn_web::presence_stream`
    snippet is correct and complete: Cargo turns `ws` on for them. Subtracting
    only the default closure and then demanding each conjunct by name reported
    `ws` as missing on exactly that correct page — a gate telling an author to
    break something that works, which is the one error direction this whole
    family of gates refuses to trade for. Found by Codex review on #2800.

    The same edge runs through `mcp = ["openapi"]`, `constela = ["maud"]`,
    `offline-sync = ["db", "http-client"]` and `oauth2 = ["http-client"]`, so
    this is a rule about the manifest rather than a special case for `presence`.
    """
    out = {}
    for feature in graph:
        stack = [feature]
        seen = set()
        while stack:
            current = stack.pop()
            if current in seen:
                continue
            seen.add(current)
            stack.extend(graph.get(current, ()))
        for reached in seen:
            out.setdefault(reached, set()).add(feature)
    return out


def activation_lines(text, needed, enabled_by):
    """feature -> the first line naming ANYTHING that turns that feature on.

    Whole-text search per candidate rather than a per-line scan, because the
    corpus writes `features = [` arrays across several lines with comments
    inside them, and a line-at-a-time reader sees neither end of one.
    """
    out = {}
    for feature in needed:
        for candidate in sorted(enabled_by.get(feature, {feature})):
            line = first_naming_line(text, candidate)
            if line is not None and (feature not in out or line < out[feature]):
                out[feature] = line
    return out

FENCE = re.compile(r'^\s*```([A-Za-z0-9_+-]*)')
# The languages a reader compiles. `rs` and `rust,no_run` are both live in the
# corpus; an info string carries the language up to the first comma or space.
RUST_LANGS = ('rust', 'rs')
PATH_USE = re.compile(
    r'\bautumn_web::([A-Za-z_][A-Za-z_0-9]*)'
    r'(?:::([A-Za-z_][A-Za-z_0-9]*))?')
ATTR_USE = re.compile(r'#\[([a-z_][a-z_0-9]*)')
# `use autumn_web::{Mail, Mailer};` — ordinary use-tree syntax, and the corpus
# writes 16 of them in rust fences. `PATH_USE` wants an identifier straight
# after `::` and sees `{`, so every name in the group went unread. Found by
# Codex review on #2800. Both ends of the group have to be in the string this
# is matched against, which is why `rust_fences` joins a multi-line `use` back
# onto one line first — the claim that stood here, that the corpus writes no
# multi-line group, was never measured and is false six times over.
GROUP_USE = re.compile(r'\bautumn_web::\{([^{}]*)\}')
# `use autumn_web::storage::{blob::Blob, variant::{Transform, VariantBudget}};`
# — a group hanging off a MODULE segment, which `GROUP_USE` (anchored straight
# after `autumn_web::`) cannot see. `storage-variants.md:71` writes exactly
# that, and the inner `variant::` is where the `variants` requirement lives, so
# the line resolved to `storage` alone. Found by Codex review on #2800.
MODULE_GROUP_USE = re.compile(
    r'\bautumn_web::([a-z_][a-z_0-9]*)::\{([^{}]*(?:\{[^{}]*\}[^{}]*)*)\}')
MODULE_GROUP_ENTRY = re.compile(r'([a-z_][a-z_0-9]*)\s*::\s*\{|'
                                r'^\s*([A-Za-z_][A-Za-z_0-9]*)')
# The child segment takes UPPERCASE too. `use autumn_web::{openapi::Parameter};`
# names a gated ITEM under an unconditional module, and restricting the child to
# lowercase resolved the entry to `openapi` alone — which is ungated, so the
# line reported nothing. Round 8 widened `PATH_USE` for exactly this and left
# the group form behind. Found by Codex review on #2800.
GROUP_ENTRY = re.compile(
    r'^\s*(?:self\s*)?([A-Za-z_][A-Za-z_0-9]*)'
    r'(?:::([A-Za-z_][A-Za-z_0-9]*))?')
# A bang-macro call, `autumn_web::embed_static!()` or a bare `embed_static!()`.
# The real filter is membership in the gated `bang` set — a name this gate read
# off a `macro_rules!` declaration in the crate root — so the pattern only has
# to be loose enough to reach it. The THREE-character floor is what keeps the
# `t!` problem out by construction as well as by membership: `assert!(`,
# `insert!(` and `vec!(` all end in `t!`/`c!`, and a one-letter macro name is
# not one this gate can be right about across 212 pages.
# No length floor. The `\b` is the filter, and it is sufficient: `assert!`,
# `insert!`, `expect!` and `vec!` contain no word boundary before their final
# letter, so `\bt!` matches none of them — verified, after two rounds of this
# header asserting the opposite without testing it. Membership in the parsed
# bang set does the rest.
BANG_USE = re.compile(r'\b([a-z_][a-z_0-9]*)!')
PRELUDE_GLOB = re.compile(r'\bautumn_web::prelude::\*')
# One or more `>` markers and the space after each: a nested quote writes
# `> > `, and a fence inside one is still a fence.
BLOCKQUOTE = re.compile(r'^\s*(?:>\s?)+')
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

    The BLOCKQUOTE prefix is stripped first. A fence inside a `>` callout has
    every line — the opener, the code, the closer — prefixed with `> `, so a
    pattern anchored on optional whitespace never opens it and the snippet is
    skipped in silence. The corpus writes two of them today
    (`docs/guide/mcp.md:647` and `docs/guide/openapi.md:364`, the latter a
    `use autumn_web::openapi::…` under the non-default `openapi` feature), and
    a callout is exactly where an author puts the snippet that needs a warning
    beside it. Neither is a live defect — `openapi.md` names its feature 340
    lines earlier — but a shape the extractor cannot see is a shape the gate
    does not cover. Found by Codex review on #2800.

    A `use` whose group runs over several lines is joined back into one before
    it is yielded — see `join_use_statements`.
    """
    for start, body in rust_fence_blocks(text):
        for offset, line in join_use_statements(body):
            yield start + offset, line


USE_START = re.compile(r'^\s*(?:pub\s+)?use\b')


def join_use_statements(body):
    """Yield (offset, text) with each multi-line `use` collapsed onto one.

    rustfmt breaks a grouped import across lines as soon as it is long enough,
    and every group pattern here needs BOTH braces in the string handed to it.
    A line-at-a-time reader sees `use autumn_web::{`, then a bare `Mail,`, then
    `};` — so the group went entirely unread and the path resolved to its head
    alone, exactly the failure `MODULE_GROUP_USE` was added to fix for the
    single-line spelling. The corpus writes six of these today
    (`accessibility.md:29`, `experiments.md:253`, `feature-flags.md:284`,
    `idempotency.md:301`, `runtime-config.md:23`, `web-push.md:354`); none is a
    live defect, because every child in them is ungated surface, but
    `idempotency::{…}` is one `RedisIdempotencyStore` away from being one.
    Found by Codex review on #2800.

    The run is ended by brace DEPTH, not by a `;`, and it cannot leave the
    fence: an unclosed group swallows the rest of that snippet and no more. The
    reported line is the one the `use` STARTS on, which is where a reader looks
    for it.
    """
    offset = 0
    while offset < len(body):
        line = body[offset]
        depth = line.count('{') - line.count('}')
        if USE_START.match(line) and depth > 0:
            start, parts = offset, [line.strip()]
            while depth > 0 and offset + 1 < len(body):
                offset += 1
                parts.append(body[offset].strip())
                depth += body[offset].count('{') - body[offset].count('}')
            yield start, ' '.join(parts)
        else:
            yield offset, line
        offset += 1


def rust_fence_blocks(text):
    """Yield (first_body_line_no, [lines]) for each ```rust fence.

    Whole blocks, not loose lines, because one question is fence-scoped: does
    THIS fence bring the prelude in with a glob? A bare `Presence` means the
    gated type only in a fence that wrote `use autumn_web::prelude::*;`, and
    scoping the bare-name scan to that is what makes it safe to do at all.
    """
    lang = None
    start = 0
    body = []
    for lineno, line in enumerate(text.splitlines(), 1):
        line = BLOCKQUOTE.sub('', line)
        match = FENCE.match(line)
        if match:
            if lang is None:
                lang = match.group(1).lower().split(',')[0]
                start, body = lineno + 1, []
            else:
                if lang in RUST_LANGS:
                    yield start, body
                lang = None
            continue
        if lang is not None:
            body.append(line)
    if lang in RUST_LANGS and body:
        yield start, body


def uses(blanked, gated):
    """Yield (line_no, feature, shown) for each gated construct in a rust fence.

    `blanked` is already comment-blanked — see `check()`, which does it once
    and hands the same text to the naming check, so a `<!-- … -->` can neither
    show a construct nor satisfy the rule about naming its feature.

    One yield PER REQUIRED non-default feature: `autumn_web::presence_stream`
    needs `presence` and `ws`, and a page naming only one of them has still
    left the reader a line that does not build.

    `shown` is what the reader sees, so the report can say `#[ws]` rather than
    `ws` — the difference between a line they can find on the page and a name
    they have to go looking for.
    """
    def resolve(head, child):
        """The surface entry for `head::child`, else for `head`, else None."""
        if child:
            entry = gated.get(f'{head}::{child}')
            if entry:
                return entry, f'autumn_web::{head}::{child}'
        entry = gated.get(head)
        return (entry, f'autumn_web::{head}') if entry else (None, None)

    # Type names a prelude glob brings into scope unqualified. Scoped to a
    # fence that actually writes the glob, and to names that START UPPERCASE,
    # which is what makes this safe: the one-letter `t!` and every module and
    # macro name are excluded by construction.
    #
    # The header used to refuse this outright, arguing from a prelude surface
    # "174 items wide with names like `Format`, `Column`, `Link`, `Client`,
    # `Patch`, `Lock`, `Story` and `Transport`". That number was the wrong one:
    # every ambiguous name in it is behind `maud` or `db`, both DEFAULT, so none
    # of them is gated surface at all. The set actually at risk is 31 names, and
    # measuring the corpus found 27 occurrences across 14 page/name pairs — not
    # "most of the guide". One of those 14 was a live defect
    # (`docs/guide/presence.md`). Found by Codex review on #2800.
    bare = {n for n in gated if n[:1].isupper()}
    for start, body in rust_fence_blocks(blanked):
        if not any(PRELUDE_GLOB.search(line) for line in body):
            continue
        for offset, line in enumerate(body):
            for match in re.finditer(r'\b([A-Z][A-Za-z_0-9]*)\b', line):
                name = match.group(1)
                if name not in bare:
                    continue
                entry = gated[name]
                for feature in sorted(entry[0]):
                    yield start + offset, feature, name

    for lineno, line in rust_fences(blanked):
        for match in MODULE_GROUP_USE.finditer(line):
            head, inner = match.group(1), match.group(2)
            # Each `child::{…}` inside the group is a second segment under the
            # same head.
            for child in re.findall(r'([a-z_][a-z_0-9]*)\s*::\s*\{', inner):
                entry, shown = resolve(head, child)
                if entry:
                    for feature in sorted(entry[0]):
                        yield lineno, feature, shown
            # A DIRECT entry is `head::<entry>` — the commonest spelling of all
            # (`storage::{BlobStoreState, …}`, `widgets::{ActiveSearchConfig,
            # …}`), and reading only the nested `child::{…}` groups meant every
            # one of them fell back to the head alone. `storage-variants.md:71`
            # writes both shapes in one line. Found by Codex review on #2800.
            for piece in re.split(r',(?![^{]*\})', inner):
                piece = piece.strip()
                if not piece or '{' in piece:
                    continue
                # `variant as variants_api` — an alias renames the entry, it
                # does not change which item is imported, so the name before
                # `as` is the one to resolve. Found by Codex review on #2800.
                piece = piece.split(' as ')[0].strip()
                parts = [q.strip() for q in piece.split('::') if q.strip()]
                if not parts:
                    continue
                entry, shown = resolve(head, parts[0])
                if entry:
                    for feature in sorted(entry[0]):
                        yield lineno, feature, shown
            entry, shown = resolve(head, None)
            if entry:
                for feature in sorted(entry[0]):
                    yield lineno, feature, shown
        for match in GROUP_USE.finditer(line):
            for piece in match.group(1).split(','):
                entry_match = GROUP_ENTRY.match(piece)
                if not entry_match:
                    continue
                entry, shown = resolve(entry_match.group(1),
                                       entry_match.group(2))
                if entry:
                    for feature in sorted(entry[0]):
                        yield lineno, feature, shown
        for match in BANG_USE.finditer(line):
            entry = gated.get(match.group(1))
            if entry and 'bang' in entry[1]:
                shown = f'{match.group(1)}!'
                for feature in sorted(entry[0]):
                    yield lineno, feature, shown
        for match in PATH_USE.finditer(line):
            head, child = match.group(1), match.group(2)
            # A SECOND segment is resolved first, because a gate one level down
            # can be invisible from the first: `autumn_web::db::sqlite_types`
            # needs `sqlite`, and `db` is a DEFAULT feature, so reading only the
            # head drops the path out of the surface entirely.
            entry = gated.get(f'{head}::{child}') if child else None
            shown = f'autumn_web::{head}::{child}' if entry else None
            if entry is None:
                entry = gated.get(head)
                shown = f'autumn_web::{head}'
            if entry:
                for feature in sorted(entry[0]):
                    yield lineno, feature, shown
        for match in ATTR_USE.finditer(line):
            entry = gated.get(match.group(1))
            # An attribute is only judged against an attribute MACRO. A module
            # named `storage` is not `#[storage]`, and reading it as one would
            # have this gate guessing at a construct that does not exist.
            if entry and 'attribute' in entry[1]:
                shown = f'#[{match.group(1)}]'
                for feature in sorted(entry[0]):
                    yield lineno, feature, shown


# A command line that selects some OTHER package: `cargo install <crate>`,
# `-p <pkg>` or `--package <pkg>` naming anything but autumn-web. Used as a
# negative lookahead so the `--features` flag on such a line is not read as
# autumn-web's. `autumn-web` and `autumn_web` both spell the crate.
# Options come BEFORE the dependency in `cargo add [OPTIONS] <DEP>…`, so they
# have to be skipped before asking which crate the `--features` flag targets.
# Reading `--optional` as the dependency made `cargo add --optional autumn-web
# --features ws` look foreign and rejected a valid enabling line — a false
# positive, the error direction this gate refuses. Value-taking options are
# listed so their VALUE is skipped too, rather than being mistaken for the
# crate. Found by Codex review on #2800.
#
# The package token must START with a word character. `[a-z0-9_-]+` accepts a
# leading `-`, so the first attempt at this fix still read `--optional` as the
# dependency and still rejected the line — the fix looked right and changed
# nothing, which is why it was run against the cases before being believed.
_OPT_WITH_VALUE = (r'--(?:features|rename|path|git|branch|tag|rev|registry'
                   r'|manifest-path|target|vers|version|root|index|profile'
                   r'|bin|example)|-F')
_SKIP_OPTS = rf'(?:(?:{_OPT_WITH_VALUE})(?:=\S+|\s+\S+)\s+|--?[a-z][-a-z0-9]*\s+)*'
_FOREIGN_PKG = (
    rf'[^\n]*(?:cargo\s+(?:install|add)\s+{_SKIP_OPTS}'
    rf'(?!autumn[-_]web\b)[a-z0-9_][a-z0-9_-]*'
    rf'|(?:-p|--package)[= ]\s*(?!autumn[-_]web\b)[a-z0-9_][a-z0-9_-]*)')

# What may sit between the `[` and `dependencies` of a dependency table. Cargo
# puts platform-specific dependencies under `[target.<what>.dependencies.<dep>]`
# where `<what>` is either a quoted cfg expression or a bare target triple, and
# the corpus writes both forms (`docs/guide/edge.md:114`,
# `docs/guide/platform-support.md:70`). A prefix of `[a-z-]+\.` components can
# consume neither the quotes and parentheses of `'cfg(unix)'` nor the digits and
# underscores of `x86_64-pc-windows-msvc`, so a page declaring a platform-gated
# autumn-web with its features was rejected while carrying a complete enabling
# instruction. Found by Codex review on #2800.
_TABLE_PREFIX = r'''(?:(?:[a-z][a-z0-9_-]*|'[^'\n]*'|"[^"\n]*")\.)*'''


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
        # A `features = […]` array TIED TO autumn-web. Untied, any dependency's
        # array satisfied the gate: `skills/autumn-web/references/
        # api-reference.md:1303` carries `axum = { version = "0.8", features =
        # ["macros", "ws"] }`, which enables axum's websocket support and does
        # nothing for autumn-web's `ws`. `ws`, `mail`, `tls`, `redis`,
        # `openapi`, `markdown` and `csv` are all ordinary feature names in
        # other crates, so this was a standing false-negative channel rather
        # than one page's bad luck. Found by Codex review on #2800.
        #
        # Two spellings, both live in the corpus: the inline table
        # `autumn-web = { version = "0.7", features = [ … ] }` (the array may
        # wrap over lines and carry comments, which `SKILL.md` does), and the
        # `[dependencies.autumn-web]` section with the array beneath it. The
        # window is bounded so a later, unrelated dependency cannot be read as
        # autumn-web's.
        # TOML has two string forms and Cargo reads both, so a page writing
        # `features = ['ws']` carries a complete enabling instruction. Matching
        # only the double-quoted form rejected it. Found by Codex review on
        # #2800.
        # `[dev-dependencies.autumn-web]` and `[build-dependencies.…]` are
        # dependency tables too — the first is where test documentation
        # naturally puts `test-support` — and requiring the bare segment
        # `dependencies` rejected both. Found by Codex review on #2800.
        #
        # Cargo does not treat `-` and `_` as interchangeable in a dependency
        # table KEY, so `autumn_web = { … }` is autumn-web only when it also
        # carries `package = "autumn-web"`. That spelling is real — this
        # repository's own CHANGELOG documents `autumn generate auth` learning
        # to patch it — and requiring the literal `autumn-web` key rejected a
        # page carrying a complete enabling instruction. Found by Codex review
        # on #2800.
        rf'autumn-web\s*=\s*\{{[^}}]*features\s*=\s*\[[^\]]*[\'"]{name}[\'"]',
        rf'autumn_web\s*=\s*\{{(?=[^}}]*package\s*=\s*[\'"]autumn-web[\'"])'
        rf'[^}}]*features\s*=\s*\[[^\]]*[\'"]{name}[\'"]',
        rf'\[{_TABLE_PREFIX}(?:dev-|build-)?dependencies\.autumn-web\]'
        rf'[^\[]*?'
        rf'features\s*=\s*\[[^\]]*[\'"]{name}[\'"]',
        rf'\[{_TABLE_PREFIX}(?:dev-|build-)?dependencies\.autumn_web\]'
        rf'(?=[^\[]*package\s*=\s*[\'"]autumn-web[\'"])'
        rf'[^\[]*?features\s*=\s*\[[^\]]*[\'"]{name}[\'"]',
        # A `--features` flag belongs to the package the COMMAND selects, and
        # the corpus runs `cargo install diesel_cli --no-default-features
        # --features postgres` three times. Round 9 tied dependency ARRAYS to
        # autumn-web and left this alone, on the stated grounds that the flag
        # "names the feature without claiming a crate" — which is wrong: that
        # is exactly what `cargo install <crate>` and `-p <pkg>` do. Found by
        # Codex review on #2800, one round after the array fix it belonged
        # with.
        #
        # `-F` is Cargo's documented short form of `--features`
        # (`-F, --features <FEATURES>`), so `cargo build -F ws` is a complete
        # instruction and rejecting it was a false positive. Found by Codex
        # review on #2800.
        #
        # The command counts unless it explicitly selects a package that is not
        # autumn-web. `cargo test -p autumn-web --features test-support`,
        # `cargo add autumn-web --features constela` and `autumn build
        # --features acme` (the framework's own CLI, building the reader's app)
        # all count; `cargo install diesel_cli --features postgres` does not.
        #
        # `--features autumn-web/tls` is the dependency-qualified spelling, and
        # it is not hypothetical: `docs/guide/tls.md:71` and `:334` are the
        # corpus's own fallback instruction for a reader who has not declared
        # the forwarding `[features]` row. Requiring a delimiter immediately
        # before the bare name rejected a complete instruction this repository
        # publishes. The qualifier has to be autumn-web's own — `--features
        # diesel/postgres` still names nothing of autumn-web's. Found by Codex
        # review on #2800.
        #
        # `-Fws` attaches the value straight to the short flag, which Cargo
        # accepts (`-F, --features <FEATURES>`) and before which there is no
        # delimiter at all — so the separator is optional after `-F` and
        # required after `--features`. Found by Codex review on #2800, one
        # round after `-F` itself was added: the flag reached the command
        # parser and not the naming rule, the fourth time in this review that a
        # fix landed on one of two code paths.
        #
        # One alternation rather than two patterns because this is the most
        # expensive row in the tuple by an order of magnitude — 0.06s per pass
        # against 0.002s for a dependency table — and it runs once per (page,
        # feature) pair. A second copy of it cost the whole gate 20%.
        rf'(?m)^(?:[^\n]*?\$\s*)?(?!{_FOREIGN_PKG})'
        rf'[^\n]*(?:--features[^\n]*(?:[",\s=]|^)'
        rf'|(?<![-\w])-F(?:[^\n]*(?:[",\s=]|^))?)'
        rf'(?:autumn[-_]web/)?{name}(?:[",\s]|$)',
        rf'`{name}`(?:\s+Cargo)?\s+features?\b',
        rf'\bfeatures?\b(?:\s+flag)?\s+`{name}`',
        # A `[features]` table row in the reader's OWN manifest counts only
        # when it forwards: `ws = ["autumn-web/ws"]`. Matching the row name
        # alone accepted `ws = ["dep:tokio-stream"]`, a local feature that
        # activates nothing of autumn-web's. Found by Codex review on #2800,
        # the third spelling in this tuple to need the same tie.
        rf'^\s*[a-z0-9_-]+\s*=\s*\[[^\]]*[\'"]autumn[-_]web/{name}[\'"]',
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
    _closure, _declared, graph = default_features(root)
    enabled_by = enablers(graph)
    needed = set().union(*(f for f, _ in gated.values()))
    problems = []
    checked = 0
    waived = 0
    late = []
    for rel in corpus(root):
        text = (pathlib.Path(root) / rel).read_text(
            encoding='utf-8', errors='ignore')
        # No prefilter. `check()` once skipped any page containing neither
        # `autumn_web` nor `#[`, which `--list` did not — so a fence whose only
        # gated construct was a bare `embed_static!()` was reported by `--list`
        # and silently accepted by CI. A fast path that disagrees with the
        # listing is worse than no fast path: it makes the gate's own
        # diagnostic wrong about the gate. The whole corpus is 212 files and
        # the run is under a second. Found by Codex review on #2800.
        # Blanked ONCE, and used for both halves. The first version blanked
        # inside `uses()` and passed the raw text to the naming check, so a page
        # carrying a live `autumn_web::pdf` fence and a hidden
        # `<!-- features = ["pdf"] -->` reported zero defects: the gate accepted
        # as the reader's enabling line a string the reader cannot see. A
        # comment renders as nothing, so it must satisfy nothing. The waiver
        # scan reads the RAW text, since a waiver is a comment by construction.
        # Caught by Codex review on #2800.
        blanked = blank_comments(text)
        covered = waived_lines(text)
        first = {}
        for lineno, feature, shown in uses(blanked, gated):
            checked += 1
            if lineno in covered.get(feature, ()):
                waived += 1
                continue
            first.setdefault(feature, (lineno, shown))
        if not first:
            continue
        # Naming an IMPLYING feature counts: see `enablers()`.
        active = activation_lines(blanked, needed, enabled_by)
        for feature, (lineno, shown) in sorted(first.items()):
            named = active.get(feature)
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
    closure, _declared, _graph = default_features(ROOT)
    gated = surface(ROOT)
    print(f'default feature closure ({len(closure)}): '
          f'{", ".join(sorted(closure))}')
    print(f'items behind a non-default feature: {len(gated)}')
    by_feature = {}
    for name, (features, kinds) in gated.items():
        # An item needing two non-default features is listed under each, with
        # the other named beside it, so `--surface` shows the conjunction
        # rather than hiding half of it under one heading.
        for feature in sorted(features):
            also = sorted(features - {feature})
            suffix = f'  (also needs {", ".join(also)})' if also else ''
            by_feature.setdefault(feature, []).append(
                ('+'.join(sorted(kinds)), f'{name}{suffix}'))
    for feature in sorted(by_feature):
        print(f'  {feature}')
        for kinds, name in sorted(by_feature[feature]):
            print(f'    {kinds:13s} {name}')
    return 0


def list_uses():
    gated = surface(ROOT)
    _closure, _declared, graph = default_features(ROOT)
    enabled_by = enablers(graph)
    needed = set().union(*(f for f, _ in gated.values()))
    total = 0
    for rel in corpus(ROOT):
        text = (pathlib.Path(ROOT) / rel).read_text(
            encoding='utf-8', errors='ignore')
        blanked = blank_comments(text)
        covered = waived_lines(text)
        rows = []
        active = activation_lines(blanked, needed, enabled_by)
        for lineno, feature, shown in uses(blanked, gated):
            named = active.get(feature)
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
        'ws': ({'ws'}, {'module', 'attribute'}),
        'channels': ({'ws'}, {'module'}),
        'pdf': ({'pdf'}, {'module'}),
        'storage': ({'storage'}, {'module'}),
        'mailer': ({'mail'}, {'attribute'}),
        # The conjunction shape: two non-default requirements on one item.
        'presence_stream': ({'presence', 'ws'}, {'item'}),
        # One level down, under a DEFAULT parent — the case that is invisible
        # from the head segment alone.
        'db::sqlite_types': ({'sqlite'}, {'module'}),
        # A nested module whose requirements include the parent's.
        'storage::variant': ({'storage', 'variants'}, {'module'}),
        # A gated ITEM inside an UNCONDITIONAL module — the openapi case.
        'openapi::Parameter': ({'openapi'}, {'item'}),
        # Items reachable only through a brace group, and a bang macro.
        'Mail': ({'mail'}, {'item'}),
        'Mailer': ({'mail'}, {'item'}),
        'embed_static': ({'embed-assets'}, {'bang'}),
        # A prelude-glob type, and one that must stay unread.
        'Presence': ({'presence'}, {'item'}),
        # The one-letter bang macro. Reachable because `\b` excludes
        # `assert!`/`insert!`/`vec!` on its own — no length floor needed.
        't': ({'i18n'}, {'bang'}),
    }

    def found(text):
        # `uses()` takes ALREADY-BLANKED text, the way `check()` hands it over.
        return sorted(uses(blank_comments(text), gated))

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

    # `cfg(any(test, feature = "X"))`: `cfg(test)` is set for the crate being
    # tested, never for its dependencies, so a reader genuinely needs `X`.
    expect('any(test, feature) yields the feature',
           _cfg_requirement(
               ['#[cfg(any(test, feature = "test-support"))]'], 0)[0],
           {'test-support'})
    # A `not(…)` is a build constraint; the positive conjuncts beside it are
    # still required. `capsule::build_recording_pool` is the live case.
    expect('positive conjuncts survive a not() sibling',
           _cfg_requirement(['#[cfg(all(feature = "test-support", '
                             'feature = "db", not(feature = "sqlite")))]'],
                            0)[0],
           {'test-support', 'db'})
    expect('a gate that is only not() still yields nothing',
           _cfg_requirement(['#[cfg(not(feature = "seed"))]'], 0)[0], None)
    expect('...even with several not() predicates',
           _cfg_requirement(
               ['#[cfg(all(not(feature = "a"), not(feature = "b")))]'],
               0)[0], None)
    expect('a genuine feature disjunction is still dropped',
           _cfg_requirement(
               ['#[cfg(any(feature = "a", feature = "b"))]'], 0)[0], None)
    expect('any() with no feature at all yields nothing',
           _cfg_requirement(['#[cfg(any(test, doc))]'], 0)[0], None)
    # `\bt!` matches none of these, which is why the length floor came off.
    expect('a one-letter bang macro does not match other macros',
           found('```rust\nassert!(x);\ninsert!(y);\nvec![1];\n```\n'), [])
    expect('...but does match its own call',
           found('```rust\nlet s = t!(locale, "welcome.title");\n```\n'),
           [(2, 'i18n', 't!')])

    # A DIRECT entry in a module-qualified group — the commonest import shape
    # in the corpus, and one that used to fall back to the head alone.
    expect('a direct entry in a module group resolves under the head',
           sorted(set(found(
               '```rust\nuse autumn_web::openapi::{Parameter};\n```\n'))),
           [(2, 'openapi', 'autumn_web::openapi::Parameter')])
    # An `as` alias renames the entry; it does not change what is imported.
    expect('an aliased child module still resolves',
           sorted(set(found(
               '```rust\nuse autumn_web::storage::{variant as variants_api};'
               '\n```\n'))),
           [(2, 'storage', 'autumn_web::storage'),
            (2, 'storage', 'autumn_web::storage::variant'),
            (2, 'variants', 'autumn_web::storage::variant')])
    expect('a mixed group resolves both shapes',
           sorted(set(found(
               '```rust\nuse autumn_web::storage::{Blob, variant::{Transform}};'
               '\n```\n'))),
           [(2, 'storage', 'autumn_web::storage'),
            (2, 'storage', 'autumn_web::storage::variant'),
            (2, 'variants', 'autumn_web::storage::variant')])

    # An uppercase second segment names an item, not a module, and resolves the
    # same way. `pub mod openapi;` is unconditional; `Parameter` is not.
    expect('a gated item under an ungated module is read',
           found('```rust\nuse autumn_web::openapi::Parameter;\n```\n'),
           [(2, 'openapi', 'autumn_web::openapi::Parameter')])
    expect('an ungated item under the same module reports nothing',
           found('```rust\nuse autumn_web::openapi::ApiDoc;\n```\n'), [])

    # A group hanging off a MODULE segment, not off `autumn_web::` itself.
    expect('a group after a module segment resolves the inner child',
           sorted(set(found(
               '```rust\nuse autumn_web::storage::{variant::{Transform}};\n```\n'))),
           [(2, 'storage', 'autumn_web::storage'),
            (2, 'storage', 'autumn_web::storage::variant'),
            (2, 'variants', 'autumn_web::storage::variant')])

    # A bare type name a prelude glob brought into scope. Scoped to a fence
    # that writes the glob, and to names starting uppercase.
    glob = '```rust\nuse autumn_web::prelude::*;\n'
    expect('bare gated type read after a prelude glob',
           found(glob + 'async fn v(p: Presence) {}\n```\n'),
           [(3, 'presence', 'Presence')])
    expect('no glob, no bare-name scan',
           found('```rust\nasync fn v(p: Presence) {}\n```\n'), [])
    expect('a lowercase name is never read bare',
           found(glob + 'let x = storage;\n```\n'), [])
    expect('an ungated type is not read bare',
           found(glob + 'let x: Widget = w;\n```\n'), [])
    # The direction that matters: `presence = ["ws"]`, so naming `ws` does NOT
    # supply `presence`. This is the live defect on `docs/guide/presence.md`.
    expect('naming the implied feature does not satisfy the implying one',
           names_feature('features = ["ws"]', 'presence'), False)

    # Ordinary use-tree syntax. `PATH_USE` wants an identifier straight after
    # `::` and sees `{`, so every name in a group went unread.
    expect('grouped import read',
           found('```rust\nuse autumn_web::{Mail, Mailer};\n```\n'),
           [(2, 'mail', 'autumn_web::Mail'),
            (2, 'mail', 'autumn_web::Mailer')])
    # An UPPERCASE child in a root group names a gated item under an
    # unconditional module — the group form of round 8's `PATH_USE` widening.
    expect('an uppercase child in a root group resolves',
           found('```rust\nuse autumn_web::{openapi::Parameter};\n```\n'),
           [(2, 'openapi', 'autumn_web::openapi::Parameter')])
    expect('a nested entry inside a group resolves its own segment',
           found('```rust\nuse autumn_web::{pdf::Pdf};\n```\n'),
           [(2, 'pdf', 'autumn_web::pdf')])
    expect('an ungated group reports nothing',
           found('```rust\nuse autumn_web::{get, post};\n```\n'), [])
    expect('a group outside a rust fence is not read',
           found('`use autumn_web::{Mail, Mailer};`\n'), [])

    # rustfmt breaks a group across lines as soon as it is long enough, and a
    # line-at-a-time reader sees neither end of one. The corpus writes six.
    # Reported against the line the `use` STARTS on.
    expect('a multi-line root group is read',
           found('```rust\nuse autumn_web::{\n    Mail,\n    Mailer,\n};\n```\n'),
           [(2, 'mail', 'autumn_web::Mail'),
            (2, 'mail', 'autumn_web::Mailer')])
    # The invariant that matters for the module-segment forms is that breaking
    # a group over lines changes NOTHING: same features, same shown paths, same
    # line, same duplicates. Asserted against the single-line twin rather than
    # a hand-written list, so it cannot drift away from the shape it mirrors.
    expect('a multi-line module group reads as its single-line twin',
           found('```rust\nuse autumn_web::storage::{\n'
                 '    variant::Transform,\n};\n```\n'),
           found('```rust\nuse autumn_web::storage::{variant::Transform};\n'
                 '```\n'))
    expect('a multi-line nested group reads as its single-line twin',
           found('```rust\nuse autumn_web::storage::{\n    blob::Blob,\n'
                 '    variant::{\n        Transform,\n    },\n};\n```\n'),
           found('```rust\nuse autumn_web::storage::'
                 '{blob::Blob, variant::{Transform}};\n```\n'))
    # ...and that the twin it mirrors reaches the inner segment at all.
    expect('...and that twin reaches `variants`',
           sorted({f for _, f, _ in found(
               '```rust\nuse autumn_web::storage::{\n'
               '    variant::Transform,\n};\n```\n')}),
           ['storage', 'variants'])
    expect('a multi-line uppercase child resolves',
           found('```rust\nuse autumn_web::{\n    openapi::Parameter,\n};\n```\n'),
           [(2, 'openapi', 'autumn_web::openapi::Parameter')])
    expect('an ungated multi-line group reports nothing',
           found('```rust\nuse autumn_web::{\n    get,\n    post,\n};\n```\n'),
           [])
    # The join is ended by brace depth AND by the fence, so an unclosed group
    # cannot swallow the snippet after it.
    expect('an unclosed multi-line group stops at the fence',
           found('```rust\nuse autumn_web::{\n    Mail,\n```\n\nprose\n\n'
                 '```rust\nuse autumn_web::pdf::Pdf;\n```\n'),
           [(9, 'pdf', 'autumn_web::pdf')])
    # A `use` with no group is not joined, so the line after it keeps its own
    # number.
    expect('consecutive single-line uses keep their own lines',
           found('```rust\nuse autumn_web::pdf::Pdf;\n'
                 'use autumn_web::channels::Channels;\n```\n'),
           [(2, 'pdf', 'autumn_web::pdf'), (3, 'ws', 'autumn_web::channels')])

    # A bang macro exported from the crate root under a `#[cfg]`. The three-
    # character floor keeps `t!` out by construction; membership in the `bang`
    # set keeps everything else out.
    expect('bang macro read, qualified',
           found('```rust\nlet d = autumn_web::embed_static!();\n```\n'),
           [(2, 'embed-assets', 'autumn_web::embed_static'),
            (2, 'embed-assets', 'embed_static!')])
    expect('bang macro read, bare',
           found('```rust\nlet d = embed_static!();\n```\n'),
           [(2, 'embed-assets', 'embed_static!')])
    expect('a module name is not a bang macro',
           found('```rust\nlet x = storage!();\n```\n'), [])
    expect('an ordinary macro call is not reported',
           found('```rust\nassert!(x);\nvec![1];\nprintln!("hi");\n```\n'),
           [])

    # A fence inside a `>` callout is still a fence. Every line carries the
    # prefix — opener, code and closer — so a pattern anchored on whitespace
    # alone never opens it and the snippet is skipped in silence.
    expect('blockquoted fence read',
           found('> ```rust\n> use autumn_web::pdf::Pdf;\n> ```\n'),
           [(2, 'pdf', 'autumn_web::pdf')])
    expect('nested blockquote read',
           found('> > ```rust\n> > use autumn_web::pdf::Pdf;\n> > ```\n'),
           [(2, 'pdf', 'autumn_web::pdf')])
    expect('a blockquoted fence still closes',
           found('> ```rust\n> let x = 1;\n> ```\n\n`autumn_web::pdf`\n'), [])

    # A second path segment is resolved BEFORE the head, because a gate one
    # level down can be invisible from the first — and worst when the parent is
    # a DEFAULT feature, as `db` is.
    expect('a nested gated module is reported under its own feature',
           found('```rust\nuse autumn_web::db::sqlite_types::SqliteUuid;\n```\n'),
           [(2, 'sqlite', 'autumn_web::db::sqlite_types')])
    expect('an ungated second segment falls back to the head',
           found('```rust\nuse autumn_web::pdf::Pdf;\n```\n'),
           [(2, 'pdf', 'autumn_web::pdf')])
    expect('a path with no gated segment is not reported',
           found('```rust\nuse autumn_web::db::Db;\n```\n'), [])

    # A comment renders as nothing, so it can neither show a construct NOR
    # name a feature. The second half is the regression: `uses()` blanked and
    # the naming check did not, so a page with a live `autumn_web::pdf` fence
    # and a hidden `<!-- features = ["pdf"] -->` passed the gate while showing
    # the reader nothing. Asserted through `check()`-shaped inputs, not on the
    # helper alone — the helper was already right; the call path was not.
    expect('comment blanked',
           found('```rust\n<!-- autumn_web::pdf -->\n```\n'), [])
    # Both halves use a properly SCOPED array, so this pair isolates the
    # comment-blanking behaviour rather than re-testing the autumn-web tie.
    enabling = 'autumn-web = { version = "0.7", features = ["pdf"] }'
    hidden = ('```rust\nuse autumn_web::pdf::Pdf;\n```\n'
              f'\n<!-- {enabling} -->\n')
    expect('a hidden enabling line does not satisfy the naming rule',
           names_feature(blank_comments(hidden), 'pdf'), False)
    expect('...while the same line in view does',
           names_feature(
               blank_comments(f'```toml\n{enabling}\n```\n'), 'pdf'),
           True)
    expect('a conjunction is reported once per non-default feature',
           found('```rust\nuse autumn_web::presence_stream;\n```\n'),
           [(2, 'presence', 'autumn_web::presence_stream'),
            (2, 'ws', 'autumn_web::presence_stream')])

    # The naming rule.
    for spelling in (
            'autumn-web = { version = "0.7", features = ["ws"] }',
            'autumn-web = { version = "0.7", features = [\n'
            '    "mail",  # email\n    "ws",\n] }',
            '[dependencies.autumn-web]\nversion = "0.7"\n'
            'features = ["ws"]\n',
            'cargo build --features ws',
            'the `ws` feature',
            'the `ws` Cargo feature',
            'gated behind the feature `ws`',
            'behind the feature flag `ws`',
            'ws = ["autumn-web/ws"]',
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
           names_feature(
               'autumn-web = { features = ["ws-compat"] }', 'ws'), False)
    # A features array belonging to ANOTHER crate enables that crate, not this
    # one. `api-reference.md:1303` carries exactly this axum line.
    expect('another crate\'s features array does not satisfy the gate',
           names_feature(
               'axum = { version = "0.8", features = ["macros", "ws"] }', 'ws'),
           False)
    expect('...even directly beside an autumn-web dependency',
           names_feature(
               'autumn-web = { version = "0.7" }\n'
               'axum = { version = "0.8", features = ["ws"] }\n', 'ws'),
           False)
    expect('a bare features array with no crate does not satisfy it either',
           names_feature('features = ["ws"]', 'ws'), False)

    # A `[features]` row in the reader's own manifest counts only when it
    # FORWARDS. The row name alone says nothing about autumn-web.
    expect('a forwarding feature row counts',
           names_feature('ws = ["autumn-web/ws"]', 'ws'), True)
    expect('...under any local name',
           names_feature('realtime = ["autumn-web/ws"]', 'ws'), True)
    expect('a same-named local feature that forwards nothing does not',
           names_feature('ws = ["dep:tokio-stream"]', 'ws'), False)
    expect('an empty same-named local feature does not either',
           names_feature('ws = []', 'ws'), False)

    # TOML has two string forms and Cargo reads both.
    expect('a literal-string array counts',
           names_feature(
               "autumn-web = { version = '0.7', features = ['ws'] }", 'ws'),
           True)
    expect('...in the section spelling too',
           names_feature("[dependencies.autumn-web]\nfeatures = ['ws']\n",
                         'ws'), True)
    expect('...and in a forwarding row',
           names_feature("realtime = ['autumn-web/ws']", 'ws'), True)
    # Cargo does not normalise `-`/`_` in a dependency KEY, so the underscore
    # spelling is autumn-web only alongside `package = "autumn-web"`.
    # `dev-`/`build-dependencies` are dependency tables too, and `-F` is
    # Cargo's short form of `--features`.
    for spelling, feature, want in (
            ('[dev-dependencies.autumn-web]\nfeatures = ["ws"]\n',
             'ws', True),
            ('[build-dependencies.autumn-web]\nfeatures = ["ws"]\n',
             'ws', True),
            ('[dev-dependencies.autumn_web]\npackage = "autumn-web"\n'
             'features = ["ws"]\n', 'ws', True),
            ('[dev-dependencies.axum]\nfeatures = ["ws"]\n', 'ws', False),
            ('cargo build -F ws', 'ws', True),
            ('cargo test -p autumn-web -F test-support', 'test-support', True),
            ('cargo install diesel_cli -F postgres', 'postgres', False),
            ('cargo add axum -F ws', 'ws', False),
            ('some--Fws thing', 'ws', False),
            # `-F` takes its value attached as well as separated, and no
            # delimiter precedes the name in that spelling.
            ('cargo build -Fws', 'ws', True),
            ('cargo build -Fautumn-web/ws', 'ws', True),
            ('cargo install diesel_cli -Fpostgres', 'postgres', False),
            # The dependency-qualified spelling, which the corpus itself
            # publishes at `docs/guide/tls.md:71` and `:334` as the fallback
            # for a reader with no forwarding `[features]` row.
            ('cargo run --features autumn-web/tls', 'tls', True),
            ('cargo run --features autumn_web/tls', 'tls', True),
            ('cargo build --release --features autumn-web/acme', 'acme', True),
            # The qualifier has to be autumn-web's own.
            ('cargo run --features diesel/postgres', 'postgres', False),
            ('cargo run --features tokio/ws', 'ws', False),
            # Platform-specific dependency tables are dependency tables.
            ("[target.'cfg(unix)'.dependencies.autumn-web]\n"
             'features = ["ws"]\n', 'ws', True),
            ("[target.'cfg(unix)'.dev-dependencies.autumn-web]\n"
             'features = ["ws"]\n', 'ws', True),
            ('[target."cfg(windows)".dependencies.autumn-web]\n'
             'features = ["ws"]\n', 'ws', True),
            ('[target.x86_64-pc-windows-msvc.dependencies.autumn-web]\n'
             'features = ["ws"]\n', 'ws', True),
            ("[target.'cfg(unix)'.dependencies.autumn_web]\n"
             'package = "autumn-web"\nfeatures = ["ws"]\n', 'ws', True),
            # ...and a platform-specific table for a DIFFERENT crate is still
            # a different crate.
            ("[target.'cfg(unix)'.dependencies.axum]\nfeatures = [\"ws\"]\n",
             'ws', False)):
        if names_feature(spelling, feature) != want:
            failures.append(
                f'naming: {spelling!r} / {feature} -> '
                f'{names_feature(spelling, feature)}, want {want}')

    expect('a renamed dependency key counts',
           names_feature(
               'autumn_web = { package = "autumn-web", features = ["ws"] }',
               'ws'), True)
    expect('...in the section spelling too',
           names_feature('[dependencies.autumn_web]\n'
                         'package = "autumn-web"\nfeatures = ["ws"]\n',
                         'ws'), True)
    expect('an underscore key WITHOUT the rename is a different crate',
           names_feature(
               'autumn_web = { version = "0.7", features = ["ws"] }', 'ws'),
           False)
    expect('another crate\'s literal-string array still does not',
           names_feature("axum = { version = '0.8', features = ['ws'] }",
                         'ws'), False)

    # A `--features` flag belongs to the package the command SELECTS. Every
    # accepting row here is a real corpus line; every rejecting one is a shape
    # the corpus writes (`cargo install diesel_cli …` appears three times) or
    # the obvious way to fake it.
    for command, feature, want in (
            ('cargo test -p autumn-web --features test-support',
             'test-support', True),
            ('cargo add autumn-web --features constela', 'constela', True),
            ('$ AUTUMN_ENV=production autumn build --embed --features acme',
             'acme', True),
            ('cargo run -p autumn-web --release --features sim-testing',
             'sim-testing', True),
            ('cargo build --features ws', 'ws', True),
            ('  --features "ws,mail,offline-sync" \\', 'ws', True),
            ('cargo install diesel_cli --no-default-features '
             '--features postgres', 'postgres', False),
            ('cargo install diesel_cli --features ws', 'ws', False),
            ('cargo test -p some-other-crate --features ws', 'ws', False),
            ('cargo build --package other --features mail', 'mail', False),
            # autumn-cli is a different package from autumn-web.
            ('cargo install autumn-cli --features ws', 'ws', False),
            # `cargo add [OPTIONS] <DEP>…` selects a package the same way.
            ('cargo add autumn-web --features constela', 'constela', True),
            ('cargo add axum --features ws', 'ws', False),
            ('cargo add tokio --features ws', 'ws', False),
            # OPTIONS precede the dependency in `cargo add [OPTIONS] <DEP>…`.
            ('cargo add --optional autumn-web --features ws', 'ws', True),
            ('cargo add --no-default-features autumn-web --features ws',
             'ws', True),
            ('cargo install --locked autumn-cli --features ws', 'ws', False),
            ('cargo add --optional axum --features ws', 'ws', False)):
        if names_feature(command, feature) != want:
            failures.append(
                f'--features scope: {command!r} / {feature} -> '
                f'{names_feature(command, feature)}, want {want}')
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
    closure, declared, graph = default_features(ROOT)
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
    for name, features, kinds in (
            ('ws', {'ws'}, {'module', 'attribute'}),
            ('pdf', {'pdf'}, {'module'}),
            ('mailer', {'mail'}, {'attribute'}),
            ('storage', {'storage'}, {'module'}),
            ('managed_pg', {'managed-pg'}, {'module'}),
            # `all(feature = "presence", feature = "maud")` — `maud` is
            # default, so only `presence` survives into the requirement.
            ('presence_badge', {'presence'}, {'item'}),
            # The line-wrapped four-predicate conjunction, of which two are
            # non-default. Missing this shape entirely is the defect Codex
            # review found on #2800: a fence could name `presence_stream` and
            # this gate would vouch for it.
            ('presence_stream', {'presence', 'ws'}, {'item'})):
        if real.get(name) != (features, kinds):
            failures.append(
                f'truth set: {name} -> {real.get(name)!r}, want '
                f'{(features, kinds)!r}')
    # A whole crate re-exported under a new name — `pub use autumn_edge as
    # edge;` — has no `::` for the two `pub use` patterns to bite on, so it
    # parsed as nothing and `autumn_web::edge::…` was ungated.
    if real.get('edge') != ({'edge'}, {'module'}):
        failures.append(
            f'truth set: edge -> {real.get("edge")!r}, want '
            f'({{\'edge\'}}, {{\'module\'}}) — `pub use autumn_edge as edge;`')

    # Naming an IMPLYING feature is enough, because Cargo activates what it
    # implies. `presence = ["ws"]`, so a page pinning only `presence` beside a
    # `presence_stream` snippet compiles, and reporting `ws` missing there is
    # the gate telling an author to break a page that works.
    if graph.get('presence') != ['ws']:
        failures.append(
            f'manifest: presence -> {graph.get("presence")!r}, want [\'ws\'] '
            f'— the implication this case is built on has moved')
    enabled_by = enablers(graph)
    expect('an implying feature enables the implied one',
           'presence' in enabled_by.get('ws', set()), True)
    expect('implication does not run backwards',
           'ws' in enabled_by.get('presence', set()), False)
    expect('a feature always enables itself',
           'ws' in enabled_by.get('ws', set()), True)
    page = 'autumn-web = { version = "0.7", features = ["presence"] }\n'
    active = activation_lines(page, {'presence', 'ws'}, enabled_by)
    expect('naming `presence` satisfies `ws`', active.get('ws'), 1)
    expect('naming `presence` satisfies `presence`', active.get('presence'), 1)
    expect('naming `ws` alone does not satisfy `presence`',
           activation_lines('features = ["ws"]\n', {'presence'},
                            enabled_by).get('presence'), None)

    # The proc-macro kind map, parsed rather than guessed. Two of the six names
    # the header once called attribute macros are bang macros.
    kinds = proc_macro_kinds(ROOT)
    for name, want in (('ws', 'attribute'), ('mailer', 'attribute'),
                       ('t', 'bang'), ('mail_previews', 'bang'),
                       ('wire_client', 'bang'), ('routes', 'bang')):
        if kinds.get(name) != want:
            failures.append(
                f'proc-macro kind: {name} -> {kinds.get(name)!r}, want {want!r}')
    # `t` reaches the crate root as `pub use crate::i18n::t`, a plain
    # re-export — so only the kind map can tell it is callable as `t!`.
    got = real.get('t')
    if got is None or got[0] != {'i18n'} or 'bang' not in got[1]:
        failures.append(
            f'truth set: t -> {got!r}, want features {{\'i18n\'}} and kind bang')

    # Gated `macro_rules!` exports, against the real crate. Neither is reachable
    # as a module or a `pub use`, so both were discarded before this.
    for name, features in (('embed_static', {'embed-assets'}),
                           ('embed_locales', {'embed-assets', 'i18n'})):
        got = real.get(name)
        if got is None or got[0] != features or 'bang' not in got[1]:
            failures.append(
                f'truth set: {name} -> {got!r}, want features {features!r} '
                f'and kind bang')
    # Crate-internal plumbing a seed macro expands into is not a call a reader
    # writes.
    if '__autumn_register_fake_seeder' in real:
        failures.append(
            'truth set: __autumn_register_fake_seeder is internal plumbing '
            'and must not be reported as a macro a reader calls')

    # COMPLEMENTARY cfg arms. `autumn/src/db.rs` defines `RuntimeConnection`
    # under both `not(feature = "sqlite")` and `feature = "sqlite"`, so it
    # exists in a default build — recording it as needing `sqlite` would tell a
    # reader on the ordinary Postgres path to swap their database backend.
    for name in ('db::RuntimeConnection', 'db::RuntimeBackend'):
        if name in real:
            failures.append(
                f'truth set: {name} -> {real[name]!r}, but it is also declared '
                f'under a `not(…)` arm and exists in a DEFAULT build; '
                f'reporting it would be a false positive')
    expect('an unconditional declaration strips a gated sibling',
           _strip_unconditional({'a::B': ({'x'}, {'item'}),
                                 'a::C': ({'x'}, {'item'})},
                                {('a::B', 'type')}),
           {'a::C': ({'x'}, {'item'})})
    # A name in BOTH namespaces keeps the half that is still gated: `edge` is
    # an ungated attribute macro AND a gated module re-export.
    expect('an ungated macro does not strip a gated module of the same name',
           _strip_unconditional({'edge': ({'edge'}, {'module'})},
                                {('edge', 'macro')}),
           {'edge': ({'edge'}, {'module'})})

    # Visibility. `pub(crate) mod session_redis;` (`lib.rs:632`) is gated by
    # `redis` and is NOT a path a reader can write — enabling `redis` would not
    # make it resolve downstream — so demanding the feature for it would be a
    # false positive the reader could do nothing about.
    expect('a bare `pub` module is public', _is_public('pub '), True)
    expect('`pub(crate)` is not', _is_public('pub(crate) '), False)
    expect('`pub(super)` is not', _is_public('pub(super) '), False)
    expect('no visibility at all is not', _is_public(None), False)
    if 'session_redis' in real:
        failures.append(
            f'truth set: session_redis -> {real["session_redis"]!r}, but '
            f'`lib.rs` declares it `pub(crate)` — a reader cannot write that '
            f'path under any feature set')
    # ...and the crate-private declaration must still CONSUME its `#[cfg]`,
    # rather than letting the gate fall through onto whatever follows. `redis`
    # really does gate `session_redis`, so a leak would show up as the next
    # public declaration wrongly requiring it.
    def scan(*lines):
        """`gated_items()` over a synthetic crate root, for shape questions."""
        with tempfile.TemporaryDirectory() as tmp:
            src = pathlib.Path(tmp) / 'autumn' / 'src'
            src.mkdir(parents=True)
            (src / 'lib.rs').write_text('\n'.join(lines) + '\n')
            (src / 'prelude.rs').write_text('')
            return gated_items(tmp, {})

    expect('a private module still swallows its own gate',
           scan('#[cfg(feature = "redis")]',
                'pub(crate) mod session_redis;',
                '#[cfg(feature = "pdf")]',
                'pub mod pdf;'),
           {'pdf': ({'pdf'}, {'module'})})
    expect('...while its public twin is recorded',
           scan('#[cfg(feature = "redis")]',
                'pub mod session_redis;'),
           {'session_redis': ({'redis'}, {'module'})})
    expect('a private module does not vouch for a gated public namesake',
           scan('mod edge;',
                '#[cfg(feature = "edge")]',
                'pub use autumn_edge as edge;'),
           {'edge': ({'edge'}, {'module'})})

    # `metrics::testing` is `#[cfg(any(test, feature = "test-support"))]`, and
    # `docs/guide/metrics.md:465` hands it to the reader.
    got = real.get('metrics::testing')
    if got is None or got[0] != {'test-support'}:
        failures.append(
            f'truth set: metrics::testing -> {got!r}, want features '
            f'{{\'test-support\'}} — `any(test, feature = …)`')

    # Gated public ITEMS one level down, not just gated `pub mod` children.
    # `openapi` the module is unconditional; `openapi::Parameter` is gated, and
    # `docs/migrations/next.md:259` imported it on a page naming no feature.
    for name, features in (('openapi::Parameter', {'openapi'}),
                           ('sse::stream', {'ws'})):
        got = real.get(name)
        if got is None or got[0] != features:
            failures.append(
                f'truth set: {name} -> {got!r}, want features {features!r}')

    # The nested pass descends into UNCONDITIONAL parents too. `pub mod data;`
    # carries no `#[cfg]`, so seeding from the gated set alone skipped it — and
    # `data::csv` was a live defect behind that gap (`docs/guide/jobs.md:824`).
    roots = root_modules(ROOT)
    for name in ('data', 'db', 'storage'):
        if name not in roots:
            failures.append(
                f'root_modules: {name} missing — the nested pass cannot '
                f'descend into it')
    expect('an unconditional root module carries no requirement',
           roots.get('data'), set())
    expect('a gated root module carries its own',
           roots.get('pdf'), {'pdf'})
    if real.get('data::csv') != ({'csv'}, {'module'}):
        failures.append(
            f'truth set: data::csv -> {real.get("data::csv")!r}, want '
            f'({{\'csv\'}}, {{\'module\'}}) — a gated child under an '
            f'unconditional parent')

    # The nested pass, against the real crate. `db` is a DEFAULT feature, so
    # `db::sqlite_types` exists only because the second segment is resolved.
    for name, features in (('db::sqlite_types', {'sqlite'}),
                           ('storage::variant', {'storage', 'variants'})):
        got = real.get(name)
        if got is None or got[0] != features:
            failures.append(
                f'truth set: {name} -> {got!r}, want features {features!r}')
    # Only `pub mod` is a path a reader can write. 11 of the 40 gated child
    # modules are private `mod tests`, and none of them may appear.
    for name in ('openapi::tests', 'widgets::tests', 'lock::tests'):
        if name in real:
            failures.append(
                f'truth set: {name} is a private module and must not be '
                f'reported as a path')

    # An item whose every requirement is default is not gated surface at all:
    # `live` is behind `all(feature = "htmx", feature = "maud")`.
    if 'live' in real:
        failures.append(
            'truth set: `live` needs only default features and must not be '
            'reported as gated surface')
    # `#[cfg(not(feature = "seed"))]` marks an item that exists when the
    # feature is OFF. Reading the name out of it would tell a reader to enable
    # the one feature that REMOVES the item.
    expect('a `not(…)` gate yields no requirement',
           _cfg_requirement(['#[cfg(not(feature = "seed"))]'], 0)[0], None)
    expect('an `any(…)` gate yields no requirement',
           _cfg_requirement(['#[cfg(any(feature = "a", feature = "b"))]'],
                            0)[0], None)
    expect('a conjunction yields every conjunct',
           _cfg_requirement(['#[cfg(all(feature = "presence", '
                             'feature = "maud"))]'], 0)[0],
           {'presence', 'maud'})
    expect('a line-wrapped conjunction is read whole',
           _cfg_requirement(['#[cfg(all(', '    feature = "presence",',
                             '    feature = "ws",', '))]'], 0)[0],
           {'presence', 'ws'})
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
    features = sorted(set().union(*(f for f, _ in gated.values())))
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
