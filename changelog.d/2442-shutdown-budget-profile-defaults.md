### Fixed

- **cli:** `autumn serve` and `autumn dev` now read the shutdown budget
  from the profile's smart defaults (issue #2442). The `dev` profile
  waits 0 + 1 s, not 5 + 30 s. The `prod` profile uses 5 + 35 s.
- **cli:** `autumn sbom --binary` now explains why it rejects a
  byte-swapped fat Mach-O header (issue #2394).
