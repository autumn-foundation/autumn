### Added

- **bloom:** `BloomFilter` and the shareable `SeenSet` answer "have I seen
  this key before?" (webhook delivery ids, first-time logins, spent tokens,
  crawled URLs) in a fixed number of bits: about 1.2 bytes a key at a 1 %
  false-positive rate. There are no false negatives, and `insert` says
  whether the key was new, so check-and-mark is one call. `new` sizes the
  filter from the expected keys and a target rate, `false_positive_rate`
  reports the live rate as it fills, and `estimated_len` reads back roughly
  how many keys went in. Filters of one shape `merge` into the union, and
  `to_bytes`/`from_bytes` carry one between processes. No feature, no
  dependency.
