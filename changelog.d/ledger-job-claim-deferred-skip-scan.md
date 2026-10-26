### Changed

- **jobs:** the durable Postgres job claim no longer scans the parked backlog while
  deferrable job names are deferred by the cost signal (issue #1720). A new
  `idx_autumn_jobs_queue_name_ready` index (migration
  `20261026170000_add_jobs_queue_name_ready_index`; plain `CREATE INDEX`, a brief
  `SHARE` lock while it builds) lets the claim cost O(distinct ready job names)
  buffers per poll instead of O(parked backlog). The claim without deferral is unchanged.
