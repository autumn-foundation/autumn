### Fixed

- **a11y:** two `RadioGroup`s never give the same choice id (issue #3454).
  Before, group `a-` with choice `b`, and group `a` with choice `-b`, both
  gave `a---b`. Now each byte other than a letter, digit or `_` is written
  `--HH`. An id with other bytes has a new value. So has the second of two
  equal values. See the [migration guide](docs/migrations/next.md).
