### Breaking Changes

- **Breaking:** **jobs:** `JobTrackingStore` has a new required method,
  `update_record` (issue #3208). `mark_running`, `set_progress`, `complete`,
  `fail` and `get` now have default bodies. A custom store implements only
  `update_record`. The trait applies the status rules
  ([migration guide](docs/migrations/next.md)).
