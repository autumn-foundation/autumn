### Added

- **hot_keys:** `HotKeys<K>`, a bounded-memory top-K tracker (Space-Saving).
  `record(key)` per event; `top(n)` lists the busiest keys with a count that
  never under-counts and its error bound; `heavy_hitters(share_ppm)` lists
  keys over a share of traffic and says which are certainly over it;
  `decay()` halves every count so a timer can age old traffic out. Memory is
  `capacity` entries however many distinct keys arrive.
