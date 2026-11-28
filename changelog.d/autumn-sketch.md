### Added

- **autumn-sketch:** a new crate of fixed-memory stream sketches, kept out of
  `autumn-web` so apps that do not use them do not compile them. Add
  `autumn-sketch = "0.11"` to use them. It is std only, with no dependencies.
  Each sketch has a shared form behind a mutex, and sketches of one shape
  merge, so per-replica sketches add up to the fleet's answer.
  - `heavy_hitters`: `SpaceSaving` / `HeavyHitters` find the top keys of a
    stream in a fixed number of counters. Every key over `total / capacity`
    stays in the table, and each answer carries an upper bound (`count`) and a
    lower bound (`guaranteed()`).
  - `frequency`: `CountMinSketch` / `FrequencyCounter` count *any* key, not
    only the top ones: `new(0.001, 0.01)` is about 106 KiB and overcounts by
    at most 0.1 % of the stream, 99 % of the time. It never undercounts, and
    `error_bound` reads the current overcount.
  - `distinct`: `HyperLogLog` / `DistinctCounter` count distinct keys to
    within `1.04 / sqrt(2^precision)` (0.81 % in 16 KiB).
  - `quantiles`: `DdSketch` / `Quantiles` track p50, p99 and p99.9 within the
    relative accuracy you choose, with exact min and max.
  - `reservoir`: `Reservoir` / `Sampler` keep a uniform random sample of `k`
    items, and `offer` says whether an item was kept.
  - `bloom`: `BloomFilter` / `SeenFilter` answer "have we seen this key
    before?" with no false negatives: a million keys at 1 % fit in about
    1.2 MB.
  - `minhash`: `MinHash` estimates the Jaccard similarity of two sets from a
    fixed number of slots, and `of_text` sketches word shingles for
    near-duplicate detection.
