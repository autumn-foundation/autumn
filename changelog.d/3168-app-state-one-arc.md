### Performance

- **state:** `AppState` now holds its fields behind one `Arc`. A clone is one
  reference count, not one for each field. The state also moves through the
  ingress futures as one pointer. On the `request_pipeline` bench this removes
  about 8% of the instructions of each request (issue #3168).
