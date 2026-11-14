# 2026-11-14 — Header tenancy let a client select its tenant

## Surface

`autumn_web::tenancy` (`tenancy_middleware`, the `Tenant` extractor,
`extract_tenant_from_parts`), the generated `tenant_scoped` repository
filter, and the `ShardedDb` routing key.

## Problem

`[tenancy] enabled = true` with no `source` used `source = "header"`. The
middleware accepted any value of `x-tenant-id` and put it in
`CURRENT_TENANT`. `#[secured]` checks the session role only. It does not
compare the tenant.

Attack:

1. Sign in to tenant A as a user or a tenant admin.
2. Send `x-tenant-id: B`.
3. The role check passes. The repository filter and the shard router use
   tenant B.

Result: read, write and shard access in tenant B without the credentials of
tenant B.

## Fix

- `source` defaults to `"session"`.
- `source = "header"` needs `header_trust`. Without it, the boot fails, and a
  router built without `run` returns `500`.
  - `trusted_proxy`: only a peer address in `security.trusted_proxies.ranges`
    can set the header. Other peers get `403`. `trusted_hops` does not count.
    The boot fails when no range is valid.
  - `any_client`: the named unsafe opt-in.
- Two header values get `400`. The MCP replay forwards every value.
- Session binding: for every source except `session`, when the session holds
  `tenancy.session_key`, the resolved tenant must be equal to it, or `403`.
- All checks are in `extract_tenant_impl`. The middleware, the `Tenant`
  extractor, the `ShardedDb` fallback and the fleet router use that path.

## Tests

- `autumn/tests/integration/tenancy_header_trust.rs`
- `autumn/src/sharding.rs`: `the_routing_key_follows_the_session_tenant`,
  `the_routing_key_needs_a_trusted_proxy`
- `autumn/src/config.rs`: `env_override_tenancy_header_trust`,
  `tenancy_defaults_to_the_session_source_with_no_header_trust`

## Limits

- `any_client` with a session that does not hold `session_key` is still
  open. The app must check membership itself.
- `subdomain` and `jwt` bind only when the app writes `session_key` to the
  session at sign-in.
- The session check compares exact values. A capsule does not keep the
  session cookie, so the replay of a binding failure fails with `500`.
- `ShardedDb` keeps the `403`. The fleet router still returns `400` for it.

See [Tenant sources](../../guide/tenant-cells.md#tenant-sources) and the
[migration guide](../../migrations/next.md#tenancy-the-tenant-header-needs-header_trust).
