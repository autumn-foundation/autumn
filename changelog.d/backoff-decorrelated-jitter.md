### Added

- **backoff:** `decorrelated_jitter_ms` and `decorrelated_jitter` draw each
  retry delay from `[base, prev * 3]`, capped, so clients that fail together
  drift apart faster than with full jitter. Seeded by `Entropy`, so a `Sim`
  seed replays the same delays.
