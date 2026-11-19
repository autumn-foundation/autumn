#!/usr/bin/env python3
"""Write the Postgres SQL that autumn/src/repository_commit_hooks.rs ships as
pgbench scripts, and the queue schema from the shipped migrations.

Usage: extract_sql.py OUT_DIR [REPO_ROOT]

Writes:
  OUT_DIR/schema.sql   every migration statement that names
                       autumn_repository_commit_hooks, in migration order, from
                       $MIGRATIONS_DIR (default autumn/migrations, the full
                       framework set; autumn/repository_commit_hook_migrations
                       is the standalone set). A DO block is kept whole.
                       $SKIP_MIGRATIONS (comma-separated directory names)
                       are left out.
  OUT_DIR/stale_ms     HOOK_STALE_CLAIM_AFTER in milliseconds
  OUT_DIR/tick.pgbench one idle worker tick: both stale-recovery UPDATEs,
                       then one claim attempt
  OUT_DIR/hook.pgbench one #[after_commit] hook end to end: pending insert,
                       mark succeeded, finalize, claim, ack
  OUT_DIR/recover_pending.sql / recover_running.sql  the recovery statements
                       with literal binds, for EXPLAIN
"""
import os
import re
import sys
from pathlib import Path

out = Path(sys.argv[1])
root = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).resolve().parents[3]
src = (root / "autumn/src/repository_commit_hooks.rs").read_text()


def rust_str(body: str) -> str:
    # `\` + newline skips the newline and the next line's leading whitespace.
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


def pg_const(name: str) -> str:
    return literal_after(src, f'#[cfg(not(feature = "sqlite"))]\nconst {name}: &str = "')


def bind(sql: str, names: list) -> str:
    # $n -> :name, highest index first so $1 never eats $10.
    for i in range(len(names), 0, -1):
        sql = sql.replace(f"${i}", f":{names[i - 1]}")
    assert "$" not in sql.replace("'$", ""), sql
    return sql


running = pg_const("HOOK_RECOVER_STALE_RUNNING_SQL")
pending = pg_const("HOOK_RECOVER_STALE_PENDING_SQL")
pending_insert = pg_const("HOOK_PENDING_INSERT_SQL")
mark = pg_const("HOOK_MARK_AFTER_HOOK_SUCCEEDED_SQL")
finalize = pg_const("HOOK_FINALIZE_AFTER_HOOK_SQL")
ack = pg_const("HOOK_ACK_SUCCESS_SQL")
cols = pg_const("HOOK_SELECT_COLS")
fn = src[src.index("async fn pg_claim_next_repository_commit_hook("):]
claim = literal_after(fn, 'let sql = format!(\n        "').replace("{HOOK_SELECT_COLS}", cols)
assert "{" not in claim, claim

stale_ms = int(re.search(r"HOOK_STALE_CLAIM_AFTER: Duration = Duration::from_secs\((\d+)\)", src).group(1)) * 1000

out.mkdir(parents=True, exist_ok=True)

# Schema: the statements of every shipped migration that touch the queue table.
stmts = []
migrations = root / os.environ.get("MIGRATIONS_DIR", "autumn/migrations")
skip = set(filter(None, os.environ.get("SKIP_MIGRATIONS", "").split(",")))
for up in sorted(migrations.glob("*/up.sql")):
    if up.parent.name in skip:
        continue
    text = re.sub(r"--[^\n]*", "", up.read_text())
    if "$$" in text:
        if "autumn_repository_commit_hooks" in text:
            stmts.append(f"-- {up.parent.name}\n{text.strip()}")
        continue
    for stmt in text.split(";"):
        if "autumn_repository_commit_hooks" in stmt and stmt.strip():
            stmts.append(f"-- {up.parent.name}\n{stmt.strip()};")
(out / "schema.sql").write_text("\n\n".join(stmts) + "\n")
(out / "stale_ms").write_text(f"{stale_ms}\n")

# String binds come from run.sh as `pgbench -D`; `\\set` is numeric only.
tick = f"""{bind(running, ['err', 'stale'])};
{bind(pending, ['perr', 'stale'])};
{bind(claim, ['keys', 'worker'])};
"""
(out / "tick.pgbench").write_text(tick)

# Handler-key skew 70/15/10/5. The key is picked by a harness SELECT that
# touches no table; report.sql leaves it out.
hook = f"""\\set id random(1, 2000000000)
\\set k random(1, 100)
SELECT CASE WHEN :k <= 70 THEN 'app::PostRepository' WHEN :k <= 85 THEN 'app::CommentRepository' WHEN :k <= 95 THEN 'app::UserRepository' ELSE 'app::VoteRepository' END AS hkey \\gset
{bind(pending_insert, ['id', 'hkey', 'hname', 'ctx', 'rec', 'owner'])};
{bind(mark, ['ctx', 'rec', 'id', 'owner'])};
{bind(finalize, ['id', 'owner'])};
{bind(claim, ['keys', 'worker'])} \\gset c_
{bind(ack, ['c_id', 'worker'])};
"""
(out / "hook.pgbench").write_text(hook)


def literal(sql: str, vals: list) -> str:
    for i in range(len(vals), 0, -1):
        sql = sql.replace(f"${i}", vals[i - 1])
    return sql


(out / "recover_pending.sql").write_text(
    literal(pending, ["'stale pending after hook recovered by worker-1'", str(stale_ms)]) + ";\n")
(out / "recover_running.sql").write_text(
    literal(running, ["'stale claim recovered by worker-1'", str(stale_ms)]) + ";\n")
