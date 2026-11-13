#!/usr/bin/env python3
"""Gate: every workload has a Bolt profile, and every query has a Ledger profile.

  scripts/check-perf-profiles.py bolt
      Every bench target in the workspace (from `cargo metadata`) must have an
      entry in docs/perf/bolt/workloads.toml and, unless the entry is exempt,
      a committed profile in docs/perf/bolt/<package>/<bench>/. An entry for
      a bench that no longer exists fails too. A profile whose bench source
      changed since it was taken is reported, not failed.

  scripts/check-perf-profiles.py ledger <capture-dir>
      Every gated statement in a Ledger capture (see
      autumn/src/ledger_capture.rs) must have a profile in
      docs/perf/ledger/queries/<fingerprint>.toml. A profile that the capture
      did not see is reported, not failed: a capture covers one sweep, not
      every code path.

Fix a failure with `scripts/bolt-profile.sh <package>/<bench>` or
`scripts/ledger-profile.sh`. See docs/perf/README.md.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BOLT = ROOT / "docs/perf/bolt"
REQUIRED_BOLT_FILES = ("profile.toml", "callgrind.txt", "dhat.txt")
GITHUB = os.environ.get("GITHUB_ACTIONS") == "true"

sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / "scripts/perf"))


def error(msg: str) -> None:
    print(f"::error::{msg}" if GITHUB else f"error: {msg}")


def warn(msg: str) -> None:
    print(f"::warning::{msg}" if GITHUB else f"warning: {msg}")


def bench_targets() -> dict[str, Path]:
    meta = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    out = {}
    for pkg in json.loads(meta)["packages"]:
        for t in pkg["targets"]:
            if "bench" in t["kind"]:
                out[f"{pkg['name']}/{t['name']}"] = Path(t["src_path"])
    return out


def check_bolt() -> int:
    manifest = tomllib.loads((BOLT / "workloads.toml").read_text())["workload"]
    entries = {f"{w['package']}/{w['bench']}": w for w in manifest}
    targets = bench_targets()
    failures = 0
    for key in sorted(set(targets) - set(entries)):
        error(f"bench {key} has no entry in docs/perf/bolt/workloads.toml. Add one, then run scripts/bolt-profile.sh {key}")
        failures += 1
    for key in sorted(set(entries) - set(targets)):
        error(f"docs/perf/bolt/workloads.toml names {key}, which is not a bench target. Remove the entry and its profile")
        failures += 1
    for key in sorted(set(entries) & set(targets)):
        w = entries[key]
        if w.get("exempt"):
            if not str(w["exempt"]).strip():
                error(f"{key}: `exempt` needs a reason")
                failures += 1
            continue
        for field in ("mode",):
            if field not in w:
                error(f"{key}: manifest entry has no `{field}`")
                failures += 1
        if w.get("mode") == "marginal" and not ("callgrind_n" in w and "dhat_n" in w):
            error(f"{key}: a marginal workload needs `callgrind_n` and `dhat_n`")
            failures += 1
        pdir = BOLT / w["package"] / w["bench"]
        required = REQUIRED_BOLT_FILES + (("ledger.txt",) if w.get("ledger") == "postgres" else ())
        missing = [f for f in required if not (pdir / f).is_file() or (pdir / f).stat().st_size == 0]
        if missing:
            error(f"bench {key} has no Bolt profile ({', '.join(missing)} missing in {pdir.relative_to(ROOT)}). Run scripts/bolt-profile.sh {key}")
            failures += 1
            continue
        profile = tomllib.loads((pdir / "profile.toml").read_text())
        totals = profile.get("totals", {})
        if not isinstance(totals.get("instructions"), int) or totals["instructions"] <= 0:
            error(f"{key}: profile.toml has no positive totals.instructions")
            failures += 1
        src_hash = hashlib.sha256(targets[key].read_bytes()).hexdigest()
        if profile.get("source_sha256") != src_hash:
            warn(f"{key}: the bench source changed since its Bolt profile was taken. Re-run scripts/bolt-profile.sh {key}")
    profiled = sum(1 for k in entries if not entries[k].get("exempt"))
    print(f"bolt: {len(targets)} bench targets, {profiled} need a profile, {len(entries) - profiled} exempt, {failures} failures")
    return failures


def check_ledger(capture: Path) -> int:
    import ledger_catalog

    cap = ledger_catalog.gated(ledger_catalog.load_capture(capture))
    catalog = ledger_catalog.CATALOG
    have = {p.stem for p in catalog.glob("*.toml")} if catalog.exists() else set()
    failures = 0
    for fp in sorted(set(cap) - have):
        e = cap[fp]
        site = next(iter(sorted(s for s in e["sites"] if s[0])), ("?", "?"))
        error(
            f"query {fp} ({e['kind']}) has no Ledger profile: {e['statement'][:160]} "
            f"-- issued from {site[1]} ({site[0]}). Run scripts/ledger-profile.sh, "
            f"or scripts/perf/ledger_catalog.py update <capture>, and commit "
            f"docs/perf/ledger/queries/{fp}.toml"
        )
        failures += 1
    for p in sorted(catalog.glob("*.toml")) if catalog.exists() else []:
        try:
            doc = tomllib.loads(p.read_text())
        except tomllib.TOMLDecodeError as exc:
            error(f"{p.relative_to(ROOT)} is not valid TOML: {exc}")
            failures += 1
            continue
        if doc.get("fingerprint") != p.stem or not doc.get("statement") or "plan" not in doc:
            error(f"{p.relative_to(ROOT)}: needs `fingerprint` equal to the file name, `statement` and a [plan] table")
            failures += 1
    stale = sorted(have - set(cap))
    if stale:
        warn(f"ledger: {len(stale)} profiles were not seen by this capture (removed queries, or paths the sweep does not reach): {' '.join(stale[:20])}")
    print(f"ledger: {len(cap)} gated statements captured, {len(have)} profiles, {failures} failures")
    return failures


def main(argv: list[str]) -> None:
    if argv[:1] == ["bolt"]:
        sys.exit(1 if check_bolt() else 0)
    if argv[:1] == ["ledger"] and len(argv) == 2:
        sys.exit(1 if check_ledger(Path(argv[1])) else 0)
    sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
