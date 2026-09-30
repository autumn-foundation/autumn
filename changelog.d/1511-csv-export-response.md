### Added

- **data/csv:** `CsvExport` response primitive (PR #1511) [no-plugin] —
  `autumn_web::data::csv::CsvExport(filename, records)` wraps any iterator of
  `CsvSchema` records and implements Axum `IntoResponse`, returning a
  `text/csv; charset=utf-8` attachment. The filename goes through the same
  `Content-Disposition` encoding as `Download`, so a caller-supplied name
  cannot inject header directives.
