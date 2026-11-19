#!/usr/bin/env python3
"""Write the enqueue SQL that autumn/src/job.rs ships (pg_insert_job without
telemetry-otlp, and pg_evict_expired_unique_key) as pgbench-ready SQL files.

Usage: extract_sql.py OUT_DIR [JOB_RS]
Writes OUT_DIR/insert.sql and OUT_DIR/evict.sql, with the $n placeholders
the store binds, unchanged.
"""
import re
import sys
from pathlib import Path

out = Path(sys.argv[1])
src_path = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).resolve().parents[3] / "autumn/src/job.rs"
src = src_path.read_text()


def rust_str(body: str) -> str:
    # A Rust string literal body: `\` + newline skips the newline and the
    # leading whitespace of the next line; `\"` is a quote.
    body = re.sub(r"\\\n\s*", "", body)
    return body.replace('\\"', '"')


def literal_after(text: str, marker: str) -> str:
    i = text.index(marker) + len(marker)
    j = i
    while True:
        j = text.index('"', j)
        if text[j - 1] != "\\":
            break
        j += 1
    return rust_str(text[i:j])


fn = src[src.index("async fn pg_insert_job("):]
fn = fn[: fn.index("\nasync fn ", 10)]
guard = literal_after(fn, 'const DEDUP_GUARD: &str = "')
conflict = literal_after(fn, 'const UNIQUE_CONFLICT: &str = "')
tmpl = literal_after(fn, '#[cfg(not(feature = "telemetry-otlp"))]\n    let query = diesel::sql_query(format!(\n        "')
insert = tmpl.replace("{DEDUP_GUARD}", guard).replace("{UNIQUE_CONFLICT}", conflict)
assert "{" not in insert, insert

ev = src[src.index("async fn pg_evict_expired_unique_key("):]
evict = literal_after(ev, 'diesel::sql_query(\n        "')

out.mkdir(parents=True, exist_ok=True)
(out / "insert.sql").write_text(insert + "\n")
(out / "evict.sql").write_text(evict + "\n")
