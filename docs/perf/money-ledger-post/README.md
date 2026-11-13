# `money::ledger::post`: no read-back after the insert

A Ledger run on `ledger::post` (`autumn/src/money/ledger.rs`). The harness
is `autumn/tests/integration/money_ledger_post_perf.rs`.

## Reproduce

```sh
docs/perf/money-ledger-post/run.sh baseline   # or: after
```

Each size is one run of
`cargo test -p autumn-web --test integration_tests -- --ignored money_ledger_post_profile`
with `LEDGER_PERF_TXNS` set to 20,000, 200,000 and 2,000,000. Docker is
needed, unless `AUTUMN_PERF_PG_URL` names a Postgres 16 server that
preloads `pg_stat_statements` with `pg_stat_statements.track=all`. These
files came from a local Postgres 16.15 run in that mode.

## Fixture

| size | transactions | postings | wallets | sellers | house |
|--:|--:|--:|--:|--:|--:|
| 20k | 20,000 | 41,200 | 800 | 500 | 4 |
| 200k | 200,000 | 412,000 | 8,000 | 500 | 4 |
| 2M | 2,000,000 | 4,120,000 | 80,000 | 500 | 4 |

60% of the history is wallet top-ups (2 lines) and 40% is purchases (2 lines,
or 3 with a fee line). Wallets are picked with a cubic skew, so a few hold most
of the history. The house accounts hold all of it. 5% of the wallets have a
`NULL` stored balance, like rows from before the stored balance (#3339).
Setting them to `NULL` leaves dead tuples in the accounts table. The fixture
then runs `VACUUM ANALYZE`.

## Workload

400 calls of the public `ledger::post`, each in its own transaction:

* 55% top-ups
* 25% purchases
* 10% 7-line marketplace splits
* 10% retries of an earlier key

That gives 360 posted calls and 40 replays. Every input comes from the
operation index, so two runs post the same money.

## Profile (baseline)

All buffers are `shared_blks_hit + shared_blks_read` from
`pg_stat_statements`, summed over the top-level statements. The buffers of a
nested statement (the balance trigger, the accounts foreign key) are
already counted in the INSERT that fired it.

At 200k:

| statement | calls | % calls | buffers | % buffers |
|:--|--:|--:|--:|--:|
| postings `INSERT` (2, 3 and 7 lines; includes trigger + accounts FK) | 360 | 8.9% | 20,381 | 46.6% |
| `NULL`-balance sum (`account_id IN (...) GROUP BY`) | 12 | 0.3% | 6,022 | 13.8% |
| deferred `transactions` FK check, at COMMIT | 953 | 23.5% | 4,765 | 10.9% |
| transactions `INSERT ... ON CONFLICT DO NOTHING` | 360 | 8.9% | 3,995 | 9.1% |
| accounts `SELECT ... FOR UPDATE` (2, 3, 7 ids) | 400 | 9.9% | 4,163 | 9.5% |
| **transaction by key** | **760** | **18.7%** | **2,680** | **6.1%** |
| **postings of a transaction** | **400** | **9.9%** | **1,612** | **3.7%** |

The transaction-by-key read and the postings read run twice in a posted call:

1. Once before the writes. This is the idempotency check: a miss, and on a
   replay a hit followed by the postings read.
2. Once after the writes, to read back the rows the call has just
   inserted.

The second pass is 2 of the 6 statements a posted call issues. It reads data
the call already has, or could get from the INSERT itself.

## Hypothesis

The read-back is a round trip that returns no new information. `RETURNING` on
the transaction INSERT gives back the stored row: the database's own
`posted_at`, rendered by the same `CAST(posted_at AS TEXT)`. `RETURNING seq,
account_id, amount_minor, currency` on the postings INSERT gives back the
postings as written. So the read-back can go, with no extra work in the
INSERTs. The rows the INSERTs return are already in hand.

## Change

`write_postings` loads the `RETURNING` rows and sorts them by `seq`, because
neither backend promises the order of `RETURNING` rows. The transaction
INSERT loads its `RETURNING` row. An empty result is the "key taken
concurrently" branch, which used to be `inserted == 0`. It is unchanged. The
read-back after the writes is removed. There is no migration, no index and no
schema change. SQLite has had `RETURNING` since 3.35, and the bundled
`libsqlite3-sys` 0.38 ships a newer version.

## Measurement

The tool for every row is `pg_stat_statements`. The WAL row also uses
`pg_current_wal_lsn()`.

| | 20k before | 20k after | 200k before | 200k after | 2M before | 2M after |
|:--|--:|--:|--:|--:|--:|--:|
| app statements (not BEGIN/COMMIT/FK) | 2,314 | **1,594** | 2,304 | **1,584** | 2,320 | **1,600** |
| per posted call | 6 | **4** | 6 | **4** | 6 | **4** |
| all top-level statements | 4,067 | 3,347 | 4,057 | 3,337 | 4,073 | 3,353 |
| total buffers | 37,263 | 34,728 | 43,728 | 40,831 | 76,157 | 72,911 |
| total buffers Δ | | −6.8% | | −6.6% | | −4.3% |
| by-key read: calls / buffers | 760 / 1,920 | 400 / 840 | 760 / 2,680 | 400 / 1,240 | 760 / 2,680 | 400 / 1,240 |
| by-key read buffers Δ | | −56% | | −54% | | −54% |
| postings read: calls / buffers | 400 / 1,607 | 40 / 160 | 400 / 1,612 | 40 / 161 | 400 / 2,009 | 40 / 201 |
| transactions INSERT buffers | 3,279 | 3,277 | 3,995 | 3,991 | 4,351 | 4,353 |
| WAL, LSN delta (bytes) | 803,376 | 787,072 | 796,480 | 784,520 | 4,565,720 | 4,576,728 |
| temp blocks written | 0 | 0 | 0 | 0 | 0 | 0 |

The impact floor, stated plainly:

* The transaction-by-key read is 5.2% (20k) and 6.1% (200k) of the
  workload's buffers. Its buffers drop by 54–56%. That clears the
  "≥20% on a statement that is ≥5% of buffers" floor at those two sizes.
  At 2M it is 3.5%, because the one-time `NULL`-balance sums dominate there.
* Total buffers drop by only 4–7%. The real gain is round trips: a posted
  call goes from 6 statements to 4 (−33%) at every size. Each statement is a
  network round trip inside a transaction that holds account row locks. So
  the change also shortens the time those locks are held, for every poster
  on the same accounts.
* The change is free: no index, no write cost, and no more buffers in the
  INSERTs that now return rows.

WAL does not change beyond run-to-run noise (±1.5%). This is not a
write-path claim.

## Equivalence

* **Returned values.** Each run prints every `PostOutcome` that `post`
  returned: key, currency, memo, and every line with its side, account and
  amount. The id and the timestamp are left out, because they are random and
  clock values. The "returned outcomes" sections of `baseline-N.txt` and
  `after-N.txt` are byte-identical at all three sizes.
* **Stored rows.** Each run hashes a dump of the posted transactions (key,
  currency, memo, lines in `seq` order) and of every account's stored
  balance. The SHA-256 digests match before and after: `2765546a58b46eea`
  (20k), `b80bbce88fbd4bc6` (200k), `966118c8a77b0d6c` (2M).
* **`posted_at`.** It comes from the same row through the same
  `CAST(posted_at AS TEXT)` in the same session. Only the statement it is
  read in changed.
* **Existing suites, unchanged.** `money_ledger_postgres` passes 21 of 21,
  run against Postgres 16. It covers concurrent duplicate submits, which
  exercise the "key taken concurrently" branch, the negative-balance race,
  replay equality (`second.transaction().id() == first`), and the
  stored-balance repair. `sqlite_money_ledger`, the golden suite, and the
  `money::ledger` unit tests also pass.

## Findings that are not this change

* **One-time `NULL`-balance sum.** At 2M it is 41.9% of buffers from 20
  calls: 1,600 buffers each, mostly heap reads, through a Bitmap Heap Scan
  on `_autumn_money_postings_account_idx`. Each call sums one pre-#3339
  wallet's history once and stores the result, so the cost is
  O(history) once per account, and that is by design. A covering
  `(account_id) INCLUDE (amount_minor)` index would make it index-only, but
  every posting would pay for that index on write, forever, to speed up a
  path that runs once per account. Not proposed.
* **Deferred `transactions` foreign-key check.** There is one RI query per
  posting line at COMMIT: 953 calls, 23.5% of all top-level statements and
  11–13% of buffers. The same holds for the immediate accounts foreign key,
  which is nested in the INSERT and adds 9–11%. Both are O(lines), and both
  are inherent to row-level foreign keys. Removing them is a schema change,
  and the deferred one is what makes `post` cancellation-safe. Not proposed.
* **`_autumn_money_postings_pkey`** (on the `BIGSERIAL` `id`) is never read
  by any framework query. Every posting line pays for its index insert. The
  natural key `UNIQUE (transaction_id, seq)` already exists. Dropping the
  pkey is a schema decision for a person, with `idx_scan` from production,
  so it is recorded here only.
