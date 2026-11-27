### Documentation

- **fleet:** the SQLite fleet guide ("What a generation costs") and the
  `ShipMode::Generations` rustdoc state the replica cost. Each changed file
  ships whole. Replica writes a day ≈
  `changed files × file size × 86,400 / every`. Pick `every` from that write
  volume (issue #3772).
