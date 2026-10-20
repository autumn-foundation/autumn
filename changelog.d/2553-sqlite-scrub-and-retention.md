### Added

- **db:** `autumn db scrub` runs against a `sqlite://` database and a `.sqlite`
  backup artifact (issue #2553). It uses the Postgres classifier, so an
  unclassified column still aborts the run. The production guard, `--check` and
  `--dry-run` work the same. After the commit, `VACUUM` and a WAL truncate remove
  the old values from the file. `--sample` is Postgres-only and is refused.
- **retention:** `autumn db retention` sweeps `job_history`, `commit_hooks` and
  `experiment_assignments` in a SQLite file (issue #2553). The report has the
  same shape as on Postgres.
