### Fixed

- **upgrade:** a connection accepted just before a drain or in-place upgrade
  cutover is no longer reset. The server waits up to 2 s for its first byte,
  then drains it (issue #2398).
