### Performance

- **state:** `AppState` now holds its fields behind one `Arc`. A clone is one
  reference count, not one for each field. The state also moves through the
  ingress futures as one pointer. On the `request_pipeline` bench this removes
  about 8% of the instructions of each request (issue #3168).

### Breaking Changes

- **Breaking:** the `AppState` getters are no longer `const fn` (issue
  #3168). A `const fn` cannot read through the shared pointer. No
  `AppState` can exist in a const context, so this changes no caller in
  practice ([migration guide](docs/migrations/next.md)).
