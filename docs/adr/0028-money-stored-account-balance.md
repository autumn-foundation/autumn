# ADR 0028: The Money Ledger Stores Each Account Balance

- Status: Accepted
- Date: 2026-10-29
- Deciders: Autumn maintainers
- Tags: money, ledger, performance, migrations

## Context

`money::ledger::post` checked the balance that a transaction leaves. To get
the balance, it summed all postings of each account it touched (issue #3339).
So the cost of one post increased with the history, and N posts cost O(N²).
On the `ledger_post` bench, 3 times the payouts cost 8 times the instructions.

The ledger tables are append-only. `post` locks the account rows
(`FOR UPDATE` on Postgres). `post` writes the postings before the transaction
row, so a dropped future cannot commit half a transaction.

## Options

1. A covering index `(account_id, amount_minor)`. It removes the table lookups
   (22-35% fewer instructions), but the cost stays O(history).
2. A balance column on the account row, kept by `post` in Rust.
3. A balance column on the account row, kept by a database trigger on the
   postings table.
4. A running balance on each posting row. The read is "the last posting of
   the account". It needs a backfill of append-only rows, and it depends on
   the order of the posting ids.

## Decision

Option 3, with `NULL` for "not known".

- `balance_minor BIGINT` on `_autumn_money_accounts`. New rows start at `0`.
  Rows from before the migration start at `NULL`, so the migration does not
  read the postings.
- Postgres: a statement trigger with a transition table. It adds the net
  change of each account in `NUMERIC`, so a step cannot overflow.
- SQLite: a row trigger. SQLite has no statement trigger. When a step would
  leave `i64`, the trigger writes `NULL`, because SQLite would change the
  value to an inexact `REAL`.
- SQLite: an `INSERT OR REPLACE` of an account with postings sets the balance
  to `NULL`. Otherwise the column default `0` replaces the balance.
- `post` reads the balance with the account lock. For a `NULL` balance, it
  sums the postings once and writes the sum back.
- `ledger::balance_drift` reports a stored balance that is not the sum of the
  postings. Setting `NULL` repairs it.

## Planning notes

**Brainstorming.** Covering index; balances table; balance column; balance on
each posting; materialized view; periodic snapshots plus a delta sum; keep the
balance in Rust; keep it in a trigger; `NULL` as "not known".

**Reverse brainstorming** (how can the stored balance lie?):

| Failure | Answer |
| --- | --- |
| An old binary posts during a rolling deploy | The trigger keeps the balance for every `INSERT` |
| An operator inserts postings by hand | The same trigger |
| SQLite `INSERT OR REPLACE` resets the row | A trigger sets `NULL` |
| One step of a transaction leaves `i64` | Postgres nets in `NUMERIC`; SQLite writes `NULL` |
| A dropped `post` future | The trigger runs in the posting `INSERT`; a written-back sum is true before the post |
| A manual `UPDATE` of the column | `balance_drift` reports it; `NULL` repairs it |
| A slow migration on a large ledger | Existing rows start at `NULL`; no scan |
| Postgres `REPEATABLE READ` reads a stale balance | The trigger updates the account row, so a second poster gets `40001` |

**Six thinking hats.**

- White (facts): per-payout cost grew from 9.9M to 52.7M instructions as the
  history grew. The index cut 22-35% only. The ledger shipped in 0.8.0, so the
  change needs a new migration.
- Red (feeling): a cached balance in a money ledger feels risky. The database
  must own it, not each caller.
- Black (risks): drift, overflow in a step, `REAL` in SQLite, rolling deploys,
  churn on a hot account row in Postgres. See the table above.
- Yellow (benefits): post cost is O(accounts in the transaction). One query
  less per post. `balance` is O(1).
- Green (ideas): `NULL` as "not known" removes the backfill and gives a
  repair tool. A transition table nets each statement on Postgres.
- Blue (process): tests first on both backends, then the migration and code,
  then a refactor of the balance check into a function without I/O.

## Consequences

- Each post updates the account rows it touches. On Postgres, this adds row
  versions on a hot account. The column has no index, so the updates can be
  HOT updates.
- Apps must run `autumn migrate` before they deploy the new binary.
- A manual `UPDATE` of `balance_minor` can make drift. `balance_drift` finds
  it. The triggers are a guard-rail against accidents, as in the rest of the
  ledger. They are not a boundary against all SQL.
