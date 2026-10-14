### Fixed

- **db:** a SQLite fleet database now stops its commit-hook worker, and waits
  for it, before its pool closes (issue #3231). A hook that runs during an
  idle close, a `max_open` eviction or a shutdown is acked, not replayed.
  `start_repository_commit_hook_worker` now returns `Option<JoinHandle<()>>`.
  New: `FleetDatabase::register_worker`.
