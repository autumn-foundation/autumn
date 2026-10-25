### Fixed

- **schema:** `autumn schema diff` on SQLite now writes the decimal `CHECK` that
  `autumn generate` writes (issue #2598). Before, a declarative `CREATE TABLE`
  had no `CHECK`. A table rebuild for a different change removed the `CHECK`
  silently. The two lanes use one `CHECK` builder, so the SQL is the same byte
  for byte. The model parser now reads
  `#[decimal_shape(precision = p, scale = s)]`. Before, it read each decimal as
  `{12, 2}`. A bad `#[decimal_shape]` skips the column with a diagnostic. On
  Postgres, an old snapshot can hold `NUMERIC(12,2)` for a column with a
  different declared shape. Then `schema diff` plans a type change. Run
  `autumn schema snapshot` again to correct the snapshot.
