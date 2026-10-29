### Fixed

- **custom domains:** A hostname's write gate now also covers its certificate
  and cache writes. An offboard, a prune or a cache refill can no longer delete
  or overwrite a successor's or a renewed certificate. A handshake that loads a
  stale certificate no longer replaces a renewed one in the cache (issue #2649).

### Added

- **custom domains:** `CustomDomainRegistry::with_hostname` and `exclusive` run
  code inside a hostname's write gate, or alone. `Gated` is the handle for
  registry writes inside the gate. [no-plugin]
