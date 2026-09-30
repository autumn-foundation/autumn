### Fixed

- `autumn-search`: the unconditional batched index's tombstone clear and
  `delete()`'s tombstone insert now acquire ledger rows in the same
  deterministic ascending `record_id` order. Previously the clear locked
  tombstones in the query plan's scan order while `delete()` wrote them in
  caller order, so the two racing over the same tombstones could deadlock and
  PostgreSQL would abort one request ([#2310](https://github.com/autumn-foundation/autumn/issues/2310)).
