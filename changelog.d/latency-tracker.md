### Added

- **latency:** `autumn_web::latency_tracker::LatencyTracker`, a lock-free,
  fixed-memory latency histogram with `record`, `quantile`, `decay` and
  `reset`. `HedgePolicy::from_tracker` sets a hedge delay from the observed
  p95 of a call instead of a hand-picked number.
