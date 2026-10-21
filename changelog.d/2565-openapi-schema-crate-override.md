### Added

- **macros:** `#[derive(OpenApiSchema)]` accepts `#[openapi_schema(crate = "...")]`
  (issue #2565). Use it when one crate holds two copies of `autumn-web`. The
  generated impl and schema registration then use the named copy. [no-plugin]
