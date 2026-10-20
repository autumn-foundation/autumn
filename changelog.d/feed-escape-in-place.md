### Changed

- **feed:** `Feed::render` escapes text fields straight into the output
  buffer instead of allocating a `String` per field. On the 30-entry Atom
  feed workload: 22% fewer instructions and 42% fewer allocations per render.
