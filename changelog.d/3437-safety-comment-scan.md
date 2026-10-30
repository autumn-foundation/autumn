### Fixed

- **cli:** `autumn migrate check` no longer misses statements after a `--`
  comment that contains `/*` (issue #3437). A `--` inside a quoted string,
  such as `DEFAULT '--'`, no longer hides a later `DROP COLUMN`.
  `autumn migrate down` no longer treats such a `down.sql` as empty.
