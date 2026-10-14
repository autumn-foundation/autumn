### Fixed

- **retention:** a failed audit write sets the dataset `error` (issue #2396).
  `autumn db retention --purge` exits non-zero. The `autumn-retention-sweep`
  task fails when any dataset reports an error. It runs all datasets first.
  `/actuator/tasks` and the task failure alert show the error.
