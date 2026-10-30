### Fixed

- **a11y:** two `RadioGroup`s never give the same choice id (issue #3454).
  Before, group `a-` with choice `b` and group `a` with choice `-b` both gave
  `a---b`, and `plan type` and `plan-type` gave the same ids. Now each byte
  other than a letter, digit or `_` is written `--HH`. Ids of plain names do
  not change. Ids of names with `-`, `.` or a space do
  ([migration guide](docs/migrations/next.md)).
