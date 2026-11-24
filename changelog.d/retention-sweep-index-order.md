### Performance

- **retention:** each batch of a Postgres retention sweep of job history,
  job tracking records and commit hooks now reads the stale rows in the order
  of the index that serves the dataset's predicate. Before, each batch was a
  sequential scan from the first heap page, and it re-read the dead rows of
  every batch before it, so a pass cost the square of its rows. A pass over
  1,000,000 rows reads 45% fewer buffers on `autumn_jobs`, 77% fewer on
  `autumn_job_tracking` and 62% fewer on `autumn_repository_commit_hooks`. A
  full pass deletes the same rows. A pass that stops at the batch cap now
  deletes the oldest stale rows first.
