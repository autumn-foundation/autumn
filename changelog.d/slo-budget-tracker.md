### Added

- **slo:** `slo_budget::BudgetTracker` evaluates an `[[slo]]` objective in
  the app. It counts each request outcome in one-minute buckets and reports
  the burn rate over any window, the share of the error budget left, and which
  of the three burn-rate alerts fire now: the same windows and factors that
  `autumn slo generate` writes for Prometheus. It reads no clock, so a test
  replays an exact timeline. See `docs/guide/slo.md`, "Read the budget in the
  app".
