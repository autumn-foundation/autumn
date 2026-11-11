### Added

- **wire contracts:** `#[contract_checked]`, `#[listener]` and
  `#[event_consumer]` record a nested read as a dotted path:
  `item.details.color` reads `details.color`. A nested struct literal in a
  request records its fields the same way. `autumn contracts check` uses the
  paths at each nested level. It reports the removal of an optional nested
  field that a caller reads. It reports a new optional nested field that a
  new caller reads. It reports a nested field that the call site sets and
  the callee drops.

### Fixed

- **wire contracts:** `autumn contracts check` no longer counts a request
  field that the type may skip (`skip_serializing_if`) as sent only because
  the call site sets it. It counts as sent when the caller's own release
  also requires it. A field such as `tags: vec![]` is not on the wire.
- **wire contracts:** a `let`, an `if let`, a `while let` or a `match` inside
  a macro body (maud `@let`, `@if let`, `@match`) binds its own names. A
  name it binds no longer counts as a response read.

### Breaking Changes

- **Breaking:** `autumn contracts export` writes bundle format 5, which an
  older CLI refuses. This CLI reads formats 1 to 5
  ([migration guide](docs/migrations/next.md)).
