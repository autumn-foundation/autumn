### Fixed

- **migrate check:** the statements in a Postgres `DO` body are now
  classified (#3637). `DO $$ BEGIN DROP TABLE users; END $$;` is
  `destructive`. A `DO` block is `manual-review` when its body holds
  `EXECUTE`, or when the check cannot read the body. A comment in a `DO` or
  function body no longer hides a `standard_conforming_strings` change.
  Dollar-quoted data and quoted names, such as `"RESET ALL"`, no longer
  cause a false `manual-review`.
