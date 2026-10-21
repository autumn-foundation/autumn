### Fixed

- **acme:** an order for an offboarded custom domain still counts against the
  hourly budget after a restart (issue #3110).

### Changed

- **acme:** `CustomDomainStore` has three new methods: `load_attempts`,
  `append_attempt` and `merge_attempts`. Their defaults keep nothing. `MemoryCustomDomainStore` and
  `FsCustomDomainStore` implement them. `CustomDomainTask::hydrate_limiter` is
  now `async`. [no-plugin]
