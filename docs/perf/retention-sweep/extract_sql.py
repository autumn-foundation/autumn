#!/usr/bin/env python3
"""Print the retention sweep's Postgres SQL from autumn/src/data_retention.rs.

run.sh sources the output, so the harness measures the statements the source
at the checked-out commit issues: the table, predicate and batch key per
dataset, the COUNT, and the batched DELETE with every `{placeholder}` of its
format! string filled in. `$1` is the cutoff and `$2` the batch size.

    extract_sql.py [path/to/data_retention.rs]
"""
import re
import shlex
import sys
from pathlib import Path

SRC = Path(sys.argv[1]) if len(sys.argv) > 1 else (
    Path(__file__).resolve().parents[3] / "autumn/src/data_retention.rs")
text = SRC.read_text()


def literals(chunk):
    """The Rust string literals in `chunk`, in order, with `//` comments skipped."""
    out, i = [], 0
    while i < len(chunk):
        if chunk.startswith("//", i):
            nl = chunk.find("\n", i)
            i = len(chunk) if nl < 0 else nl
        elif chunk[i] == '"':
            j, buf = i + 1, []
            while chunk[j] != '"':
                if chunk[j] == "\\" and chunk[j + 1] == "\n":
                    j += 2
                    while chunk[j] in " \t\n":
                        j += 1
                    continue
                buf.append(chunk[j])
                j += 1
            out.append("".join(buf))
            i = j + 1
        else:
            i += 1
    return out


def fn_body(name):
    start = text.index(f"fn {name}(")
    open_ = text.index("{", text.index(")", start))
    depth, i = 0, open_
    while True:
        depth += {"{": 1, "}": -1}.get(text[i], 0)
        if depth == 0:
            return text[open_:i]
        i += 1


def arm(fn, dataset):
    """The literals of `dataset`'s match arm in `fn`, up to the next arm."""
    body = fn_body(fn)
    m = re.search(rf"RetentionDataset::{dataset}\b[^=]*=>", body)
    if not m:
        return None
    rest = body[m.end():]
    nxt = re.search(r"\n\s*(?:\|\s*)?RetentionDataset::\w+|\n\s*_\s*=>", rest)
    return literals(rest[: nxt.start() if nxt else len(rest)])


def lookup(fn, dataset):
    got = arm(fn, dataset)
    if got:
        return got[0]
    return literals(fn_body(fn).split("_ =>", 1)[1])[0]


# The batch statement is built in `sweep_batch_sql` when the source has one,
# else inline in `sweep_postgres`.
sweep = fn_body("sweep_batch_sql" if "fn sweep_batch_sql(" in text else "sweep_postgres")
delete = next(s for s in literals(sweep) if s.startswith("DELETE FROM"))
count = next(s for s in literals(fn_body("count_stale")) if s.startswith("SELECT COUNT"))


def order(dataset):
    """`{order}`: " ORDER BY <sweep_order>", or "" where sweep_order is None."""
    got = arm("sweep_order", dataset)
    return f" ORDER BY {got[0]}" if got else ""


# Per-dataset helpers the batch format! may name, besides the table and
# predicate.
helpers = {"key": lambda d: lookup("sweep_key_column", d), "order": order}

for var, dataset in (("JOBS", "JobHistory"), ("TRACK", "JobTracking"),
                     ("HOOKS", "CommitHooks"), ("EXP", "ExperimentAssignments")):
    table, predicate = arm("stale_row_predicate", dataset)[:2]
    values = {"table": table, "predicate": predicate}
    for ph in set(re.findall(r"\{(\w+)\}", delete)) - set(values):
        values[ph] = helpers[ph](dataset)
    print(f"{var}_TABLE={shlex.quote(table)}")
    print(f"{var}_KEY={shlex.quote(values['key'])}")
    print(f"{var}_COUNT={shlex.quote(count.format(**values))}")
    print(f"{var}_BATCH={shlex.quote(delete.format(**values))}")
