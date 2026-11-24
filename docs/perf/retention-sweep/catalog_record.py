#!/usr/bin/env python3
"""Write a Ledger capture record for each retention sweep batch DELETE.

The Docker sweep has no Postgres retention test with stale rows to batch, so
its capture may miss these statements. This writes the records the capture
would: the normalized text and fingerprint (a port of `normalize` and
`fingerprint` in autumn/src/ledger_capture.rs, checked against the catalog
below), the site, and EXPLAIN (GENERIC_PLAN, COSTS OFF) with and without
enable_seqscan, run with psql on the PG* database. Render the profiles with
`scripts/perf/ledger_catalog.py update <out_dir>`.

    catalog_record.py <out_dir>
"""
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def skip_string(s, i):
    i += 1
    while i < len(s):
        if s[i] == "'":
            if s[i + 1:i + 2] == "'":
                i += 2
                continue
            return i + 1
        i += 1
    return len(s)


def lex(sql):
    out, i, space = [], 0, False

    def push(t):
        nonlocal space
        if space and out:
            out.append(" ")
        space = False
        out.append(t)

    while i < len(sql):
        c = sql[i]
        if c.isspace():
            space, i = True, i + 1
        elif c == ",":
            out.append(",")
            space, i = True, i + 1
        elif c == "'":
            i = skip_string(sql, i)
            push("?")
        elif c == "$" and sql[i + 1:i + 2].isdigit():
            i += 1
            while i < len(sql) and sql[i].isdigit():
                i += 1
            push("$?")
        elif c.isdigit() and not (not space and out and (out[-1][-1].isalnum() or out[-1][-1] == "_")):
            while i < len(sql) and (sql[i].isdigit() or sql[i] == "."):
                i += 1
            push("?")
        elif c == '"':
            j = sql.index('"', i + 1) + 1
            push(sql[i:j])
            i = j
        else:
            push(c)
            i += 1
    return "".join(out)


def param_at(s, i):
    if s.startswith("$?", i):
        n = 2
    elif s[i:i + 1] == "?":
        n = 1
    else:
        return None
    return n if i + n >= len(s) or s[i + n] in ",) " else None


def collapse_params(s):
    out, i = [], 0
    while i < len(s):
        starts = i == 0 or s[i - 1] in "( ,"
        n = param_at(s, i) if starts else None
        if n:
            end, items = i + n, 1
            while s.startswith(", ", end):
                m = param_at(s, end + 2)
                if not m:
                    break
                end, items = end + 2 + m, items + 1
            if items >= 2:
                out.append(s[i:i + n] + ", ...")
                i = end
                continue
        out.append(s[i])
        i += 1
    return "".join(out)


def normalize(sql):
    # The sweep statements carry no comments and no repeated tuples.
    return collapse_params(lex(sql))


def fingerprint(normalized):
    return hashlib.sha256(normalized.encode()).hexdigest()[:16]


def explain(sql, forced):
    setting = "SET enable_seqscan = off;" if forced else ""
    out = subprocess.run(
        ["psql", "-X", "-At", "-v", "ON_ERROR_STOP=1", "-c", "BEGIN;",
         "-c", setting or "SELECT 1 WHERE false;",
         "-c", f"EXPLAIN (GENERIC_PLAN, COSTS OFF) {sql}", "-c", "ROLLBACK;"],
        check=True, capture_output=True, text=True).stdout
    return [l for l in out.splitlines() if l and l not in ("BEGIN", "SET", "ROLLBACK")]


def main():
    out_dir = Path(sys.argv[1])
    out_dir.mkdir(parents=True, exist_ok=True)
    env = subprocess.run([sys.executable, str(HERE / "extract_sql.py")],
                         check=True, capture_output=True, text=True).stdout
    sql = {}
    for line in env.splitlines():
        k, v = line.split("=", 1)
        sql[k] = subprocess.run(["bash", "-c", f"printf %s {v}"], check=True,
                                capture_output=True, text=True).stdout
    site = {"file": "autumn/src/data_retention.rs",
            "function": "autumn_web::data_retention::sweep_postgres"}
    lines = []
    for var in ("JOBS", "TRACK", "HOOKS", "EXP"):
        for part in ("BATCH", "COUNT"):
            raw = sql[f"{var}_{part}"]
            norm = normalize(raw)
            fp = fingerprint(norm)
            kind = "write" if part == "BATCH" else "read"
            lines.append({"type": "site", "fingerprint": fp, "statement": norm, "kind": kind,
                          "backend": "postgres", **site})
            lines.append({"type": "plan", "fingerprint": fp, "planned_on": "test database, Postgres 16",
                          "plan": explain(raw, False), "plan_index_forced": explain(raw, True), **site})
            print(f"{fp}  {var}_{part}")
    (out_dir / "capture-retention-sweep.jsonl").write_text(
        "".join(json.dumps(r) + "\n" for r in lines))


if __name__ == "__main__":
    main()
