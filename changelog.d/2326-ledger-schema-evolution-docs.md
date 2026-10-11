### Documentation

- **ledger:** an `Option` field added to a ledgered model needs no attribute.
  Old revisions read it as `None`. `ledger_diff` shows no change for it
  (issue #2326).
