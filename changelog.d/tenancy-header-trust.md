### Security

- **tenancy:** a client can no longer select its tenant with a header. A
  user of tenant A could send `x-tenant-id: B` and read or write the rows
  and shard of tenant B. The tenant header now needs `tenancy.header_trust`.
  A session that holds `tenancy.session_key` binds the request to that
  tenant. A header, host name or token for a different tenant gets `403`.

### Breaking Changes

- **Breaking:** `[tenancy] source` defaults to `"session"`, not `"header"`.
  `source = "header"` needs `header_trust = "trusted_proxy"` or
  `"any_client"`, or the app does not start. `TenancyConfig` has a new
  `header_trust` field
  ([migration guide](docs/migrations/next.md#tenancy-the-tenant-header-needs-header_trust)).
