### Fixed

- **schema:** `autumn schema diff` on SQLite now writes the decimal `CHECK` that
  `autumn generate` writes (issue #2598). Before, a declarative `CREATE TABLE`
  had no `CHECK`, and a table rebuild for an unrelated change removed it
  silently. The two lanes use one `CHECK` builder, so the SQL is the same byte
  for byte. The model parser now reads `#[decimal_shape(precision, scale)]`.
  Before, it read each decimal as `{12, 2}`. A bad `#[decimal_shape]` skips the
  column with a diagnostic. On Postgres, a snapshot written before this fix
  can hold `NUMERIC(12,2)` for a column the model declares with a different
  shape. Run `autumn schema snapshot` again to correct it.
