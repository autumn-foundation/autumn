### Fixed

- **`#[commentable]`:** a raw-identifier model name (`struct r#type`) no
  longer leaks the `r#` prefix into the `commentable_type` discriminator
  (issue #2272). The model-name default now goes through the same
  route/selector validation as an explicit `type_name` override, so it fails
  at compile time with a directed message — naming the `type_name = "type"`
  pin that fixes it — instead of rendering `/comments/r#type/…`, which a
  browser reads as a fragment and posts every form to the wrong path.
