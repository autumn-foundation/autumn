# ADR 0025: OpenAPI Schemas Are Chosen by Direction

- Status: Accepted
- Date: 2026-10-25
- Deciders: Autumn maintainers
- Tags: openapi, mcp, model, schema
- Issue: #2607

## Context

The spec builder found the schema of a referenced type by type identity only.
It used one schema for a request body and for a response body.

Some types have two wire shapes. `#[private]`, `#[classified]` and a hidden
`#[encrypted]` field skip the field on serialize only. `skip_serializing_if`
also changes only the serialize side. Thus:

1. A route that takes `Json<Model>` showed the read schema. The schema did not
   have the hidden field. The handler requires it, so a generated client got a
   400.
2. A route that returns `Json<NewModel>` showed the write schema. The schema
   required the classified field. The response does not have it, so a strict
   client did not decode the response.

No attribute rule can fix this. One schema cannot describe two shapes.

## Decision

1. A reference has a direction: `SchemaDirection::Request` (request body,
   query) or `SchemaDirection::Response` (response body).
2. `DerivedSchemaDescriptor` does not change. It is the default for both
   directions.
3. A new `DirectionalSchemaDescriptor` replaces the default for one direction.
   `#[model]` submits:
   - a request schema for the model (the shape `Deserialize` reads), and
   - a response schema for `New*` when it has a classified field.

   When a deserialize-side adapter (`deserialize_with`, `with`, `flatten`,
   `from`, `try_from`, `transparent`, `tag`) changes the request shape, the
   request override has no schema. The request then uses the opaque
   placeholder, and `autumn openapi export --strict` reports it.
4. The spec builder records the directions of each reference, also for nested
   references. A type gets two components only when both directions refer to
   it and the two schemas differ, or a type it refers to has two components.
5. The response component keeps the base key (`Widget`). The request
   component is `WidgetRequest`. When that key is used, it is
   `WidgetRequest-2`, `WidgetRequest-3`, and so on. A user or route
   registration keeps its key.
6. Query parameters and MCP tool `inputSchema` use the request direction.

## Consequences

- A type with one shape keeps one component. Existing specs do not change for
  it.
- A type that both directions use, with two shapes, gets a component pair. A
  generated client gets two types for it.
- A `#[derive(OpenApiSchema)]` type stays direction-free. The derive refuses
  the attributes that make the shapes differ.
- `schema_entry_to_value` renders a response reference. Callers that render a
  request use `schema_entry_to_value_in`.

## Alternatives

- **Two descriptors for every type.** Rejected: a pair for every type makes
  the common case worse.
- **Refuse to register an asymmetric type.** Rejected: it removes the
  field-level schema of the models that #802 is for.
- **Add a field to `DerivedSchemaDescriptor`.** Rejected: it breaks code that
  builds the struct by hand.
