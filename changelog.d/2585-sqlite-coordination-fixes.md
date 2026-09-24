### Fixed

- **sqlite scheduler:** `SqliteLeaseSchedulerCoordinator::new` clamps a sub-second lease TTL to the 1s floor (issue #2585): a zero TTL previously wrote `expires_at == now_ms`, which the reap-first predicate treated as already dead, so a second coordinator could take the identical tick while the first task was still running.
- **sqlite lock:** `Lock::lock_timeout` now bounds each poll's pool checkout by the remaining deadline (issue #2585): under pool pressure a small budget previously ran into deadpool's own seconds-long wait and surfaced `PoolUnavailable` instead of `Timeout`.
