### Fixed

- **acme:** an order for an offboarded custom domain still counts against the
  hourly budget after a restart (issue #3110).
- **cli:** `autumn new --bundled-pg` ships a `deny.toml` waiver for RUSTSEC-2026-0381
  (`webpki-roots` 0.26.11, pinned by `sqlx-core`). No fixed 0.26 release exists. [no-plugin]

### Changed

- **acme:** `CustomDomainStore` has three new methods: `load_attempts`,
  `append_attempt` and `merge_attempts`. Their defaults keep nothing. `MemoryCustomDomainStore` and
  `FsCustomDomainStore` implement them. `CustomDomainTask::hydrate_limiter` is
  now `async`. [no-plugin]
