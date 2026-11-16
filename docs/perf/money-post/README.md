# Money ledger `post`: a time-ordered transaction id

Harness: `gen_scripts.py` writes the Postgres SQL that
`autumn/src/money/ledger.rs` `post` issues as pgbench scripts (it fails if a
format string it copies is no longer in the source), and the schema from the
two shipped money migrations. `fixture.sql`, `uuid_v7.sql`, `report.sql`,
`run.sh [T] [txns]`, `explain.sh v4|v7`, `equivalence.sh [T] [txns]`.
Artifacts: `baseline-<txns>.txt` and `explain-before-1000000.txt` (the
baseline commit); `after-<txns>.txt`, `after-v7history-1000000.txt`,
`explain-after-1000000.txt`, `equivalence.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded,
`pg_stat_statements.track = all`, autovacuum off, PG* env set, `pg_waldump`
on the PATH of the database host. `TXN_ID=v4 run.sh 2000 <txns>` is the
baseline, `run.sh 2000 <txns>` the after (the id follows ledger.rs). Then
`explain.sh v4` / `explain.sh v7` on the database `run.sh` left, and
`equivalence.sh 2000 100000`.

## Workload

`money::ledger::post`, the ledger's one write path, in the shape a
marketplace runs it. 2,000 pgbench transactions right after a `CHECKPOINT`,
one client, seeded: 85% a new 3-leg charge (customer wallet, `platform:fees`,
seller), 10% a retry of a charge already in the books (replay), 5% a new
20-leg payout (`payout:pool` and 19 sellers). 2,000 posts is one default
5-minute checkpoint cycle at about 7 posts a second.

Fixture: 220,003 accounts (200,000 customer wallets, 20,000 sellers, 3 house
accounts; 1% of wallets dormant with a NULL stored balance), and a history
of 100,000, 1,000,000 or 3,000,000 transactions over a year: 95% charges,
5% payouts, customers skewed `random()^2`, sellers `random()^3`. Keys are
sha256 hex, as `IdempotencyKey::derive` makes them; history ids are v4, as
every `post` before this change made them. At 3,000,000 the postings table
holds 11.6M rows.

## Profile (3,000,000 history, 2,000 posts, `pg_stat_statements`)

A nested statement (the balance trigger, the account FK check) is counted
inside the top-level statement that fired it, so the top-level rows sum to
the whole.

| statement | calls | % calls | buffers | % buffers | WAL bytes | % WAL |
| --- | --- | --- | --- | --- | --- | --- |
| INSERT postings (3 legs) | 1,685 | 9.0% | 127,823 | 39.8% | 31,878,935 | 40.0% |
| INSERT transaction | 1,795 | 9.6% | 25,337 | 7.9% | 22,303,626 | 28.0% |
| INSERT postings (20 legs) | 110 | 0.6% | 54,975 | 17.1% | 13,254,797 | 16.6% |
| lock accounts (3 ids) `FOR UPDATE` | 1,890 | 10.1% | 29,603 | 9.2% | 9,826,620 | 12.3% |
| deferred FK check on the transaction, at COMMIT | 7,255 | 38.8% | 43,530 | 13.5% | 96,930 | 0.1% |
| by-key SELECT (miss, replay, read-back) | 3,795 | 20.3% | 17,180 | 5.3% | 0 | 0% |
| read-back postings by transaction id | 2,000 | 10.7% | 10,097 | 3.1% | 0 | 0% |

Buffers per post barely move with the history (293,874 / 310,860 /
321,441 at the three sizes): every statement is a B-tree probe, and the work
is the index maintenance a posting needs. The plans are what they should be
(`explain-before-1000000.txt`, and the trigger's nested loop over the
transition table probes `_autumn_money_accounts_pkey` once per account).

WAL is what grows: 20.0 kB, 34.0 kB and 39.8 kB per post. `pg_waldump` over
the workload's WAL, full-page images by relation:

| relation | 100k | 1M | 3M |
| --- | --- | --- | --- |
| `_autumn_money_postings_account_idx` | 711 | 2,720 | 4,177 |
| `_autumn_money_postings_transaction_id_seq_key` | 1,490 | 1,870 | 1,935 |
| `_autumn_money_transactions_idempotency_key_key` | 1,055 | 1,714 | 1,808 |
| `_autumn_money_transactions_pkey` | 798 | 1,659 | 1,774 |
| `_autumn_money_accounts` | 1,377 | 1,360 | 1,397 |
| `_autumn_money_accounts_pkey` | 236 | 415 | 448 |

## Mechanism

The first change to a page after a checkpoint writes the whole 8 kB page to
WAL. `post` names each transaction `Uuid::new_v4()`, a random string. Two
indexes are keyed by it: `_autumn_money_transactions_pkey` (`id`) and
`_autumn_money_postings_transaction_id_seq_key` (`UNIQUE (transaction_id,
seq)`). A random key lands on a random leaf, so while the leaves outnumber
the posts in a checkpoint cycle, nearly every post dirties a fresh leaf of
each and logs it whole: 1,935 + 1,774 of 2,000 posts at 3M. The history only
grows, so this only gets closer to one image per index per post.

A v7 id starts with its 48-bit millisecond timestamp, so each id sorts after
the previous one and both indexes take their inserts on the same few leaves.

The change: `Uuid::now_v7()` for `Uuid::new_v4()` in `post`, and uuid's
`v7` feature (no new crate; Cargo.lock is unchanged). No schema change, no
migration. The id stays an opaque 36-character UUID string in the same
`TEXT` column. The v4 ids already in the books stay as they are; the new
ids gather in one region of the key space among them (`01…` for this
decade) instead of spreading over all of it.

## Measurement (`pg_stat_wal`, `pg_stat_statements`, `pg_waldump`; 2,000 posts)

| history | WAL bytes before | after | delta | FPIs before | after | delta | buffers before | after |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | 39,975,263 | 27,044,887 | −32.3% | 5,671 | 3,391 | −40.2% | 293,893 | 293,881 |
| 1,000,000 | 67,977,070 | 47,421,201 | −30.2% | 9,742 | 6,221 | −36.1% | 310,860 | 310,900 |
| 3,000,000 | 79,668,322 | 58,058,237 | −27.1% | 11,543 | 7,842 | −32.1% | 321,441 | 321,442 |

Full-page images on the two id-keyed indexes go from 2,288 / 3,529 / 3,709
to 8 / 8 / 8, flat in the history. Per statement at 3M: the transaction
INSERT 12,425 → 6,633 WAL bytes a call (−46.6%), the 3-leg postings INSERT
18,919 → 12,755 (−32.6%). Statement counts, rows and buffers are unchanged
(no plan changes); the INSERTs' cache misses fall (postings 446 → 10 blocks
read, transaction 2,054 → 33), since the hot leaves stay in cache. Temp
blocks are 0 throughout.

A second after-run at 1M and 3M reproduced the FPI counts exactly (6,221
and 7,842). `after-v7history-1000000.txt` is a deployment that only ever
ran a v7 `post`: 6,219 FPIs, 46,956,939 WAL bytes. What is left is random by
nature: which customer pays (`_autumn_money_accounts`, `account_idx`) and
the idempotency key, a hash.

## Equivalence

`equivalence.sh 2000 100000` runs the same seeded workload with v4 and v7
ids on the same fixture and compares, with each id replaced by the
idempotency key that owns it: every account row with its stored balance,
every transaction (key, request hash, currency, memo), every posting (key,
seq, account, amount, currency), the trial balance per currency, and the
rows of `load_transaction`'s postings query in its `ORDER BY seq`. All
hashes match (`equivalence.txt`). No orphan postings (the deferred FK held
at every COMMIT), ids unique, every id a canonical UUID: 1,795 version 7
in the v7 run. Nothing in the framework orders by the transaction id or
reads its version; the id is compared for equality only.

The existing suites pass unchanged: `money_ledger_postgres` (Docker, 21 tests) and the
60 `money::` lib tests.

## Not changed, for a human decision

* `_autumn_money_postings_account_idx` is the largest source of full-page
  images at 3M (4,177) and was scanned 22 times in 2,000 posts, all for
  dormant NULL balances. Since the stored balance (#3339) it serves only
  that fallback sum and `balance` on a NULL row. Without it, the first post
  to each dormant account would scan the postings table. Backfilling the
  NULL balances would let it go, but dropping an index needs a decision.
* `post` reads the transaction back after writing it (two of the nine statements a
  new charge runs); #3722 replaces that with `RETURNING` and is not repeated here.
