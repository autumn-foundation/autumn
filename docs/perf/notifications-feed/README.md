# Notifications: feed index in the default sort order

Decision: [ADR 0045](../../adr/0045-notifications-feed-index.md).

The harness is `fixture.sql`, `indexes.sql`, `workload.sql`, `run.sh` and `write_cost.sql`.
The artifacts are `<variant>.txt` (one for each variant) and `write_cost.txt`.

To reproduce, use Postgres with `pg_stat_statements` preloaded, and set the PG* env vars:

```sh
./run.sh shipped fixture > shipped.txt
for v in feed feed_created wide; do ./run.sh "$v" > "$v.txt"; done
for v in shipped feed feed_created wide; do
  psql -v variant="$v" -f indexes.sql && psql -v variant="$v" -f write_cost.sql
done > write_cost.txt
```

Each `<variant>.txt` records the server version (Postgres 16.15), the row count of each
recipient, and the plans.

The fixture has 2,000,000 rows and 50,000 recipients, skewed (`floor(50000 * random()^3)`),
with about 15% unread. Recipient 1 has 49,209 rows (heavy). Recipient 58 has 1,092 (mid).
Recipient 6373 has 40 (typical). The skew is an assumption. It is not a measurement of a real
deployment.

Variants (all keep `idx_notifications_unread`):

| variant | indexes on `(tenant_id, recipient_id, …)` |
| --- | --- |
| `shipped` | `()`, the older generator output |
| `feed` | `(id)` in place of `()`, the current generator output |
| `feed_created` | `feed` plus `(created_at DESC, id DESC)`, the opt-in index |
| `wide` | `shipped` plus `(created_at DESC, id DESC)`, the #3616 candidate |

Buffers per call (`pg_stat_statements`, 20 calls each). A page is 20 rows. The default sort is
`id DESC`.

| recipient | statement | shipped | feed | feed_created | wide |
| --- | --- | ---: | ---: | ---: | ---: |
| heavy | total count | 47 | 247 | 247 | 47 |
| heavy | page 1 | 26 | 16 | 16 | 26 |
| heavy | page 5 | 117 | 62 | 62 | 117 |
| heavy | `kind` page | 106 | 55 | 55 | 106 |
| heavy | `sort=created_at` page | 28,129 | 28,329 | 23 | 23 |
| mid | page 1 | 1,083 | 23 | 23 | 1,356 |
| mid | page 5 | 1,083 | 101 | 101 | 1,083 |
| mid | `kind` page | 1,083 | 98 | 98 | 1,083 |
| mid | `sort=created_at` page | 1,084 | 1,089 | 23 | 23 |
| typical | page 1 | 43 | 23 | 23 | 43 |
| any | unread count, unread page | same in all variants | | | |

`shipped` and `wide` use the same plan for the mid page 1. The 1,356 includes a first call
that read more pages.

Plans: with `shipped`, the mid page scans all 1,092 index entries and their rows, and then does
a top-N sort. The heavy page walks the primary key backward and removes 851 rows by filter. It
is cheap only because the heavy recipient owns 2.5% of all rows. With `feed`, every page is an
`Index Scan Backward using idx_notifications_feed`, with no sort. The heavy total count is an
index-only scan in all variants. With `feed` it reads 250 buffers, not 50. `id` is unique, so
B-tree deduplication cannot merge entries.

Same rows: the md5 of the first 100 ids for each recipient, in `id DESC` order and in
`created_at DESC, id DESC` order, is the same in all four variants.

Size at 2M rows (`pg_size_pretty`, MiB): `(tenant_id, recipient_id)` 18, feed 77, `created_at`
index 95.

Write cost (`write_cost.txt`): WAL for 100,000 inserts, then `ROLLBACK`, so each variant gets
the same rows. Cold is the first batch after `CHECKPOINT`, with full-page images. Warm is the
second batch. Units are MB (10^6 bytes).

| variant | cold | warm |
| --- | ---: | ---: |
| shipped | 66.8 | 43.3 |
| feed | 113.6 (+70%) | 56.1 (+30%) |
| feed_created | 193.2 (+189%) | 83.4 (+93%) |
| wide | 146.3 (+119%) | 70.6 (+63%) |
