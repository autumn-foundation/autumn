### Added

- **reservoir:** `Reservoir` and the shareable `Sampler` keep a uniform random
  sample of `k` items from an endless stream (example requests, failing
  payloads, slow traces) in memory for `k` items. Every item offered has the
  same `k / n` chance of being kept, and `offer` says whether it was, so you
  can capture something expensive only for the sampled ones. Reservoirs of one
  capacity `merge` into a uniform sample of both streams, weighted by what
  each side saw, so per-pod samples add up to a fair fleet sample.
  `with_seed` replays a stream to the same sample. No feature, no dependency.
