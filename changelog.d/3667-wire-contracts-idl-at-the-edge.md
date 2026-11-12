### Documentation

- **wire contracts:** ADR 0048 records why contracts come from Rust types
  inside a workspace. At the workspace edge, an IDL (protobuf for
  endpoints, Avro for events) is an export, not a source. The
  wire-contracts guide links it.
