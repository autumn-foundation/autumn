### Performance

- **ledger:** `save_many`, `save_many_skip_invalid`, `update_many`,
  `delete_many` and `upsert_many` on a `ledgered = true` repository write the
  version history and the ledger in batches (issue #3345). Before, each row
  sent 5 statements. Now each chunk of 1,000 rows or fewer sends 5: one
  history `INSERT`, one reload, one chain-state read, one revision `INSERT`
  and one high-water upsert. The chains and the #2323 guards do not change. A
  record that occurs two times in one batch has one chain, in input order. See
  [ADR 0029](docs/adr/0029-ledger-batched-bulk-append.md).

### Fixed

- **version history:** `update_many` on a repository with `hooks = ...` and
  `versioned = true` now writes a history row for each row (issue #3345).
  Before, it wrote none, and on a `ledgered = true` repository it appended no
  revision, so the ledger did not show the change.
