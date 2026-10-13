### Fixed

- **db:** processes that share a SQLite fleet root now elect one replicator per
  database (issue #3230). The owner holds an OS lock on `<db>.replicator.lock`.
  Other processes wait and take over, with a new generation, when the owner
  exits. A restore no longer misses commits. `replication:fleet` health reports
  `unowned` databases.
