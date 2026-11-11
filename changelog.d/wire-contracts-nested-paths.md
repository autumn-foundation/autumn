### Added

- **wire contracts:** `#[contract_checked]`, `#[listener]` and
  `#[event_consumer]` record a nested read as a dotted path:
  `item.details.color` reads `details.color`. A nested struct literal in a
  request records its fields the same way. `autumn contracts check` uses the
  paths at each nested level. It reports the removal of an optional nested
  field that a caller reads, a new optional nested field that a new caller
  reads, and a nested field that the call site sets and the callee drops.

### Breaking Changes

- **Breaking:** `autumn contracts export` writes bundle format 5, which an
  older CLI refuses. This CLI reads formats 1 to 5
  ([migration guide](docs/migrations/next.md)).
