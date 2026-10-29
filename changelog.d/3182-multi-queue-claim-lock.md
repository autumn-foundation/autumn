### Fixed

- **jobs:** a Postgres worker with two or more `[jobs] queues` no longer locks
  the first job of a lower-priority queue during a claim (issue #3182). Before
  this fix, a second worker could skip that job and take a job of lower
  priority.
