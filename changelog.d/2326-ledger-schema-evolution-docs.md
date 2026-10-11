### Changed

- **ledger:** documented how old revisions read after a model gains a field.
  An `Option` field needs no attribute. Tests now cover this and `ledger_diff`
  across the change, and a bulk delete under a database trigger (issue #2326).
