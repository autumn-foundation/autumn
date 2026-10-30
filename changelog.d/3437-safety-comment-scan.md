### Fixed

- **cli:** `autumn migrate check` no longer misses statements after a `--`
  comment that holds `/*` (issue #3437). A `--` inside a quoted string, such
  as `DEFAULT '--'`, no longer hides the rest of the line. `migrate down`
  now sees the statements in such a `down.sql`.
