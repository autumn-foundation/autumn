#!/usr/bin/env bash
# Symbol drift gate: every workspace-crate path the reader-facing docs put in
# front of someone must name an item that exists — and live in a crate a reader
# can actually import.
#
# WHY THIS EXISTS: the corpus already gates the four things a reader copies off
# a page and the one thing they cannot copy at all.
# `scripts/check-docs-links.sh` gates its *links* (a 404),
# `scripts/check-docs-cli.sh` its *commands* (`unrecognized subcommand`),
# `scripts/check-docs-config.sh` the `AUTUMN_*` variables they SET (a silent
# no-op), `scripts/check-docs-toml.sh` the `autumn.toml` keys they WRITE
# (dropped silently), and `scripts/check-docs-orphans.sh` asserts the page can
# be reached at all. Nothing gated the thing the guide is mostly MADE of: Rust.
#
# The reader-facing corpus names 1,495 `autumn_web::…` paths — 864 of them
# inside `rust` fences, the rest in prose a reader reads as authoritative — a
# larger copy-surface than the env layer (689 occurrences) and the `autumn.toml`
# layer (172 fences) combined. A renamed or never-shipped item leaves behind a
# line that looks exactly like a working one, and nothing in the tree could tell
# the difference.
#
# WIDENED FROM `autumn_web::` TO EVERY PUBLISHED CRATE. For its first several
# revisions this gate scanned one prefix, `autumn_web::`, even though it already
# MODELLED `autumn_macros`, `autumn_edge` and `autumn_search` — it had to, to
# follow an `autumn_web` re-export into them. The gap that left is the whole
# reason for the widening: the sibling crates are how a reader adopts a plugin
# (`use autumn_billing::BillingPlugin;`, `use autumn_storage_s3::S3BlobStore;`),
# the corpus writes 67 such paths, and not one of them was a passing check. They
# were not checked at all. A path into a sibling fails exactly the way an
# `autumn_web::` one does — E0432 against the reader's own file — so there was
# never a reason for the two to be gated differently, only an accident of which
# prefix got written first.
#
# Widening it meant seeding resolution with the crate the path NAMES instead of
# `autumn_web` unconditionally. That single hardcoded root was wrong in both
# directions, and the self-test now pins both: a real item in a sibling crate
# resolved against `autumn_web` is a false FAILURE, and — worse — a sibling path
# that happens to collide with an `autumn_web` module would have passed while
# naming something that does not exist where the reader is looking.
#
# AND THE CLASS THAT NO RENAME FIXES: a crate with no library target. See
# `binary_only_crates` — `autumn-cli` is `src/main.rs` and nothing else, so
# `pub` inside it is visible only within the binary. The baseline run of the
# widened gate found the corpus's one instance, and it is the nastiest shape
# here: `docs/guide/accessibility.md` offered a "Programmatic use" section whose
# `use autumn_cli::check::{A11yCheckOptions, run_a11y_check, print_report};`
# names a module that is real, items that are real and really `pub`, in a crate
# `cargo add` installs without complaint. Every signal a reader has says the
# line is fine. What does not exist is a library to import it from, and no
# spelling of the path fixes that — which is why it is reported as its own
# defect class rather than as a dead segment with a `did you mean` hint.
#
# WHERE IT SITS ON THE VISIBILITY SCALE: both ends of it, which is the reason
# to gate the whole surface rather than the import lines alone.
#
# In most positions an unresolved path is a compile error — LOUD, like a bad
# link or a bad command. That does not make it cheap: `rustc` reports it
# against the READER's file, not against the page, so they are told their code
# is wrong when the documentation is, at the first build of a feature they have
# not used before. `docs/guide/maintenance-mode.md` is that case: it hands over
# `use autumn_web::middleware::{MaintenanceLayer, MaintenanceState};` and only
# the first of the two is there. `MaintenanceState` lives in
# `autumn_web::maintenance`, a DIFFERENT module that happens to have a
# same-named sibling under `middleware::maintenance`, so the reader gets E0432
# on a line where half the import is right.
#
# But some positions are SILENT, and they are the ones no reader can defend
# against. `#[autumn_web::main]` parses the function it decorates and emits
# `fn main()` fresh (`autumn-macros/src/main_macro.rs`): `input_fn.sig` is read
# only to check `async`, and the declared return type is never re-emitted. So
# in the FIRST fence of `docs/guide/api-versioning.md`,
#
#     async fn main() -> Result<(), autumn_web::Error> {
#
# the name `autumn_web::Error` never reaches name resolution at all. There is no
# such type — the crate root exports `AutumnError` and `AutumnResult` — and
# the snippet still builds. The reader copies a working example and carries away
# the wrong name for the framework's error type, with nothing anywhere to
# correct them. That is the `AUTUMN_*` failure mode exactly: not a dead end the
# reader can see, but a confident wrong answer they cannot.
#
# WHAT IT CHECKS (single fast job, no Rust toolchain needed):
#   1. Every `autumn_web::a::b::C` path a reader-facing page names resolves
#      through the crate's real module tree to an item that exists — including
#      when the page writes it brace-grouped (`autumn_web::{get, post}`), which
#      is how most import lines in the guide are written — including the 14
#      groups that nest (`storage::{BlobStoreState, variant::{Transform, …}}`)
#      or run across lines, whose symbols a line-at-a-time reader never sees.
#   2. Resolution follows what Rust actually does, not what the source looks
#      like, because a documented path is almost never a path to where the item
#      is DEFINED:
#        - `pub use` re-exports, including aliased ones (`pub use http_client
#          as http` is why `autumn_web::http::Client` is a real path and
#          `autumn_web::http_client::Client` is the one nobody writes),
#        - glob re-exports (`pub use prelude::*`),
#        - re-exports THROUGH a private module, which is the normal facade
#          shape here (`ui/mod.rs` is the only public thing between the reader
#          and `pub const WIDGETS_CSS_PATH` in a private `widgets_css`),
#        - `#[macro_export]` macros, which land at the CRATE ROOT no matter
#          which module they are written in, so `autumn_web::declassify` is
#          correct for a macro defined in `classify/mod.rs`,
#        - inline `mod x { … }` blocks, which is where the feature-gated
#          `db_impl` facades put `Lock`, `LockGuard` and friends,
#        - paths that leave the crate into a sibling in this workspace
#          (`autumn_macros`, `autumn_edge`, `autumn_search`), which is where
#          every attribute macro a handler is decorated with actually lives,
#          including a whole crate re-exported under an alias
#          (`pub use autumn_edge as edge`),
#        - the TYPE namespace winning for traversal where one name is both a
#          module and a value: `pub mod app` beside `pub use app::app`, and
#          `pub use autumn_edge as edge` beside `pub use autumn_macros::edge`.
#          Letting the value win turns every path under it into an unchecked
#          "associated item".
#   3. VISIBILITY, which is the difference between an item existing and a reader
#      being able to name it. Only bare `pub` counts: `autumn/src/lib.rs` has 49
#      `pub(crate)`/`pub(super)` modules, and a path through one is E0603 in the
#      reader's crate however public the item inside it is. `route` is
#      `pub(crate) mod` while `Route` is re-exported at the crate root, so
#      `::autumn_web::Route` is right and `::autumn_web::route::Route` — which
#      `macro-transparency.md` showed as the macro's own output — is not.
#
# WHAT IT DELIBERATELY DOES NOT CHECK:
#   - Anything past the first item segment. `AutumnError::not_found_msg` is
#     checked as far as `AutumnError`; associated functions, methods, trait
#     items and enum variants need type resolution, and guessing at them is how
#     a gate starts reporting confident nonsense.
#   - Paths that leave the workspace. `autumn_web::reexports::axum::…`,
#     `autumn_web::PreEscaped` (maud) and `autumn_web::db::Pool` (diesel) are
#     real re-exports of crates whose source is not in this tree, so the gate
#     records them as OPAQUE and says so rather than pretending to have checked
#     them. 57 of the 1,495 occurrences land here; `--list` prints all of them,
#     because an opaque count that grows quietly is how a gate goes hollow.
#   - Feature gates. The surface is read as a superset with every `#[cfg]`
#     ignored, so a path that only exists under `--features ws` still resolves.
#     Gating on the default feature set would report an item a reader can
#     absolutely use as missing; that direction of error is not worth trading
#     for, and `check-docs.sh` already builds the real posture.
#   - Bare identifiers. A fence that writes `TestApp::build()` after a
#     `use autumn_web::prelude::*` names no path, and inferring one would need
#     the compiler.
#
# TRUTH SET: the crate sources themselves (`autumn/src`, `autumn-macros/src`,
# `autumn-edge/src`, `autumn-search/src`). There is no snapshot to regenerate
# and nothing to keep in sync — a rename lands in the same commit as the
# surface it renames, which is the property that makes this gate cheap to keep.
#
# WAIVERS — rules, not a list of paths. A page SHOWS a path as often as it tells
# someone to write one, and in output a module path is a label to read rather
# than a line to copy. Two shapes are read as output:
#
#   - The CELL carrying a compiler error code — not the whole row.
#     `docs/migrations/TEMPLATE.md` carries the migration cheat-sheet row
#
#       | `error[E0432]: unresolved import `autumn_web::foo`` | … | `use …;` |
#
#     whose first cell exists to display a path that does not resolve. The next
#     cell gives the FIX, and a fix is a live recommendation: `0.7.0.md` pairs
#     ``error[E0063]: missing field `seo` …`` with
#     `autumn_web::seo::SeoRouteDefaults::EMPTY`, and waiving the whole row
#     would leave unchecked the one path in it a reader actually copies. So the
#     exemption runs from the error code to the end of its own table cell.
#   - A log line (`INFO`, `WARN`, …). The path in one is the tracing TARGET that
#     emitted it — the module's real position in the crate, routinely a private
#     one. `docs/guide/bot-protection.md` quotes the crate's own startup log,
#     `INFO  autumn_web::router: bot_protection provider=…`, and `router` is
#     `pub(crate) mod`: correct as output, unwritable as a path.
#
# A third shape is not a path claim at all and is dropped before resolution
# rather than waived: a brace group containing `(`. A `use` group never does,
# and the skill's api-reference writes
# `autumn_web::widgets::{localized_path(path, locale), locale_switcher(path,
# current_locale, …)}` — prose listing SIGNATURES. Splitting that on commas
# invents `autumn_web::widgets::current_locale` out of an argument name, so the
# module prefix is kept as the claim and the group is discarded.
#
# All three are rules because a named list would have to grow every time a guide
# quotes a real rename or a real log line; these do not.
#
# USAGE:
#   scripts/check-docs-symbols.sh              # gate the corpus
#   scripts/check-docs-symbols.sh --list       # what the gate read
#   scripts/check-docs-symbols.sh --self-test  # synthetic-crate tests

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

run_py() {
  python3 - "$@" <<'PYEOF'
import collections
import glob as globlib
import os
import pathlib
import re
import tomllib
import subprocess
import sys
import tempfile

MODE = sys.argv[1]
ROOT = sys.argv[2]

# The workspace crates a documented path can reach into.
#
# `autumn_web` is the one readers name most; `autumn_macros`, `autumn_edge` and
# `autumn_search` were originally modelled only because `autumn_web` re-exports
# out of them, and a path that lands in one has to keep resolving there.
#
# The rest are here because readers name them DIRECTLY. Adopting a plugin crate
# is a `use autumn_billing::…`/`use autumn_storage_s3::…` line in the reader's
# own file — the corpus writes 67 such sibling-crate paths across 26 distinct
# spellings — and until this gate scanned for them only the `autumn_web::`
# prefix was audited. A path into a sibling fails exactly the way an
# `autumn_web::` one does (E0432 against the reader's own file), so there is no
# reason for the two to be gated differently.
CRATES = {
    'autumn_web': 'autumn/src',
    'autumn_macros': 'autumn-macros/src',
    'autumn_edge': 'autumn-edge/src',
    'autumn_search': 'autumn-search/src',
    'autumn_billing': 'autumn-billing/src',
    'autumn_storage_s3': 'autumn-storage-s3/src',
    'autumn_cache_redis': 'autumn-cache-redis/src',
    'autumn_admin_plugin': 'autumn-admin-plugin/src',
    'autumn_media_plugin': 'autumn-media-plugin/src',
    'autumn_schema_core': 'autumn-schema-core/src',
}
# NOT modelled: `autumn_plugin_reference`. It was listed here at first, which
# was inconsistent with this gate's own rule — the crate is `publish = false`,
# so `workspace_crates` deliberately keeps it out of the reader-facing set,
# while a `CRATES` entry put its prefix back into `prefix_re` and built a
# resolvable surface for it. A page recommending
# `autumn_plugin_reference::ReferencePlugin` would then have PASSED, for a
# crate no reader can depend on at all. It is not re-exported by any published
# crate (only named in one `plugin_contract.rs` doc comment) and the corpus
# writes no path into it, so the entry bought nothing and cost that.


def workspace_crates(root):
    """ident -> (src dir, has a library target), read from the workspace.

    DERIVED rather than listed, for the reason the plugin-root scan in
    `check-docs-toml.sh` is derived: a list of crates outlives the tree it
    describes. A new publishable crate has to be MODELLED (see
    `undeclared_crates`) or a reader can be handed a path into it that nothing
    resolves, and a crate that stops shipping a library has to stop being
    resolvable the same day.

    Only PUBLISHED members count, because a reader reaches a crate by depending
    on it, so a crate nobody can depend on is not a prefix anybody can write:
    that rules out the `examples/` and `benchmarks/` members, and also
    `example-e2e` and `autumn-plugin-reference`, which sit at the top level
    beside the real crates and would otherwise be demanded here as
    reader-facing surface.

    Publishability is `_published`, NOT a literal `publish is False` test of my
    own. Cargo spells "do not publish" three ways — `publish = false`,
    `publish = []`, and `publish.workspace = true` inheriting a `false` from
    `[workspace.package]` — and this file already had all three right in
    `_published`, for the corpus scan. A second, weaker copy here would read a
    private library as published and fail the gate as an undeclared crate,
    which is the self-maintaining check firing on the one shape it exists to
    tolerate.
    """
    with open(os.path.join(root, 'Cargo.toml'), 'rb') as fh:
        manifest = tomllib.load(fh)
    ws = manifest.get('workspace') or {}
    # The members enumerated here are the ROOT workspace's, so the root
    # manifest is the one an inherited `publish` resolves against.
    ws_package = ws.get('package') or {}
    out = {}
    for rel in _member_dirs(root, ws):
        path = os.path.join(root, rel, 'Cargo.toml')
        if not os.path.exists(path):
            continue
        with open(path, 'rb') as fh:
            data = tomllib.load(fh)
        pkg = data.get('package') or {}
        name = pkg.get('name')
        if not name or not _published(pkg, ws_package):
            continue
        lib = _lib_file(root, rel, data)
        out[_import_ident(data, name)] = (
            os.path.join(rel, 'src'),
            # Anchored at the MANIFEST directory, which is where Cargo anchors
            # it. A declared root need not live under `src/` (`[lib] path =
            # "lib/api.rs"` is valid), and treating the path as `src`-relative
            # scanned `<crate>/src/lib/api.rs` — a file that does not exist,
            # so the crate came back empty, or worse, one that does and the
            # wrong API got audited.
            os.path.join(rel, lib) if lib is not None else None)
    return out


def _member_dirs(root, ws):
    """Workspace member directories, repo-relative, globs expanded.

    `members` accepts Cargo's globs (`crates/*`), and reading each entry as a
    literal directory silently drops every package a glob matches. That is not
    a missing check but a VOIDED one: `undeclared_crates` could no longer see
    those crates, so the "every published crate is modelled" guarantee would
    keep passing while covering none of them — the same
    unaudited-looks-like-clean failure as an unmatched `[lib] name`.

    `exclude` semantics are cargo's (`WorkspaceRootConfig::is_excluded`) and
    are not symmetrical with `members`: entries are literal path PREFIXES, so
    `exclude = ["crates/*"]` matches nothing, and an explicitly listed member
    always wins over an exclude. Mirrored from
    `scripts/check-example-bin-names.sh`, which worked these rules out first —
    the shape, not a second guess at it.

    IN-TREE PATH DEPENDENCIES ARE MEMBERS TOO, even when `members` does not
    name them — cargo's documented behaviour, and the reason this function
    walks them. An earlier revision of this gate deliberately did not, on the
    reasoning that a path dependency reflects what the workspace BUILDS rather
    than what it PUBLISHES. That reasoning was wrong: cargo makes such a
    dependency a real member, so a published one is a crate a reader can name,
    and omitting it left `undeclared_crates` green with that crate's whole
    prefix unaudited. Adding a published sibling through `path = "../new"`
    alone is enough to trigger it.

    Membership is NOT taken from `cargo metadata`, which would be the
    authoritative answer: this gate is a toolchain-free job that reports in
    seconds (see its step in ci.yml), and shelling out to cargo would cost
    that. The rules are mirrored from `scripts/check-example-bin-names.sh`
    instead, which resolved them first on #2712 — including the two that are
    easy to get wrong: a `[workspace.dependencies]` path is relative to the
    ROOT while a direct `path` is relative to the dependent package, and a
    dependency carrying its own `[workspace]` table is a separate workspace
    and is not recursed into.
    """
    raw = ws.get('members', []) or []
    exclude = ws.get('exclude', []) or []
    ws_deps = ws.get('dependencies') or {}
    if not isinstance(ws_deps, dict):
        ws_deps = {}
    rootp = pathlib.Path(root).resolve()

    def under(rel, pat):
        pat = pat.strip('/')
        return bool(pat) and (rel == pat or rel.startswith(pat + '/'))

    def excluded(rel):
        return (any(under(rel, p) for p in exclude)
                and not any(under(rel, p) for p in raw))

    out, seen, queue = [], set(), []

    def note(rel):
        if rel and rel not in seen and not excluded(rel):
            seen.add(rel)
            out.append(rel)
            queue.append(rel)

    for pattern in raw:
        if globlib.has_magic(pattern):
            matches = sorted(p.relative_to(rootp).as_posix()
                             for p in rootp.glob(pattern)
                             if p.is_dir() and (p / 'Cargo.toml').is_file())
        else:
            matches = [pattern.strip('/')]
        for rel in matches:
            note(rel)

    # The ROOT package is a member too, when the root manifest carries both
    # `[package]` and `[workspace]` — cargo adds it even though `members` does
    # not say `"."`. Seeded last so an explicit `.` entry keeps its position.
    # This repository's root manifest has no `[package]` (only `patch`,
    # `profile` and `workspace`), so nothing is added here today; a published
    # root library would otherwise never reach `undeclared_crates` at all.
    try:
        with open(os.path.join(root, 'Cargo.toml'), 'rb') as fh:
            if 'package' in tomllib.load(fh):
                note('.')
    except (OSError, tomllib.TOMLDecodeError):
        pass

    while queue:
        rel = queue.pop(0)
        try:
            with open(os.path.join(root, rel, 'Cargo.toml'), 'rb') as fh:
                data = tomllib.load(fh)
        except (OSError, tomllib.TOMLDecodeError):
            continue
        for dep_path, from_root in _path_deps(data, ws_deps):
            base = rootp if from_root else (rootp / rel)
            try:
                dep = (base / dep_path).resolve().relative_to(rootp).as_posix()
            except ValueError:
                continue            # outside the workspace: never a member
            manifest = os.path.join(root, dep, 'Cargo.toml')
            if dep in seen or not os.path.isfile(manifest):
                continue
            try:
                with open(manifest, 'rb') as fh:
                    dep_data = tomllib.load(fh)
            except (OSError, tomllib.TOMLDecodeError):
                continue
            if 'workspace' in dep_data:
                continue            # its own workspace root, not this one
            note(dep)
    return out


def _path_deps(manifest, ws_deps):
    """`(path, is_relative_to_workspace_root)` for one manifest's path deps.

    Covers `dependencies`, `dev-dependencies` and `build-dependencies`, and the
    same three under every `[target.*]` table — a crate reachable only through
    a target-gated dependency is still a member.

    `dep.workspace = true` inherits from `[workspace.dependencies]`, whose key
    is `package = "…"` when the dependency is renamed, and whose paths are
    relative to the workspace root rather than to the dependent package.
    """
    out = []
    tables = [manifest]
    targets = manifest.get('target')
    if isinstance(targets, dict):
        tables += [t for t in targets.values() if isinstance(t, dict)]
    for table in tables:
        for section in ('dependencies', 'dev-dependencies',
                        'build-dependencies'):
            deps = table.get(section)
            if not isinstance(deps, dict):
                continue
            for name, spec in deps.items():
                if not isinstance(spec, dict):
                    continue
                if isinstance(spec.get('path'), str):
                    out.append((spec['path'], False))
                elif spec.get('workspace') is True:
                    inherited = ws_deps.get(spec.get('package', name), {})
                    if isinstance(inherited, dict) and isinstance(
                            inherited.get('path'), str):
                        out.append((inherited['path'], True))
    return out


def _import_ident(data, pkg_name):
    """The identifier a reader writes at the head of a path into this crate.

    `[lib] name = "sdk"` means downstream paths start `sdk::`, NOT with the
    package name — so deriving the prefix from `[package].name` alone would ask
    for a `CRATES` entry under a name nobody writes and, worse, leave the
    crate's real prefix out of `prefix_re` entirely. That is a silent hole in
    the every-published-crate guarantee this gate now makes, which is the one
    kind of gap worth being fussy about: an unaudited prefix looks exactly like
    a clean one.

    Cargo normalizes `-` to `_` for the import name in both cases.
    """
    declared = (data.get('lib') or {}).get('name')
    chosen = declared if isinstance(declared, str) and declared else pkg_name
    return chosen.replace('-', '_')


def _lib_file(root, rel, data):
    """The crate-root FILE of this package's library target, or None.

    Returns a path rather than a boolean, because the file is the thing the
    scanner needs and a boolean silently discards it. `[lib] path =
    "src/api.rs"` is a valid published library; classifying it as "has a lib"
    and then reading `src/lib.rs` anyway yields an EMPTY surface, and an empty
    surface does not fail — it reports every documented path into that crate as
    dead. Worse, `undeclared_crates` would have just instructed the author to
    map the crate, so the gate's own advice produces the wrong answer. (The
    empty-surface guard in `main` is the second half of that fix; a path is
    only useful if a missing one is loud.)

    Three cases, in the order Cargo resolves them:

      1. An explicit `[lib]` table declares one outright. Its `path` wins,
         defaulting to `src/lib.rs` when the table omits it. Checked first
         because `autolib` governs auto-DISCOVERY and says nothing about a
         target the manifest declares by hand.
      2. `package.autolib = false` turns auto-discovery off. A `src/lib.rs`
         then sits in the tree with no library built from it, so the file's
         presence is not proof and cannot be the last word.
      3. Otherwise Cargo auto-discovers `src/lib.rs`.

    Getting (2) wrong is not a cosmetic miss. A published package with
    `autolib = false` and a leftover `src/lib.rs` would be read as a library,
    so it would skip the binary-only class entirely: instead of the accurate
    "ships no library target", a path into it would be demanded as a modelled
    crate and then reported as an unresolved SYMBOL — sending the reader to
    look for a renamed item in a crate that exposes nothing at all.
    """
    if 'lib' in data:
        libtable = data.get('lib') or {}
        if not _rust_linkable(libtable):
            return None
        declared = libtable.get('path')
        return declared if isinstance(declared, str) else 'src/lib.rs'
    if (data.get('package') or {}).get('autolib') is False:
        return None
    if os.path.exists(os.path.join(root, rel, 'src', 'lib.rs')):
        return 'src/lib.rs'
    return None


# Workspace crates that ship NO library target, and so can never appear in a
# path a reader writes.
#
# `autumn-cli` is the whole class today: it is `src/main.rs` and nothing else,
# with no `src/lib.rs` and no `[lib]` section, so `pub` inside it is visible
# only within the binary. That is the nastiest shape this gate meets, because
# every OTHER signal a reader has says the path is fine — the module is real,
# the items are real and really `pub`, and `cargo add autumn-cli` succeeds,
# since the crate does publish a binary. What does not exist is a library to
# import, and no spelling of the path fixes that: the reader cannot route
# around it the way they can route around a renamed item.
#
# So a path into one is reported as its own defect rather than as a dead
# segment. "No `check` in `autumn_cli`" would be actively misleading advice —
# `autumn-cli/src/check.rs` is right there, `pub`, and the suggestion machinery
# would happily propose a near-miss inside a crate that cannot be imported at
# all. The answer is never a different path; it is the CLI, or an API the
# library crates actually expose.
#: Crate types a dependent crate can actually `use`. `cdylib` and `staticlib`
#: build an artifact for C, not an rlib for rustc, so a package whose only
#: crate types are those "provides no linkable target" (cargo says so in as
#: many words) and every `use pkg::Thing` against it is E0432 — even though
#: `src/lib.rs` is right there and this gate could resolve the path out of it.
#:
#: That is the SAME shape as the `autumn-cli` defect this gate was widened to
#: catch: a crate that reads as importable and is not. Different manifest key,
#: identical consequence for the reader, so it belongs in the same class rather
#: than in a check of its own.
#:
#: `proc-macro` is linkable and must stay so — `autumn-macros` is one, declared
#: as `[lib] proc-macro = true`, and its derives are named through re-exports
#: the resolver follows.
RUST_LINKABLE = frozenset({'lib', 'rlib', 'dylib', 'proc-macro'})


def _rust_linkable(libtable):
    """Whether `[lib]`'s crate types include one rustc can link against.

    ABSENT and EMPTY are different answers, and an earlier revision of this
    function got that wrong in a committed test that asserted `crate-type = []`
    was "the default, not a denial". It is a denial. Measured rather than
    reasoned, which is what settled it:

        [lib] crate-type = []          # in a path dependency
        $ cargo metadata   -> kind=[] crate_types=[]
        $ cargo check -p dependent
        warning: the package `emptyct` provides no linkable target
        error: could not compile `emptyct`

    So only an absent key gets the default — `lib`, or `proc-macro` under
    `proc-macro = true`, which is what all 10 modelled crates rely on. An
    explicit list answers for itself, and an empty one names nothing linkable.

    A non-list value is a manifest cargo itself rejects; it is read as the
    default rather than as a denial, so a malformed manifest cannot manufacture
    a docs defect out of this gate.
    """
    if 'crate-type' in libtable:
        kinds = libtable['crate-type']
    elif 'crate_type' in libtable:
        kinds = libtable['crate_type']
    else:
        return True
    if not isinstance(kinds, list):
        return True
    return any(k in RUST_LINKABLE for k in kinds if isinstance(k, str))


def binary_only_crates(root):
    return {ident: rel for ident, (rel, lib) in workspace_crates(root).items()
            if lib is None}


def undeclared_crates(root):
    """Publishable library crates that `CRATES` does not model.

    The declaration has to keep being true rather than merely keep matching
    something: a crate added to the workspace with a library target is a
    prefix readers can write, and leaving it out of `CRATES` would make every
    path into it silently unaudited — the exact gap this gate closed for the
    seven sibling crates. Failing here is how the next one gets modelled on the
    day it lands instead of the day a reader files a bug.
    """
    return sorted(ident for ident, (_, lib) in workspace_crates(root).items()
                  if lib is not None and ident not in CRATES)

# ------------------------------------------------------------------ parsing

# Bare `pub` ONLY. `pub(crate)`, `pub(super)` and `pub(in …)` are not visible to
# a reader's crate, so an item or module carrying one cannot appear in a path a
# reader writes. `autumn/src/lib.rs` has 49 restricted modules, and treating
# them as public blessed `::autumn_web::route::Route` in macro-transparency.md
# — `route` is `pub(crate) mod`, so that path is E0603 downstream even though
# `Route` itself is re-exported at the crate root.
PUB_ITEM = re.compile(
    r'^[ \t]*pub(?!\s*\()\s+'
    r'(?:async\s+|unsafe\s+|extern\s+"[^"]*"\s+|const\s+)*'
    r'(?:struct|enum|trait|fn|type|const|static|union)\s+([a-zA-Z_]\w*)', re.M)
PUB_MOD_DECL = re.compile(
    r'^[ \t]*pub(?!\s*\()\s+mod\s+([a-zA-Z_]\w*)\s*;', re.M)
# Every `mod x;`, public or not. A private module is not itself public surface,
# but it is routinely the FILE a public facade re-exports out of, so the tree
# has to contain it or the re-export target cannot be followed.
ANY_MOD_DECL = re.compile(
    r'^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([a-zA-Z_]\w*)\s*;', re.M)
# Bare `pub` only, for the same reason as items and modules: a
# `pub(crate) use node::LEAVE_BUDGET;` (autumn/src/cluster/mod.rs) republishes
# the name inside the crate and nowhere else.
PUB_USE = re.compile(
    r'^[ \t]*pub(?!\s*\()\s+use\s+(.+?);[ \t]*$', re.M | re.S)
INLINE_MOD = re.compile(
    r'^([ \t]*)(pub(\s*\([^)]*\))?\s+)?mod\s+([a-zA-Z_]\w*)\s*\{', re.M)
MACRO_RULES = re.compile(r'macro_rules!\s+([a-zA-Z_]\w*)')
# A derive macro is EXPORTED under the name in the attribute, which is not the
# name of the function carrying it (`#[proc_macro_derive(OpenApiSchema)] pub fn
# derive_open_api_schema`). Reading only the fn name loses every derive the
# guide tells a reader to write.
PROC_MACRO_DERIVE = re.compile(
    r'#\[proc_macro_derive\s*\(\s*([a-zA-Z_]\w*)')
# `#[macro_export]` may sit above doc comments and further attributes.
MACRO_EXPORT = re.compile(
    r'#\[macro_export\][^\n]*\n(?:[ \t]*(?:#\[[^\]]*\]|//[^\n]*)\n)*'
    r'[ \t]*macro_rules!\s+([a-zA-Z_]\w*)')


def mask_literals(text):
    """Blank string/char literals and comments, preserving length and newlines.

    Brace counting decides module scope, so a `{` inside a string or a doc
    comment must not be counted. Length is preserved so an offset into the mask
    is the same offset into the source.
    """
    out, i, n = list(text), 0, len(text)

    def blank(a, b):
        for k in range(a, min(b, n)):
            if out[k] != '\n':
                out[k] = ' '

    while i < n:
        c = text[i]
        if c == '/' and text[i + 1:i + 2] == '/':
            j = text.find('\n', i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
            continue
        if c == '/' and text[i + 1:i + 2] == '*':
            depth, j = 1, i + 2
            while j < n and depth:                 # Rust block comments nest
                if text[j:j + 2] == '/*':
                    depth += 1
                    j += 2
                elif text[j:j + 2] == '*/':
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)
            i = j
            continue
        if c == 'r' and text[i + 1:i + 2] in ('#', '"'):
            k, hashes = i + 1, 0
            while k < n and text[k] == '#':
                hashes += 1
                k += 1
            if k < n and text[k] == '"':
                close = '"' + '#' * hashes
                j = text.find(close, k + 1)
                j = n if j < 0 else j + len(close)
                blank(i, j)
                i = j
                continue
        if c == '"':
            j = i + 1
            while j < n:
                if text[j] == '\\':
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    break
                j += 1
            blank(i, j)
            i = j
            continue
        if c == "'":
            # A char literal closes within a couple of characters; anything
            # else starting with `'` is a lifetime and must be left alone.
            m = re.match(r"'(?:\\.|[^\\'])'", text[i:i + 4])
            if m:
                blank(i, i + m.end())
                i += m.end()
                continue
        i += 1
    return ''.join(out)


def is_module_ish(binding):
    """Whether a resolved name occupies the TYPE namespace, i.e. can be walked
    through as a module: a `pub mod` declared here, or a re-export of one."""
    return (binding == 'mod'
            or (isinstance(binding, tuple) and binding and binding[0] == 'modref'))


def brace_depths(masked):
    """Depth BEFORE each character, so a declaration's own offset reads 0."""
    depths, cur = [], 0
    for ch in masked:
        depths.append(cur)
        if ch == '{':
            cur += 1
        elif ch == '}':
            cur = max(0, cur - 1)
    return depths


def matching_brace(masked, start):
    """Offset of the `}` closing the first `{` at or after `start`."""
    open_at = masked.find('{', start)
    if open_at < 0:
        return None, None
    depth, i = 0, open_at
    while i < len(masked):
        if masked[i] == '{':
            depth += 1
        elif masked[i] == '}':
            depth -= 1
            if depth == 0:
                return open_at, i
        i += 1
    return open_at, None


def expand_braces(spec):
    """`a::{b, c::{d, e}}` -> ['a::b', 'a::c::d', 'a::c::e']."""
    i = spec.find('{')
    if i < 0:
        return [spec]
    depth = 0
    for j in range(i, len(spec)):
        if spec[j] == '{':
            depth += 1
        elif spec[j] == '}':
            depth -= 1
            if depth == 0:
                break
    prefix, inner, suffix = spec[:i], spec[i + 1:j], spec[j + 1:]
    parts, depth2, cur = [], 0, ''
    for ch in inner:
        if ch == '{':
            depth2 += 1
        elif ch == '}':
            depth2 -= 1
        if ch == ',' and depth2 == 0:
            parts.append(cur)
            cur = ''
        else:
            cur += ch
    parts.append(cur)
    out = []
    for p in parts:
        p = p.strip()
        if not p:
            continue
        if p == 'self' or p.startswith('self as '):
            # `a::{self}` names `a` itself, not a child called `self`.
            head = prefix.rstrip(':').rstrip(':')
            out.extend(expand_braces(head + p[4:] + suffix))
        else:
            out.extend(expand_braces(prefix + p + suffix))
    return out


class Crate:
    """The public surface of one crate, read statically from its sources."""

    def __init__(self, ident, srcdir, rootpath=None):
        self.ident = ident
        # `rootpath` is the crate-root FILE, and when given it decides BOTH
        # halves: the file to read and the directory submodules resolve
        # against, which Rust takes to be the crate root's own directory. They
        # have to move together — a `[lib] path = "lib/api.rs"` crate whose
        # root is read from `lib/` but whose `mod` lookups still ran against
        # `src/` would resolve the root and then lose every submodule under it.
        if rootpath is not None:
            self.src = os.path.dirname(rootpath)
            self.rootfile = os.path.basename(rootpath)
        else:
            self.src = srcdir
            self.rootfile = 'lib.rs'
        self.mods = {}            # tuple(path) -> {name: 'item'|'mod'}
        self.uses = {}            # tuple(path) -> [(target, leaf, alias, glob)]
        self.exported_macros = set()
        if os.path.isfile(os.path.join(self.src, self.rootfile)):
            self._scan_file([])
        # `#[macro_export]` hoists a macro to the crate root regardless of the
        # module it is written in.
        for name in self.exported_macros:
            self.mods.setdefault((), {})[name] = 'item'

    def _modfile(self, mp):
        if not mp:
            return os.path.join(self.src, self.rootfile)
        p = self.src
        for seg in mp[:-1]:
            p = os.path.join(p, seg)
        flat = os.path.join(p, mp[-1] + '.rs')
        nested = os.path.join(p, mp[-1], 'mod.rs')
        if os.path.isfile(flat):
            return flat
        return nested if os.path.isfile(nested) else None

    def _scan_file(self, mp):
        key = tuple(mp)
        if key in self.mods:
            return
        path = self._modfile(mp)
        self.mods[key], self.uses[key] = {}, []
        if not path:
            return
        with open(path, encoding='utf8', errors='replace') as fh:
            self._scan_text(key, fh.read(), file_mp=mp)

    def _scan_text(self, key, txt, file_mp=None):
        """Register the externally-public surface declared at THIS module scope.

        Everything is filtered to brace depth 0. A regex anchored with
        `^[ \t]*pub` matches an indented method inside an `impl` block just as
        happily as a free function, which credited `AppBuilder::run` to the
        `app` MODULE and made the nonexistent `autumn_web::app::run` resolve.
        Only a declaration at module scope is a module-level item.
        """
        self.mods.setdefault(key, {})
        self.uses.setdefault(key, [])
        masked = mask_literals(txt)
        depths = brace_depths(masked)

        def at_module_scope(m):
            return m.start() < len(depths) and depths[m.start()] == 0

        for m in MACRO_EXPORT.finditer(txt):
            if at_module_scope(m):
                self.exported_macros.add(m.group(1))
        # A `macro_rules!` WITHOUT `#[macro_export]` is textually scoped: it is
        # not addressable by any path, so it never joins the surface. Only the
        # exported ones do, at the crate root (below), plus whatever a
        # `pub use <name>;` republishes at a module path.
        for m in PUB_ITEM.finditer(txt):
            if at_module_scope(m):
                self.mods[key][m.group(1)] = 'item'
        for m in PROC_MACRO_DERIVE.finditer(txt):
            if at_module_scope(m):
                self.mods[key][m.group(1)] = 'item'
        for m in PUB_MOD_DECL.finditer(txt):
            if at_module_scope(m):
                self.mods[key][m.group(1)] = 'mod'
        for m in PUB_USE.finditer(txt):
            if at_module_scope(m):
                self._parse_use(key, m.group(1))
        for m in INLINE_MOD.finditer(txt):
            if not at_module_scope(m):
                continue
            open_at, close_at = matching_brace(masked, m.start())
            if open_at is None or close_at is None:
                continue
            # Bare `pub` only: group(2) is the `pub…` prefix, group(3) its
            # `(crate)`/`(super)` restriction when present.
            if bool(m.group(2)) and not m.group(3):
                self.mods[key][m.group(4)] = 'mod'
            self._scan_text(key + (m.group(4),), txt[open_at + 1:close_at])
        if file_mp is not None:
            for m in ANY_MOD_DECL.finditer(txt):
                if at_module_scope(m):
                    self._scan_file(list(file_mp) + [m.group(1)])

    def _parse_use(self, key, raw):
        # Collapse whitespace but KEEP the separator around `as`: stripping all
        # of it turns `http_client as http` into one token and loses every
        # aliased re-export in the crate.
        raw = re.sub(r'\s+', ' ', raw).strip()
        raw = re.sub(r'\s*::\s*', '::', raw)
        raw = re.sub(r'\s*([{},])\s*', r'\1', raw)
        for item in expand_braces(raw):
            item = item.strip()
            if not item:
                continue
            # A leading `::` names the EXTERNAL crate explicitly and bypasses
            # local items entirely. `include_dir` is a module here AND a
            # dependency, and `pub mod include_dir { pub use ::include_dir::*; }`
            # re-exports the dependency; resolving it as the local module of the
            # same name makes the glob resolve to itself and silently blesses
            # every path under it.
            absolute = item.startswith('::')
            marker = [''] if absolute else []
            if item.endswith('*'):
                target = item.rstrip('*').rstrip(':').split('::')
                self.uses[key].append(
                    (marker + [s for s in target if s], None, None, True))
                continue
            alias = None
            m = re.match(r'^(.*?)\s+as\s+(\w+)$', item)
            if m:
                item, alias = m.group(1), m.group(2)
            segs = [s for s in item.split('::') if s]
            if not segs:
                continue
            self.uses[key].append(
                (marker + segs[:-1], segs[-1], alias or segs[-1], False))


class Surface:
    """Every workspace crate, with path resolution across their re-exports."""

    def __init__(self, root, crates=CRATES, rootpaths=None):
        # `rootpaths` maps a crate ident to its crate-root FILE, already
        # absolute. Given, it overrides the `<mapped dir>/lib.rs` default
        # entirely — path and submodule base together. Absent (and in the
        # self-test's synthetic trees) the default holds, which is what all 11
        # crates in this workspace actually use.
        rootpaths = rootpaths or {}
        self.crates = {
            ident: Crate(ident, os.path.join(root, rel),
                         rootpaths.get(ident))
            for ident, rel in crates.items()}
        self.external = set()
        self._memo = {}

    def empty(self):
        """Modelled crates whose crate root published no names at all.

        A crate that resolves to nothing is the one failure this gate cannot
        report as a defect, because it does not look like one: every path into
        it comes back `dead:<first segment>`, which reads exactly like a batch
        of renamed items. The cause is always the model, not the docs — a
        mapped directory with no crate root in it, or a `[lib] path` pointing
        somewhere the mapping does not reach — so it is raised as a gate error
        before any page is judged.
        """
        return sorted(ident for ident, c in self.crates.items()
                      if not self.names_of(c, ()))

    def names_of(self, crate, mp, depth=0):
        """Public names visible at `crate::mp`, following `pub use`."""
        key = (crate.ident, mp)
        if key in self._memo:
            return self._memo[key]
        if depth > 14:
            return {}
        self._memo[key] = {}       # cycle guard
        out = dict(crate.mods.get(mp, {}))
        local = dict(out)
        for (target, leaf, alias, is_glob) in crate.uses.get(mp, []):
            # `#[macro_export] macro_rules! m` … `pub use m;` is the idiom that
            # makes a crate-root macro addressable at its module path too, and
            # is why `autumn_web::storage::migrations::add_blob_column` is a
            # real path. The bare `pub use` has no path to walk.
            if not target and not is_glob and leaf in crate.exported_macros:
                out[alias] = 'item'
                continue
            # `pub use autumn_edge as edge;` -- a single-segment re-export whose
            # one segment names a CRATE, not an item in this module. Falling
            # through to the leaf lookup below resolved it to nothing, so the
            # only binding `edge` ever got was the later
            # `pub use autumn_macros::edge;` proc macro, and every path under
            # `autumn_web::edge::…` was waved through as its associated item.
            if not target and not is_glob and leaf in self.crates:
                out[alias] = ('modref', leaf, ())
                continue
            r = self._resolve_target(crate, mp, target)
            if is_glob:
                if r and r[0] == 'mod':
                    for n, k in self.names_of(r[1], r[2], depth + 1).items():
                        out.setdefault(n, k)
                elif r and r[0] == 'opaque':
                    out.setdefault('*OPAQUE*', 'opaque')
                continue
            if r is None:
                continue
            prior = out.get(alias)
            if r[0] == 'opaque':
                out[alias] = 'opaque'
                if is_module_ish(prior):
                    out[alias] = prior
                continue
            tc, tm = r[1], r[2]
            sub = self.names_of(tc, tm, depth + 1)
            if (tm + (leaf,)) in tc.mods and sub.get(leaf) != 'item':
                out[alias] = ('modref', tc.ident, tm + (leaf,))
            elif leaf in sub:
                out[alias] = (('modref', tc.ident, tm + (leaf,))
                              if sub[leaf] == 'mod' else sub[leaf])
            elif '*OPAQUE*' in sub:
                out[alias] = 'opaque'
            else:
                out[alias] = 'unknown'
            # A re-export we could not follow must never DOWNGRADE a name the
            # module already declares itself. `openapi.rs` declares
            # `pub trait OpenApiSchema` and then re-exports the derive macro of
            # the same name out of `autumn_macros`; letting the unresolved
            # re-export win turned a resolvable trait into an opaque one.
            if out.get(alias) in ('unknown', 'opaque') and alias in local:
                out[alias] = local[alias]
            # Rust resolves a path SEGMENT in the type namespace, and one name
            # can be a module there and a value elsewhere. Whichever order the
            # two are written in, the module is the one a path can be walked
            # THROUGH, so it wins:
            #   `pub mod app` + `pub use app::app;`      (local module, value
            #                                             after it)
            #   `pub use autumn_edge as edge;` … later
            #   `pub use autumn_macros::edge;`           (re-exported module,
            #                                             macro after it)
            # Letting the value win made `autumn_web::app::run` and
            # `autumn_web::edge::Bogus` resolve as "associated items" of a leaf.
            if is_module_ish(prior) and not is_module_ish(out.get(alias)):
                out[alias] = prior
            if local.get(alias) == 'mod' and not is_module_ish(out.get(alias)):
                out[alias] = 'mod'
        self._memo[key] = out
        return out

    def _resolve_target(self, crate, curmod, segs):
        if segs and segs[0] == '':
            # `::foo` -- an explicitly external path. Never fall back to a
            # local item of the same name.
            segs = segs[1:]
            if not segs:
                return None
            if segs[0] in self.crates:
                return self._walk(self.crates[segs[0]], (), segs[1:])
            self.external.add(segs[0])
            return ('opaque', None, None)
        if not segs:
            return ('mod', crate, curmod)
        head = segs[0]
        if head == 'crate':
            return self._walk(crate, (), segs[1:])
        if head == 'self':
            return self._walk(crate, curmod, segs[1:])
        if head == 'super':
            return self._walk(crate, curmod[:-1], segs[1:])
        if head in self.crates:
            return self._walk(self.crates[head], (), segs[1:])
        # Rust 2018 uniform paths: a `use` may start at a local item or at the
        # crate root before it means an external crate.
        r = self._walk(crate, curmod, segs) or self._walk(crate, (), segs)
        if r:
            return r
        self.external.add(head)
        return ('opaque', None, None)

    def _walk(self, crate, mp, segs):
        cur, c = mp, crate
        for s in segs:
            # A `pub mod` tree is cycle-free, so resolving structurally first
            # can never be poisoned by the names_of recursion guard.
            if (cur + (s,)) in c.mods:
                cur = cur + (s,)
                continue
            v = self.names_of(c, cur).get(s)
            if v is None:
                return None
            if isinstance(v, tuple) and v[0] == 'modref':
                c, cur = self.crates[v[1]], v[2]
                continue
            if v == 'opaque':
                return ('opaque', None, None)
            return None
        return ('mod', c, cur)

    def resolve(self, path, root='autumn_web'):
        """'ok' | 'opaque' | 'dead:<the prefix that broke>'."""
        segs = path.split('::')
        c, cur = self.crates[root], ()
        for i, s in enumerate(segs):
            # Deliberately NOT the structural `(cur + (s,)) in c.mods` shortcut
            # used for re-export targets below: that tree contains private and
            # `pub(crate)` modules (it has to, to follow a facade re-export out
            # of one), and walking it here blesses a path a reader's crate
            # cannot name. Only what `names_of` publishes — bare-`pub` items and
            # submodules, plus re-exports — is externally nameable.
            names = self.names_of(c, cur)
            v = names.get(s)
            if v is None:
                if '*OPAQUE*' in names:
                    return 'opaque'
                return 'dead:' + '::'.join(segs[:i + 1])
            if v == 'opaque' or v == 'unknown':
                return 'opaque'
            if isinstance(v, tuple) and v[0] == 'modref':
                c, cur = self.crates[v[1]], v[2]
                continue
            if v == 'mod':
                cur = cur + (s,)
                continue
            # A leaf item: everything after it is an associated item, which
            # this gate does not claim to check.
            return 'ok'
        return 'ok'

    def suggest(self, path, root='autumn_web'):
        """Closest existing sibling for the segment that broke, or None."""
        import difflib
        segs = path.split('::')
        c, cur = self.crates[root], ()
        for s in segs:
            names = self.names_of(c, cur)
            if s in names:
                v = names[s]
                if isinstance(v, tuple) and v[0] == 'modref':
                    c, cur = self.crates[v[1]], v[2]
                    continue
                if v == 'mod':
                    cur = cur + (s,)
                    continue
                return None
            pool = [n for n in names if not n.startswith('*')]
            near = difflib.get_close_matches(s, pool, n=1, cutoff=0.6)
            return near[0] if near else None
        return None


# ------------------------------------------------------------------- corpus

# Spelled the same in `check-docs-cli.sh`, `check-docs-config.sh` and
# `check-docs-toml.sh`: a page covered by one gate and not the others is how a
# page ends up with no owner.
# `.claude/skills/` is a SECOND skill tree, not a copy of `skills/`: the agent
# machinery loads a `SKILL.md` there by name, which is why
# `check-docs-orphans.sh` seeds both trees as reader entry surfaces and why
# `check-docs-routes.sh` reads both for `/actuator/…` paths. `run-autumn` lives
# only here, and its SKILL.md is copy-and-run text end to end — `autumn seed
# --package`, `autumn routes --bin`, `AUTUMN_SERVER__PORT`,
# `AUTUMN_DATABASE__URL`, `-p autumn-web`. It already carries
# `route-surface-allow` waivers for the routes gate, so the tree was reader-
# facing to one gate and invisible to this one: exactly the split the note
# above says these definitions exist to prevent. Corpus 199 -> 200 here, and
# this gate stays green over it.
INCLUDE_DIRS = ('docs/guide/', 'docs/migrations/', 'skills/', 'agents/',
                '.claude/skills/')
INCLUDE_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md',
                 'docs/plugins.md')
INCLUDE_README_DIRS = ('examples/',)


def reader_facing(path):
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
    out = subprocess.run(['git', 'ls-files', '-z', '*.md', '*.md.tmpl'],
                         cwd=root, capture_output=True, text=True).stdout
    published = package_readmes(root)
    files = [f for f in out.split('\0')
             if f and (reader_facing(f) or f.endswith('.md.tmpl')
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


# Every crate prefix a reader can write, longest first so `autumn_web::` can
# never shadow a longer ident that starts with it. Built from the modelled
# crates plus the binary-only ones: a path into a crate with no library target
# is a defect this gate must SEE in order to report, so its prefix has to scan.
def prefix_re(root):
    idents = sorted(set(CRATES) | set(binary_only_crates(root)),
                    key=len, reverse=True)
    return re.compile(r'\b(' + '|'.join(idents) + ')::')
IDENT_RE = re.compile(r'[a-zA-Z_]\w*')
# A path claim, once braces are expanded: identifiers separated by `::` and
# nothing else. The guide also writes brace groups that are PROSE rather than
# imports — `autumn_web::widgets::{localized_path(path, locale),
# locale_switcher(path, current_locale, …)}` in the skill's api-reference lists
# signatures, not names — and an expansion of one is not a path anybody can
# write. Those are skipped rather than reported.
PATH_SHAPE = re.compile(r'[a-zA-Z_]\w*(?:::[a-zA-Z_]\w*)*\Z')
# Lines that SHOW a path rather than tell a reader to write one. Both are
# output, and in output a module path is a label, not something to copy.
#   - a compiler error quotes a path precisely because it does not resolve
#     (`docs/migrations/TEMPLATE.md`'s migration cheat-sheet row exists to
#     display one);
#   - a log line names the tracing target that emitted it, which is the
#     module's real position in the crate and routinely a private one --
#     `INFO  autumn_web::router: bot_protection provider=…` in
#     `docs/guide/bot-protection.md` is the crate's own log output, and
#     `router` is `pub(crate)`.
# Counted as waived rather than suppressed by name, so the number stays visible.
ERROR_CODE = re.compile(r'error\[E\d{4}\]')
LOG_LEVEL = re.compile(r'^\s*(?:TRACE|DEBUG|INFO|WARN|WARNING|ERROR)\b')


def is_shown_as_output(line, col):
    """Whether the path at column `col` is being SHOWN rather than recommended.

    A log line is output end to end, so the whole line is exempt. A compiler
    error is not: the migration guides quote one in a table row whose next cell
    gives the FIX, and that fix is a live recommendation to audit.
    `docs/migrations/0.7.0.md` pairs ``error[E0063]: missing field `seo` …``
    with `add `seo: autumn_web::seo::SeoRouteDefaults::EMPTY``; waiving the
    whole row would leave the corrective path unchecked, which is the one a
    reader actually copies. So the exemption runs from the error code to the end
    of its own table cell.
    """
    if LOG_LEVEL.search(line):
        return True
    m = ERROR_CODE.search(line)
    if not m or col < m.start():
        return False
    cell_end = line.find('|', m.end())
    return col < (cell_end if cell_end != -1 else len(line))


def scan_paths(text, pattern):
    """Yield (crate ident, raw path spelling, offset) for every crate path.

    Scans the whole document rather than line by line, and matches braces by
    counting them, because the guide writes grouped imports BOTH nested
    (`storage::{BlobStoreState, variant::{Transform, VariantBudget}}`) and
    across lines (`push::{\\n    MemoryPushSubscriptionStore, …\\n}`). A
    single-line `\\{[^{}]*\\}` pattern silently degrades to the module prefix on
    all 14 of those in this corpus: the symbols a reader copies off them were
    never audited at all, which is the failure this gate exists to prevent.
    """
    for m in pattern.finditer(text):
        crate = m.group(1)
        i, parts = m.end(), []
        while True:
            if i < len(text) and text[i] == '{':
                depth, j = 0, i
                while j < len(text):
                    if text[j] == '{':
                        depth += 1
                    elif text[j] == '}':
                        depth -= 1
                        if depth == 0:
                            break
                    j += 1
                if j >= len(text):
                    break          # unbalanced; not a path claim
                group = text[i:j + 1]
                if '(' in group:
                    # Not an import list: a `use` group never contains
                    # parentheses. The skill's api-reference writes
                    # `autumn_web::widgets::{localized_path(path, locale),
                    # locale_switcher(path, current_locale, …)}` -- prose
                    # listing SIGNATURES. Splitting it on commas invents
                    # `autumn_web::widgets::current_locale` out of an argument
                    # name. Keep the module prefix, which is a real claim, and
                    # drop the group.
                    break
                parts.append(group)
                i = j + 1
                break
            im = IDENT_RE.match(text, i)
            if not im:
                break
            parts.append(im.group(0))
            i = im.end()
            if text[i:i + 2] == '::':
                i += 2
                continue
            break
        if parts:
            yield crate, '::'.join(parts), m.start()


def occurrences(root, files, pattern):
    """[(crate, path, file, line, waived)] for every documented crate path."""
    found = []
    for rel in files:
        full = os.path.join(root, rel)
        try:
            with open(full, encoding='utf8', errors='replace') as fh:
                text = fh.read()
        except OSError:
            continue
        for crate, raw, offset in scan_paths(text, pattern):
            line_no = text.count('\n', 0, offset) + 1
            line_end = text.find('\n', offset)
            line = text[text.rfind('\n', 0, offset) + 1:
                        line_end if line_end != -1 else len(text)]
            col = offset - (text.rfind('\n', 0, offset) + 1)
            waived = is_shown_as_output(line, col)
            spec = re.sub(r'\s+', ' ', raw).strip()
            spec = re.sub(r'\s*::\s*', '::', spec)
            spec = re.sub(r'\s*([{},])\s*', r'\1', spec)
            for path in expand_braces(spec):
                path = path.strip()
                m = re.match(r'^(.*?)\s+as\s+\w+$', path)
                if m:
                    path = m.group(1)
                if path and PATH_SHAPE.match(path):
                    found.append((crate, path, rel, line_no, waived))
    return found


def audit(root):
    # The crate root each modelled crate actually declares. `CRATES` maps to a
    # `src` DIRECTORY, so without this a crate whose manifest points its
    # library somewhere else is scanned for a `lib.rs` it does not have.
    rootpaths = {ident: os.path.join(root, lib)
                 for ident, (_, lib) in workspace_crates(root).items()
                 if lib is not None and ident in CRATES}
    surface = Surface(root, rootpaths=rootpaths)
    files = corpus(root)
    binary_only = binary_only_crates(root)
    occ = occurrences(root, files, prefix_re(root))
    dead, unimportable = [], []
    opaque, ok, waived = collections.Counter(), 0, 0
    for (crate, path, rel, line, is_waived) in occ:
        if is_waived:
            waived += 1
            continue
        # A crate with no library target is not a resolution question. Reported
        # before `resolve` is consulted, because there is nothing to consult:
        # the crate is not in `CRATES` and never can be.
        if crate in binary_only:
            unimportable.append((crate, path, rel, line, binary_only[crate]))
            continue
        r = surface.resolve(path, crate)
        if r.startswith('dead:'):
            dead.append((crate, path, rel, line, r[5:],
                         surface.suggest(path, crate)))
        elif r == 'opaque':
            opaque[f'{crate}::{path}'] += 1
        else:
            ok += 1
    return (surface, files, occ, dead, unimportable, opaque, ok, waived)


def main():
    unmodelled = undeclared_crates(ROOT)
    if unmodelled:
        print('FAIL: publishable library crates that `CRATES` does not model: '
              + ', '.join(unmodelled))
        print('')
        print('Each is a prefix a reader can write in a `use` line, so every')
        print('path into it would go unaudited. Add it to `CRATES` with its')
        print('`src` directory rather than letting the gate skip the crate.')
        return 1

    surface, files, occ, dead, unimportable, opaque, ok, waived = audit(ROOT)
    blank = surface.empty()
    if blank:
        print('FAIL: modelled crates that published no names at all: '
              + ', '.join(blank))
        print('')
        print('This is a MODEL error, not a docs defect: every path into such')
        print('a crate comes back dead, which reads like a batch of renamed')
        print('items. Check that the `CRATES` entry points at the directory')
        print('holding the crate root, and that a `[lib] path` outside `src/`')
        print('is reachable from it.')
        return 1
    aw = surface.crates['autumn_web']
    by_crate = collections.Counter(c for c, *_ in occ)
    print(f'corpus: {len(files)} reader-facing markdown files')
    print(f'surface: {len(aw.mods)} modules, '
          f'{len(surface.names_of(aw, ()))} names at the crate root, '
          f'{len(surface.crates)} workspace crates modelled')
    print(f'checked: {len(occ)} crate-path occurrences '
          f'({by_crate.get("autumn_web", 0)} `autumn_web::`, '
          f'{len(occ) - by_crate.get("autumn_web", 0)} sibling-crate)')
    print(f'  resolved: {ok}')
    print(f'  opaque (re-export of a crate outside this workspace): '
          f'{sum(opaque.values())}')
    print(f'  waived (shown as output: compiler error or log line): {waived}')
    print('')
    for (crate, path, rel, line, broke, near) in sorted(
            dead, key=lambda d: (d[2], d[3])):
        hint = f'  (did you mean `{near}`?)' if near else ''
        parent = ('::' + '::'.join(broke.split('::')[:-1])
                  if '::' in broke else '')
        print(f'{rel}:{line}: `{crate}::{path}` does not resolve '
              f'-- no `{broke.split("::")[-1]}` in `{crate}{parent}`{hint}')
    for (crate, path, rel, line, src) in sorted(unimportable,
                                                key=lambda d: (d[2], d[3])):
        print(f'{rel}:{line}: `{crate}::{path}` cannot be imported '
              f'-- `{crate}` ships no Rust-linkable library target '
              f'(no `lib.rs` under `{src}` and no `[lib]` declaring one, or a '
              f'`[lib] crate-type` with no `lib`/`rlib`/`dylib`/`proc-macro` '
              f'in it), so nothing in it is nameable from another crate. Point '
              f'the reader at the CLI or at a library crate instead of a path.')
    print(f'defects: {len(dead) + len(unimportable)} '
          f'({len(dead)} unresolved, {len(unimportable)} unimportable; '
          f'{waived} waived)')
    return 1 if (dead or unimportable) else 0


def do_list():
    surface, files, occ, dead, unimportable, opaque, ok, waived = audit(ROOT)
    print(f'corpus: {len(files)} reader-facing markdown files')
    print(f'occurrences: {len(occ)}')
    print('')
    print('BY CRATE -- the prefixes the corpus actually writes.')
    for crate, n in sorted(collections.Counter(c for c, *_ in occ).items(),
                           key=lambda kv: (-kv[1], kv[0])):
        print(f'  {n:4d}  {crate}::')
    print('')
    print('OPAQUE -- re-exports of crates whose source is not in this tree.')
    print('A path here is NOT checked; the count is printed so it cannot grow')
    print('quietly.')
    for path, n in sorted(opaque.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f'  {n:4d}  {path}')
    print(f'  total: {sum(opaque.values())}')
    print('')
    print(f'external crates reached: {", ".join(sorted(surface.external))}')
    return 0


# ---------------------------------------------------------------- self-test

def _write(base, rel, text):
    p = os.path.join(base, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, 'w', encoding='utf8') as fh:
        fh.write(text)


def self_test():
    passed = failed = 0

    def check(name, got, want):
        nonlocal passed, failed
        if got == want:
            passed += 1
        else:
            failed += 1
            print(f'  FAIL {name}: got {got!r}, want {want!r}')

    with tempfile.TemporaryDirectory() as tmp:
        _write(tmp, 'fake/src/lib.rs', '''
pub mod app;
pub mod ui;
mod slug;
pub use slug::{contains_letter_or_number, slugify};
#[cfg(feature = "http-client")]
pub mod http_client;
#[cfg(feature = "http-client")]
pub use http_client as http;
pub use error::{AutumnError, AutumnResult};
pub mod error;
pub use maud::PreEscaped;
pub mod lock;
pub mod prelude;
pub mod openapi;
pub mod storage;
pub mod extract;
pub mod cluster;
pub use app::app;
pub(crate) mod route;
pub use route::Route;
pub use fake_macros::get;
pub use fake_edge as edge;
pub use fake_macros::edge;
pub mod reexports {
    pub use axum;
}
#[cfg(feature = "embed-assets")]
pub mod include_dir {
    pub use ::include_dir::*;
}
''')
        # `run` is a METHOD inside an impl block, not a module-level item, and
        # `app` is a module that also publishes a value of the same name.
        _write(tmp, 'fake/src/app.rs',
               'pub struct AppBuilder;\npub struct ApiVersion;\n'
               'pub(crate) struct InternalOnly;\n'
               'pub fn app() -> AppBuilder { AppBuilder }\n'
               'impl AppBuilder {\n'
               '    /// a doc comment with a stray { brace\n'
               '    pub async fn run(self) { let s = "a } brace in a string"; }\n'
               '}\n')
        _write(tmp, 'fake/src/route.rs', 'pub struct Route;\n')
        # A public facade re-exporting out of a PRIVATE module.
        _write(tmp, 'fake/src/ui/mod.rs',
               'mod widgets_css;\npub use widgets_css::WIDGETS_CSS_PATH;\n')
        _write(tmp, 'fake/src/ui/widgets_css.rs',
               'pub const WIDGETS_CSS_PATH: &str = "/x.css";\n')
        _write(tmp, 'fake/src/slug.rs',
               'pub fn contains_letter_or_number(s: &str) -> bool { true }\n'
               'pub fn slugify(s: &str) -> String { String::new() }\n')
        _write(tmp, 'fake/src/http_client.rs', 'pub struct Client;\n')
        _write(tmp, 'fake/src/error.rs',
               'pub struct AutumnError;\npub type AutumnResult<T> = Result<T, AutumnError>;\n')
        # An inline module behind a facade, plus a `#[macro_export]` macro that
        # must land at the crate root rather than in `lock`.
        _write(tmp, 'fake/src/lock.rs', '''
pub use db_impl::{Lock, LockGuard};
mod db_impl {
    pub struct Lock;
    pub struct LockGuard;
}
#[macro_export]
/// doc comment between the attribute and the macro
macro_rules! declassify { () => {} }
''')
        _write(tmp, 'fake/src/prelude.rs', 'pub use crate::error::AutumnError;\n')
        # An unexported `macro_rules!` is textually scoped -- no path names it.
        # A `pub(crate) use` republishes inside the crate only.
        _write(tmp, 'fake/src/extract.rs',
               'macro_rules! impl_extractor_deref { () => {} }\n'
               'pub struct Path;\n')
        _write(tmp, 'fake/src/cluster.rs',
               'mod node {\n'
               '    pub const LEAVE_BUDGET: u64 = 1;\n'
               '    pub const OPEN: u64 = 2;\n'
               '}\n'
               'pub(crate) use node::LEAVE_BUDGET;\n'
               'pub use node::OPEN;\n')
        # A trait and a derive macro of the SAME name, the derive re-exported
        # from the macros crate under the name in its attribute rather than the
        # name of the function carrying it.
        _write(tmp, 'fake/src/openapi.rs',
               'pub trait OpenApiSchema {}\npub use fake_macros::OpenApiSchema;\n')
        # `#[macro_export]` + `pub use <name>;`: addressable at the crate root
        # AND at this module path.
        _write(tmp, 'fake/src/storage/mod.rs', 'pub mod migrations;\n')
        _write(tmp, 'fake/src/storage/migrations.rs',
               '#[macro_export]\nmacro_rules! add_blob_column { () => {} }\n'
               'pub use add_blob_column;\n')
        _write(tmp, 'fake_edge/src/lib.rs', 'pub struct CapsuleRequest;\n')
        _write(tmp, 'fake_macros/src/lib.rs',
               '#[proc_macro_attribute]\npub fn get(a: TokenStream) -> TokenStream { a }\n'
               '#[proc_macro_attribute]\npub fn edge(a: TokenStream) -> TokenStream { a }\n'
               '#[proc_macro_derive(OpenApiSchema, attributes(schema))]\n'
               'pub fn derive_open_api_schema(a: TokenStream) -> TokenStream { a }\n')

        # A sibling a reader names DIRECTLY, rather than one reached through an
        # `autumn_web` re-export: the shape of every plugin crate in the real
        # workspace, and the surface this gate was blind to.
        _write(tmp, 'fake_sibling/src/lib.rs', 'pub mod plugin;\n')
        _write(tmp, 'fake_sibling/src/plugin.rs',
               'pub struct Thing;\npub(crate) struct Hidden;\n')

        s = Surface(tmp, {'autumn_web': 'fake/src', 'fake_macros': 'fake_macros/src',
                          'fake_edge': 'fake_edge/src',
                          'fake_sibling': 'fake_sibling/src'})

        check('plain module item', s.resolve('app::AppBuilder'), 'ok')
        check('module itself', s.resolve('app'), 'ok')
        check('missing item in real module', s.resolve('app::Nope'),
              'dead:app::Nope')
        check('missing module', s.resolve('nosuch::Thing'), 'dead:nosuch')
        check('crate-root re-export', s.resolve('AutumnError'), 'ok')
        check('crate-root re-export (type alias)', s.resolve('AutumnResult'), 'ok')
        # The defect this gate was built on.
        check('crate-root name that does not exist', s.resolve('Error'),
              'dead:Error')
        check('re-export from a private module', s.resolve('slugify'), 'ok')
        check('re-export through a private facade module',
              s.resolve('ui::WIDGETS_CSS_PATH'), 'ok')
        check('aliased module re-export', s.resolve('http::Client'), 'ok')
        check('aliased re-export keeps its own name too',
              s.resolve('http_client::Client'), 'ok')
        check('item behind an inline module facade', s.resolve('lock::Lock'), 'ok')
        check('#[macro_export] lands at the crate root',
              s.resolve('declassify'), 'ok')
        check('macro is NOT also under its defining module',
              s.resolve('lock::declassify'), 'dead:lock::declassify')
        check('re-export from a sibling workspace crate', s.resolve('get'), 'ok')
        check('derive macro is exported under its attribute name',
              s.resolve('openapi::OpenApiSchema'), 'ok')
        check('`pub use <crate-root macro>;` makes the module path real',
              s.resolve('storage::migrations::add_blob_column'), 'ok')
        check('glob re-export target', s.resolve('prelude::AutumnError'), 'ok')
        check('outside-the-workspace re-export is opaque',
              s.resolve('PreEscaped'), 'opaque')
        check('opaque module stays opaque at depth',
              s.resolve('reexports::axum::routing::get'), 'opaque')
        # `pub use ::include_dir::*` inside `mod include_dir` re-exports the
        # DEPENDENCY, not the module it sits in. Resolving the leading `::`
        # locally makes the glob resolve to itself and reports every real path
        # under it as dead.
        check('leading `::` names the external crate, not the local module',
              s.resolve('include_dir::Dir'), 'opaque')
        check('associated item past a real type is not checked',
              s.resolve('AutumnError::not_found_msg'), 'ok')
        check('suggestion for a near-miss', s.suggest('app::AppBuildr'),
              'AppBuilder')
        # A `pub(crate) mod` is not nameable from a reader's crate even when the
        # items inside it are `pub` and re-exported at the crate root. This
        # blessed `::autumn_web::route::Route` until the gate distinguished bare
        # `pub` from a restricted one.
        check('path through a pub(crate) module is dead',
              s.resolve('route::Route'), 'dead:route')
        check('…while its crate-root re-export resolves',
              s.resolve('Route'), 'ok')
        check('pub(crate) item is not externally nameable',
              s.resolve('app::InternalOnly'), 'dead:app::InternalOnly')
        # A method lives on the type, not in the module. `PUB_ITEM` anchored as
        # `^[ \t]*pub` matched it anyway and made `autumn_web::app::run` resolve.
        check('impl-block method is not a module item',
              s.resolve('app::run'), 'dead:app::run')
        check('module-scope fn still resolves', s.resolve('app::app'), 'ok')
        # `pub mod app` + `pub use app::app` -- the type namespace must win, or
        # the value shadows the module and anything under it resolves.
        check('module wins over a same-named value for traversal',
              s.resolve('app::AppBuilder'), 'ok')
        # `pub use fake_edge as edge;` is a single-segment re-export naming a
        # CRATE, and a macro of the same name is re-exported after it. The
        # module must survive in the type namespace or everything under
        # `edge::` is waved through as an associated item of the macro.
        check('crate re-exported under an alias is traversable',
              s.resolve('edge::CapsuleRequest'), 'ok')
        check('…and a bogus item under it is still dead',
              s.resolve('edge::Bogus'), 'dead:edge::Bogus')
        check('unexported macro_rules! is not nameable',
              s.resolve('extract::impl_extractor_deref'),
              'dead:extract::impl_extractor_deref')
        check('…while a real item beside it resolves',
              s.resolve('extract::Path'), 'ok')
        check('pub(crate) use is not a public re-export',
              s.resolve('cluster::LEAVE_BUDGET'), 'dead:cluster::LEAVE_BUDGET')
        check('…while a bare pub use beside it resolves',
              s.resolve('cluster::OPEN'), 'ok')
        # Braces inside strings and comments must not shift module scope.
        check('literals and comments do not break scope tracking',
              s.resolve('app::ApiVersion'), 'ok')

        # -- brace expansion, as the guide actually writes imports ------------
        check('brace group', sorted(expand_braces('a::{b,c}')), ['a::b', 'a::c'])
        check('nested brace group', sorted(expand_braces('a::{b,c::{d,e}}')),
              ['a::b', 'a::c::d', 'a::c::e'])
        check('self in brace group', sorted(expand_braces('a::{self,b}')),
              ['a', 'a::b'])

        # -- corpus extraction ------------------------------------------------
        pat = re.compile(r'\b(autumn_web|fake_sibling|autumn_cli)::')
        _write(tmp, 'docs/guide/x.md',
               'use autumn_web::{app::AppBuilder, Error};\n'
               '| `error[E0432]: unresolved import `autumn_web::foo`` | x | y |\n')
        found = occurrences(tmp, ['docs/guide/x.md'], pat)
        paths = sorted(p for (_, p, _, _, w) in found if not w)
        check('brace-grouped doc import is expanded', paths,
              ['Error', 'app::AppBuilder'])
        check('compiler-error line is waived',
              [p for (_, p, _, _, w) in found if w], ['foo'])
        # The waiver covers the error's own table cell, not the row: the FIX
        # column is a live recommendation and must stay audited.
        _write(tmp, 'docs/guide/mig.md',
               '| `error[E0063]: missing field` | a literal | '
               'add `autumn_web::app::AppBuilder` |\n'
               'INFO  autumn_web::route::Route: started\n')
        rows = occurrences(tmp, ['docs/guide/mig.md'], pat)
        check('path inside the error cell is waived',
              sorted(p for (_, p, _, _, w) in rows if w),
              ['route::Route'])
        check('path in the fix column is still audited',
              sorted(p for (_, p, _, _, w) in rows if not w),
              ['app::AppBuilder'])

        # -- sibling crates are scanned, and attributed to their own crate ----
        # Before this, `PREFIX_RE` was `autumn_web::` alone: a path into a
        # sibling was not a passing check, it was not a check at all.
        _write(tmp, 'docs/guide/sib.md',
               'use fake_sibling::plugin::Thing;\n'
               'use autumn_web::app::AppBuilder;\n')
        sib = occurrences(tmp, ['docs/guide/sib.md'], pat)
        check('sibling-crate path is scanned',
              sorted((c, p) for (c, p, _, _, w) in sib if not w),
              [('autumn_web', 'app::AppBuilder'),
               ('fake_sibling', 'plugin::Thing')])
        # The crate a path is resolved AGAINST has to be the one it names.
        # Resolving a sibling path against `autumn_web` is how a real item in
        # the wrong crate passes, and a real item in the right crate fails.
        check('sibling path resolves against its own crate',
              s.resolve('plugin::Thing', 'fake_sibling'), 'ok')
        check('…and a sibling miss is dead, not silently ok',
              s.resolve('plugin::Nope', 'fake_sibling'), 'dead:plugin::Nope')
        check('pub(crate) in a sibling is not reader-nameable',
              s.resolve('plugin::Hidden', 'fake_sibling'),
              'dead:plugin::Hidden')
        # The same spelling must NOT resolve against `autumn_web`: that is the
        # bug a single hardcoded root produces in both directions.
        check('a sibling path is not resolved against autumn_web',
              s.resolve('plugin::Thing'), 'dead:plugin')

        # -- crates with no library target ------------------------------------
        # The defect this class exists for: every other signal says the path is
        # fine, and none of them is the one that matters.
        _write(tmp, 'bin_only/Cargo.toml',
               '[package]\nname = "bin-only"\nversion = "0.1.0"\n')
        _write(tmp, 'bin_only/src/main.rs', 'pub mod check;\nfn main() {}\n')
        _write(tmp, 'lib_crate/Cargo.toml',
               '[package]\nname = "lib-crate"\nversion = "0.1.0"\n')
        _write(tmp, 'lib_crate/src/lib.rs', 'pub mod thing;\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["bin_only", "lib_crate", '
               '"examples/demo"]\n')
        ws = workspace_crates(tmp)
        check('binary-only crate is detected as having no lib',
              ws.get('bin_only'), (os.path.join('bin_only', 'src'), None))
        check('library crate reports its crate-root file',
              ws.get('lib_crate'),
              (os.path.join('lib_crate', 'src'),
               os.path.join('lib_crate', 'src', 'lib.rs')))
        check('binary-only set is exactly the crates with no lib target',
              sorted(binary_only_crates(tmp)), ['bin_only'])
        check('a missing member directory is skipped, not fatal',
              'demo' in ws, False)
        # `[lib]` with no `src/lib.rs` still ships a library (the manifest can
        # point `path` anywhere), so the manifest has to be consulted too.
        _write(tmp, 'bin_only/Cargo.toml',
               '[package]\nname = "bin-only"\nversion = "0.1.0"\n'
               '[lib]\npath = "src/other.rs"\n')
        check('an explicit [lib] counts even without src/lib.rs',
              sorted(binary_only_crates(tmp)), [])

        # -- `autolib = false` disables auto-discovery of src/lib.rs ----------
        # The file is present and Cargo builds NO library from it. Reading the
        # file as proof would skip the binary-only class and report a path into
        # such a crate as an unresolved symbol instead.
        _write(tmp, 'no_autolib/Cargo.toml',
               '[package]\nname = "no-autolib"\nversion = "0.1.0"\n'
               'autolib = false\n')
        _write(tmp, 'no_autolib/src/lib.rs', 'pub mod thing;\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["bin_only", "lib_crate", '
               '"no_autolib", "examples/demo"]\n')
        check('autolib = false means no library target despite src/lib.rs',
              sorted(binary_only_crates(tmp)), ['no_autolib'])
        # An explicit [lib] still wins: `autolib` governs DISCOVERY only.
        _write(tmp, 'no_autolib/Cargo.toml',
               '[package]\nname = "no-autolib"\nversion = "0.1.0"\n'
               'autolib = false\n[lib]\npath = "src/lib.rs"\n')
        check('an explicit [lib] beats autolib = false',
              sorted(binary_only_crates(tmp)), [])

        # -- every Cargo spelling of "not published" is honoured --------------
        # Reusing `_published` rather than testing `publish is False` here:
        # a private LIBRARY read as published fails the gate as an undeclared
        # crate, which is the self-maintaining check firing on the one shape it
        # exists to tolerate.
        _write(tmp, 'no_autolib/Cargo.toml',
               '[package]\nname = "no-autolib"\nversion = "0.1.0"\n')
        _write(tmp, 'priv_empty/Cargo.toml',
               '[package]\nname = "priv-empty"\nversion = "0.1.0"\n'
               'publish = []\n')
        _write(tmp, 'priv_empty/src/lib.rs', 'pub mod thing;\n')
        _write(tmp, 'priv_inherit/Cargo.toml',
               '[package]\nname = "priv-inherit"\nversion = "0.1.0"\n'
               'publish.workspace = true\n')
        _write(tmp, 'priv_inherit/src/lib.rs', 'pub mod thing;\n')
        _write(tmp, 'registry_only/Cargo.toml',
               '[package]\nname = "registry-only"\nversion = "0.1.0"\n'
               'publish = ["some-registry"]\n')
        _write(tmp, 'registry_only/src/lib.rs', 'pub mod thing;\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["lib_crate", "priv_empty", '
               '"priv_inherit", "registry_only"]\n'
               '[workspace.package]\npublish = false\n')
        ws2 = workspace_crates(tmp)
        check('publish = [] is not published', 'priv_empty' in ws2, False)
        check('publish.workspace = true inherits false',
              'priv_inherit' in ws2, False)
        check('publish = ["registry"] IS published',
              'registry_only' in ws2, True)
        check('an absent publish key is still published',
              'lib_crate' in ws2, True)

        # -- `[lib] path` is preserved, not reduced to a boolean -------------
        # A boolean said "library"; the scanner then read `src/lib.rs`, found
        # nothing, and reported every path into the crate as dead — after the
        # gate had just told the author to map it.
        _write(tmp, 'odd_lib/Cargo.toml',
               '[package]\nname = "odd-lib"\nversion = "0.1.0"\n'
               '[lib]\npath = "src/api.rs"\n')
        _write(tmp, 'odd_lib/src/api.rs', 'pub mod thing;\npub struct Real;\n')
        _write(tmp, 'odd_lib/src/thing.rs', 'pub struct Inner;\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["lib_crate", "odd_lib"]\n')
        check('[lib] path is reported, anchored at the manifest dir',
              workspace_crates(tmp).get('odd_lib'),
              (os.path.join('odd_lib', 'src'),
               os.path.join('odd_lib', 'src', 'api.rs')))
        check('a [lib] table with no path defaults to src/lib.rs',
              _lib_file(tmp, 'lib_crate', {'lib': {}}), 'src/lib.rs')
        # Scanned through the declared root, the surface is real.
        s2 = Surface(tmp, {'odd_lib': 'odd_lib/src'},
                     rootpaths={'odd_lib': os.path.join(tmp, 'odd_lib',
                                                        'src', 'api.rs')})
        check('a crate scanned via its declared root resolves',
              s2.resolve('Real', 'odd_lib'), 'ok')
        check('…and its submodules resolve too',
              s2.resolve('thing::Inner', 'odd_lib'), 'ok')
        check('a declared-root crate is not empty', s2.empty(), [])
        # The guard: the same crate mapped WITHOUT its root file is silent
        # otherwise — every path into it reads as a rename.
        s3 = Surface(tmp, {'odd_lib': 'odd_lib/src'})
        check('a crate whose root file is missing is reported empty',
              s3.empty(), ['odd_lib'])
        check('…and its paths would otherwise look merely renamed',
              s3.resolve('Real', 'odd_lib'), 'dead:Real')

        # -- a library root OUTSIDE `src/` ------------------------------------
        # `[lib] path = "lib/api.rs"` is valid. Treating the declared path as
        # `src`-relative looked for `<crate>/src/lib/api.rs`: a file that does
        # not exist (so the crate came back empty) or, worse, one that does and
        # the wrong API got audited. Both halves have to anchor at the manifest
        # directory — the root file AND the submodule base under it.
        _write(tmp, 'out_of_src/Cargo.toml',
               '[package]\nname = "out-of-src"\nversion = "0.1.0"\n'
               '[lib]\npath = "lib/api.rs"\n')
        _write(tmp, 'out_of_src/lib/api.rs',
               'pub mod deep;\npub struct Outside;\n')
        _write(tmp, 'out_of_src/lib/deep.rs', 'pub struct Nested;\n')
        # A decoy at the path the old `src`-relative join would have read.
        _write(tmp, 'out_of_src/src/lib/api.rs', 'pub struct Decoy;\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["lib_crate", "odd_lib", '
               '"out_of_src"]\n')
        check('a root outside src/ is anchored at the manifest dir',
              workspace_crates(tmp).get('out_of_src'),
              (os.path.join('out_of_src', 'src'),
               os.path.join('out_of_src', 'lib', 'api.rs')))
        s4 = Surface(tmp, {'out_of_src': 'out_of_src/src'},
                     rootpaths={'out_of_src': os.path.join(
                         tmp, 'out_of_src', 'lib', 'api.rs')})
        check('the real API resolves through a root outside src/',
              s4.resolve('Outside', 'out_of_src'), 'ok')
        check('submodules resolve beside that root, not under src/',
              s4.resolve('deep::Nested', 'out_of_src'), 'ok')
        check('the decoy under src/ is NOT what got audited',
              s4.resolve('Decoy', 'out_of_src'), 'dead:Decoy')

        # -- `[lib] name` is the prefix a reader writes -----------------------
        # Downstream paths start with the LIBRARY name, not the package name.
        # Deriving the ident from `[package].name` alone asks for a `CRATES`
        # entry nobody writes and leaves the real prefix out of `prefix_re`,
        # which is an unaudited prefix that looks exactly like a clean one.
        _write(tmp, 'renamed/Cargo.toml',
               '[package]\nname = "renamed-pkg"\nversion = "0.1.0"\n'
               '[lib]\nname = "sdk"\n')
        _write(tmp, 'renamed/src/lib.rs', 'pub struct Client;\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["lib_crate", "renamed"]\n')
        ws3 = workspace_crates(tmp)
        check('[lib] name becomes the import ident', 'sdk' in ws3, True)
        check('…and the package name is not the ident',
              'renamed_pkg' in ws3, False)
        check('a dash in [lib] name normalizes to an underscore',
              _import_ident({'lib': {'name': 'my-sdk'}}, 'pkg'), 'my_sdk')
        check('an absent [lib] name falls back to the package name',
              _import_ident({'lib': {'proc-macro': True}}, 'autumn-macros'),
              'autumn_macros')

        # -- globbed `members`, and cargo's asymmetric `exclude` -------------
        # Reading a glob as a literal directory does not merely miss crates,
        # it VOIDS the coverage guarantee: `undeclared_crates` stops seeing
        # them, so the gate keeps passing while modelling none of them.
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n')
        _write(tmp, 'crates/alpha/src/lib.rs', 'pub struct A;\n')
        _write(tmp, 'crates/beta/Cargo.toml',
               '[package]\nname = "beta"\nversion = "0.1.0"\n')
        _write(tmp, 'crates/beta/src/lib.rs', 'pub struct B;\n')
        _write(tmp, 'crates/notacrate/README.md', 'no manifest here\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["crates/*"]\n')
        check('a glob expands to the manifests under it',
              _member_dirs(tmp, {'members': ['crates/*']}),
              ['crates/alpha', 'crates/beta'])
        check('a globbed member is actually read',
              sorted(workspace_crates(tmp)), ['alpha', 'beta'])
        # `exclude` is literal-prefix, NOT a glob: cargo's own asymmetry.
        check('exclude drops a globbed member by prefix',
              _member_dirs(tmp, {'members': ['crates/*'],
                                 'exclude': ['crates/beta']}),
              ['crates/alpha'])
        check('a glob in exclude matches nothing, as cargo has it',
              _member_dirs(tmp, {'members': ['crates/*'],
                                 'exclude': ['crates/*']}),
              ['crates/alpha', 'crates/beta'])
        check('an explicitly listed member wins over exclude',
              _member_dirs(tmp, {'members': ['crates/alpha', 'crates/beta'],
                                 'exclude': ['crates/beta']}),
              ['crates/alpha', 'crates/beta'])
        check('a literal member entry still works',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha'])

        # -- implicit members: in-tree path dependencies ---------------------
        # Cargo treats an unexcluded in-tree path dependency as a member even
        # when `members` omits it. An earlier revision of this gate did not
        # walk them, on the reasoning that they are what the workspace BUILDS
        # rather than what it PUBLISHES — which was wrong, and left a
        # published sibling added through `path = "../new"` alone entirely
        # unaudited while `undeclared_crates` stayed green.
        _write(tmp, 'implicit/Cargo.toml',
               '[package]\nname = "implicit"\nversion = "0.1.0"\n')
        _write(tmp, 'implicit/src/lib.rs', 'pub struct I;\n')
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n'
               '[dependencies]\nimplicit = { path = "../../implicit" }\n')
        check('an in-tree path dep is an implicit member',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha', 'implicit'])
        # End to end, through the root manifest: the implicit member arrives
        # in `workspace_crates`, which is what `undeclared_crates` reads.
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["crates/alpha"]\n')
        check('…and it reaches workspace_crates, so coverage can see it',
              sorted(workspace_crates(tmp)), ['alpha', 'implicit'])
        # A dependency outside the workspace root is never a member.
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n'
               '[dependencies]\nout = { path = "../../../elsewhere" }\n')
        check('a path dep outside the root is not a member',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha'])
        # A dependency with its own [workspace] table is a separate workspace.
        _write(tmp, 'nested/Cargo.toml',
               '[workspace]\nmembers = []\n'
               '[package]\nname = "nested"\nversion = "0.1.0"\n')
        _write(tmp, 'nested/src/lib.rs', 'pub struct N;\n')
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n'
               '[dependencies]\nnested = { path = "../../nested" }\n')
        check('a nested workspace root is not absorbed as a member',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha'])
        # dev- and build-dependencies count, and so do target-gated ones.
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n'
               '[dev-dependencies]\nimplicit = { path = "../../implicit" }\n')
        check('a dev-dependency path is a member too',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha', 'implicit'])
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n'
               '[target."cfg(unix)".dependencies]\n'
               'implicit = { path = "../../implicit" }\n')
        check('a target-gated path dep is a member too',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha', 'implicit'])
        # An inherited `workspace = true` path resolves against the ROOT, not
        # the dependent package — and the lookup key is `package` when renamed.
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n'
               '[dependencies]\nrenamed-dep = { workspace = true, '
               'package = "implicit" }\n')
        check('an inherited workspace path dep resolves against the root',
              _member_dirs(tmp, {'members': ['crates/alpha'],
                                 'dependencies': {
                                     'implicit': {'path': 'implicit'}}}),
              ['crates/alpha', 'implicit'])
        # `exclude` still wins over an implicit member.
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n'
               '[dependencies]\nimplicit = { path = "../../implicit" }\n')
        check('exclude drops an implicit member',
              _member_dirs(tmp, {'members': ['crates/alpha'],
                                 'exclude': ['implicit']}),
              ['crates/alpha'])

        # -- the root package is a member when the root manifest has one -----
        _write(tmp, 'crates/alpha/Cargo.toml',
               '[package]\nname = "alpha"\nversion = "0.1.0"\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["crates/alpha"]\n')
        check('no [package] at the root adds no root member',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha'])
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["crates/alpha"]\n'
               '[package]\nname = "rootpkg"\nversion = "0.1.0"\n')
        _write(tmp, 'src/lib.rs', 'pub struct Root;\n')
        check('a root [package] is a member even without "." in members',
              _member_dirs(tmp, {'members': ['crates/alpha']}),
              ['crates/alpha', '.'])
        check('…and the root package reaches workspace_crates',
              'rootpkg' in workspace_crates(tmp), True)

        # -- crate types rustc cannot link against ---------------------------
        # Same shape as the `autumn-cli` defect this gate exists to catch: a
        # `src/lib.rs` sits right there and the path still does not resolve for
        # a dependent, because cdylib/staticlib build for C, not for rustc.
        check('cdylib alone is not importable',
              _rust_linkable({'crate-type': ['cdylib']}), False)
        check('staticlib alone is not importable',
              _rust_linkable({'crate-type': ['staticlib']}), False)
        check('cdylib + rlib IS importable',
              _rust_linkable({'crate-type': ['cdylib', 'rlib']}), True)
        check('an absent crate-type defaults to lib',
              _rust_linkable({'proc-macro': True}), True)
        check('proc-macro stays linkable',
              _rust_linkable({'crate-type': ['proc-macro']}), True)
        # An explicitly EMPTY list is a denial, not the default. Verified
        # against cargo: `crate_types=[]` in metadata, and a dependent's
        # `cargo check` prints "provides no linkable target" and fails.
        check('an empty crate-type list is a denial',
              _rust_linkable({'crate-type': []}), False)
        check('an absent crate-type key is the default',
              _rust_linkable({}), True)
        check('the crate_type spelling is honoured too',
              _rust_linkable({'crate_type': ['cdylib']}), False)
        check('a malformed crate-type is read as the default, not a defect',
              _rust_linkable({'crate-type': 'cdylib'}), True)
        _write(tmp, 'cdyl/Cargo.toml',
               '[package]\nname = "cdyl"\nversion = "0.1.0"\n'
               '[lib]\ncrate-type = ["cdylib"]\n')
        _write(tmp, 'cdyl/src/lib.rs', 'pub struct NotReachable;\n')
        _write(tmp, 'Cargo.toml',
               '[workspace]\nmembers = ["crates/alpha", "cdyl"]\n')
        check('a cdylib-only crate lands in the unimportable class',
              sorted(binary_only_crates(tmp)), ['cdyl'])
        check('…so it is NOT demanded as an undeclared library crate',
              'cdyl' in undeclared_crates(tmp), False)

        # -- the reader-facing scope matches the sibling gates ----------------
        check('guide is corpus', reader_facing('docs/guide/a.md'), True)
        check('plans are not corpus', reader_facing('docs/plans/a.md'), False)
        check('example README is corpus',
              reader_facing('examples/todo/README.md'), True)
        check('example source doc is not corpus',
              reader_facing('examples/todo/NOTES.md'), False)

    print(f'self-test: {passed}/{passed + failed} passed')
    return 1 if failed else 0



def print_corpus():
    """Print this gate's resolved corpus, one path per line.

    `scripts/check-docs-scope.sh` compares these lists across the four gates
    that share a reader-facing corpus. It asks each gate what it reads rather
    than re-deriving it from this file's source, because a corpus is widened in
    several places at once — the `git ls-files` globs, the scope tuples, the
    `.md.tmpl` clause, the crate `readme =` manifests — and a checker that
    models some of those rules reports agreement over the rest. Asking cannot
    drift from the answer; modelling can, and did.
    """
    for f in sorted(corpus(ROOT)):
        print(f)
    return 0

if MODE == '--self-test':
    sys.exit(self_test())
elif MODE == '--list':
    sys.exit(do_list())
elif MODE == '--corpus':
    sys.exit(print_corpus())
else:
    sys.exit(main())
PYEOF
}

case "${1:-}" in
  --list)
    run_py --list "$root"
    ;;
  --self-test)
    run_py --self-test "$root"
    ;;
  --corpus)
    run_py --corpus "$root"
    ;;
  "")
    echo "Checking workspace-crate symbol paths across the reader-facing docs..."
    if run_py --check "$root"; then
      echo "Symbol drift gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the docs put a workspace-crate path in front of a reader that they cannot
write (above).

Where the path reaches name resolution, `rustc` reports it against the READER's
file, not against the page, so they are told their code is wrong when the
documentation is — at the first build of a feature they have not used before.
Where it does NOT (a return type under `#[autumn_web::main]`, which the macro
discards), nothing reports it at all and the reader simply carries away a name
that does not exist.

Fix each one where it lives:
  - no library     -> the crate ships only a binary (`autumn-cli`), so NO path
                      into it resolves and no rename helps. Document the
                      command, or an API a library crate exports.
  - renamed item   -> use the current name (the `did you mean` hint is the
                      closest name in the same module)
  - moved item     -> use the path a reader can actually write; it is usually a
                      re-export, not where the item is defined
                      (`autumn_web::http::Client`, not
                      `autumn_web::http_client::Client`)
  - never existed  -> drop it, or name the item that does the job
  - shown, not written -> a path inside a compiler-error line or a log line is
                      already waived as output; if you are illustrating a
                      failure, quote the compiler error with it
  - non-public     -> a `pub(crate)`/`pub(super)` module is E0603 for a reader
                      even when the item inside is re-exported at the crate
                      root: name the re-export (`::autumn_web::Route`, not
                      `::autumn_web::route::Route`)

Inspect what the gate read:  scripts/check-docs-symbols.sh --list
EOF
      exit 1
    fi
    ;;
  *)
    echo "usage: $0 [--list|--self-test]" >&2
    exit 2
    ;;
esac
