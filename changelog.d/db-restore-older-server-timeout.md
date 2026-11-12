### Fixed

- **db:** `autumn db restore` and `autumn db scrub --artifact` no longer fail
  when a `pg_restore` from PostgreSQL 17 or later restores a custom-format
  artifact into an older server. The older server does not know
  `transaction_timeout`, which `pg_restore` turns off. When that is the only
  error, the restore is complete and the CLI prints a note. Any other error
  still fails the restore.
