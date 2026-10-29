### Fixed

- **jobs:** a Postgres worker with two or more `[jobs] queues` no longer locks
  the head job of a lower-priority queue while it claims a job (issue #3182).
  Before, a second worker could skip that job and take a lower-priority job.
