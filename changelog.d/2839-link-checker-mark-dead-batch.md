### Performance

- **🗃️ Ledger: batch `examples/bookmarks-distributed`'s link-checker
  dead-link writes into one `UPDATE` per shard (statement calls
  780→16 per run):** the hourly `#[scheduled]` link-checker job
  (`tasks::check_links` → `process_shard`) called
  `BookmarkRepository::mark_dead(id)` — a fresh pooled-connection
  checkout plus a single-row `UPDATE bookmarks SET alive = false WHERE
  id = $1` — once per dead link found, sequentially, inside the probe
  loop; the statement count scaled with how many of a shard's alive
  bookmarks rotted since the last run, not with anything fixed.
  `BookmarkRepository` gains `mark_dead_many(dead: &[(i64, String)])`:
  one batched `UPDATE ... FROM unnest($1::bigint[], $2::text[])`
  matching each row on `(id, url)`, not `id` alone — a bookmark
  repaired concurrently (`PUT /api/bookmarks/{id}` never touches
  `alive`) between its old URL being probed and the batch write
  running would otherwise still match `id = ANY($1) AND alive = true`
  and get permanently marked dead on the strength of a URL that was
  never probed, since nothing in this app ever sets `alive` back to
  `true`. `process_shard` now collects dead `(id, url)` pairs during
  the probe loop and issues one write after it instead of writing
  inline per probe. Profiled against a 10,000-bookmark fixture (780
  rotted links spread across all 16 shards): the write was 80.4% of
  the run's `pg_stat_statements` buffers before and after — buffer
  count was never the defect, statement count was, and it drops from
  one per dead link to one per shard (a fixed ceiling of 16). No
  schema change.
