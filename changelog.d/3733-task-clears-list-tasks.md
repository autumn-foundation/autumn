### Fixed

- **cli:** `autumn task <name>` now runs the task when the shell exports
  `AUTUMN_LIST_TASKS=1` (issue #3733). Before, the app read the inherited
  value first and listed the tasks. The task did not run.
