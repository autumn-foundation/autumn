### Fixed

- **acme:** an order for an offboarded custom domain still counts against the
  hourly budget after a restart (issue #3110). `CustomDomainStore` has two new
  methods, `load_attempts` and `append_attempt`, with no-op defaults. The
  memory and filesystem stores override them. `CustomDomainTask::hydrate_limiter`
  is now `async`.
