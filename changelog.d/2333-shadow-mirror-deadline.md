### Changed

- **shadow:** The mirror deadline now covers the comparison too (issue #2333).
  Decode, parse, digest and record run on the blocking pool under the same
  deadline as the shadow request. A comparison that does not finish is counted
  as `abandoned` (`comparisons_abandoned` in `/actuator/shadow`) and frees its
  `max_in_flight` slot. `max_in_flight` bounds outstanding mirrors, end to end.
- **shadow:** `autumn_shadow_comparisons_total` has a new `abandoned` outcome.
  `ShadowStats` has a new public field, `comparisons_abandoned`.
