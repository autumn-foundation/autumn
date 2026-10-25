### Fixed

- **schema:** `autumn schema diff` on SQLite now writes the decimal `CHECK` that
  `autumn generate` writes (issue #2598). Before, a declarative `CREATE TABLE`
  had no `CHECK`. A table rebuild for a different change removed the `CHECK`
  silently. The two lanes use one `CHECK` builder, so the SQL is the same byte
  for byte.
- **schema:** the model parser now reads
  `#[decimal_shape(precision = p, scale = s)]` (issue #2598). Before, it read
  each decimal as `{12, 2}`. On Postgres, `schema diff --dev-url` then
  reported a false type change for a `decimal{10,2}` field. A bad
  `#[decimal_shape]` skips the column with a diagnostic. A model from
  `autumn generate` 0.8.0 has no `#[decimal_shape]`. Add it with the declared
  precision and scale, or the declarative lane reads `{12, 2}`.
- **schema:** on SQLite, a change of decimal precision or scale is now a type
  change (issue #2598). Before, `schema diff` found no change, and the old
  `CHECK` stayed.
