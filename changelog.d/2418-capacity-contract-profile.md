### Changed

- **capacity:** the runtime now checks the profile a `capacity.lock` was
  calibrated under (issue #2418). If it is not the running profile, the
  runtime ignores the contract and logs a warning. The profile default then
  applies.
- **capacity:** `autumn calibrate` now starts the child in the package
  directory of the binary it found. This is true with or without `-p`.
