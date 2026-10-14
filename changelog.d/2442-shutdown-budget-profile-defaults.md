### Fixed

- **cli:** `autumn serve` and `autumn dev` now read the shutdown budget
  from the profile's smart defaults (issue #2442). The `dev` profile
  uses 0 s prestop and 1 s shutdown. The `prod` profile uses 5 s and 35 s.
- **cli:** `autumn sbom --binary` now explains why it rejects a
  byte-swapped fat Mach-O header (issue #2394).
