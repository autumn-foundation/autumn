### Fixed

- **plugin-sandbox:** the host now charges fuel for parsing stdout, one unit
  per byte, on top of the copy charge (issue #2460). A guest can no longer
  pad a frame with whitespace to get CPU work at the bulk-copy rate.
