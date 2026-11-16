#!/usr/bin/env python3
"""Write the Postgres SQL that autumn/src/money/ledger.rs `post` issues as
pgbench scripts, plus the schema from the shipped migrations.

Usage: [TXN_ID=v4|v7] gen_scripts.py OUT_DIR [REPO_ROOT]

TXN_ID picks how the harness makes the transaction id that `post` binds:
v4 is gen_random_uuid() (what Uuid::new_v4 makes), v7 is uuid_v7() from
uuid_v7.sql (the layout of Uuid::now_v7: a 48-bit Unix millisecond
timestamp first). The default follows ledger.rs.

Each `post` statement is rebuilt from the format strings in ledger.rs
(checked below: the script fails if a string is no longer in the source).
Statements a script runs only to pick its inputs start with `/*harness*/`
and report.sql leaves them out.

  OUT_DIR/schema.sql      the two Postgres money migrations, in order
  OUT_DIR/charge.pgbench  a new 3-leg charge: customer, platform:fees, seller
                          (a dormant wallet takes the sum-and-store path)
  OUT_DIR/replay.pgbench  a retried charge: its key is already posted
  OUT_DIR/payout.pgbench  a new 20-leg payout: payout:pool and 19 sellers
"""
import os
import sys
from pathlib import Path

out = Path(sys.argv[1])
root = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).resolve().parents[3]
src = (root / "autumn/src/money/ledger.rs").read_text()
mig = root / "autumn/migrations"
out.mkdir(parents=True, exist_ok=True)
(out / "schema.sql").write_text(
    (mig / "20260917120000_create_money_ledger/up.sql").read_text()
    + "\n"
    + (mig / "20261029183512_add_money_account_balance/up.sql").read_text()
)

for needle in [
    'const ACCOUNT_COLUMNS: &str = "id, currency, allow_negative, balance_minor";',
    "WHERE id IN ({placeholders}) ORDER BY id{FOR_UPDATE}",
    'const FOR_UPDATE: &str = " FOR UPDATE";',
    "CAST(posted_at AS TEXT) AS posted_at \\\n         FROM {TRANSACTIONS_TABLE} WHERE idempotency_key = {}",
    "WHERE transaction_id = {} ORDER BY seq",
    "(transaction_id, seq, account_id, amount_minor, currency, posted_at) \\\n         VALUES {values}",
    '"({}, {}, {}, {}, {}, {NOW})"',
    "(id, idempotency_key, request_hash, currency, memo, posted_at) \\\n         VALUES ({}, {}, {}, {}, {}, {NOW}) \\\n         ON CONFLICT (idempotency_key) DO NOTHING",
    "FROM {POSTINGS_TABLE} WHERE account_id IN ({placeholders}) GROUP BY account_id",
    "WHERE id = {} AND balance_minor IS NULL",
    "CAST(COALESCE(SUM({column} / {SPLIT}), 0) AS BIGINT) AS hi",
    "const SPLIT: i64 = 1_000_000;",
]:
    if needle not in src:
        sys.exit(f"ledger.rs no longer contains: {needle}")

if "uuid::Uuid::new_v4().to_string()" in src:
    shipped = "v4"
elif "uuid::Uuid::now_v7().to_string()" in src:
    shipped = "v7"
else:
    sys.exit("ledger.rs makes the transaction id some other way")
TXN_ID = os.environ.get("TXN_ID", shipped)
NEW_ID = {"v4": "gen_random_uuid()::text", "v7": "uuid_v7()"}[TXN_ID]
(out / "txn_id").write_text(TXN_ID + "\n")

ACC = "_autumn_money_accounts"
TXN = "_autumn_money_transactions"
POS = "_autumn_money_postings"
SUMS = ("CAST(COALESCE(SUM(amount_minor / 1000000), 0) AS BIGINT) AS hi, "
        "CAST(COALESCE(SUM(amount_minor % 1000000), 0) AS BIGINT) AS lo")


def lock(ids):
    return (f"SELECT id, currency, allow_negative, balance_minor FROM {ACC} "
            f"WHERE id IN ({', '.join(ids)}) ORDER BY id FOR UPDATE;")


def load_txn():
    return [
        f"SELECT id, idempotency_key, request_hash, currency, memo, "
        f"CAST(posted_at AS TEXT) AS posted_at FROM {TXN} WHERE idempotency_key = :key \\gset t_",
        f"SELECT account_id, amount_minor, currency FROM {POS} "
        f"WHERE transaction_id = :t_id ORDER BY seq;",
    ]


def miss():
    return (f"SELECT id, idempotency_key, request_hash, currency, memo, "
            f"CAST(posted_at AS TEXT) AS posted_at FROM {TXN} WHERE idempotency_key = :key;")


def write(legs):
    values = ", ".join(f"(:tid, :q{seq}, {acct}, {amt}, :cur, CURRENT_TIMESTAMP)"
                       for seq, (acct, amt) in enumerate(legs))
    return [
        f"INSERT INTO {POS} (transaction_id, seq, account_id, amount_minor, currency, posted_at) "
        f"VALUES {values};",
        f"INSERT INTO {TXN} (id, idempotency_key, request_hash, currency, memo, posted_at) "
        f"VALUES (:tid, :key, :rh, :cur, :memo, CURRENT_TIMESTAMP) "
        f"ON CONFLICT (idempotency_key) DO NOTHING;",
    ]


def dormant(var):
    # sum_postings_by_account + store_summed_balances for one NULL-balance account.
    return [
        f"\\if :dormant",
        # \\aset leaves the defaults when the account has no postings.
        "\\set d_hi 0",
        "\\set d_lo 0",
        f"SELECT account_id, {SUMS} FROM {POS} WHERE account_id IN ({var}) GROUP BY account_id \\aset d_",
        "\\set bal :d_hi * 1000000 + :d_lo",
        f"UPDATE {ACC} SET balance_minor = :bal WHERE id = {var} AND balance_minor IS NULL;",
        f"\\endif",
    ]


seeds = r"""\set c 1 + int(200000 * pow(random(0, 999999) / 1000000.0, 2))
\set s 1 + int(20000 * pow(random(0, 999999) / 1000000.0, 3))
\set amt random(500, 20499)
\set fee :amt / 10
\set net :amt - :fee
\set n random(1, 4000000000000000000)
\set neg -1 * :amt"""

# post binds the sorted, deduplicated ids: customer:* < platform:fees < seller:*.
charge = [seeds,
          "/*harness*/ SELECT 'customer:' || :c::text AS cid, 'seller:' || :s::text AS sid, "
          f"{NEW_ID} AS tid, encode(sha256(('new:' || :n::text)::bytea), 'hex') AS key, "
          "encode(sha256(('req-new-' || :n::text)::bytea), 'hex') AS rh, 'order ' || :n::text AS memo, "
          f"(SELECT balance_minor IS NULL FROM {ACC} WHERE id = 'customer:' || :c::text) AS dormant \\gset",
          "BEGIN;",
          lock([":cid", ":fees", ":sid"]),
          miss(),
          *dormant(":cid"),
          *write([(":cid", ":neg"), (":sid", ":net"), (":fees", ":fee")]),
          *load_txn(),
          "END;"]

# A retry of a history transaction: lock its accounts, find the key. For the 5%
# that are payouts the harness locks three of the twenty (seq 0..2).
replay = [r"\set g random(1, :txns)",
          "/*harness*/ SELECT encode(sha256(('order:' || g)::bytea), 'hex') AS key, "
          "max(account_id) FILTER (WHERE seq = 0) AS a0, "
          "max(account_id) FILTER (WHERE seq = 1) AS a1, "
          "max(account_id) FILTER (WHERE seq = 2) AS a2 "
          f"FROM (SELECT :g::int AS g) x LEFT JOIN {TXN} t ON t.idempotency_key = encode(sha256(('order:' || g)::bytea), 'hex') "
          f"LEFT JOIN {POS} p ON p.transaction_id = t.id GROUP BY g \\gset",
          "BEGIN;",
          lock([":a0", ":a2", ":a1"]),
          *load_txn(),
          "END;"]

legs = 19
pick = ", ".join(f"'seller:' || (1 + (:s::int + {k} * 997) % 20000) AS s{k}" for k in range(1, legs + 1))
payout = [seeds,
          f"/*harness*/ SELECT {pick}, {NEW_ID} AS tid, "
          "encode(sha256(('payout:' || :n::text)::bytea), 'hex') AS key, "
          "encode(sha256(('req-payout-' || :n::text)::bytea), 'hex') AS rh, 'payout run ' || :n::text AS memo \\gset",
          f"\\set total -{legs} * :amt",
          "BEGIN;",
          lock([":pool"] + [f":s{k}" for k in range(1, legs + 1)]),
          miss(),
          *write([(":pool", ":total")] + [(f":s{k}", ":amt") for k in range(1, legs + 1)]),
          *load_txn(),
          "END;"]

for name, lines in [("charge", charge), ("replay", replay), ("payout", payout)]:
    (out / f"{name}.pgbench").write_text("\n".join(lines) + "\n")
