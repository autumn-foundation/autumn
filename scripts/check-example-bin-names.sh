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
  - auto-discovered `src/bin/*.rs` files when the manifest declares no
    `[[bin]]` (any explicit `[[bin]]` disables autobins for the package).

  (`src/main.rs` targets are named after the package, hence unique by
  construction, and are skipped.)

  It fails if the same name is claimed by more than one member.

WHY A SCRIPT
  The collision is timing-dependent, so CI only catches it when two links
  happen to overlap. A manifest gate catches the reintroduction at review
  time, deterministically, with no toolchain.

Run locally with:

    ./scripts/check-example-bin-names.sh
"""
from __future__ import annotations

import sys
import tomllib
from collections import defaultdict
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def workspace_members(root: Path) -> list[Path]:
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    members = manifest["workspace"]["members"]
    return [root / m for m in members]


def bin_target_names(member_dir: Path) -> list[str]:
    manifest_path = member_dir / "Cargo.toml"
    manifest = tomllib.loads(manifest_path.read_text())
    explicit = [b["name"] for b in manifest.get("bin", []) if "name" in b]
    if explicit:
        # Any explicit [[bin]] disables autobins for the package.
        return explicit
    names = []
    for src in sorted((member_dir / "src" / "bin").glob("*.rs")):
        names.append(src.stem)
    return names


def main() -> int:
    owners: dict[str, list[str]] = defaultdict(list)
    for member in workspace_members(REPO_ROOT):
        for name in bin_target_names(member):
            owners[name].append(member.relative_to(REPO_ROOT).as_posix())

    collisions = {name: pkgs for name, pkgs in owners.items() if len(pkgs) > 1}
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
    print(f"ok: {sum(len(v) for v in owners.values())} binary targets, all names unique")
    return 0


if __name__ == "__main__":
    sys.exit(main())
