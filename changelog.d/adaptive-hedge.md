### Added

- **hedge:** `hedge::adaptive::AdaptiveHedge` learns the hedge delay from the
  calls it makes. It keeps a window of recent latencies and hedges at their
  p95 (configurable quantile, window, warm-up and `[floor, ceiling]` bounds),
  so the delay follows the upstream as it speeds up or slows down instead of
  staying at a hand-picked constant.
