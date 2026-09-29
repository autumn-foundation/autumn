### Fixed

- `autumn generate scaffold --import`: the columns an uploaded file must carry
  are now derived at request time from the live `CsvSchema::csv_columns()`,
  kept only where `{Pascal}Form` can set the column (`csv_required_columns()`),
  instead of a list baked in at generation time. Dropping a column from the
  export's schema can no longer leave a stale requirement behind that rejects
  the app's own export as "missing columns", and adding a computed,
  export-only column to a hand-written `csv_columns()` does not make every
  upload need it ([#2331](https://github.com/autumn-foundation/autumn/issues/2331)).
