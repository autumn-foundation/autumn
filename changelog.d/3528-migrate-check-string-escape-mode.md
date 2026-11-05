### Fixed

- **migrate check:** a Postgres statement that can change
  `standard_conforming_strings` (`SET`, `RESET`, `RESET ALL`, `DISCARD ALL`,
  `set_config`, an `UPDATE` of `pg_settings`, also in a `DO` body) is now `manual-review` (#3528). With the
  setting off, `\'` does not end `'..'` text, and the check possibly misses a
  later risky statement.
- **migrate check:** an `E'..'` string that continues on the next line keeps
  its escape rules (#3528). Before, `E'a'` and then `'\''` on a new line hid
  the statements after it from the check.
