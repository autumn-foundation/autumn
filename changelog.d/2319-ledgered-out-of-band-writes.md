### Breaking Changes

- **Breaking:** a ledgered table refuses framework writes that record no
  revision (issue #2319). A counter-cache column on a ledgered parent, and a
  `dependent(..., on_delete = delete_all | nullify)` cascade into a ledgered
  child, fail with `LedgerError::OutOfBandWrite`. Before, they changed the table
  and `ledger_verify` reported `LiveStateMismatch`. Use `on_delete = destroy`,
  or stop ledgering the table ([migration guide](docs/migrations/next.md)).
