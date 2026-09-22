### Fixed

- `autumn generate scaffold --import`: the columns an uploaded file must carry
  are now derived at request time from the live `CsvSchema::csv_columns()`
  minus the columns the import cannot set (`csv_required_columns()`), instead
  of a list baked in at generation time. Dropping a column from the export's
  schema can no longer leave a stale requirement behind that rejects the app's
  own export as "missing columns" ([#2331](https://github.com/autumn-foundation/autumn/issues/2331)).
