### Fixed

- **a11y:** two `RadioGroup`s never give the same choice id (issue #3454).
  Before, group `a-` with choice `b` and group `a` with choice `-b` both gave
  `a---b`, and `plan type` and `plan-type` gave the same ids. Now each byte
  other than a letter, digit or `_` is written `--HH`. An id made only of
  letters, digits and `_` does not change, unless a value repeats. An id with
  any other character does
  ([migration guide](docs/migrations/next.md)).
