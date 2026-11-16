### Fixed

- **cli:** `autumn task <name>` now runs the task when the shell or CI
  environment exports `AUTUMN_LIST_TASKS=1` (issue #3733). In earlier
  versions, the app read the inherited value first and listed the tasks. The
  task did not run.
