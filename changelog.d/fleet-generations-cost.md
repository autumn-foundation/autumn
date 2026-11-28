### Documentation

- **fleet:** the SQLite fleet guide ("What a generation costs") and the
  `ShipMode::Generations` rustdoc state the replica cost. Core ships each
  changed file whole. Bytes written to the replica a day ≈
  `changed files × file size × 86,400 / every`. Calculate `every` from that
  write volume (issue #3772).
