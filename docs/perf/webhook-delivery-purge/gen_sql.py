#!/usr/bin/env python3
"""Write the purge workload's SQL from autumn/src/webhook_outbound/sql_store.rs.

Runs docs/perf/webhook-reactivate/extract_sql.py (schema.sql and the two
pgbench delivery scripts the store issues), moves the deliveries' timestamps
into the workload hour, and adds purge.sql: the statement
`purge_deliveries_before` runs. Fails when that statement is not in the source.

Run: python3 docs/perf/webhook-delivery-purge/gen_sql.py <out-dir>
"""
import os
import pathlib
import re
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[2]
SRC = pathlib.Path(os.environ.get(
    "SQL_STORE_RS", ROOT / "autumn/src/webhook_outbound/sql_store.rs")).read_text()
out = pathlib.Path(sys.argv[1])
subprocess.run([sys.executable, str(HERE.parent / "webhook-reactivate/extract_sql.py"), str(out)],
               check=True)

m = re.search(r'pub async fn purge_deliveries_before.*?sql\(\s*"((?:[^"\\]|\\.)*)"', SRC, re.S)
if not m:
    sys.exit("purge_deliveries_before's DELETE not found in sql_store.rs")
purge = re.sub(r'\\\n\s*', '', m.group(1))
if not purge.startswith("DELETE FROM autumn_webhook_deliveries"):
    sys.exit(f"unexpected purge statement: {purge}")
# The workload hour is (1762905600000, 1762909200000]; a 30-day retention.
(out / "purge.sql").write_text(purge.replace("$1", ":cutoff") + ";\n")

OLD = r"\set ts random(1762819200000, 1762905599000)"
NEW = r"\set ts random(1762905600000, 1762909199999)"
# The store stamps `logged_at` with the clock at each write of a log (the
# pending log, then every outcome), so the k-th write of one log carries a
# later time. extract_sql.py binds one :ts to all of them.
for name in ("deliver_ok.pgbench", "deliver_retry.pgbench"):
    p = out / name
    text = p.read_text()
    if OLD not in text:
        sys.exit(f"{name}: timestamp line not found")
    parts = text.split(", :ts)")
    if len(parts) < 3:
        sys.exit(f"{name}: expected two or more log writes")
    sets = [NEW] + [f"\\set ts{k} :ts + {k} * :elapsed" for k in range(1, len(parts) - 1)]
    text = parts[0] + "".join(
        (", :ts)" if k == 0 else f", :ts{k})") + part for k, part in enumerate(parts[1:]))
    p.write_text(text.replace(OLD, "\n".join(sets)))
