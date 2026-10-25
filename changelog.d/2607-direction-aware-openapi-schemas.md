### Fixed

- **openapi:** a referenced type now gets its schema from its position
  (issue #2607). A request body or query uses the shape the type
  deserializes from. A response uses the shape it serializes to. A route that
  takes `Json<Model>` now shows the `#[private]`, `#[classified]` and hidden
  `#[encrypted]` fields, and requires them unless they are `Option` or have a
  default. A route that returns `Json<NewModel>` no
  longer requires a classified field that the response does not have. A type
  with two different shapes in use gets two components, `Widget` and
  `WidgetRequest`. A type with one shape keeps one component. MCP tool
  `inputSchema` uses the request shape. The `New*` schema now requires a
  field that has `skip_serializing_if` on the model, because `New*` does not
  copy that attribute.

### Changed

- **openapi:** a request `$ref` to a type with two shapes in use now points to
  `WidgetRequest`. Regenerate clients. A type used only in requests now holds
  its request schema. A `#[model]` with a deserialize-side adapter or `alias`
  is now opaque in a request, so `--strict` can fail on it (issue #2607).
- **openapi:** `schema_entry_to_value` renders a response reference. Use
  `schema_entry_to_value_in` for a request (issue #2607).

### Added

- **openapi:** `SchemaDirection`, `registered_derived_schema_for`,
  `schema_entry_to_value_in` and `schema_component_index_for` (issue #2607).
