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
  `BookmarkRepository` gains `mark_dead_many(ids: &[i64])`: one
  `UPDATE ... WHERE id = ANY($1) AND alive = true` for the whole
  shard's dead-id batch. `process_shard` now collects dead ids during
  the probe loop and issues one write after it instead of writing
  inline per probe. Profiled against a 10,000-bookmark fixture (780
  rotted links spread across all 16 shards): the write was 80.4% of
  the run's `pg_stat_statements` buffers before and after — buffer
  count was never the defect, statement count was, and it drops from
  one per dead link to one per shard (a fixed ceiling of 16). No
  schema change.
