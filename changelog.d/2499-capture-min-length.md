### Fixed

- **Posture diff:** `segments_overlap` no longer reports an overlap between a
  mixed capture segment and a concrete segment shorter than the capture's
  minimum match length (#2499). A capture must consume at least one character —
  matchit 404s otherwise — so `/file.{ext}` never serves `/file.`; the old
  edge-only check invented a blocking `route_shadow_exposed` finding when a
  guarded `/file.` was deleted beside a public `/file.{ext}`. The rule (a
  segment with L literal characters and n captures matches only text of
  length >= L + n) flows through `intersect`, `covers`, and the shadow
  analysis; all previously-overlapping pairs still overlap.
