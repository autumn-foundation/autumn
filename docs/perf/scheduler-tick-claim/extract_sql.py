#!/usr/bin/env python3
"""Write the SQL that autumn/src/scheduler.rs ships for the Postgres tick
coordinator: the table DDL, the prune and the claim, as files in OUT_DIR.

Usage: extract_sql.py SCHEDULER_RS OUT_DIR
"""
import re
import sys
from pathlib import Path

src = Path(sys.argv[1]).read_text()
out = Path(sys.argv[2])
out.mkdir(parents=True, exist_ok=True)


def rust_str(start: int) -> str:
    """Decode the Rust string literal that begins at src[start] == '"'."""
    i = start + 1
    buf = []
    while True:
        c = src[i]
        if c == "\\":
            n = src[i + 1]
            if n == "\n":
                i += 2
                while src[i] in " \t":
                    i += 1
                continue
            buf.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\"}[n])
            i += 2
            continue
        if c == '"':
            return "".join(buf)
        buf.append(c)
        i += 1


def literal_after(anchor: str) -> str:
    at = src.index(anchor)
    return rust_str(src.index('"', at + len(anchor) - 1))


ddl = literal_after('pub const PG_TICK_TABLE_DDL: &str = "')
prune_at = src.index('diesel::sql_query("DELETE FROM autumn_scheduler_ticks WHERE expires_at')
prune = rust_str(src.index('"', prune_at))
claim_at = src.index('"INSERT INTO autumn_scheduler_ticks')
claim = rust_str(claim_at)
free_at = src.index('"DELETE FROM autumn_scheduler_ticks \\')
free = rust_str(free_at)

(out / "ddl.sql").write_text(ddl + "\n")
(out / "prune.sql").write_text(prune + "\n")
(out / "claim.sql").write_text(claim + "\n")
(out / "free.sql").write_text(free + "\n")
for name in ("prune", "claim", "free"):
    print(f"-- {name}:\n{(out / (name + '.sql')).read_text()}")
