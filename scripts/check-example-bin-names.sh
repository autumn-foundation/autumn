#!/usr/bin/env python3
"""Verify that no two workspace members declare a binary target with the same
name (autumn #2639).

WHAT THE INVARIANT IS
  On Windows, the linker cannot replace an output file that is still open, so
  when two workspace members produce a binary with the same file name
  (e.g. `seed.exe`), whichever link starts second fails intermittently with
  LNK1104 "cannot open file". On Unix the collision is invisible
  (unlink-then-write), which is why it only ever surfaces on
  `Test (windows-latest)`.

WHAT IT CHECKS
  For every member of the workspace rooted at the repo root — the listed
  `[workspace].members` (globs expanded), the root package itself when the
  root manifest carries `[package]`, plus in-tree path dependencies, which
  cargo automatically treats as members even when `members` does not name them
  (including `[workspace.dependencies]`-inherited paths) —
  it enumerates the binary target names cargo would build, honoring cargo's
  auto-discovery rules (including the edition-2015 manual-target opt-out and
  `edition.workspace = true` inheritance). `[workspace].exclude` follows
  cargo's `WorkspaceRootConfig::is_excluded` exactly: entries are literal path
  prefixes (never globs), and an explicitly listed `members` entry always wins
  over `exclude`.

  - explicit `[[bin]]` entries in the member's Cargo.toml, plus
  - auto-discovered targets whenever `[package] autobins` is not `false`:
    `src/main.rs` (named after the package), `src/bin/*.rs` (named after the
    file stem), and `src/bin/<name>/main.rs` (named after the directory).

  An explicit `[[bin]]` does NOT disable auto-discovery (autumn #2690): only
  `autobins = false` does. Auto-discovered paths already claimed by an
  explicit entry — via its `path`, or the default `src/bin/<name>.rs` when no
  `path` is given — are excluded so one target is not counted twice.

  It fails if the same name is claimed by more than one member. Names are
  compared case-insensitively: on Windows `Seed.exe` and `seed.exe` are the
  same output file, and this gate exists for the Windows linker.

WHY A SCRIPT
  The collision is timing-dependent, so CI only catches it when two links
  happen to overlap. A manifest gate catches the reintroduction at review
  time, deterministically, with no toolchain.

Run locally with:

    ./scripts/check-example-bin-names.sh              # self-test, then check
    ./scripts/check-example-bin-names.sh --self-test  # self-test only

The default invocation runs the self-test FIRST so a refactor that silently
stops catching things fails loud rather than going green on an empty scan.
"""
from __future__ import annotations

import argparse
import glob
import sys
import tempfile
import tomllib
from collections import defaultdict
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def _in_tree_dep_paths(manifest: dict, workspace_deps: dict) -> list[tuple[str, bool]]:
    """`path = ...` dependencies declared in one member's manifest.

    Returns `(path, relative_to_workspace_root)` pairs: `workspace_deps` is
    the root's `[workspace.dependencies]` table, used to resolve
    `dep.workspace = true` inheritances back to their path — and those paths
    are relative to the workspace root, unlike direct `path` specs which are
    relative to the dependent package.
    """
    paths: list[tuple[str, bool]] = []
    tables: list[dict] = [manifest]
    targets = manifest.get("target", {})
    if isinstance(targets, dict):
        tables += [t for t in targets.values() if isinstance(t, dict)]
    for table in tables:
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            deps = table.get(section, {})
            if not isinstance(deps, dict):
                continue
            for dep_name, spec in deps.items():
                if not isinstance(spec, dict):
                    continue
                if isinstance(spec.get("path"), str):
                    paths.append((spec["path"], False))
                elif spec.get("workspace") is True:
                    # Inherited from [workspace.dependencies]; the lookup key
                    # is `package = "..."` when the dependency is renamed.
                    inherited = workspace_deps.get(spec.get("package", dep_name), {})
                    if isinstance(inherited, dict) and isinstance(
                        inherited.get("path"), str
                    ):
                        paths.append((inherited["path"], True))
    return paths


def _expand_member_pattern(root: Path, pattern: str) -> list[Path]:
    """Expand one `[workspace].members` entry, supporting cargo's globs.

    A non-glob entry is a literal directory; a glob like `crates/*` expands to
    the matching directories that actually contain a manifest.
    """
    if glob.has_magic(pattern):
        return sorted(
            p.resolve()
            for p in root.glob(pattern)
            if p.is_dir() and (p / "Cargo.toml").is_file()
        )
    return [(root / pattern).resolve()]


def workspace_members(root: Path) -> list[Path]:
    """Cargo's effective workspace members for the gate.

    The listed `[workspace].members` plus in-tree path dependencies, which
    cargo automatically treats as workspace members even when `members` does
    not name them (Codex review on #2712): a bin-name collision inside such a
    dependency breaks the Windows link exactly like a listed member's would,
    so the gate must scan it too. `[workspace].exclude` is honored, and a
    path dependency that is itself a workspace root (nested workspace) is not
    recursed into.
    """
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    ws = manifest.get("workspace", {})
    exclude = ws.get("exclude", [])
    workspace_deps = ws.get("dependencies", {})
    if not isinstance(workspace_deps, dict):
        workspace_deps = {}

    ordered: list[Path] = []
    seen: set[Path] = set()

    def note(member_dir: Path) -> None:
        if member_dir not in seen:
            seen.add(member_dir)
            ordered.append(member_dir)

    def _under(rel: str, pat: str) -> bool:
        # Component-wise prefix match, mirroring cargo's
        # `Path::starts_with` in `WorkspaceRootConfig::is_excluded`.
        pat = pat.strip("/")
        return bool(pat) and (rel == pat or rel.startswith(pat + "/"))

    def is_excluded(member_dir: Path) -> bool:
        # True cargo semantics (`WorkspaceRootConfig::is_excluded`): `exclude`
        # entries are literal path PREFIXES (not globs — `exclude =
        # ["crates/*"]` matches nothing), and an explicitly listed `members`
        # entry always wins over `exclude`.
        manifest_rel = (member_dir / "Cargo.toml").relative_to(root).as_posix()
        raw_members = ws.get("members", [])
        excluded = any(_under(manifest_rel, pat) for pat in exclude)
        explicit = any(_under(manifest_rel, pat) for pat in raw_members)
        return excluded and not explicit

    queue: list[Path] = []
    for m in ws.get("members", []):
        for member_dir in _expand_member_pattern(root, m):
            if (member_dir / "Cargo.toml").is_file() and not is_excluded(member_dir):
                note(member_dir)
                queue.append(member_dir)
    if "package" in manifest and not is_excluded(root):
        # Root package: a root Cargo.toml with both [package] and [workspace]
        # is automatically a member even when `members` omits ".".
        note(root)
        queue.append(root)

    while queue:
        member_dir = queue.pop(0)
        try:
            member_manifest = tomllib.loads((member_dir / "Cargo.toml").read_text())
        except OSError:
            continue
        for dep_path, from_root in _in_tree_dep_paths(member_manifest, workspace_deps):
            # Direct `path` specs are relative to the dependent package;
            # `[workspace.dependencies]` paths are relative to the root.
            dep_dir = (
                (root / dep_path) if from_root else (member_dir / dep_path)
            ).resolve()
            try:
                dep_dir.relative_to(root)
            except ValueError:
                continue  # outside the workspace root: never a member
            if dep_dir in seen or is_excluded(dep_dir):
                continue
            if not (dep_dir / "Cargo.toml").is_file():
                continue
            dep_manifest = tomllib.loads((dep_dir / "Cargo.toml").read_text())
            if "workspace" in dep_manifest:
                continue  # nested workspace root: a separate workspace
            note(dep_dir)
            queue.append(dep_dir)
    return ordered


def explicit_bins(member_dir: Path) -> tuple[list[str], set[Path]]:
    """Return (names, claimed source paths) for the member's [[bin]] entries."""
    manifest = tomllib.loads((member_dir / "Cargo.toml").read_text())
    names: list[str] = []
    claimed: set[Path] = set()
    for entry in manifest.get("bin", []):
        name = entry.get("name")
        if not name:
            continue
        names.append(name)
        # Cargo's default path for `[[bin]]` is `src/bin/<name>.rs`.
        claimed.add((member_dir / entry.get("path", f"src/bin/{name}.rs")).resolve())
    return names, claimed


def bin_target_names(member_dir: Path, workspace_edition: str | None = None) -> list[str]:
    manifest = tomllib.loads((member_dir / "Cargo.toml").read_text())
    package = manifest.get("package", {})
    explicit_names, claimed_paths = explicit_bins(member_dir)

    discovered: list[str] = []
    # Auto-discovery is on unless `[package] autobins = false` — EXCEPT in
    # edition 2015 (also the default when `edition` is omitted), where any
    # manually specified target ([[bin]] or [lib]) disables auto-discovery
    # entirely. (In edition 2018+ explicit [[bin]] entries do NOT disable it.)
    # The edition itself may be inherited: `edition.workspace = true` resolves
    # against the root's `[workspace.package] edition`.
    edition_spec: object = package.get("edition", "2015")
    if (
        isinstance(edition_spec, dict)
        and edition_spec.get("workspace") is True
        and workspace_edition is not None
    ):
        edition_spec = workspace_edition
    auto = package.get("autobins", True) is not False
    if auto and str(edition_spec) == "2015":
        auto = not ("bin" in manifest or "lib" in manifest)
    if auto:
        src = member_dir / "src"
        # `src/main.rs` is auto-discovered as a binary named after the package
        # (cargo `inferred_bins`): it is NOT limited to explicit [[bin]]
        # claims, so an explicit `[[bin]]` named like another member's package
        # would collide on the linker output and must be caught.
        package_name = package.get("name")
        main_rs = src / "main.rs"
        if (
            package_name
            and main_rs.is_file()
            and main_rs.resolve() not in claimed_paths
        ):
            discovered.append(package_name)
        src_bin = src / "bin"
        if src_bin.is_dir():
            for path in sorted(src_bin.glob("*.rs")):
                if path.resolve() in claimed_paths:
                    continue  # same target, declared explicitly
                # cargo names these after the file stem — including the odd
                # `src/bin/main.rs`, which becomes a target literally named
                # "main", NOT the package name.
                discovered.append(path.stem)
            for d in sorted(src_bin.iterdir()):
                main = d / "main.rs"
                if d.is_dir() and main.is_file() and main.resolve() not in claimed_paths:
                    # `src/bin/<name>/main.rs` is named after the directory.
                    discovered.append(d.name)

    # Deduplicated per member: an explicit bin and an unclaimed auto-discovered
    # file can only share a name when cargo itself would reject the package
    # (duplicate target names), and for the cross-member check each member
    # claims a name at most once.
    seen: set[str] = set()
    names: list[str] = []
    for name in explicit_names + discovered:
        if name not in seen:
            seen.add(name)
            names.append(name)
    return names


def check(root: Path) -> tuple[int, dict[str, list[str]]]:
    # Grouped case-insensitively: the gate exists for the Windows linker, and
    # on Windows `Seed.exe` vs `seed.exe` alias the same output file even
    # though cargo accepts both target names.
    root_manifest = tomllib.loads((root / "Cargo.toml").read_text())
    ws_package = root_manifest.get("workspace", {}).get("package", {})
    workspace_edition = (
        ws_package.get("edition") if isinstance(ws_package, dict) else None
    )
    owners: dict[str, list[str]] = defaultdict(list)
    display: dict[str, str] = {}
    for member in workspace_members(root):
        for name in bin_target_names(member, workspace_edition):
            key = name.casefold()
            owners[key].append(member.relative_to(root).as_posix())
            display.setdefault(key, name)

    collisions = {display[k]: pkgs for k, pkgs in owners.items() if len(pkgs) > 1}
    return len(owners), collisions


def report(root: Path) -> int:
    total, collisions = check(root)
    if collisions:
        print("duplicate binary target names across workspace members:", file=sys.stderr)
        for name in sorted(collisions):
            print(f"  {name}: {', '.join(collisions[name])}", file=sys.stderr)
        print(
            "\nRename the colliding targets so each is crate-unique "
            "(see autumn #2639); on Windows the linker cannot open an output "
            "file another link is still writing (LNK1104).",
            file=sys.stderr,
        )
        return 1
    print(f"ok: {total} binary targets, all names unique")
    return 0


# --- self-test --------------------------------------------------------------


def _make_member(tmp: Path, name: str, manifest: str, bins: dict[str, str] | None = None) -> None:
    member = tmp / name
    (member / "src" / "bin").mkdir(parents=True)
    (member / "Cargo.toml").write_text(manifest)
    for rel, body in (bins or {}).items():
        path = member / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)


def _make_workspace(
    tmp: Path,
    members: list[str],
    exclude: list[str] | None = None,
    extra: str = "",
) -> Path:
    root = tmp / "root"
    root.mkdir()
    manifest = "[workspace]\nmembers = [\n" + "".join(f'  "{m}",\n' for m in members) + "]\n"
    if exclude:
        manifest += "exclude = [\n" + "".join(f'  "{e}",\n' for e in exclude) + "]\n"
    manifest += extra
    (root / "Cargo.toml").write_text(manifest)
    return root


def self_test() -> int:
    """Synthetic workspaces exercising the #2690 enumeration rules."""
    failures: list[str] = []

    def expect(label: str, cond: bool) -> None:
        print(f"  {'ok' if cond else 'FAIL'}: {label}")
        if not cond:
            failures.append(label)

    # Case 1 (#2690): explicit [[bin]] plus an auto-discovered helper in the
    # SAME member — the old script returned early on `explicit` and missed it.
    # (Edition 2021: explicit targets do not disable auto-discovery there;
    # case 19 covers the edition-2015 opt-out.)
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n'
            '[[bin]]\nname = "app-seed"\npath = "src/bin/seed.rs"\n',
            {"src/bin/seed.rs": "fn main() {}\n", "src/bin/helper.rs": "fn main() {}\n"},
        )
        expect(
            "explicit bin + auto-discovered helper are both enumerated",
            sorted(bin_target_names(root / "app")) == ["app-seed", "helper"],
        )

    # Case 2: cross-member collision between an explicit bin and an
    # auto-discovered bin is caught.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["a", "b"])
        _make_member(
            tmp / "root", "a",
            '[package]\nname = "a"\nversion = "0.1.0"\n\n[[bin]]\nname = "seed"\npath = "src/seed_main.rs"\n',
            {"src/seed_main.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "b",
            '[package]\nname = "b"\nversion = "0.1.0"\n',
            {"src/bin/seed.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect("explicit-vs-autodiscovered cross-member collision caught", "seed" in collisions)

    # Case 3: `autobins = false` disables auto-discovery.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\nautobins = false\n\n'
            '[[bin]]\nname = "app-seed"\npath = "src/bin/seed.rs"\n',
            {"src/bin/seed.rs": "fn main() {}\n", "src/bin/helper.rs": "fn main() {}\n"},
        )
        expect(
            "autobins = false hides auto-discovered bins",
            bin_target_names(root / "app") == ["app-seed"],
        )

    # Case 4: the `src/bin/<name>/main.rs` auto-discovery form is enumerated.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n',
            {"src/bin/tool/main.rs": "fn main() {}\n"},
        )
        expect(
            "src/bin/<name>/main.rs is enumerated",
            bin_target_names(root / "app") == ["tool"],
        )

    # Case 5: an explicit bin with the default path is not double-counted.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[[bin]]\nname = "tool"\n',
            {"src/bin/tool.rs": "fn main() {}\n"},
        )
        expect(
            "explicit bin at default path counted once",
            bin_target_names(root / "app") == ["tool"],
        )

    # Case 6: no collision across distinct names passes the gate.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["a", "b"])
        _make_member(
            tmp / "root", "a",
            '[package]\nname = "a"\nversion = "0.1.0"\n',
            {"src/bin/alpha.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "b",
            '[package]\nname = "b"\nversion = "0.1.0"\n',
            {"src/bin/beta.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect("distinct names pass", not collisions and total == 2)

    # Case 7: an explicit [[bin]] claiming `src/main.rs` is counted once — the
    # auto-discovery pass excludes the claimed path.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[[bin]]\nname = "app"\npath = "src/main.rs"\n',
            {"src/main.rs": "fn main() {}\n"},
        )
        expect(
            "explicit bin claiming src/main.rs counted once",
            bin_target_names(root / "app") == ["app"],
        )

    # Case 8 (Codex review): an unclaimed `src/main.rs` is auto-discovered as a
    # binary named after the package.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["widget"])
        _make_member(
            tmp / "root", "widget",
            '[package]\nname = "widget"\nversion = "0.1.0"\n',
            {"src/main.rs": "fn main() {}\n"},
        )
        expect(
            "unclaimed src/main.rs enumerated under the package name",
            bin_target_names(root / "widget") == ["widget"],
        )

    # Case 9 (Codex review): an explicit bin named like another member's package
    # collides with that member's auto-discovered `src/main.rs` binary.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["widget", "tools"])
        _make_member(
            tmp / "root", "widget",
            '[package]\nname = "widget"\nversion = "0.1.0"\n',
            {"src/main.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "tools",
            '[package]\nname = "tools"\nversion = "0.1.0"\n\n[[bin]]\nname = "widget"\npath = "src/bin/widget.rs"\n',
            {"src/bin/widget.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect(
            "explicit bin vs another member's src/main.rs collision caught",
            "widget" in collisions,
        )

    # Case 10: `src/bin/main.rs` is named after the file stem ("main"), matching
    # cargo's auto-discovery — not the parent directory.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n',
            {"src/bin/main.rs": "fn main() {}\n"},
        )
        expect(
            "src/bin/main.rs named 'main' like cargo",
            bin_target_names(root / "app") == ["main"],
        )

    # Case 11 (Codex review): an in-tree path dependency not named in
    # `[workspace].members` is still a workspace member to cargo, so its bins
    # are scanned — a collision between it and a listed member is caught.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app", "other"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[dependencies]\ndep = { path = "../dep" }\n',
            {},
        )
        _make_member(
            tmp / "root", "other",
            '[package]\nname = "other"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "dep",
            '[package]\nname = "dep"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        expect(
            "unlisted in-tree path dependency is scanned as a member",
            any((root / "dep").resolve() == m for m in workspace_members(root)),
        )
        total, collisions = check(root)
        expect(
            "collision inside unlisted path dependency caught",
            "clash" in collisions,
        )

    # Case 12: `[workspace].exclude` keeps an in-tree path dependency out of
    # the scan, matching cargo.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app", "other"], exclude=["dep"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[dependencies]\ndep = { path = "../dep" }\n',
            {},
        )
        _make_member(
            tmp / "root", "other",
            '[package]\nname = "other"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "dep",
            '[package]\nname = "dep"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect("excluded path dependency not scanned", not collisions and total == 1)

    # Case 13: a path dependency outside the workspace root is never a member.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        outside = tmp / "outside"
        (outside / "src" / "bin").mkdir(parents=True)
        (outside / "Cargo.toml").write_text('[package]\nname = "outside"\nversion = "0.1.0"\n')
        (outside / "src" / "bin" / "clash.rs").write_text("fn main() {}\n")
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[dependencies]\noutside = { path = "../../outside" }\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect(
            "out-of-tree path dependency not scanned",
            not collisions and total == 1,
        )

    # Case 14 (Codex review): `[workspace].members` globs are expanded before
    # scanning — a glob entry is not treated as a literal directory.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["crates/*"])
        _make_member(
            tmp / "root", "crates/a",
            '[package]\nname = "a"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "crates/b",
            '[package]\nname = "b"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect(
            "members glob expanded; collision among globbed crates caught",
            "clash" in collisions,
        )

    # Case 15 (Codex review): a path dependency inherited via
    # `[workspace.dependencies]` (`dep.workspace = true`) is followed to its
    # in-tree path and scanned as a member.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(
            tmp, ["app", "other"],
            extra='[workspace.dependencies]\ndep = { path = "dep" }\n',
        )
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[dependencies]\ndep.workspace = true\n',
            {},
        )
        _make_member(
            tmp / "root", "other",
            '[package]\nname = "other"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "dep",
            '[package]\nname = "dep"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect(
            "workspace-inherited path dependency scanned; collision caught",
            "clash" in collisions,
        )

    # Case 16 (Codex review): names differing only by case collide — on Windows
    # `Seed.exe` and `seed.exe` are the same output file.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["a", "b"])
        _make_member(
            tmp / "root", "a",
            '[package]\nname = "a"\nversion = "0.1.0"\n',
            {"src/bin/Seed.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "b",
            '[package]\nname = "b"\nversion = "0.1.0"\n',
            {"src/bin/seed.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect("case-only name collision caught", "seed" in collisions or "Seed" in collisions)

    # Case 17 (Codex review, verified against cargo's
    # `WorkspaceRootConfig::is_excluded` source): `exclude` entries are literal
    # path PREFIXES and an explicitly listed `members` entry always wins —
    # `exclude = ["examples/x"]` neither drops the listed member `examples/x`
    # nor the path dependency `examples/x/nested` living under its prefix, so
    # the collision between the nested dep and `other` is caught.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app", "other", "examples/x"], exclude=["examples/x"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n'
            '[dependencies]\nnested = { path = "../examples/x/nested" }\n',
            {},
        )
        _make_member(
            tmp / "root", "examples/x",
            '[package]\nname = "x"\nversion = "0.1.0"\n',
            {"src/bin/unique.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "examples/x/nested",
            '[package]\nname = "nested"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "other",
            '[package]\nname = "other"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        members = [m.relative_to(root).as_posix() for m in workspace_members(root)]
        expect(
            "explicit member wins over exclude",
            "examples/x" in members,
        )
        expect(
            "dep under explicit prefix still scanned; collision caught",
            "examples/x/nested" in members and "clash" in collisions,
        )

    # Case 17b: the documented `exclude` use — a literal entry removes a
    # glob-matched member (`members = ["crates/*"]`, `exclude = ["crates/old"]`).
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["crates/*"], exclude=["crates/old"])
        _make_member(
            tmp / "root", "crates/old",
            '[package]\nname = "old"\nversion = "0.1.0"\n',
            {"src/bin/gone.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "crates/new",
            '[package]\nname = "new"\nversion = "0.1.0"\n',
            {"src/bin/here.rs": "fn main() {}\n"},
        )
        members = [m.relative_to(root).as_posix() for m in workspace_members(root)]
        expect(
            "literal exclude drops the glob-matched member",
            "crates/old" not in members and "crates/new" in members,
        )

    # Case 18 (Codex review): a root Cargo.toml with both [package] and
    # [workspace] makes the root package itself a member.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = tmp / "root"
        root.mkdir()
        (root / "Cargo.toml").write_text(
            '[package]\nname = "rootpkg"\nversion = "0.1.0"\n\n'
            "[workspace]\nmembers = [\n  \"app\",\n]\n"
        )
        (root / "src").mkdir(parents=True)
        (root / "src" / "main.rs").write_text("fn main() {}\n")
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n'
            '[[bin]]\nname = "rootpkg"\npath = "src/bin/rootpkg.rs"\n',
            {"src/bin/rootpkg.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect(
            "root package scanned; collision with its src/main.rs bin caught",
            "rootpkg" in collisions,
        )

    # Case 19 (Codex review): edition 2015 (the default when `edition` is
    # omitted) plus any explicit target disables auto-discovery entirely —
    # `src/bin/extra.rs` is NOT a target, so no false collision is reported.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["a", "b"])
        _make_member(
            tmp / "root", "a",
            '[package]\nname = "a"\nversion = "0.1.0"\n\n'
            '[[bin]]\nname = "real"\npath = "src/bin/real.rs"\n',
            {"src/bin/real.rs": "fn main() {}\n", "src/bin/extra.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "b",
            '[package]\nname = "b"\nversion = "0.1.0"\n',
            {"src/bin/extra.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect(
            "edition-2015 explicit target disables auto-discovery; no false collision",
            not collisions and sorted(bin_target_names(root / "a")) == ["real"],
        )

    # Case 20 (Codex review): an explicitly listed member is kept even when
    # `exclude` names it — cargo reports both.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["a", "b"], exclude=["b"])
        _make_member(
            tmp / "root", "a",
            '[package]\nname = "a"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "b",
            '[package]\nname = "b"\nversion = "0.1.0"\n',
            {"src/bin/clash.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        members = [m.relative_to(root).as_posix() for m in workspace_members(root)]
        expect(
            "explicit member survives exclude; collision caught",
            "b" in members and "clash" in collisions,
        )

    # Case 21 (Codex review): the edition-2015 opt-out also applies when the
    # edition is inherited — root `[workspace.package] edition = "2015"` plus
    # member `edition.workspace = true` with an explicit [[bin]]: no
    # auto-discovery, so `src/bin/extra.rs` is not invented.
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(
            tmp, ["a", "b"],
            extra='[workspace.package]\nedition = "2015"\n',
        )
        _make_member(
            tmp / "root", "a",
            '[package]\nname = "a"\nversion = "0.1.0"\nedition.workspace = true\n\n'
            '[[bin]]\nname = "real"\npath = "src/bin/real.rs"\n',
            {"src/bin/real.rs": "fn main() {}\n", "src/bin/extra.rs": "fn main() {}\n"},
        )
        _make_member(
            tmp / "root", "b",
            '[package]\nname = "b"\nversion = "0.1.0"\n',
            {"src/bin/extra.rs": "fn main() {}\n"},
        )
        total, collisions = check(root)
        expect(
            "inherited edition 2015 disables auto-discovery; no false collision",
            not collisions,
        )

    if failures:
        print(f"self-test: {len(failures)} case(s) FAILED", file=sys.stderr)
        for label in failures:
            print(f"  - {label}", file=sys.stderr)
        return 1
    print("self-test: all 21 cases passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Gate duplicate workspace binary target names.")
    parser.add_argument("--self-test", action="store_true", help="run the synthetic self-test only")
    parser.add_argument("--check-only", action="store_true", help="run the real check only")
    args = parser.parse_args()

    if not args.check_only:
        rc = self_test()
        if rc != 0 or args.self_test:
            return rc
    return report(REPO_ROOT)


if __name__ == "__main__":
    sys.exit(main())
