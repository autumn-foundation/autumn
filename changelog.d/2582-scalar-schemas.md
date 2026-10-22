### Changed

- **openapi:** `Decimal`, `SqliteUuid` and `SqliteDecimal` model fields now
  document as scalars in the OpenAPI spec and the MCP `inputSchema`
  (issue #2582). They were an object `$ref`. `OpenApiSchema` is now
  implemented for `Uuid`, `DateTime<Utc>`, `NaiveDate`, `NaiveDateTime` and
  `Decimal`. Existing Postgres apps with a `Decimal` field get a different
  spec: regenerate API clients. `Decimal` is `string` with `format: decimal`,
  or `number` if the build enables a `rust_decimal` number feature.
