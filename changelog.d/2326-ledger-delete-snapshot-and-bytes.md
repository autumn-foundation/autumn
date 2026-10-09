### Fixed

- **ledger:** a delete revision now snapshots the full row after the soft-delete
  `UPDATE`. Before, a trigger that changed another column (such as `updated_at`)
  made `ledger_verify` report `LedgerBreak::LiveStateMismatch` after every delete (issue #2326).
- **ledger:** the ledger refuses a stored snapshot that is not in canonical form
  (changed spacing, other key order, duplicate keys) with
  `LedgerError::SnapshotNotCanonical`. `ledger_verify` reports it as
  `LedgerBreak::SnapshotNotCanonical` (issue #2326).
- **ledger:** a snapshot that does not decode into the current model returns
  `LedgerError::SnapshotSchemaMismatch`, not `ChainUnreadable`. Add
  `#[serde(default)]` to a new field to keep old revisions readable (issue #2326).
