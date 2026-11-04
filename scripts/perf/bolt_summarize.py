#!/usr/bin/env python3
"""Turn raw callgrind and dhat output into a committed Bolt profile.

`scripts/bolt-profile.sh` runs the bench under valgrind and calls this script.
It writes three files into the profile directory:

  profile.toml   metadata and headline totals
  callgrind.txt  the instruction profile, by function, self cost
  dhat.txt       the allocation profile, by allocation site

In marginal mode, the script subtracts the zero-iteration run from the
N-iteration run and divides by N. The output then shows the cost of one
iteration, not process start-up and warm-up.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

TOP_FUNCTIONS = 30
TOP_SITES = 20

# Frames that only say "this allocated". The site that matters is the first
# frame outside them.
ALLOC_PLUMBING = re.compile(
    r"^(?:malloc|calloc|realloc|free|posix_memalign|memalign|aligned_alloc|"
    r"__rust_|__rdl_|__rg_|_rjem_|alloc::|<alloc::|core::|<core::|std::|<std::|"
    r"hashbrown::|<hashbrown::|smallvec::|<smallvec::|bytes::|<bytes::|"
    r"\?\?\?|0x[0-9a-fA-F]+:)"
)
WORKSPACE_PREFIXES = ("autumn_", "<autumn_", "autumn::", "<autumn::")

ANNOTATE_LINE = re.compile(r"^\s*([\d,]+)\s+\(\s*[\d.]+%\)\s+(.*)$")


def read_callgrind(path: Path) -> tuple[int, dict[str, int]]:
    """Return (total Ir, self Ir by function) from a callgrind out file."""
    out = subprocess.run(
        ["callgrind_annotate", "--inclusive=no", "--threshold=100", str(path)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    total = 0
    by_fn: dict[str, int] = defaultdict(int)
    in_table = False
    for line in out.splitlines():
        if "PROGRAM TOTALS" in line:
            total = int(line.split()[0].replace(",", ""))
            continue
        if "file:function" in line:
            in_table = True
            continue
        if not in_table:
            continue
        m = ANNOTATE_LINE.match(line)
        if not m:
            continue
        name = clean_fn(m.group(2))
        by_fn[name] += int(m.group(1).replace(",", ""))
    if total == 0:
        sys.exit(f"bolt_summarize: no PROGRAM TOTALS in {path}")
    return total, dict(by_fn)


def clean_fn(raw: str) -> str:
    """Drop the object and file prefix, and hash suffixes, from a function."""
    raw = re.sub(r"\s+\[[^\]]*\]$", "", raw.strip())
    if ":" in raw and not raw.startswith("<"):
        head, _, tail = raw.partition(":")
        if "/" in head or head.endswith((".rs", ".c", ".h", ".S")) or head == "???":
            raw = tail
    return simplify(re.sub(r"::h[0-9a-f]{16}", "", raw))


def simplify(name: str) -> str:
    """Drop generic arguments, keep qualified paths: `<A<T> as B>::f::<U>` -> `<A as B>::f`.

    Tower and axum service types nest hundreds of characters deep. The
    arguments add nothing to "which function is this", and they make a
    profile unreadable.
    """
    out: list[str] = []
    i, n = 0, len(name)
    while i < n:
        c = name[i]
        prev = out[-1] if out else ""
        if c == "<" and prev and (prev.isalnum() or prev in "_:"):
            depth = 0
            while i < n:
                if name[i] == "<":
                    depth += 1
                elif name[i] == ">" and name[i - 1] != "-":
                    depth -= 1
                    if depth == 0:
                        break
                i += 1
            if out and out[-1] == ":" and len(out) > 1 and out[-2] == ":":
                del out[-2:]
            i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def read_dhat(path: Path) -> dict:
    """Return totals and per-site figures from a valgrind dhat JSON file."""
    data = json.loads(path.read_text())
    ftbl = data["ftbl"]
    sites: dict[str, list[int]] = defaultdict(lambda: [0, 0])
    total_bytes = total_blocks = peak_bytes = 0
    for pp in data["pps"]:
        total_bytes += pp["tb"]
        total_blocks += pp["tbk"]
        peak_bytes += pp.get("gb", 0)
        site = site_of([ftbl[i] for i in pp["fs"]])
        sites[site][0] += pp["tb"]
        sites[site][1] += pp["tbk"]
    return {
        "bytes": total_bytes,
        "blocks": total_blocks,
        "peak_bytes": peak_bytes,
        "sites": {k: tuple(v) for k, v in sites.items()},
    }


def site_of(frames: list[str]) -> str:
    """Name an allocation by its first workspace frame, else its first real one."""
    names = []
    for frame in frames:
        # Frames look like "0x4C2: name (file.rs:12:3)" or "[root]".
        text = re.sub(r"^0x[0-9A-Fa-f]+:\s*", "", frame)
        text = re.sub(r"\s+\((?:in )?[^()]*\)$", "", text)
        names.append(simplify(re.sub(r"::h[0-9a-f]{16}", "", text)))
    for name in names:
        if name.startswith(WORKSPACE_PREFIXES):
            return name
    for name in names:
        if name != "[root]" and not ALLOC_PLUMBING.match(name):
            return name
    return names[-1] if names else "[unknown]"


def per_iter(run: float, base: float, n: int) -> float:
    return (run - base) / n if n else run


def fmt_int(v: float) -> str:
    return f"{round(v):,}"


def write_callgrind(out: Path, header: str, total: float, by_fn: dict[str, float]) -> None:
    rows = sorted(by_fn.items(), key=lambda kv: -kv[1])[:TOP_FUNCTIONS]
    lines = [header, "", f"{'Ir':>16}  {'share':>6}  function", ""]
    for name, ir in rows:
        share = 100.0 * ir / total if total else 0.0
        lines.append(f"{fmt_int(ir):>16}  {share:5.1f}%  {name}")
    lines.append("")
    lines.append(f"{fmt_int(total):>16}  100.0%  TOTAL")
    out.write_text("\n".join(lines) + "\n")


def write_dhat(out: Path, header: str, totals: dict, sites: dict[str, tuple[float, float]]) -> None:
    rows = sorted(sites.items(), key=lambda kv: -kv[1][0])[:TOP_SITES]
    lines = [
        header,
        "",
        f"bytes allocated: {fmt_int(totals['bytes'])}",
        f"blocks allocated: {fmt_int(totals['blocks'])}",
        f"peak live bytes (whole run): {fmt_int(totals['peak_bytes'])}",
        "",
        f"{'bytes':>14}  {'blocks':>10}  site",
        "",
    ]
    for name, (b, k) in rows:
        lines.append(f"{fmt_int(b):>14}  {fmt_int(k):>10}  {name}")
    out.write_text("\n".join(lines) + "\n")


def toml_str(v: str) -> str:
    return json.dumps(v)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--raw", required=True, type=Path, help="directory with raw valgrind output")
    ap.add_argument("--mode", required=True, choices=["marginal", "whole", "criterion"])
    ap.add_argument("--package", required=True)
    ap.add_argument("--bench", required=True)
    ap.add_argument("--callgrind-n", type=int, default=0)
    ap.add_argument("--dhat-n", type=int, default=0)
    ap.add_argument("--build", required=True, help="the build command line")
    ap.add_argument("--callgrind-cmd", required=True)
    ap.add_argument("--dhat-cmd", required=True)
    ap.add_argument("--source-sha256", required=True)
    ap.add_argument("--rustc", required=True)
    ap.add_argument("--valgrind", required=True)
    ap.add_argument("--commit", required=True)
    ap.add_argument("--date", required=True)
    a = ap.parse_args()

    raw = a.raw
    a.out.mkdir(parents=True, exist_ok=True)
    marginal = a.mode == "marginal"

    cg_total, cg_fns = read_callgrind(raw / "callgrind-run.out")
    dh = read_dhat(raw / "dhat-run.json")
    if marginal:
        base_total, base_fns = read_callgrind(raw / "callgrind-base.out")
        dh_base = read_dhat(raw / "dhat-base.json")
        n = a.callgrind_n
        fns = {
            k: per_iter(cg_fns.get(k, 0), base_fns.get(k, 0), n)
            for k in set(cg_fns) | set(base_fns)
        }
        fns = {k: v for k, v in fns.items() if v >= 0.5}
        total = per_iter(cg_total, base_total, n)
        dn = a.dhat_n
        sites = {}
        for k in set(dh["sites"]) | set(dh_base["sites"]):
            rb, rk = dh["sites"].get(k, (0, 0))
            bb, bk = dh_base["sites"].get(k, (0, 0))
            b, blk = per_iter(rb, bb, dn), per_iter(rk, bk, dn)
            if b >= 0.5:
                sites[k] = (b, blk)
        dtot = {
            "bytes": per_iter(dh["bytes"], dh_base["bytes"], dn),
            "blocks": per_iter(dh["blocks"], dh_base["blocks"], dn),
            "peak_bytes": dh["peak_bytes"],
        }
        unit = "per iteration (N-iteration run minus zero-iteration run, divided by N)"
    else:
        fns, total, sites, dtot = cg_fns, cg_total, dh["sites"], dh
        base_total = 0
        dh_base = {"bytes": 0, "blocks": 0}
        unit = "whole process"
        if a.mode == "criterion":
            unit += " (criterion --profile-time: the iteration count is wall-clock bound; read the shares)"

    head = f"# Bolt profile: {a.package}/{a.bench}\n# unit: {unit}"
    write_callgrind(a.out / "callgrind.txt", head + "\n# tool: valgrind --tool=callgrind, self cost by function", total, fns)
    write_dhat(a.out / "dhat.txt", head + "\n# tool: valgrind --tool=dhat, by first workspace frame", dtot, sites)

    top = sorted(fns.items(), key=lambda kv: -kv[1])[:1]
    lines = [
        "# Generated by scripts/bolt-profile.sh. Do not edit by hand.",
        f"package = {toml_str(a.package)}",
        f"bench = {toml_str(a.bench)}",
        f"mode = {toml_str(a.mode)}",
        f"date = {toml_str(a.date)}",
        f"commit = {toml_str(a.commit)}",
        f"rustc = {toml_str(a.rustc)}",
        f"valgrind = {toml_str(a.valgrind)}",
        f"source_sha256 = {toml_str(a.source_sha256)}",
        f"build = {toml_str(a.build)}",
        f"callgrind = {toml_str(a.callgrind_cmd)}",
        f"dhat = {toml_str(a.dhat_cmd)}",
        "",
        "[totals]",
        f"unit = {toml_str(unit)}",
        f"instructions = {round(total)}",
        f"alloc_bytes = {round(dtot['bytes'])}",
        f"alloc_blocks = {round(dtot['blocks'])}",
        f"peak_live_bytes = {dtot['peak_bytes']}",
    ]
    if marginal:
        lines += [
            f"callgrind_n = {a.callgrind_n}",
            f"dhat_n = {a.dhat_n}",
            f"instructions_base = {base_total}",
            f"instructions_run = {cg_total}",
            f"alloc_bytes_base = {dh_base['bytes']}",
            f"alloc_bytes_run = {dh['bytes']}",
        ]
    if top:
        name, ir = top[0]
        lines.append(f"top_function = {toml_str(name)}")
        lines.append(f"top_function_share = {round(100.0 * ir / total, 1) if total else 0.0}")
    (a.out / "profile.toml").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
