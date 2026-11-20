# `DbIdempotencyStore` request path: one write of the held row, not two

This is a Ledger run on the request path of the database idempotency store
(`backend = "database"`, `autumn/src/idempotency/db_store.rs`). A handler that
takes `IdempotencyTx` writes its response with `IdempotencyTx::commit` in its
own transaction, so the response commits with the mutation.

`commit` wrote the row and then ran `keep_past_crash_lock`: a second `UPDATE`
of the same row, in the same transaction. It moved `expires_at_ms` to
`locked_until_ms + ttl` while the lock is live. The first write had just set
`expires_at_ms` to `now + ttl`, and a live lock ends after `now`, so the
second `UPDATE` always fired. `set_recovery_point` did the same.

The change computes the expiry as a `CASE` in the first write. Same result,
one statement and one row version fewer per request.

## Workload and fixture

`autumn/tests/integration/idempotency_db_request_profile.rs` drives real
`POST` requests with an `Idempotency-Key` through the middleware of a
`TestApp` configured with `backend = "database"`.

* One day of keys at the default 24 h TTL: 100,000 / 1,000,000 / 3,000,000
  rows keyed `v2:<sha256 hex>`, as the middleware writes them, with expiries
  spread over the next 24 h. 96% hold a stored response of 200 to 1,400
  bytes, 2% a recovery point, 0.5% a live lock. 1,500 rows are already
  expired and wait for the sweep. 3% are updated before `ANALYZE` with no
  `VACUUM` (3,044 / 30,421 / 90,359 dead tuples). 108 / 1,082 / 3,245 MB.
* Three phases, each with its own app pool (warmed by one request before the
  counters reset, and closed before they are read, so the backends flush
  their table statistics), each after a `CHECKPOINT`:
  1. `fresh-tx`: 1,024 first-time `POST /pay`. The handler inserts a payment
     and calls `IdempotencyTx::commit` in the same `Db::tx`.
  2. `fresh-plain`: 1,024 first-time `POST /plain`. The middleware stores the
     response.
  3. `replay`: 512 retries of `fresh-tx` keys.

Postgres 16.15, `pg_stat_statements.track = all`, autovacuum off, one client.

## Profile (baseline, 1M rows, `fresh-tx`)

From `pg_stat_statements`. Full output in `baseline-*.txt`.

| statement | calls/req | share of buffers | share of calls |
|:--|--:|--:|--:|
| `keep_past_crash_lock` `UPDATE` (the target) | 1 | **16.7%** | 4.8% |
| `unlock` release `UPDATE` | 1 | 16.5% | 4.8% |
| `commit` record `UPDATE` | 1 | 15.5% | 4.8% |
| `try_lock` lock `INSERT … ON CONFLICT` | 1 | 13.5% | 4.8% |
| `get` record `SELECT` (two per request) | 2 | 9.4% | 9.5% |
| `SELECT 1` (pool recycle check) | 6 | 0% | **28.6%** |
| everything else | 9 | 28.4% | 42.7% |

The phase is 21.01 statements and 95.9 shared buffers per request. Every
statement on `autumn_idempotency_keys` is a primary-key probe. No temp blocks
anywhere. None of the 3,074 row updates was HOT: every update changes
`expires_at_ms`, which `autumn_idempotency_keys_expires_at_idx` indexes.

## Plan

`explain-before.txt` and `explain-after.txt` (`explain.sh`). Both before and
after, each statement is an `Index Scan using autumn_idempotency_keys_pkey`
that returns one row. The plan does not change. What goes away is the second
`Update` node:

```
--- before: commit: write the record
 Update on public.autumn_idempotency_keys   Buffers: shared hit=7   WAL: records=1 bytes=439
--- before: commit: keep_past_crash_lock
 Update on public.autumn_idempotency_keys   Buffers: shared hit=17 dirtied=1   WAL: records=4 fpi=1 bytes=8431
--- after: commit: write the record
 Update on public.autumn_idempotency_keys   Buffers: shared hit=15 dirtied=1   WAL: records=3 fpi=1 bytes=3804
```

Both versions leave the row with the same expiry (`now + 86,460,000` ms, the
60 s lock plus the 24 h TTL).

## Measurement

`fresh-tx`, per 1,024 requests. Tools: `pg_stat_statements` (statements,
buffers), `pg_stat_user_tables` (row updates, HOT updates).

| size | statements | buffers | target statement buffers | key-row updates | HOT |
|:--|--:|--:|--:|--:|--:|
| 100k before | 21,512 | 78,586 | 13,553 | 3,074 | 0 |
| 100k after | 20,488 | 65,199 (−17.0%) | 0 | 2,050 (−33%) | 0 |
| 1M before | 21,512 | 98,200 | 16,445 | 3,074 | 0 |
| 1M after | 20,488 | 81,736 (−16.8%) | 0 | 2,050 (−33%) | 0 |
| 3M before | 21,512 | 98,393 | 16,352 | 3,074 | 0 |
| 3M after | 20,488 | 82,690 (−16.0%) | 0 | 2,050 (−33%) | 0 |

The commit write itself does not get more expensive: 15,257 buffers before
and 15,233 after, at 1M rows.

`fresh-plain` and `replay` do not call `IdempotencyTx::commit` and are
unchanged (13.02 and 2.00 statements per request, the same buffers).

WAL, from `wal.sh` (`pg_stat_wal`, 1,024 replays of the key-row writes of
one request, one transaction per write as on the request path; `wal.txt`):

| | records | full-page images | bytes/request |
|:--|--:|--:|--:|
| before, `full_page_writes = off` | 14,122 – 14,433 | 0 | 1,507 – 1,666 |
| after, `full_page_writes = off` | 12,977 – 12,987 | 0 | 1,417 – 1,541 |
| before, `full_page_writes = on` | 14,165 – 14,168 | 990 | 7,659 – 7,857 |
| after, `full_page_writes = on` | 13,019 – 13,030 | 1,037 – 1,065 | 8,068 – 8,075 |

WAL records drop 8–10% and record bytes 6–8%. After a checkpoint the bytes
are mostly full-page images, one for each page a write touches first, and
their count varies between runs by more than this change moves the total. So
this PR claims no total-WAL win, only fewer records and record bytes.

### Against the impact floor

The removed statement was 16.7% of the phase's buffers, and its buffers go
from 16,445 to 0 with no cost added to the write that absorbs it. That clears
"≥20% reduction in total buffers for a statement that is ≥5% of the
workload". The workload total drops 16.8%. The change also takes one
statement, one dead row version and two index entries (primary key and
`expires_at_ms`) off every `IdempotencyTx` request, and it adds no index.

## Equivalence

* `equivalence.sh` (`equivalence.txt`) runs the old pair of `UPDATE`s and
  the new one on copies of the same rows and compares whole rows: a live
  lock, a lock that ends exactly at `now` (not live), an expired lock, a row
  another owner holds (no write), a row whose expiry was later than both, a
  TTL at the `i64::MAX / 16` clamp, and a zero TTL. 0 rows differ.
* The old second `UPDATE` ran only while `locked_until_ms > now`, and then it
  always fired, because the first write had just set `now + ttl` and
  `locked_until_ms + ttl` is larger. The `CASE` has that condition and those
  two results.
* `idempotency_db_store`'s five Postgres tests (crash after commit replays,
  stale-owner fence, recovery point survives a crash, stale-cookie retry)
  pass unchanged against Postgres 16. So do the 53 `idempotency` unit tests
  and the 25 `sim_idempotency_db` tests on SQLite (crash after commit,
  recovery points through two crashes, short and zero TTLs).
* The one `now` reading replaces two that were microseconds apart. The old
  second reading could only see a lock as expired that the first saw as
  live, which gave the shorter expiry. The new write keeps the longer one,
  which is the side `keep_past_crash_lock` exists for.

## Not changed (findings)

* **`SELECT 1` six times per request, 28.6% of all statements.** The
  statement is diesel-async's `RecyclingMethod::Verified` check, the
  default, which Autumn's pool keeps: one bound `SELECT 1` on every
  checkout. So a request checks out a connection six times, because each
  store call takes its own. Each check costs a round trip and no buffers. Fewer checkouts, or a different recycle method, is a pool
  change and needs a maintainer's decision.
* **No-op probes.** For a new key, `try_lock`'s expired-key `DELETE` and
  `unlock`'s `DELETE` of a row with no record find nothing (4 and 5 buffers
  per request). Both are needed on other paths.
* **No HOT updates at all.** Every write to the row changes the indexed
  `expires_at_ms`. Keeping the sweep's index and getting HOT updates are in
  conflict. Dropping the index needs `idx_scan` evidence from a real
  deployment, which this harness cannot give.

## Reproduce

```text
# a Postgres 16 server with pg_stat_statements preloaded, autovacuum off
export AUTUMN_LEDGER_PG_URL=postgres://postgres@127.0.0.1:5432/postgres
AUTUMN_LEDGER_IDEMPOTENCY_KEYS=1000000 cargo test -p autumn-web \
  --test integration_tests -- --ignored idempotency_db_request_profile \
  --nocapture --test-threads=1
docs/perf/idempotency-db-request/explain.sh "$AUTUMN_LEDGER_PG_URL" before   # or after
docs/perf/idempotency-db-request/wal.sh "$AUTUMN_LEDGER_PG_URL"
docs/perf/idempotency-db-request/equivalence.sh "$AUTUMN_LEDGER_PG_URL"
```

Without `AUTUMN_LEDGER_PG_URL`, the harness starts a `postgres:16-alpine`
testcontainer.
