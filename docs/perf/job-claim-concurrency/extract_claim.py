#!/usr/bin/env python3
"""Print the SQL that `pg_claim_sql_single_queue` in autumn/src/job.rs builds.

Applies Rust's `\\`-newline string continuation and substitutes
PG_JOB_SELECT_COLS (the non-OTLP column list), so the harness runs the
shipped text byte for byte.
"""
import re, sys
from pathlib import Path

src = Path(sys.argv[1] if len(sys.argv) > 1 else "autumn/src/job.rs").read_text()
body = re.search(r'fn pg_claim_sql_single_queue\(\) -> String \{\s*format!\(\s*"(.*?)"\s*\)\s*\}', src, re.S).group(1)
cols = re.search(r'const PG_JOB_SELECT_COLS: &str = "(.*?)";', src, re.S).group(1)  # first = non-OTLP
unescape = lambda s: re.sub(r'\\\n\s*', '', s)
print(unescape(body).replace("{PG_JOB_SELECT_COLS}", unescape(cols)))
