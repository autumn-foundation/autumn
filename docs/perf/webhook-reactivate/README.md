# Webhook delivery: do not rewrite a healthy subscription on every success

Harness: `extract_sql.py` writes the SQL that
`autumn/src/webhook_outbound/sql_store.rs` ships (schema, the two pgbench
delivery scripts, `REACTIVATE_SQL`). `fixture.sql`, `run.sh <N>`,
`report.sql`, `explain.sh`, `lock_probe.sh`, `contention.sh <N>`,
`equivalence.sh [BASE]`.
Artifacts: `baseline-<N>.txt` and `explain-before.txt`, `lock-before.txt`,
`contention-before.txt` (commit 857350f); `after-<N>.txt`, `explain-after.txt`,
`lock-after.txt`, `contention-after.txt`, `equivalence.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded (autovacuum off,
so the counters do not depend on when it runs), PG* env set, then
`run.sh 20000`, `explain.sh`, `lock_probe.sh`, `contention.sh 40000` and
`equivalence.sh 857350f`. Every script reads the SQL from the working tree,
or from the file `SQL_STORE_RS` names. For the baseline, run them with
`SQL_STORE_RS=<(git show 857350f:autumn/src/webhook_outbound/sql_store.rs)`.
(857350f holds the baseline numbers. The scripts were left out of it by the
`*.sh`/`*.py` ignore rule and were added in a later commit.)

Fixture: 200 subscriptions (194 healthy, 3 with 1-4 failures in a row, 2
disabled, 1 failed) and 1,000,000 delivery logs over 30 days (97.5% 2xx, 1%
retrying, 1% DLQ, 0.5% pending), receivers zipf-skewed. Workload: N
deliveries in the order the store issues them, 98% a first-attempt success
and 2% a 500 then a success on attempt 2, receiver drawn zipf(1.2), seeded.

## Mechanism

A successful delivery runs `REACTIVATE_SQL` twice: in the log transaction
(`log_with_outcome`, `Outcome::Success`) and again after it
(`reset_subscription_after_success`). It had no condition but the id, so it
wrote a new version of a subscription row that was already `active` with 0
failures. That is 40,000 row versions for 20,000 deliveries, WAL for each, a
row lock held until the log transaction commits, and dead versions that the
next dispatch `SELECT` of the active subscriptions prunes (and logs).

The change adds `AND (consecutive_failures <> 0 OR status = 'failed')`: the
statement writes only a row it changes.

## Measurement (pg_stat_statements, pg_stat_user_tables, pg_current_wal_lsn)

| N = 20,000, 1 client | before | after | delta |
| --- | --- | --- | --- |
| `REACTIVATE_SQL` calls | 40,000 | 40,000 | |
| rows it writes | 40,000 | 431 | -98.9% |
| its buffers | 280,028 | 80,880 | -71.1% |
| its WAL bytes | 3,935,737 | 68,091 | -98.3% |
| dispatch `SELECT … WHERE status = 'active'` WAL bytes (pruning) | 1,183,594 | 0 | -100% |
| subscription `n_tup_upd` | 40,427 | 858 | -97.9% |
| workload buffers | 1,016,804 | 817,667 | -19.6% |
| workload WAL bytes | 36,011,546 | 30,975,990 | -14.0% |
| WAL, LSN delta | 38,985,632 | 32,858,416 | -15.7% |
| temp blocks written | 0 | 0 | |

`REACTIVATE_SQL` was 21.8% of calls, 27.5% of buffers and 10.9% of WAL. The
log `INSERT … ON CONFLICT` is the rest of the WAL (85%) and is inherent.

Linear in N (per delivery, workload): buffers 50.8 → 40.9 at 5,000, 20,000
and 50,000; LSN WAL 9,942,064 → 8,407,752 (N = 5,000), 97,115,568 → 81,787,616
(N = 50,000).

Plan (`explain-*.txt`), healthy subscription: before, `Update` writes the row
(it takes a transaction id), 7 buffers. After, the planner uses the primary
key, the filter removes the row, nothing is written, no transaction id, 2
buffers.

Lock: `lock_probe.sh` holds a success log transaction open. Before, a
concurrent `FOR UPDATE NOWAIT` on the subscription fails ("could not obtain
lock on row"). After, the row is free. With 8 workers (`contention.sh`),
10.6% of worker wait samples were `Lock` (transactionid 9.7%, tuple 0.9%)
before and none after; the workload's buffers went 5,509,046 → 1,650,879
(-70%), WAL 69,867,011 → 62,689,138 (-10.3%). The samples depend on
scheduling and only corroborate the probe.

## Equivalence

* Workload end state: the ordered md5 of every subscription row
  (`bbac99a2…`) and of every delivery log is identical before and after, at
  N = 5,000, 20,000 and 50,000.
* `equivalence.sh`: base and new statement on every status
  (active, disabled, failed, an unknown one) x 0, 1, 49, 50 failures, plus a
  missing id, applied twice: identical rows after each pass.
* A row the old statement changed, the new one changes the same way. A row
  it skips is one the old statement left with the same values. Callers
  ignore the affected-row count (`update_subscription`, the success branch of
  `log_with_outcome`, the actuator replay).
* Concurrency: a healthy row's success no longer waits for a concurrent
  failure's uncommitted increment; it reads the committed row and skips. The
  delivery is then ordered before that failure, and the counter keeps the
  failure. Before, it waited and reset the failure. Both orders are ones the
  two deliveries could have run in.

## Write cost

No index, no migration, no lock. The statement now writes WAL only when it
changes a row.
