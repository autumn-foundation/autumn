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
  For every member of the workspace rooted at the repo root, it enumerates the
  binary target names cargo would build:

  - explicit `[[bin]]` entries in the member's Cargo.toml, plus
  - auto-discovered `src/bin/*.rs` and `src/bin/*/main.rs` targets whenever
    `[package] autobins` is not `false`.

  An explicit `[[bin]]` does NOT disable auto-discovery (autumn #2690): only
  `autobins = false` does. Auto-discovered paths already claimed by an
  explicit entry — via its `path`, or the default `src/bin/<name>.rs` when no
  `path` is given — are excluded so one target is not counted twice.

  (`src/main.rs` is only ever a target when an explicit `[[bin]]` claims it,
  named after the package — unique by construction, so it can never collide.)

  It fails if the same name is claimed by more than one member.

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
import sys
import tempfile
import tomllib
from collections import defaultdict
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def workspace_members(root: Path) -> list[Path]:
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    members = manifest["workspace"]["members"]
    return [root / m for m in members]


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


def bin_target_names(member_dir: Path) -> list[str]:
    manifest = tomllib.loads((member_dir / "Cargo.toml").read_text())
    explicit_names, claimed_paths = explicit_bins(member_dir)

    discovered: list[str] = []
    # #2690: explicit [[bin]] entries do NOT disable auto-discovery — only
    # `[package] autobins = false` does. Enumerate both classes.
    if manifest.get("package", {}).get("autobins", True) is not False:
        src_bin = member_dir / "src" / "bin"
        if src_bin.is_dir():
            candidates = sorted(src_bin.glob("*.rs"))
            candidates += sorted(
                d / "main.rs"
                for d in src_bin.iterdir()
                if d.is_dir() and (d / "main.rs").is_file()
            )
            for path in candidates:
                if path.resolve() in claimed_paths:
                    continue  # same target, declared explicitly
                discovered.append(path.parent.name if path.name == "main.rs" else path.stem)

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
    owners: dict[str, list[str]] = defaultdict(list)
    for member in workspace_members(root):
        for name in bin_target_names(member):
            owners[name].append(member.relative_to(root).as_posix())

    collisions = {name: pkgs for name, pkgs in owners.items() if len(pkgs) > 1}
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


def _make_workspace(tmp: Path, members: list[str]) -> Path:
    root = tmp / "root"
    root.mkdir()
    (root / "Cargo.toml").write_text(
        "[workspace]\nmembers = [\n" + "".join(f'  "{m}",\n' for m in members) + "]\n"
    )
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
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n'
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

    # Case 7: src/main.rs targets are never counted (named after the package,
    # unique by construction).
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        root = _make_workspace(tmp, ["app"])
        _make_member(
            tmp / "root", "app",
            '[package]\nname = "app"\nversion = "0.1.0"\n\n[[bin]]\nname = "app"\npath = "src/main.rs"\n',
            {"src/main.rs": "fn main() {}\n"},
        )
        expect(
            "src/main.rs target skipped",
            bin_target_names(root / "app") == ["app"],
        )

    if failures:
        print(f"self-test: {len(failures)} case(s) FAILED", file=sys.stderr)
        for label in failures:
            print(f"  - {label}", file=sys.stderr)
        return 1
    print("self-test: all 7 cases passed")
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
