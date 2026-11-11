### Performance

- **jobs:** the Postgres single-queue claim reads the running counts for
  concurrency limits once per claim, not once per row it scans. A saturated
  `#[job(concurrency = …)]` group's backlog is the oldest ready work, so every
  claim used to re-count the running set for each of its rows: with 50,000
  blocked rows a claim read 101,823 buffers, and now it reads 1,825. Claims
  that never reach a concurrency-limited row are unchanged. No migration.
  See `docs/perf/job-claim-concurrency/`. [no-plugin]
