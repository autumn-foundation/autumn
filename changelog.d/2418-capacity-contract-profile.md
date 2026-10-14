### Changed

- **capacity:** the runtime now checks which profile a `capacity.lock` was
  calibrated under (issue #2418). If it differs from the running profile, the
  contract is ignored with a warning, and the profile default applies.
  `autumn calibrate` also boots the child in the package directory of the
  binary it found, with or without `-p`.
