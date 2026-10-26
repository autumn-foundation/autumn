### Breaking Changes

- **Breaking:** **constela:** a render that writes the same element id two
  times now fails with `constela.duplicate`, and so does validation of a
  literal `id` in an `each` body. An id with whitespace fails with
  `constela.eval`; `true` and `""` now omit the attribute. A portal `target`
  longer than 256 bytes fails validation
  ([migration guide](docs/migrations/next.md)).
- **Breaking:** **constela:** `ConstelaError::Invalid` holds at most
  `MAX_DIAGNOSTICS` (256) faults plus one `constela.limit` diagnostic, and
  `Diagnostic::new` cuts `path` and `message` to
  `MAX_DIAGNOSTIC_FIELD_BYTES`. A copy of a value that a render or a dispatch
  keeps now charges the allocation budget, so an action that copies more than
  4 MiB fails ([migration guide](docs/migrations/next.md)).

### Security

- **constela:** every byte a render or a dispatch keeps now charges one
  budget (issue #2626). Before, a copy of a `state`, `var` or `param` value,
  a portal target, a validation diagnostic, a loop binding and a component
  param were not charged, so a hostile document could make one request keep
  gigabytes. A read that only shows a value no longer copies it. A new
  `constela` fuzz target and the `constela_memory` test check that peak
  memory stays bounded. See `docs/adr/0025-constela-retention-accounting.md`.
