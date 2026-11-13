### Changed

- **jobs:** [no-plugin] a unique job with a TTL window (`#[job(unique, ttl = …)]`) no
  longer reads the whole `autumn_jobs` table, finished history included, on
  every Postgres enqueue. The new migration
  `20261112170000_add_jobs_unique_key_enqueued_index` adds a partial index on
  `(name, unique_key, enqueued_at)` for rows with a unique key; the dedup check
  reads 7-8 buffers instead of the whole heap (36,000+ at 1M rows). The build
  takes a SHARE lock on `autumn_jobs` (about 0.5 s at 1M rows), so enqueue,
  claim and ack writes wait for it.
