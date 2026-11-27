### Added

- **heavy_hitters:** `SpaceSaving<K>` and the shareable `HeavyHitters<K>` find
  the top keys of an endless stream (tenants, routes, client IPs) in a fixed
  number of counters. Every key over `total / capacity` stays in the table,
  each answer carries an upper bound (`count`) and a lower bound
  (`guaranteed()`), `frequent(share_ppm)` reports no false positives, and ties
  break by slot order so a stream replays identically. Behind the opt-in
  `sketches` feature; no new dependency.
