#!/usr/bin/env python3
"""Read a Ledger capture and maintain the per-query catalog.

A capture is the directory of `capture-<pid>.jsonl` files that
`autumn/src/ledger_capture.rs` writes when `AUTUMN_LEDGER_CAPTURE_DIR` is set.
The catalog is `docs/perf/ledger/queries/<fingerprint>.toml`, one file per
statement, so two pull requests that add different queries never edit the
same file.

Subcommands:

  update <capture>            write a profile for each captured statement
                              that has none
  update <capture> --refresh  rewrite every captured profile; keeps `notes`
  report <capture>            print what the capture saw, with call counts
  gated <capture>             print the fingerprints the gate requires

Only `read`, `write`, `batch`, `utility` and `other` statements are gated.
DDL is left to the migration gates.
"""

from __future__ import annotations

import json
import re
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CATALOG = ROOT / "docs/perf/ledger/queries"
GATED_KINDS = {"read", "write", "batch", "utility", "other"}

SCAN = re.compile(
    r"(Seq Scan|Index Scan|Index Only Scan|Bitmap Heap Scan|Bitmap Index Scan|Tid Scan|Tid Range Scan"
    r"|Insert|Update|Delete|Merge)(?: Backward)?(?: using (\S+))? on (\S+)"
)


FN_DECL = re.compile(r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)")
IMPL_DECL = re.compile(r"^(\s*)impl\b(?:<[^{]*?>)?\s+(?:[\w:<>, ]+?\s+for\s+)?([\w:]+)")
_SOURCES: dict[str, list[str]] = {}


def module_path(file: str) -> str:
    """`autumn/src/job/store.rs` -> `autumn_web::job::store`."""
    crate, _, rest = file.partition("/src/")
    crate = "autumn_web" if crate == "autumn" else crate.rsplit("/", 1)[-1].replace("-", "_")
    parts = [p for p in rest.removesuffix(".rs").split("/") if p not in ("lib", "mod", "main")]
    return "::".join([crate, *parts])


def enclosing_fn(file: str | None, line: int | None, function: str | None) -> str:
    """Name the function holding `file:line`, when the symbol name has no path.

    Line tables give a closure or an inlined call only a bare name. The
    enclosing `fn` is the nearest declaration above the line that is indented
    less than the line itself.
    """
    function = function or ""
    if "::" in function or not file or not line:
        return function
    if file not in _SOURCES:
        try:
            _SOURCES[file] = (ROOT / file).read_text().splitlines()
        except OSError:
            _SOURCES[file] = []
    src = _SOURCES[file]
    if not 0 < line <= len(src):
        return function
    indent = len(src[line - 1]) - len(src[line - 1].lstrip())
    for i in range(line - 1, -1, -1):
        m = FN_DECL.match(src[i])
        if m and len(m.group(1)) < indent:
            name, fn_indent = m.group(2), len(m.group(1))
            owner = ""
            for j in range(i - 1, -1, -1):
                im = IMPL_DECL.match(src[j])
                if im and len(im.group(1)) < fn_indent:
                    owner = im.group(2).split("<")[0].rsplit("::", 1)[-1]
                    break
                if src[j].strip() and len(src[j]) - len(src[j].lstrip()) < fn_indent and not src[j].lstrip().startswith(("#", "//", "}")):
                    break
            return "::".join(p for p in (module_path(file), owner, name) if p)
    return function


def load_capture(capture: Path) -> dict[str, dict]:
    """Merge every capture file into one record per fingerprint."""
    files = sorted(capture.glob("capture-*.jsonl"))
    if not files:
        sys.exit(f"ledger: no capture-*.jsonl files in {capture}")
    out: dict[str, dict] = {}

    def entry(fp: str) -> dict:
        return out.setdefault(fp, {"sites": {}, "calls": defaultdict(int), "plan": None})

    for f in files:
        for line in f.read_text().splitlines():
            if not line.strip():
                continue
            rec = json.loads(line)
            fp = rec["fingerprint"]
            e = entry(fp)
            site = (rec.get("file"), enclosing_fn(rec.get("file"), rec.get("line"), rec.get("function")))
            if rec["type"] == "site":
                e["statement"] = rec["statement"]
                e["kind"] = rec["kind"]
                e["backend"] = rec["backend"] if e.get("backend") in (None, "unknown") else e["backend"]
                e["sites"][site] = True
            elif rec["type"] == "calls":
                e["calls"][site] += rec["calls"]
                e["sites"][site] = True
            elif rec["type"] == "plan":
                # Prefer a complete plan over a failed attempt.
                if e["plan"] is None or (rec.get("plan") and not e["plan"].get("plan")):
                    e["plan"] = rec
    return {fp: e for fp, e in out.items() if "statement" in e}


def gated(cap: dict[str, dict]) -> dict[str, dict]:
    return {fp: e for fp, e in cap.items() if e["kind"] in GATED_KINDS}


def findings(plan: dict | None) -> tuple[list[str], list[str], list[str]]:
    """Return (tables, indexes, findings) from the two generic plans."""
    if not plan:
        return [], [], []
    tables, indexes, out = set(), set(), []
    for lines in (plan.get("plan") or [], plan.get("plan_index_forced") or []):
        for line in lines:
            for m in SCAN.finditer(line):
                if m.group(2):
                    indexes.add(m.group(2))
                tables.add(m.group(3))
    forced = plan.get("plan_index_forced") or []
    for line in forced:
        m = re.search(r"Seq Scan on (\S+)", line)
        if m:
            out.append(f"full-scan: {m.group(1)} (no index serves it, even with enable_seqscan = off)")
        if re.search(r"->\s+(Incremental )?Sort\b|^\s*(Incremental )?Sort\b", line):
            out.append("sort: no index provides the order")
    return sorted(tables), sorted(indexes), sorted(set(out))


def toml_str(s: str) -> str:
    return json.dumps(s, ensure_ascii=False)


def toml_list(items: list[str], indent: str = "  ") -> str:
    if not items:
        return "[]"
    return "[\n" + "".join(f"{indent}{toml_str(i)},\n" for i in items) + "]"


def render(fp: str, e: dict, notes: str) -> str:
    plan = e["plan"]
    tables, indexes, finds = findings(plan)
    sites = sorted(s for s in e["sites"] if s[0] and "::" in (s[1] or ""))
    sites = sites or sorted(s for s in e["sites"] if s[0])
    parts = [
        "# Ledger profile of one statement. Generated by scripts/ledger-profile.sh",
        "# from a capture of the Docker sweep. Edit only `notes`.",
        f"fingerprint = {toml_str(fp)}",
        f"kind = {toml_str(e['kind'])}",
        f"backend = {toml_str(e.get('backend') or 'unknown')}",
        f"statement = {toml_str(e['statement'])}",
        f"tables = {toml_list(tables)}",
        f"indexes = {toml_list(indexes)}",
        f"findings = {toml_list(finds)}",
        f"notes = {toml_str(notes)}",
        "",
    ]
    for file, function in sites:
        parts += ["[[site]]", f"file = {toml_str(file)}", f"function = {toml_str(function)}", ""]
    parts.append("[plan]")
    if plan is None:
        reason = "not planned: not a Postgres read or write" if e.get("backend") != "postgres" or e["kind"] not in ("read", "write") else "not planned: no connection URL reached the capture"
        parts.append(f"status = {toml_str(reason)}")
    else:
        status = "ok" if plan.get("plan") and plan.get("plan_index_forced") else "error"
        parts.append(f"status = {toml_str(status)}")
        parts.append("method = \"EXPLAIN (GENERIC_PLAN, COSTS OFF)\"")
        if plan.get("planned_on"):
            parts.append(f"planned_on = {toml_str(plan['planned_on'])}")
        if plan.get("plan_error"):
            parts.append(f"error = {toml_str(plan['plan_error'].strip())}")
        parts.append(f"generic = {toml_list(plan.get('plan') or [])}")
        parts.append(f"index_forced = {toml_list(plan.get('plan_index_forced') or [])}")
    return "\n".join(parts) + "\n"


def existing_notes(path: Path) -> str:
    if not path.exists():
        return ""
    try:
        return tomllib.loads(path.read_text()).get("notes", "")
    except tomllib.TOMLDecodeError:
        return ""


def cmd_update(capture: Path, refresh: bool) -> None:
    cap = gated(load_capture(capture))
    CATALOG.mkdir(parents=True, exist_ok=True)
    written = 0
    for fp, e in sorted(cap.items()):
        path = CATALOG / f"{fp}.toml"
        if path.exists() and not refresh:
            continue
        path.write_text(render(fp, e, existing_notes(path)))
        written += 1
    print(f"ledger: {len(cap)} gated statements captured, {written} profiles written to {CATALOG.relative_to(ROOT)}")


def cmd_report(capture: Path) -> None:
    cap = load_capture(capture)
    rows = []
    for fp, e in cap.items():
        calls = sum(e["calls"].values())
        _, _, finds = findings(e["plan"])
        rows.append((calls, fp, e["kind"], len(e["sites"]), len(finds), e["statement"][:100]))
    rows.sort(reverse=True)
    print(f"{'calls':>8}  {'fingerprint':16}  {'kind':7} sites finds  statement")
    for calls, fp, kind, sites, finds, stmt in rows:
        print(f"{calls:>8}  {fp:16}  {kind:7} {sites:>5} {finds:>5}  {stmt}")


def main(argv: list[str]) -> None:
    if len(argv) < 2:
        sys.exit(__doc__)
    sub, capture = argv[0], Path(argv[1])
    if sub == "update":
        cmd_update(capture, "--refresh" in argv[2:])
    elif sub == "report":
        cmd_report(capture)
    elif sub == "gated":
        for fp in sorted(gated(load_capture(capture))):
            print(fp)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
