### Fixed

- **fleet:** four concurrency gaps in `DatabaseFleet` (issue #3228).
  - `delete` and `restore` unlink the sidecar files first and the main file
    last. A database that another process publishes at the path keeps its
    `-wal`.
  - Creating, restoring and deleting a database take a lock file in
    `<root>/.fleet-locks/`. Under it, the fleet clears the stray `-wal` of an
    absent database before it publishes a new one, so a crash cannot replay
    old writes onto a new database, and a live `-wal` is never removed.
  - `delete` and `restore` install the drain first. A late opener gets `410`
    or waits, and cannot create the file. An open already running ends first.
  - After `close_all` has drained the running operations, every open fails
    with `503`.
  - At shutdown, the fleet keeps shipping parked replicators until they catch
    up or the flush deadline passes (`server.shutdown_timeout_secs`, bounded
    by the shutdown wait). It logs an `error!` for each database it could not
    ship and lists them as `unshipped` in the `replication:fleet` health
    details.
