#!/usr/bin/env python3
"""Write the SQL that autumn/src/webhook_outbound/sql_store.rs ships, as the
files the harness runs: schema.sql, reactivate.sql and two pgbench delivery
scripts (a first-attempt success, and a 500 then a success on attempt 2).

Run: python3 docs/perf/webhook-reactivate/extract_sql.py <out-dir>
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
import os
SRC = pathlib.Path(os.environ.get(
    "SQL_STORE_RS", ROOT / "autumn/src/webhook_outbound/sql_store.rs")).read_text()


def rust_str(name):
    """`const NAME: &str = "...";` with Rust's backslash-newline continuation."""
    m = re.search(r'const ' + name + r': &str =\s*"((?:[^"\\]|\\.)*)"', SRC, re.S)
    if not m:
        sys.exit(f"{name} not found in sql_store.rs")
    return re.sub(r'\\\n\s*', '', m.group(1)).replace('\\"', '"')


SCHEMA = rust_str("WEBHOOK_SCHEMA_SQL")
UPSERT_LOG = rust_str("UPSERT_LOG_SQL")
SKIP_IGNORED = rust_str("SKIP_IGNORED_SQL")
REACTIVATE = rust_str("REACTIVATE_SQL")
FAILURE = rust_str("FAILURE_SQL")
LOG_COLUMNS = rust_str("LOG_COLUMNS")
SUB_COLUMNS = rust_str("SUBSCRIPTION_COLUMNS")

# The statements the store builds with format!, with the constants in.
ACTIVE_SUBS = (f"SELECT {SUB_COLUMNS} FROM autumn_webhook_subscriptions "
               "WHERE status = 'active' ORDER BY id")
LOG_BY_ID = f"SELECT {LOG_COLUMNS} FROM autumn_webhook_deliveries WHERE id = $1"
SUB_BY_ID = f"SELECT {SUB_COLUMNS} FROM autumn_webhook_subscriptions WHERE id = $1"
OUTCOME_UPSERT = UPSERT_LOG + SKIP_IGNORED

# pgbench `\set` takes numbers only: ids and times are variables, strings
# are literals.
PAYLOAD = ("'{\"id\":\"ord_7f3a\",\"event\":\"order.updated\",\"total_minor\":129900,"
           "\"currency\":\"USD\",\"items\":[{\"sku\":\"A-100\",\"qty\":2}],"
           "\"note\":\"" + "x" * 700 + "\"}'")
HEADERS = ("'{\"content-type\":\"application/json\",\"webhook-id\":\"wh\","
           "\"webhook-timestamp\":\"1762880000\",\"webhook-signature\":\"v1,"
           + "s" * 44 + "\"}'")
PRELUDE = """\\set sub random_zipfian(1, 200, 1.2)
\\set lid random(1000000000, 9999999999)
\\set elapsed random(20, 400)
\\set ts random(1762819200000, 1762905599000)
"""


def bind(sql, args):
    for i in range(len(args), 0, -1):
        sql = sql.replace(f"${i}", args[i - 1])
    return sql


def log_write(status, body, err, attempt, headers):
    """`log_delivery`: log_with_outcome's transaction, up to its outcome."""
    args = [":lid", ":sub", "'order.updated'", PAYLOAD, headers, status, body,
            ":elapsed", attempt, "5", "false", err, ":ts"]
    return ["BEGIN;", bind(LOG_BY_ID, [":lid"]) + ";", bind(OUTCOME_UPSERT, args) + ";"]


def dispatch():
    # WebhookOutboundManager::dispatch: the active subscriptions, then the
    # pending log of the matched one.
    return [ACTIVE_SUBS + ";"] + log_write("NULL", "NULL", "NULL", "1", "'{}'") + ["COMMIT;"]


def job_start():
    # deliver_webhook_job: the log by id, then the current subscription.
    return [bind(LOG_BY_ID, [":lid"]) + ";", bind(SUB_BY_ID, [":sub"]) + ";"]


def success(attempt):
    out = log_write("200", "'ok'", "NULL", attempt, HEADERS)
    out += [bind(REACTIVATE, [":sub"]) + ";", "COMMIT;"]
    # reset_subscription_after_success, after the transaction.
    return out + [bind(REACTIVATE, [":sub"]) + ";"]


def failure(attempt):
    out = log_write("500", "'boom'", "'server returned status: 500'", attempt, HEADERS)
    return out + [bind(FAILURE, ["50", ":sub"]) + ";", bind(SUB_BY_ID, [":sub"]) + ";", "COMMIT;"]


def retry_begin(attempt):
    # begin_attempt: the next attempt's pending log.
    return log_write("NULL", "NULL", "NULL", attempt, HEADERS) + ["COMMIT;"]


ok = PRELUDE + "\n".join(dispatch() + job_start() + success("1")) + "\n"
retry = PRELUDE + "\n".join(dispatch() + job_start() + failure("1")
                            + job_start() + retry_begin("2") + success("2")) + "\n"

out = pathlib.Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)
(out / "schema.sql").write_text(SCHEMA + "\n")
(out / "deliver_ok.pgbench").write_text(ok)
(out / "deliver_retry.pgbench").write_text(retry)
(out / "reactivate.sql").write_text(REACTIVATE + "\n")
