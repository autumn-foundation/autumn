#!/usr/bin/env python3
"""Write the Postgres SQL that autumn/src/outbox.rs issues as a psql
workload, plus the outbox/inbox schema it ships.

Usage: [MSG_ID=v4|v7] gen_scripts.py OUT_DIR MESSAGES [REPO_ROOT]

MSG_ID picks how the harness makes the message id that `insert_message`
binds: v4 is gen_random_uuid() (what Entropy::uuid_v4 makes), v7 is
uuid_v7() from uuid_v7.sql (the layout of Entropy::uuid_v7: a 48-bit Unix
millisecond timestamp first). The default follows outbox.rs.

Every statement the workload sends is a const string of outbox.rs, with its
placeholders bound as literals. The script fails if one is no longer there.
Statements that only pick inputs start with /*harness*/; report.sql leaves
them out.

The workload is the shape the framework writers make: each message has its
own id as the aggregate (docs/guide/outbox.md "Framework writers"). Topics
are 40% autumn.event, 30% autumn.job, 15% autumn.mail, 15% autumn.webhook.
Rounds of 100: 100 writes (one per app transaction), one relay claim of
`batch_size` 100, then per claimed row in seq order: Inbox::seen for an
autumn.event row (a listener that dedups) and MARK_SENT.

  OUT_DIR/schema.sql    SCHEMA_SQL (Postgres)
  OUT_DIR/workload.sql  the psql workload
  OUT_DIR/msg_id        v4 or v7
"""
import os
import re
import sys
from pathlib import Path

out = Path(sys.argv[1])
messages = int(sys.argv[2])
root = Path(sys.argv[3]) if len(sys.argv) > 3 else Path(__file__).resolve().parents[3]
src = (root / "autumn/src/outbox.rs").read_text()
out.mkdir(parents=True, exist_ok=True)


def rust_str(body: str) -> str:
    """The value of a Rust string literal body: `\\` + newline + indent is
    dropped, as rustc does."""
    return re.sub(r"\\\n\s*", "", body)


def const(name: str, cfg: str = "") -> str:
    m = re.search(cfg + r"(?:pub )?const " + name + r': &str = "(.*?)";', src, re.S)
    if not m:
        sys.exit(f"outbox.rs no longer has const {name}")
    return rust_str(m.group(1))


schema = const("SCHEMA_SQL", r'#\[cfg\(not\(feature = "sqlite"\)\)\]\n')
insert = const("INSERT_SQL")
mark_sent = const("MARK_SENT_SQL")
inbox_insert = const("INBOX_INSERT_SQL")
m = re.search(r'fn claim_sql\(.*?sql\(&format!\(\s*"(.*?)"\s*\)\)', src, re.S)
if not m:
    sys.exit("outbox.rs no longer has claim_sql")
claim = rust_str(m.group(1)).replace("{lock}", " FOR UPDATE SKIP LOCKED")
if "{" in claim:
    sys.exit("claim_sql has a format argument this script does not know")
if "aggregate.unwrap_or(&id)" not in src:
    sys.exit("insert_message_with_id no longer makes the id the default aggregate")

if "let id = entropy.uuid_v4().to_string();" in src:
    shipped = "v4"
elif re.search(r"let id = entropy\s*\.uuid_v7\(", src):
    shipped = "v7"
else:
    sys.exit("outbox.rs makes the message id some other way")
msg_id = os.environ.get("MSG_ID", shipped)
idfn = {"v4": "gen_random_uuid()::text", "v7": "uuid_v7()"}[msg_id]
(out / "msg_id").write_text(msg_id)
(out / "schema.sql").write_text(schema + "\n")


def bind(stmt: str, values: list[str]) -> str:
    """Replace $n with values[n-1], highest n first so $1 does not eat $10."""
    for i in range(len(values), 0, -1):
        stmt = stmt.replace(f"${i}", values[i - 1])
    if re.search(r"\$\d", stmt):
        sys.exit(f"unbound placeholder in: {stmt}")
    return stmt


def lit(s: str) -> str:
    return "'" + s.replace("'", "''") + "'"


TOPICS = ["autumn.event"] * 8 + ["autumn.job"] * 6 + ["autumn.mail"] * 3 + ["autumn.webhook"] * 3
TOPIC_LIST = ", ".join(lit(t) for t in sorted(set(TOPICS)))
CONSUMER = "listeners"
NOW = int(os.environ.get("NOW_MS", "1764000000000"))
BATCH = 100
LEASE = 60_000

lines = ["\\set QUIET on", "\\o /dev/null"]
clock = NOW
for r in range(messages // BATCH):
    for i in range(BATCH):
        n = r * BATCH + i
        topic = TOPICS[(n * 7) % len(TOPICS)]
        payload = (
            '{"type":"%s","n":%d,"actor":"user:%d","data":{"id":%d,"note":"%s"}}'
            % (topic, n, (n * 7919) % 200_000, n, "x" * (120 + (n * 31) % 160))
        )
        clock += 1
        lines.append(f"/*harness*/ SELECT {idfn} AS id \\gset")
        lines.append(
            bind(insert, [":'id'", ":'id'", lit(topic), lit(payload), str(clock), str(clock)]) + ";"
        )
    token = f"tok-{r:06d}"
    clock += 1
    lines.append(
        bind(
            claim.replace("__TOPICS__", TOPIC_LIST),
            [lit(token), str(clock + LEASE), str(clock), str(clock), str(BATCH)],
        )
        + ";"
    )
    # Per claimed row, in seq order: the handler (Inbox::seen for an event)
    # then MARK_SENT. The harness SELECT writes both statements per row; it
    # reads the claimed rows through idx_autumn_outbox_claim, so it adds no
    # sequential scan to the table counters.
    seen = bind(inbox_insert, [lit(CONSUMER), "%L", str(clock)])
    sent = bind(mark_sent, [str(clock + 1), "%s", lit(token)])
    lines.append(
        "/*harness*/ SELECT s FROM autumn_outbox o, LATERAL (VALUES "
        f"(1, CASE WHEN o.topic = 'autumn.event' THEN format({lit(seen)}, o.id) END), "
        f"(2, format({lit(sent)}, o.seq))) AS v(k, s) "
        f"WHERE o.dispatched_at IS NULL AND o.dead_at IS NULL AND o.claim_token = {lit(token)} "
        "AND s IS NOT NULL ORDER BY o.seq, v.k \\gexec"
    )
    clock += 1
lines.append("\\o")
(out / "workload.sql").write_text("\n".join(lines) + "\n")
