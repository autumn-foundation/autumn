### Added

- **bloom:** `BloomFilter` and the shareable `SeenFilter` answer "have we seen
  this key before?" for an endless stream (webhook delivery ids, replayed
  events, digests already sent) in a fixed number of bits sized from the keys
  you expect and the false-positive rate you accept: a million keys at 1 % fit
  in about 1.2 MB. There are no false negatives, and `insert` says whether the
  key was new, so "process it once" is one call. `false_positive_rate` reads
  the rate at the current fill so you can rotate before it drifts, and
  `estimated_len` reads back about how many keys went in. Filters of one shape
  `merge` into the union, and `to_bytes`/`from_bytes` ship them between
  processes. No feature, no dependency.
