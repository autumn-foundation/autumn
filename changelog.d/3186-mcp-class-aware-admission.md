### Fixed

- **mcp:** a burst of lower-class `tools/call` requests is no longer over-shed
  (issue #3186). N+1 concurrent calls at a class limit of N now shed one. A
  replay that is shed no longer gives the adaptive limiter a false fast sample.
