# Notifications: partial index over unread rows

Harness: `fixture.sql`, `workload.sql`, `run.sh <history-rows> [with_index]`, `write_cost.sql`.
Artifacts: `baseline-*.txt` (commit 5ddeb75), `after-*.txt`, `write_cost.txt`.
Reproduce: Postgres 16 with `pg_stat_statements` preloaded, PG* env set, then
`run.sh 200000` and `run.sh 200000 with_index`; `cd` here and run `psql -v hist=200000 -v with_index=true -f write_cost.sql`.

Fixture: 500,000 ordinary rows (20,000 recipients, 25 each, 92% read) plus one heavy
recipient with N rows, 99.9% read, 20 unread. The workload is 50 page views for the
heavy recipient and 50 for a median one: unread badge twice (badge + feed total) and the unread page.
The heavy/median mix is by construction; a median recipient costs 28 buffers per call either way.

| heavy history | page buffers before | after | count buffers before | after |
| --- | --- | --- | --- | --- |
| 20,000 | 31,803 | 1,400 | 28,653 | 403 |
| 200,000 | 303,503 | 1,400 | 272,603 | 403 |
| 1,000,000 | 1,511,203 | 1,400 | 1,942,203 | 453 |

(100 calls each; 1M-row `read` blocks 1.15M-1.37M before, 9 and 4 after.)
Plan: the page was a backward primary-key scan removing 189,981 rows by filter; the count
was a bitmap heap scan removing 66,660 rows per worker. Both become an index (only) scan
on `idx_notifications_unread`. Ordered unread-id md5 per recipient is identical at all three sizes.

Write cost (100,000 inserts, all unread): WAL 37,873,008 -> 48,407,608 bytes (+27.8%, ~105 B/row).
Mark-read UPDATE WAL unchanged (150,757,840 vs 150,754,760). Index size 1.6 MB vs 11 MB for the shipped one at 1M rows.
`CREATE INDEX CONCURRENTLY` on the 1M-row fixture: 223 ms; `DROP INDEX CONCURRENTLY`: 3 ms.
