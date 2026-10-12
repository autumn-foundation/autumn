### Fixed

- **fleet:** four concurrency gaps in `DatabaseFleet` (issue #3228).
  - `delete` and `restore` unlink the sidecar files first and the main file
    last. A database that another process publishes at the path keeps its
    `-wal`.
  - An opener whose entry a delete or restore settled no longer opens the
    file. It claims a new entry and gets `410`, or waits.
  - `close_all` waits for an open in flight and closes it. After it starts,
    every open fails with `503`.
  - At shutdown, the fleet keeps shipping parked replicators until they catch
    up or the flush deadline passes (`server.shutdown_timeout_secs`, bounded by the shutdown wait). It logs
    an `error!` for each database it could not ship and lists them as
    `unshipped` in the `replication:fleet` health details.
