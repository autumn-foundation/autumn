### Added

- **wire contracts:** `autumn contracts export --proto --out <dir>` writes
  protobuf schemas of the wire contract for consumers outside the workspace:
  one file per service, one per crate for events, and a `buf.yaml`. Each
  `json_name` is the serde wire name, so a protobuf JSON parser reads the JSON
  that Autumn sends. Wrapper messages hold the shapes that protobuf cannot
  write in place. `--http-annotations` adds `google.api.http` options, and
  `--well-known-types` maps a `date-time` to `google.protobuf.Timestamp`. The
  CLI needs no `protoc`.
- **wire contracts:** `contracts/proto.lock` keeps each protobuf field
  number. A new field gets the next free number, and a removed field moves to
  `reserved`. `--check` exits 1 when the lock or a file would change.
- **wire contracts:** `#[wire(tag = N)]` sets the protobuf field number of a
  field. Two equal tags on one type, or a number outside protobuf's range, is
  a compile error.
- **wire contracts:** `autumn contracts export --from` also takes a bundle
  file.
