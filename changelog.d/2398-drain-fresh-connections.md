### Fixed

- **upgrade:** the server no longer resets a connection that it accepted just
  before a drain or an in-place upgrade. It now waits up to 2 s for the first
  request of that connection, then drains it (issue #2398).
