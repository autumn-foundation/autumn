### Added

- **hot_keys:** `HotKeys<K>`, a bounded-memory top-k counter (Space-Saving).
  It answers "which IPs, tenants or routes are most of the traffic?" in
  `capacity` counters no matter how many keys arrive. Each count carries an
  error bound, `heavy_hitters(ppm)` reports keys over a share with no false
  positives, and `decay()` turns it into a "recently hot" ranking.
