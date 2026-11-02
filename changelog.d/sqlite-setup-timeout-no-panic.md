### Fixed

- **db (SQLite):** a connection setup that passes the pool's create limit
  (`connect_timeout_secs`) now gives a pool timeout error. Before, on a
  current-thread Tokio runtime, it panicked with "can call blocking only when
  running on the multi-threaded runtime", and a fleet `open` failed with that
  panic text. The setup now runs in its own task, so the timeout drops only
  the task handle.
