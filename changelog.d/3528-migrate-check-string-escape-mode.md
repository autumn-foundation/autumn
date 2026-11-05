### Fixed

- **migrate check:** a statement that can change Postgres
  `standard_conforming_strings` (`SET`, `RESET`, `set_config`) is now
  `manual-review` (#3528). When the setting is off, `\'` is a quote in `'..'`
  text, and the check can read the statements after it incorrectly. New tests
  make sure that `E'\''`, `a$$` and `"a'b"` do not hide a later statement.
