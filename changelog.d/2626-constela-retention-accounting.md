### Breaking Changes

- **Breaking:** **constela:** a render that writes one element id two times
  now fails with `constela.duplicate`, and a portal `target` longer than 256
  bytes fails validation (issue #2626,
  [migration guide](docs/migrations/next.md)).

### Security

- **constela:** every byte a render or a dispatch keeps now charges one
  budget (issue #2626). A copy of a `state`, `var` or `param` value, a portal
  target, a rendered id and a validation diagnostic were not charged before,
  so a hostile document could make one request keep gigabytes. A read that
  only shows a value no longer copies it. `ConstelaError::Invalid` holds at
  most `MAX_DIAGNOSTICS` faults. A new `constela` fuzz target and the
  `constela_memory` test check that peak memory stays bounded. See
  `docs/adr/0025-constela-retention-accounting.md`.
