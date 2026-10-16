### Added

- **agent authority:** the MCP dispatcher now enforces a grant's `rate` cap
  (issue #2451). One bucket serves the whole grant. A call over the cap is
  refused and audited. The count is per process. `spend` is still declared
  only.
