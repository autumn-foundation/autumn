### Fixed

- **derivations:** on SQLite, a `sum` derivation's backfill, `recompute`,
  drift probe and bulk delete no longer fail with `integer overflow` when a
  running total leaves `i64` but the total is in range, for example weights
  `i64::MAX, 1, -1` (issue #2663). The repair paths now accept the same rows
  as the delta paths. A total outside `i64` is still an error.
