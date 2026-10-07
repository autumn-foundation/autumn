### Added

- **routes:** `autumn routes audit` takes `--release`, `--profile`,
  `--features`, `--all-features` and `--no-default-features` (issue #2472). A
  route behind `#[cfg(not(debug_assertions))]` or a non-default feature is now
  in the manifest when you audit the build you ship.
- **routes:** the security manifest (schema v5) records its Cargo build in a
  top-level `build` object. A non-default build is part of the posture
  digest. `routes posture diff` reports `build_changed` as a widening when the
  base and head come from different builds.
- **routes:** `routes posture diff --ack-dir <DIR>` reads one file per
  pull-request comment and parses each file alone. No comment text can fake a
  comment boundary.

### Changed

- **routes:** the scaffolded `posture-gate.yml` audits the release build, from
  one `POSTURE_AUDIT_FLAGS` setting, and harvests acknowledgments with
  `--ack-dir`. After `autumn upgrade --apply`, regenerate the baseline with
  `autumn routes audit --release --manifest security-posture.json`.
  The pull request with the new baseline reports one `build_changed` widening
  to acknowledge.

### Fixed

- **routes:** `routes posture` refuses a manifest that lists one route twice.
  Before, it merged the two entries. A merge cannot show two alternative
  guards, and it can hide a widening.
- **routes:** the route dump no longer lists a built-in health probe that a
  user route replaces, or any probe when `health.enabled = false`. The router
  does not mount these probes.
