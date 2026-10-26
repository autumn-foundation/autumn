### Fixed

- **derivations:** on SQLite, a `sum` derivation's backfill, `recompute`,
  drift probe and bulk delete no longer fail with `integer overflow` when the
  total is in range (issue #2663). Before, a running total outside `i64`
  failed them, for example weights `i64::MAX, 1, -1`. A bulk delete also
  lands when only the deleted rows' sum is outside `i64`. The delta paths
  already accepted these rows. A total outside `i64` is still an error.
