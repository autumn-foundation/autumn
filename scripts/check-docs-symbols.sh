#!/usr/bin/env bash
# Symbol drift gate: every `autumn_web::…` path the reader-facing docs put in
# front of someone must name an item that exists.
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
# The reader-facing corpus carries 874 `rust` fences naming 1,218
# `autumn_web::…` paths across 389 distinct spellings — a larger copy-surface
# than the env layer (689 occurrences) and the `autumn.toml` layer (172 fences)
# combined. A renamed or never-shipped item leaves behind a line that looks
# exactly like a working one, and nothing in the tree could tell the difference.
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
#      is how most import lines in the guide are written.
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
#          every attribute macro a handler is decorated with actually lives.
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
#     them. 38 of the 1,218 occurrences land here; `--list` prints all of them,
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
# WAIVER — one rule, not a list. A path quoted inside a COMPILER ERROR MESSAGE
# is being shown as broken on purpose: `docs/migrations/TEMPLATE.md` and
# `docs/migrations/next.md` both carry the migration-guide cheat-sheet row
#
#     | `error[E0432]: unresolved import `autumn_web::foo`` | … | `use autumn_web::bar;` |
#
# whose entire job is to display a path that does not resolve. So a line
# carrying an `error[E1234]` code is read as illustrating a failure rather than
# recommending a path, and its occurrences are counted as waived rather than
# suppressed by name. A named waiver list would have to grow every time a
# migration guide quotes a real rename; this rule does not.
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
import os
import pathlib
import re
import subprocess
import sys
import tempfile

MODE = sys.argv[1]
ROOT = sys.argv[2]

# The workspace crates a documented path can reach into. `autumn_web` is the
# one readers name; the others are named only because `autumn_web` re-exports
# out of them, and a path that lands in one has to keep resolving there.
CRATES = {
    'autumn_web': 'autumn/src',
    'autumn_macros': 'autumn-macros/src',
    'autumn_edge': 'autumn-edge/src',
    'autumn_search': 'autumn-search/src',
}

# ------------------------------------------------------------------ parsing

PUB_ITEM = re.compile(
    r'^[ \t]*pub(?:\s*\([^)]*\))?\s+'
    r'(?:async\s+|unsafe\s+|extern\s+"[^"]*"\s+|const\s+)*'
    r'(?:struct|enum|trait|fn|type|const|static|union)\s+([a-zA-Z_]\w*)', re.M)
PUB_MOD_DECL = re.compile(
    r'^[ \t]*pub(?:\s*\([^)]*\))?\s+mod\s+([a-zA-Z_]\w*)\s*;', re.M)
# Every `mod x;`, public or not. A private module is not itself public surface,
# but it is routinely the FILE a public facade re-exports out of, so the tree
# has to contain it or the re-export target cannot be followed.
ANY_MOD_DECL = re.compile(
    r'^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([a-zA-Z_]\w*)\s*;', re.M)
PUB_USE = re.compile(
    r'^[ \t]*pub(?:\s*\([^)]*\))?\s+use\s+(.+?);[ \t]*$', re.M | re.S)
INLINE_MOD = re.compile(
    r'^([ \t]*)(pub(?:\s*\([^)]*\))?\s+)?mod\s+([a-zA-Z_]\w*)\s*\{', re.M)
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


def split_inline_mods(text):
    """Split `mod x { … }` blocks out of `text`.

    Returns (text_without_them, [(name, is_pub, body), …]) so the body is
    scanned as its own module rather than having its items credited to the
    parent — which is what put `Lock` at `autumn_web::lock` instead of
    `autumn_web::lock::db_impl`.
    """
    out, mods, pos = [], [], 0
    while True:
        m = INLINE_MOD.search(text, pos)
        if not m:
            break
        brace = text.index('{', m.start())
        depth, i = 0, brace
        while i < len(text):
            if text[i] == '{':
                depth += 1
            elif text[i] == '}':
                depth -= 1
                if depth == 0:
                    break
            i += 1
        out.append(text[pos:m.start()])
        mods.append((m.group(3), bool(m.group(2)), text[brace + 1:i]))
        pos = i + 1
    out.append(text[pos:])
    return ''.join(out), mods


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

    def __init__(self, ident, srcdir):
        self.ident = ident
        self.src = srcdir
        self.mods = {}            # tuple(path) -> {name: 'item'|'mod'}
        self.uses = {}            # tuple(path) -> [(target, leaf, alias, glob)]
        self.exported_macros = set()
        if os.path.isfile(os.path.join(self.src, 'lib.rs')):
            self._scan_file([])
        # `#[macro_export]` hoists a macro to the crate root regardless of the
        # module it is written in.
        for name in self.exported_macros:
            self.mods.setdefault((), {})[name] = 'item'

    def _modfile(self, mp):
        if not mp:
            return os.path.join(self.src, 'lib.rs')
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
        self.mods.setdefault(key, {})
        self.uses.setdefault(key, [])
        for name in MACRO_EXPORT.findall(txt):
            self.exported_macros.add(name)
        body, inline = split_inline_mods(txt)
        for name in PUB_ITEM.findall(body):
            self.mods[key][name] = 'item'
        for name in MACRO_RULES.findall(body):
            # A `#[macro_export]` macro is addressable at the CRATE ROOT and
            # nowhere else: `autumn_web::declassify` resolves,
            # `autumn_web::classify::declassify` does not. Registering it in
            # its defining module too would bless a path rustc rejects.
            if name not in self.exported_macros:
                self.mods[key][name] = 'item'
        for name in PROC_MACRO_DERIVE.findall(body):
            self.mods[key][name] = 'item'
        for name in PUB_MOD_DECL.findall(body):
            self.mods[key][name] = 'mod'
        for raw in PUB_USE.findall(body):
            self._parse_use(key, raw)
        for (name, is_pub, sub) in inline:
            if is_pub:
                self.mods[key][name] = 'mod'
            self._scan_text(key + (name,), sub)
        if file_mp is not None:
            for name in ANY_MOD_DECL.findall(body):
                self._scan_file(list(file_mp) + [name])

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

    def __init__(self, root, crates=CRATES):
        self.crates = {ident: Crate(ident, os.path.join(root, rel))
                       for ident, rel in crates.items()}
        self.external = set()
        self._memo = {}

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
            if r[0] == 'opaque':
                out[alias] = 'opaque'
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

    def resolve(self, path):
        """'ok' | 'opaque' | 'dead:<the prefix that broke>'."""
        segs = path.split('::')
        c, cur = self.crates['autumn_web'], ()
        for i, s in enumerate(segs):
            if (cur + (s,)) in c.mods:
                cur = cur + (s,)
                continue
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
            # A leaf item: everything after it is an associated item, which
            # this gate does not claim to check.
            return 'ok'
        return 'ok'

    def suggest(self, path):
        """Closest existing sibling for the segment that broke, or None."""
        import difflib
        segs = path.split('::')
        c, cur = self.crates['autumn_web'], ()
        for s in segs:
            if (cur + (s,)) in c.mods:
                cur = cur + (s,)
                continue
            names = self.names_of(c, cur)
            if s in names:
                v = names[s]
                if isinstance(v, tuple) and v[0] == 'modref':
                    c, cur = self.crates[v[1]], v[2]
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
INCLUDE_DIRS = ('docs/guide/', 'docs/migrations/', 'skills/', 'agents/')
INCLUDE_FILES = ('README.md', 'EXAMPLES.md', 'CONTRIBUTING.md', 'STABILITY.md',
                 'docs/plugins.md')
INCLUDE_README_DIRS = ('examples/',)


def reader_facing(path):
    return (path.startswith(INCLUDE_DIRS) or path in INCLUDE_FILES
            or (path.startswith(INCLUDE_README_DIRS)
                and pathlib.PurePath(path).name == 'README.md'))


def corpus(root):
    out = subprocess.run(['git', 'ls-files', '-z', '*.md', '*.md.tmpl'],
                         cwd=root, capture_output=True, text=True).stdout
    return [f for f in out.split('\0')
            if f and (reader_facing(f) or f.endswith('.md.tmpl'))]


# `autumn_web::` followed by a path, optionally brace-grouped at any depth --
# `autumn_web::{get, post}` and `autumn_web::db::{TxOptions, IsolationLevel}`
# are how most import lines in the guide are actually written.
PATH_RE = re.compile(
    r'\bautumn_web::((?:[a-zA-Z_]\w*::)*(?:\{[^{}]*\}|[a-zA-Z_]\w*))')
# A line quoting a compiler error is displaying a path that does NOT resolve.
ILLUSTRATIVE = re.compile(r'error\[E\d{4}\]')


def occurrences(root, files):
    """[(path, file, line, waived)] for every documented `autumn_web::` path."""
    found = []
    for rel in files:
        full = os.path.join(root, rel)
        try:
            with open(full, encoding='utf8', errors='replace') as fh:
                lines = fh.readlines()
        except OSError:
            continue
        for n, line in enumerate(lines, 1):
            waived = bool(ILLUSTRATIVE.search(line))
            for m in PATH_RE.finditer(line):
                for path in expand_braces(m.group(1)):
                    path = path.strip()
                    if path:
                        found.append((path, rel, n, waived))
    return found


def audit(root):
    surface = Surface(root)
    files = corpus(root)
    occ = occurrences(root, files)
    dead, opaque, ok, waived = [], collections.Counter(), 0, 0
    for (path, rel, line, is_waived) in occ:
        if is_waived:
            waived += 1
            continue
        r = surface.resolve(path)
        if r.startswith('dead:'):
            dead.append((path, rel, line, r[5:], surface.suggest(path)))
        elif r == 'opaque':
            opaque[path] += 1
        else:
            ok += 1
    return surface, files, occ, dead, opaque, ok, waived


def main():
    surface, files, occ, dead, opaque, ok, waived = audit(ROOT)
    aw = surface.crates['autumn_web']
    print(f'corpus: {len(files)} reader-facing markdown files')
    print(f'surface: {len(aw.mods)} modules, '
          f'{len(surface.names_of(aw, ()))} names at the crate root, '
          f'{len(surface.crates)} workspace crates')
    print(f'checked: {len(occ)} `autumn_web::` occurrences')
    print(f'  resolved: {ok}')
    print(f'  opaque (re-export of a crate outside this workspace): '
          f'{sum(opaque.values())}')
    print(f'  waived (quoted inside a compiler error message): {waived}')
    print('')
    if dead:
        for (path, rel, line, broke, near) in sorted(dead,
                                                     key=lambda d: (d[1], d[2])):
            hint = f'  (did you mean `{near}`?)' if near else ''
            print(f'{rel}:{line}: `autumn_web::{path}` does not resolve '
                  f'-- no `{broke.split("::")[-1]}` in '
                  f'`autumn_web{"::" + "::".join(broke.split("::")[:-1]) if "::" in broke else ""}`'
                  f'{hint}')
    print(f'defects: {len(dead)} ({waived} waived)')
    return 1 if dead else 0


def do_list():
    surface, files, occ, dead, opaque, ok, waived = audit(ROOT)
    print(f'corpus: {len(files)} reader-facing markdown files')
    print(f'occurrences: {len(occ)}')
    print('')
    print('OPAQUE -- re-exports of crates whose source is not in this tree.')
    print('A path here is NOT checked; the count is printed so it cannot grow')
    print('quietly.')
    for path, n in sorted(opaque.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f'  {n:4d}  autumn_web::{path}')
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
pub use fake_macros::get;
pub mod reexports {
    pub use axum;
}
#[cfg(feature = "embed-assets")]
pub mod include_dir {
    pub use ::include_dir::*;
}
''')
        _write(tmp, 'fake/src/app.rs', 'pub struct AppBuilder;\npub struct ApiVersion;\n')
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
        _write(tmp, 'fake_macros/src/lib.rs',
               '#[proc_macro_attribute]\npub fn get(a: TokenStream) -> TokenStream { a }\n'
               '#[proc_macro_derive(OpenApiSchema, attributes(schema))]\n'
               'pub fn derive_open_api_schema(a: TokenStream) -> TokenStream { a }\n')

        s = Surface(tmp, {'autumn_web': 'fake/src', 'fake_macros': 'fake_macros/src'})

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

        # -- brace expansion, as the guide actually writes imports ------------
        check('brace group', sorted(expand_braces('a::{b,c}')), ['a::b', 'a::c'])
        check('nested brace group', sorted(expand_braces('a::{b,c::{d,e}}')),
              ['a::b', 'a::c::d', 'a::c::e'])
        check('self in brace group', sorted(expand_braces('a::{self,b}')),
              ['a', 'a::b'])

        # -- corpus extraction ------------------------------------------------
        _write(tmp, 'docs/guide/x.md',
               'use autumn_web::{app::AppBuilder, Error};\n'
               '| `error[E0432]: unresolved import `autumn_web::foo`` | x | y |\n')
        found = occurrences(tmp, ['docs/guide/x.md'])
        paths = sorted(p for (p, _, _, w) in found if not w)
        check('brace-grouped doc import is expanded', paths,
              ['Error', 'app::AppBuilder'])
        check('compiler-error line is waived',
              [p for (p, _, _, w) in found if w], ['foo'])

        # -- the reader-facing scope matches the sibling gates ----------------
        check('guide is corpus', reader_facing('docs/guide/a.md'), True)
        check('plans are not corpus', reader_facing('docs/plans/a.md'), False)
        check('example README is corpus',
              reader_facing('examples/todo/README.md'), True)
        check('example source doc is not corpus',
              reader_facing('examples/todo/NOTES.md'), False)

    print(f'self-test: {passed}/{passed + failed} passed')
    return 1 if failed else 0


if MODE == '--self-test':
    sys.exit(self_test())
elif MODE == '--list':
    sys.exit(do_list())
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
  "")
    echo "Checking autumn_web:: symbol paths across the reader-facing docs..."
    if run_py --check "$root"; then
      echo "Symbol drift gate OK."
    else
      cat >&2 <<'EOF'

FAIL: the docs put an `autumn_web::…` path in front of a reader that does not
resolve (above).

Where the path reaches name resolution, `rustc` reports it against the READER's
file, not against the page, so they are told their code is wrong when the
documentation is — at the first build of a feature they have not used before.
Where it does NOT (a return type under `#[autumn_web::main]`, which the macro
discards), nothing reports it at all and the reader simply carries away a name
that does not exist.

Fix each one where it lives:
  - renamed item   -> use the current name (the `did you mean` hint is the
                      closest name in the same module)
  - moved item     -> use the path a reader can actually write; it is usually a
                      re-export, not where the item is defined
                      (`autumn_web::http::Client`, not
                      `autumn_web::http_client::Client`)
  - never existed  -> drop it, or name the item that does the job
  - shown broken   -> a path quoted inside an `error[E1234]` message is already
                      waived; if you are illustrating a failure, quote the
                      compiler error with it

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
