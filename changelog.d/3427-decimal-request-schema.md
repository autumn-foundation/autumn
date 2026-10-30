### Fixed

- **openapi:** `New*` and `Update*` request schemas now accept a `Decimal` as a
  string or a JSON number (issue #3427). The handler already read both. The
  read schema stays `string`/`decimal`.
