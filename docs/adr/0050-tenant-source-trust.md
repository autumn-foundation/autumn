# ADR 0050: The Client Does Not Select Its Tenant

- Status: Accepted
- Date: 2026-11-14
- Deciders: Autumn maintainers
- Tags: security, tenancy, sharding

## Terms

- **Tenant context**: `CURRENT_TENANT`. Generated `tenant_scoped`
  repositories filter by it. `ShardedDb` routes by it.
- **Client-selected source**: a source whose value the client writes: the
  `header` source and the `subdomain` source.
- **Session binding**: the check that the resolved tenant is equal to the
  tenant in the session key `tenancy.session_key`.

## Context

`[tenancy] enabled = true` with no `source` used `source = "header"`. The
middleware put any value of `x-tenant-id` in the tenant context.
`#[secured]` checks the session role only. Thus a tenant-A admin could send
`x-tenant-id: B` and read, write and route to the shard of tenant B.

## Planning

### Brainstorm

1. Make `session` the default source.
2. Make the header source name its trust: `trusted_proxy` or `any_client`.
3. Stop the boot for a header source with no trust, and for `trusted_proxy`
   with no proxy range.
4. Fail closed at run time for a router built without `run`.
5. Bind the tenant to the session tenant for every source except `session`.
6. Add a tenant-membership hook to `#[secured]`. Not selected: see below.
7. Add a typed principal that carries the tenant and the roles. Not selected:
   see below.

### Reverse brainstorm

How can the fix still fail?

- The proxy adds its value and keeps the client value. Reject two values,
  and let the MCP replay forward every value.
- The check is in the middleware only. The `Tenant` extractor, the
  `ShardedDb` fallback and the fleet router do not run it. Put every check
  in `extract_tenant_impl`, the one shared path.
- `trusted_proxy` with no range trusts every peer. A hop count trusts a
  chain that a direct client writes. Use the peer address and the ranges
  only, and stop the boot when no range is valid.
- The binding failure is recorded as a resolved tenant, so a replay skips
  the check. Record it as a failed lookup.
- The policy is built per request. Cache it in `AppState`, and build it only
  for the header source.

### Six hats

- **White:** the default was `header`. About 25 test files and one example
  use header tenancy. The SaaS starter and `teams` use `session`.
- **Red:** the upgrade has friction. A clear boot error is better than a
  silent hole.
- **Black:** the new default and the new field are breaking. The binding
  adds one session read per request. An app that keeps another form of the
  id in `session_key` gets `403`.
- **Yellow:** the default is safe. A header source behind a gateway still
  works. One shared path covers all the sinks.
- **Green:** reuse `CidrRange`, one enum and one variable.
- **Blue:** do items 1 to 5. Keep `#[secured]` as it is. The binding covers
  each route that reads the tenant.

## Decision

- `tenancy.source` defaults to `"session"`.
- `tenancy.header_trust` is `Option<TenantHeaderTrust>`, with no default.
  `source = "header"` needs it. `trusted_proxy` accepts the header only from
  a peer address in `security.trusted_proxies.ranges`. `any_client` is the
  named unsafe opt-in.
- `validate_tenancy_source` stops the boot for a header source with no
  trust, and for `trusted_proxy` with no valid range. At run time, the same
  config returns `500` or `403`.
- More than one value of the tenant header gets `400`.
- `check_session_tenant` applies the session binding for every source
  except `session`. A mismatch gets `403`. An empty value does not bind.
- `verification/tenant_resolution.rs` models these rules in Verus.

## Options not selected

- **A membership hook on `#[secured]`.** It covers only routes with
  `#[secured]`. A repository call in a route without it stays open. The
  binding is on the path that every sink uses.
- **A typed tenant principal.** It is the better long-term model, but it
  changes the auth API. The binding gives the same result for apps that
  write the tenant to the session.
- **Remove the header source.** A gateway that authenticates the user is a
  valid design. The framework keeps it as an explicit choice.

## Consequences

- An app that used the default header gets `401` until it sets `source`.
- A header-source app must set `header_trust`, or it does not start.
- `subdomain` and `jwt` bind only when the app writes `session_key`.
- A capsule does not keep the session cookie. The replay of a binding
  failure fails with `500`, not `403`.
- The fleet router reports a binding failure as `400`. `ShardedDb` keeps
  the `403`.

See [Tenant sources](../guide/tenant-cells.md#tenant-sources) and the
[migration guide](../migrations/next.md#tenancy-the-tenant-header-needs-header_trust).
