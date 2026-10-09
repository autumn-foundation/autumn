### Fixed

- **ledger:** a ledgered write that holds `NaN` or an infinite float now fails
  with `LedgerError::NonFiniteValue`. Before, the snapshot stored `null` and the
  record could not be read back (issue #2326).
- **ledger:** `ledger_as_of` and `ledger_diff` return `ChainUnreadable`, naming
  the column and revision, when they cannot decrypt an `#[encrypted]` column.
  Before, they returned the ciphertext as the value (issue #2326).
- **ledger:** a delete or restore revision on a `valid_time` model is valid from
  the instant of the change. Before, it kept the row's old valid time and appeared in
  valid-time queries about earlier instants (issue #2326).
