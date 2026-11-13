### Fixed

- **wire contracts:** the cross-release check now sees nested reads and
  writes (issue #3691). `#[contract_checked]`, `#[listener]` and
  `#[event_consumer]` record `item.details.color` as the path
  `details.color`. `#[contract_checked]` also records a field of a nested
  request literal as a path. `autumn contracts check` compares each nested
  type with the reads and writes under its field. So the removal of an
  optional nested field that a caller or a consumer reads is a break. A
  nested field that the call site sets and the callee drops is a break too.
  Before, the check said "safe in any deploy order". The bundle format does
  not change. Each path also lists its prefix, so an older CLI reads the
  top-level fields as before.
