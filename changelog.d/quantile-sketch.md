### Added

- **quantiles:** `DdSketch` and the shareable `Quantiles` track p50, p99 and
  p99.9 of an endless stream (latencies, payload sizes) in a few hundred
  counters. Every quantile is within the relative accuracy you choose (1 % by
  default) of the true sample at its rank, min and max are exact, and sketches
  of one accuracy `merge` exactly, so per-pod sketches add up to the fleet's
  percentiles. Past `max_buckets` the lowest buckets fold first; a quantile
  ranked in the folded bucket loses its guarantee, and `is_collapsed()` says
  when that has happened. `observe_duration` / `quantile_duration` read and write
  `Duration`s. No feature, no dependency.
