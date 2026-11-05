# Notifications: partial index on unread rows

Artifacts: `fixture.sql`, `probe.sql`, `workload.sql`, `baseline.txt` and
`pgss_baseline.txt` (commit 2b03413), `after.txt`, `pgss_after.txt`,
`write_cost.sql`, `write_cost.txt`, `eq_before.txt`, `eq_after.txt`.

Fixture: 2,000,000 rows, recipient skew `1 + floor(20000 * r^4)`, 1.5% unread.
Recipient 1 has 168,382 rows (2,461 unread); 126 has 1,092 (15); 3926 has 59 (2).
Postgres 16, autovacuum off, `VACUUM ANALYZE` after load.

Reproduce: `psql -f fixture.sql`, then `pg_stat_statements_reset()`, run
`workload.sql` with `-v rid=1|126|3926` five times each, then `snapshot.sql`.
Add the index with
`CREATE INDEX idx_notifications_unread ON notifications (tenant_id, recipient_id) WHERE read_at IS NULL`
and repeat.

Mechanism: the only index covers `(tenant_id, recipient_id)`, so
`read_at IS NULL` is a heap filter. The planner reads every row of the recipient
(168,382 index entries, 55k heap rows removed per worker) to return 2,461.

| statement (15 calls, `pg_stat_statements`) | buffers before | after |
| --- | --- | --- |
| `SELECT COUNT(*)` unread | 201,545 | 110 |
| unread page, `LIMIT 20` | 201,710 | 12,160 |
| `idx_tup_fetch` on notifications | 1,695,330 | 12,390 |

Per call, heavy recipient (`EXPLAIN (ANALYZE, BUFFERS)`): count 39,161 -> 6;
page 39,191 -> 2,399 (the page still sorts the 2,461 unread rows; keyset on
`created_at` would remove that and is an API change, not done here).
Mid recipient: count 1,078 -> 4. Light recipient: unchanged order of magnitude.
These two statements are 100% of this harness's buffers: the harness is the feed.

Write cost (`write_cost.txt`, 200k inserts, all born unread): WAL 64.7 MB ->
80.9 MB (+25%). Marking 100k read: WAL unchanged. Index size on the fixture:
528 kB (partial, 1.5% of rows). The cost is real: every notification is
unread at insert.

Equivalence: unread count and first 20 ids (md5, `created_at DESC, id DESC`)
identical before/after for recipients 1, 126, 3926 and one with no rows.
`idx_scan` on the new index: 30 of 30 calls.
Migration: `CREATE INDEX` without `CONCURRENTLY` takes `SHARE` (blocks writes)
for the build; the scaffold runs it on an empty new table. Existing apps should
use `CREATE INDEX CONCURRENTLY`.
