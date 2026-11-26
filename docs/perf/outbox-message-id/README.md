# Outbox: a time-ordered message id

Harness: `gen_scripts.py` writes the Postgres SQL that `autumn/src/outbox.rs`
ships (`SCHEMA_SQL`, `INSERT_SQL`, `claim_sql`, `INBOX_INSERT_SQL`,
`MARK_SENT_SQL`) as a psql workload. It fails if a statement it copies is no
longer in the source. `fixture.sql`, `uuid_v7.sql`, `report.sql`,
`run.sh [messages] [history]`, `explain.sh`, `equivalence.sh`.
Artifacts: `baseline-<history>.txt` (the baseline commit),
`after-<history>.txt`, `explain-1000000.txt`, `equivalence.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded,
`pg_stat_statements.track = all`, autovacuum off, PG* env set, `pg_waldump`
on the PATH of the database host. `MSG_ID=v4 run.sh 20000 <history>` is the
baseline, `run.sh 20000 <history>` the after (the id follows outbox.rs).
Then `explain.sh` on the database `run.sh` left, and
`equivalence.sh 20000 100000`.

## Workload

The outbox cycle as the framework writers make it (`docs/guide/outbox.md`,
"Framework writers"): each message has its own id as the aggregate. 20,000
messages right after a `CHECKPOINT`, in rounds of 100: 100 writes, each in its
own transaction (40% `autumn.event`, 30% `autumn.job`, 15% `autumn.mail`, 15%
`autumn.webhook`, 150-300 byte JSON payloads), one relay claim of
`batch_size` 100, then for each claimed row in `seq` order `Inbox::seen` for
an `autumn.event` row (a listener that dedups) and `MARK_SENT`. The relay
keeps up, so the backlog stays under one batch.

Fixture: 100,000, 1,000,000 or 3,000,000 messages already sent, spread over
the default 7-day `retention_ms`, 40% of them in the inbox. Ids are
`gen_random_uuid()`, as `Entropy::uuid_v4` made every one before this change.
`VACUUM (ANALYZE)` after the load. At 3,000,000: heap 1,236 MB,
`autumn_outbox_id_key` 222 MB, `autumn_inbox_pkey` 102 MB.

## Profile (1,000,000 history, message ids v4, `pg_stat_statements`)

| statement | calls | % calls | buffers | % buffers | WAL bytes | % WAL | FPIs |
| --- | --- | --- | --- | --- | --- | --- | --- |
| relay claim (`claim_sql`) | 200 | 0.4% | 557,138 | 45.7% | 19,462,435 | 16.8% | 48 |
| `MARK_SENT_SQL` | 20,000 | 41.5% | 338,511 | 27.8% | 7,788,615 | 6.7% | 2 |
| `INSERT_SQL` | 20,000 | 41.5% | 191,949 | 15.7% | 65,032,791 | 56.0% | 8,171 |
| `INBOX_INSERT_SQL` | 8,000 | 16.6% | 72,801 | 6.0% | 23,803,264 | 20.5% | 3,698 |

(The rest of the 100% is the harness's two `count(*)` checks after the
workload.)

The two inserts are 76.5% of the WAL and hold 11,869 of the 11,919 full-page
images. Buffers per message barely move with the history (60.5 / 61.0 / 70.4
at the three sizes). WAL per message does: 2,783 / 5,805 / 8,327 bytes.

`pg_waldump` over the workload's WAL, full-page images by relation:

| history | `autumn_outbox_id_key` | `autumn_inbox_pkey` | everything else |
| --- | --- | --- | --- |
| 100,000 | 964 | 444 | 8 |
| 1,000,000 | 8,215 | 3,695 | 9 |
| 3,000,000 | 14,469 | 6,019 | 17 |

## Mechanism

`insert_message` names each message `entropy.uuid_v4()`. Two B-tree indexes
are keyed by that id: the unique index on `autumn_outbox.id`, and
`autumn_inbox`'s primary key `(consumer, message_id)`. A random key lands each
insert on a random leaf. After a checkpoint, the first change to a page logs
the whole 8 kB page. With 20,000 random inserts into a 28,427-leaf index, most
inserts touch a leaf no earlier insert in the cycle touched, so most of them
pay a full page. The bigger the index, the closer that gets to one FPI per
insert. `seq`, `dispatched_at` and `seen_at` are append-only, and their indexes
take 2 FPIs each per cycle.

`idx_autumn_outbox_pending (aggregate, seq)` is keyed by the same id, because
the aggregate is the id. It holds only pending rows, so it stays one page and
takes no FPIs. `MARK_SENT` is not HOT (it sets `dispatched_at`, which
`idx_autumn_outbox_dispatched` and two partial predicates read), so it adds an
`id` index entry too. That entry lands on the leaf the insert dirtied a moment
before, so it adds no FPI.

## Change

`insert_message` makes the id with `entropy.uuid_v7(now_ms)`, through the same
seam, with the millisecond from the injected `ClockSource`. A v7 id starts with
a 48-bit Unix millisecond timestamp, so new ids sort after the old ones and
inserts land at the right edge of both indexes. No schema change, no
migration, no lock. The column stays `TEXT`, and the id stays a canonical UUID
string. Ids already stored, and ids that `outbox::remote` receives from a
producer (`insert_message_with_id`), are unchanged. Webhook delivery ids are a
hash of the message id, so they stay random.

`uuid_v7.sql` builds the same layout in SQL for the harness (Postgres 16 has
no `uuidv7()`).

## Measurement

`run.sh 20000 <history>`, the same fixture and the same server. Totals are
`pg_stat_statements` (the four framework statements plus the harness's two
checks), FPIs and WAL also from `pg_stat_wal`.

| history | buffers before | after | FPIs before | after | WAL bytes before | after | WAL Δ |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | 1,209,795 | 1,184,735 | 1,416 | 18 | 55,655,417 | 47,818,244 | −14.1% |
| 1,000,000 | 1,219,622 | 1,196,744 | 11,919 | 19 | 116,090,725 | 47,524,072 | −59.1% |
| 3,000,000 | 1,407,577 | 1,388,324 | 20,505 | 27 | 166,534,777 | 47,936,448 | −71.2% |

WAL per message after: 2,391 / 2,376 / 2,397 bytes. It no longer grows with
the history. At 3,000,000, per statement:

| statement | WAL/call before | after | FPIs before | after |
| --- | --- | --- | --- | --- |
| `INSERT_SQL` | 5,025 | 863 | 14,323 | 16 |
| `INBOX_INSERT_SQL` | 4,738 | 394 | 6,022 | 8 |
| `MARK_SENT_SQL` | 389 | 391 | 2 | 2 |
| relay claim | 101,709 | 98,513 | 158 | 1 |

Statement counts are unchanged (48,205 in both). Buffers drop 1.4-2.1%, which
is not a claim. No temp blocks either way. The plans are the same for both ids
(`explain-1000000.txt`). The WAL line differs: right after a checkpoint, the
second and third inbox inserts write 4,696-5,236 bytes with a v4 id (one FPI)
and 327 bytes with a v7 id (none).

## Equivalence

`equivalence.sh 20000 100000` runs the same workload with v4 and v7 ids on
the same fixture. With each id replaced by its row's `seq`, the digest of
every outbox row the workload wrote (`seq`, aggregate-is-id, topic, payload,
times, attempts, claim and lease columns, error, dead-letter time) and the
digest of every inbox row are identical. In both runs, all 20,000 ids are
distinct, RFC 4122, canonical text, and of the expected version. Every v7 id's
timestamp is its write time. No inbox row lacks its outbox row.

Nothing in the framework orders or compares message ids except for equality.
The relay orders by `seq`, the dead-letter read orders by `seq`, the inbox
dedups on equality, and requeue looks the id up by equality.

## Write cost

No index added. A v7 id puts the time of the write in the id. The message's
`created_at` already carries it, and the relay hands both to the handler.

## Not done here

The relay claim is 45.7% of the buffers. Its `NOT EXISTS` runs as a nested-loop
anti join over `idx_autumn_outbox_claim`. The estimate is 1 row and the actual
is 100, because the table was analyzed with an empty backlog. So each claim
removes 10,000 rows by join filter, which is quadratic in `batch_size`. That
needs its own baseline: the plan depends on statistics taken while the
backlog was empty.
