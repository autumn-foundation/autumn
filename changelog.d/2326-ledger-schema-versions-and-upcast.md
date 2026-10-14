### Added

- **ledger:** `ledgered(schema_version = N, upcast = path)` reads old revisions
  after you rename or retype a snapshot field (issue #2326). A model at version
  2 or more writes the version into each new snapshot, inside the hash. The
  `upcast` function converts an older snapshot before the model reads it.
  `LedgeredRecord` gains `LEDGER_SCHEMA_VERSION` and `ledger_upcast`, both with
  defaults. A model at version 1 writes no marker, so existing chains keep
  their hashes.

### Fixed

- **ledger:** `ledger_verify` no longer reports `LiveStateMismatch` for a record
  whose head revision was written before a schema change (issue #2326). It
  converts the head with the upcast function and compares the columns that
  both shapes have.
