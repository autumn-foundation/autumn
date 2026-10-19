# 🗃️ Ledger findings: ledgered bulk writes issue 5 statements per row

Outcome: **findings, no fix.** The counter is a clear O(n) statement count, but
removing it means batching the tamper-evident chain writer, which needs a human
decision (see "Why no PR").

## 🎯 Workload

A nightly import/purge job over a `#[repository(ledgered = true)]` table:
`save_many(n)`, `update_many(n)`, `delete_many(n)` at n = 100 / 400 / 1,600,
driven through the public repository API
(`autumn/tests/integration/ledger_bulk_write_profile.rs`). Fixture: 3,000
pre-seeded accounts, 10% (300) touched 8 times, the rest once; `ANALYZE` without
`VACUUM`; the Autumn-shipped version-history and ledger migrations applied
verbatim; Postgres 16 with `pg_stat_statements`.

## 📈 Profile (save_many(100); same shape for update/delete)

| Statement | calls | % of calls | % of buffers |
| --- | --- | --- | --- |
| `INSERT INTO _autumn_version_history` | 100 | 19.9 | 32.4 |
| `INSERT INTO _autumn_ledger_revisions` | 100 | 19.9 | 21.7 |
| `INSERT INTO _autumn_ledger_high_water` | 100 | 19.9 | 16.2 |
| chain-state read (`clock_timestamp()`, head, mark) | 100 | 19.9 | 10.7 |
| reload `SELECT` of the written row | 100 | 19.9 | 8.0 |
| the one real multi-row `INSERT` of the data | 1 | 0.2 | 11.0 |

Per-row bookkeeping is 99.6% of calls and 88% of buffers for a batch write.

## 🧭 Plan

The per-row chain read is already an index-only-ish backward index scan on
`idx_autumn_ledger_revisions_record` (6 buffers, 1 row). Nothing to fix at plan
level; the cost is the statement count. Full output in `baseline/`.

## 💡 Hypothesis

`save_many`/`update_many`/`delete_many` loop the per-row version-history +
ledger append (`ledger_append_ts` -> `append_revision`) inside one transaction,
so each row pays: reload, chain-state read, revision insert, high-water upsert,
version-history insert. Statements per row are constant (5), so total is O(n).

## 📊 Measurement (`pg_stat_statements`, same session)

| op | n | statements | per row | buffers | WAL bytes |
| --- | --- | --- | --- | --- | --- |
| save_many | 100 | 503 | 5.03 | 3,745 | 197,586 |
| save_many | 400 | 2,003 | 5.01 | 14,912 | 783,562 |
| save_many | 1,600 | 8,004 | 5.00 | 59,674 | 3,140,947 |
| update_many | 100 | 504 | 5.04 | 4,170 | 194,548 |
| update_many | 400 | 2,004 | 5.01 | 18,601 | 873,396 |
| update_many | 1,600 | 8,006 | 5.00 | 74,507 | 3,561,596 |
| delete_many | 100 | 504 | 5.04 | 4,631 | 223,158 |
| delete_many | 400 | 2,004 | 5.01 | 19,408 | 939,676 |
| delete_many | 1,600 | 8,006 | 5.00 | 77,307 | 3,763,257 |

`temp_blks_written` = 0 throughout. Every sampled chain verified intact
(`ledger_verify`) after the run.

## Why no PR

A fix would batch the chain-state read (`eq_any`), the revision/high-water/
version-history inserts, and the reload. That rewrites `append_revision`, whose
guards (missing mark, mark/head disagreement, i64::MAX, clock clamp) are the
tamper-evidence contract from #2323, and it changes the order of hashed writes
inside the transaction. Per the Ledger rules, anything touching those semantics
is "ask before". A maintainer should decide whether a batched appender is wanted
and which guards must be preserved per record (duplicate record ids in one batch
need chained seq allocation within the batch).

## 🔬 Reproduce

The consolidated `integration_tests` binary needs more than 14 GB of RAM to
link on a small box, so on a constrained host run the module as its own target
(temporary `[[test]]` that `#[path]`-includes the file), otherwise:

```text
cargo test -p autumn-web --features "test-support" --test integration_tests \
  -- --ignored ledger_bulk_write_profile --nocapture --test-threads=1
```
