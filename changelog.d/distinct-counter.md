### Added

- **distinct:** `HyperLogLog` and the shareable `DistinctCounter` count the
  distinct keys of an endless stream (unique visitors, client IPs, tenants)
  in a fixed `2^precision` bytes, to within `1.04 / sqrt(2^precision)`
  (0.81 % in 16 KiB). Duplicates never inflate the count, sketches of the
  same precision merge into the sketch of the union (count a fleet's
  visitors without double-counting), `to_bytes`/`from_bytes` carry a sketch
  between processes, and keys hash the same on every run and machine. No
  feature, no dependency.
