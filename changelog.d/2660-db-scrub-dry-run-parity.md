### Fixed

- **autumn-cli:** `db scrub --dry-run` printed the sample's SQL without the
  postcondition half the executed command runs: the generated script no longer
  re-proves the sample's row counts and foreign-key integrity after the
  materialized-view refreshes, while the real command runs
  `verify_sample_survived_refreshes` there and rolls back on a mismatch.
  The script now captures every sampled table's post-sample count into an
  `ON COMMIT DROP` temp table (a collision-resistant `pg_temp` name) at the same point the
  executor takes its own `after` snapshot, and asserts those counts plus all
  the sample's FK checks after the refreshes. A refresh whose query calls a
  function that INSERTs now aborts the pasted script the way the real command
  aborts the run (issue #2660).
- **autumn-cli:** `db scrub --dry-run` unsets `PGHOSTADDR` and `PGSERVICE`
  before every generated `\connect`. The generation-time guard only covered
  the generating session; a script pasted where either variable is set
  connected to a different server than the reconnect proof asserted, while
  psql still reported the conninfo's own host (issue #2660).
- **autumn-cli:** `db scrub --dry-run` no longer prints PostgreSQL 18's
  `oauth_client_secret` into generated scripts. The conninfo filter is now an
  allowlist of known-non-secret libpq parameters: credentials are stripped
  and anything unrecognised refuses the whole target (`UnprintableTarget`)
  rather than being printed or dropped — a denylist cannot know the next
  PostgreSQL release (issue #2660).
