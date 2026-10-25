### Added

- **cli:** `AUTUMN_GENERATE_EPOCH` sets the clock for new migration versions to
  a fixed time (issue #2601). `autumn generate` and `autumn schema diff
  --write-migration` use it. Set it to a number of Unix seconds to get the
  same migration names in two runs. Use it only in tests. If it stays set, all
  new migrations get the same version.
