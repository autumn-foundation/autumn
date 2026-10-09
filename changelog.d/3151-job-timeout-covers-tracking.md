### Fixed

- **jobs:** a stalled tracking store no longer holds a worker (issue #3151).
  The job timeout now covers `mark_running`. The final `complete` or `fail`
  call is capped at 5 seconds.
