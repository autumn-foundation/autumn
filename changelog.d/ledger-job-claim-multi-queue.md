### Fixed

- **jobs:** the Postgres claim for workers with `[jobs] queues` priority config
  (two or more queues) probes the head of each queue through
  `idx_autumn_jobs_queue_ready` instead of sorting every ready job in all polled
  queues. Claim cost no longer grows with the backlog (about 87k buffers and a
  disk sort spill at 500k ready jobs, 44 buffers now). Claim order is unchanged.
