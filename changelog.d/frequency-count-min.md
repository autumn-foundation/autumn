### Added

- **frequency:** `CountMinSketch` answers "how many times have we seen this
  key?" for any key of an endless stream, in a fixed grid of counters sized
  from the error you accept: `new(0.001, 0.01)` is about 106 KiB and
  overcounts by at most 0.1 % of the stream, 99 % of the time. It never
  undercounts, and `error_bound` reads the current overcount. Unlike
  `heavy_hitters`, it answers for keys outside the top too, so per-IP
  failure counts or per-query hit counts need no table that grows with the
  traffic. `FrequencyCounter` is the shared form; its `take` starts a fresh
  window in one step. Sketches of the same shape `merge` exactly, and
  `to_bytes`/`from_bytes` ship them between replicas. No feature, no
  dependency.
