### Added

- **quantiles:** `QuantileSketch` and the shareable `QuantileMeter` give the
  p50, p99 and p999 of an endless stream (latencies, payload sizes, job
  durations) in bounded memory, each quantile within a chosen relative
  accuracy of the true sample (1 % by default) whatever the distribution
  (DDSketch). Min and max are exact, as are the count, sum and mean.
  Sketches of the same accuracy merge exactly, so a fleet's p99 comes from
  the replicas' sketches rather than from averaging their p99s.
  `to_bytes`/`from_bytes` carry a sketch between processes, and a bucket cap
  (2048 by default) folds the lowest buckets first so the tail stays
  accurate. Negative and non-finite readings are refused. No feature, no
  dependency.
