### Fixed

- **openapi:** `Decimal`, `SqliteUuid` and `SqliteDecimal` fields now appear as
  scalars in the OpenAPI spec and the MCP `inputSchema` (issue #2582). Before,
  the spec showed an object `$ref`. `OpenApiSchema` now exists for `Uuid`,
  `DateTime<Utc>`, `NaiveDate`, `NaiveDateTime` and `Decimal`. The JSON wire
  format does not change. The spec for a Postgres `Decimal` field does change.
  Regenerate your API clients. `Decimal` is `string` with `format: decimal`. It
  is `number` if the build enables a `rust_decimal` number feature.
