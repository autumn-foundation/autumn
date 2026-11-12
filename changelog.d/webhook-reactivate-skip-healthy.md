### Performance

- **webhooks:** a successful delivery no longer rewrites a subscription that
  is already active with no failures. The SQL store's reactivate update now
  skips a row it would not change, so it writes no WAL and holds no row lock
  for a healthy receiver: 40,000 → 431 row writes for 20,000 deliveries, and
  -14% WAL for the delivery workload (`docs/perf/webhook-reactivate/`).
