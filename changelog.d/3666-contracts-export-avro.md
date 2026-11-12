### Added

- **wire contracts:** `autumn contracts export --avro --out <dir>` writes one
  Avro schema (`.avsc`) per event topic, for consumers outside the workspace
  such as data pipelines and Kafka Connect sinks. A new event version is a
  new file, so it is a new registry subject. Field names are the serde wire
  names, and a `#[serde(alias)]` is an Avro alias. A nested type is a named
  record. An opaque event is `bytes`. The schema describes the data: Autumn
  still sends plain JSON, not Avro's JSON encoding. The CLI has no Avro
  dependency.
- **wire contracts:** the Avro export refuses a form that Avro cannot hold:
  `u64`, `usize` and 128-bit integers, `any`, and a wire name that is not an
  Avro name. Each message names the field and the fix.
- **wire contracts:** `--endpoints` adds the request and response body of
  each routed endpoint. `--check` exits 1 when a schema file would change or
  go away.
