### Performance

- **state:** `AppState` now holds its fields behind one `Arc`. A clone is one
  reference count, not one for each field. A request clones the state several
  times, so this removes about 4% of the per-request instructions of the
  default ingress stack (issue #3168).
