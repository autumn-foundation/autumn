### Added

- **minhash:** `MinHash` estimates how much two sets overlap (Jaccard
  similarity) from a fixed number of slots per set, without keeping either
  set: 128 slots is 1 KiB and answers within about ±0.09. `MinHash::of_text`
  sketches a text by its word shingles, so "is this post a reworded copy of
  one we removed?" is one comparison. Sketches with the same slot count
  `merge` into the exact sketch of the union, and `to_bytes`/`from_bytes`
  store one per row or ship it between processes. Behind the opt-in `sketches`
  feature; no new dependency.
