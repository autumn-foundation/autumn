#!/usr/bin/env python3
"""Turn the extracted enqueue SQL into pgbench scripts, one per job kind.

Usage: gen_pgbench.py WORK_DIR
Reads WORK_DIR/insert.sql and evict.sql (extract_sql.py) and writes
WORK_DIR/{plain,running,pending,ttl}.pgbench. Each $n becomes the value the
store binds for it, cast to the type diesel binds; a bind that is None for
that job kind becomes a typed NULL. Every run is a custom plan either way
(diesel-async prepares a raw sql_query anew on every call; pgbench -M
extended sends an unnamed statement), so a typed NULL plans as a NULL bind.
"""
import re
import sys
from pathlib import Path

work = Path(sys.argv[1])
insert = (work / "insert.sql").read_text().strip()
evict = (work / "evict.sql").read_text().strip()
TYPES = {1: "text", 2: "text", 3: "text", 4: "integer", 5: "bigint", 6: "text",
         7: "text", 8: "bigint", 9: "text", 10: "integer", 11: "timestamptz",
         12: "text", 13: "bigint"}
TTL_MS = "3600000"
KINDS = {
    # name, key expression, window tag, ttl
    "plain": ("'webhook.send'", None, None, None),
    "running": ("'digest.send'", "'user:' || :uk", "'running'", None),
    "pending": ("'sync.pull'", "'acct:' || :ak", "'pending'", None),
    "ttl": ("'report.build'", "'acct:' || :rk", "'ttl'", TTL_MS),
}


def bind(sql, values, types):
    def sub(m):
        n = int(m.group(1))
        v = values.get(n)
        return f"(NULL::{types[n]})" if v is None else f"({v})::{types[n]}"
    return re.sub(r"\$(\d+)", sub, sql)


for kind, (name, key, tag, ttl) in KINDS.items():
    vals = {1: "'e' || :id", 2: name, 3: "'{\"n\": ' || :id || '}'", 4: "5", 5: "250",
            6: key, 7: tag, 8: ttl, 12: "'default'"}
    lines = ["\\set id random(1, 1000000000000)",
             "\\set uk random_zipfian(1, 20000, 1.1)",
             "\\set ak random_zipfian(1, 2000, 1.1)",
             "\\set rk random_zipfian(1, 2000, 1.1)"]
    if ttl:
        lines.append(bind(evict, {1: name, 2: key, 3: ttl}, {1: "text", 2: "text", 3: "bigint"}) + ";")
    lines.append(bind(insert, vals, TYPES) + ";")
    (work / f"{kind}.pgbench").write_text("\n".join(lines) + "\n")
