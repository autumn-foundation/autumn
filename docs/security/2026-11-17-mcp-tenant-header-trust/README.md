# 2026-11-17 — MCP `tools/call` replay and `header_trust = "trusted_proxy"`

**Outcome:** negative result. The hole was not found. The test is committed
as a regression gate.
**Class tested:** cross-tenant read/write through tenant selection
**Surface:** `mount_mcp` `tools/call` dispatch (`autumn/src/mcp.rs`) ×
`[tenancy] source = "header"` with `header_trust = "trusted_proxy"` (#3729)

## 🎯 Surface

`tools/call` does not run the tool in place. It builds a second request and
sends it to a clone of the assembled router (`router.rs`, the "MCP mounts
last" block). That replay is the request that reaches the handler. So the
replay is the request that the tenancy middleware resolves `CURRENT_TENANT`
from.

#3729 made `source = "header"` trust the tenant header only from a peer in
`security.trusted_proxies.ranges`. That check reads the
`ConnectInfo<SocketAddr>` extension (`tenancy.rs`, the `TrustedProxy` arm of
`extract_tenant_impl`). The replay is built by the server. Its peer is
whatever `mcp.rs` puts there.

## 🕵️ Threat model

> Against an app that resolves its tenant from a header set by its gateway
> (`source = "header"`, `header_trust = "trusted_proxy"`, a gateway range),
> exposes a handler as a tool with `#[api_doc(mcp)]`, and calls
> `mount_mcp("/mcp")`, a client that reaches `/mcp` directly — outside the
> gateway range, with no credential for tenant B — sends
> `x-tenant-id: B` with a `tools/call`. If the replay carried a loopback or a
> gateway address instead of the caller's own, the trust check would pass and
> the tool would run in tenant B.

## 🧪 Reproduction attempt

Test: `autumn/tests/integration/mcp_tenant_header_trust.rs`

```
cargo test -p autumn-web --features "mcp,test-support" --test integration_tests mcp_tenant_header_trust
```

- `the_gateway_selects_the_tenant_through_tools_call`: control. A call from
  `10.0.0.5` reaches the tool in the tenant it set, so the tool path runs.
- `a_direct_client_cannot_select_the_tenant_through_tools_call`: a call from
  `203.0.113.9` with a forged header. The JSON-RPC envelope answers `200`. The
  tool result is `isError: true` with the trust check's `403` ("the tenant
  header is accepted only from a trusted proxy"). The sentinel never appears.
- `the_replay_checks_the_callers_peer_when_the_envelope_is_exempt`: the same
  with `/mcp` in `tenancy.public_paths`. Same refusal.

Green on trunk (`6dc3497`): `after.txt`.

**The gate bites.** A mutant that makes the replay stamp a gateway address
(`10.0.0.1`) in place of the caller's peer, at `apply_replay_extensions`
(`mcp.rs`, `extensions.insert(ConnectInfo(peer))`), turns both attack tests
red with the sentinel in the tool result: `mutant-failure.txt`. The mutant was
not committed.

Note: the run was from a temporary standalone `[[test]]` copy of the module.
This container (15 GB, no swap) is OOM-killed linking the consolidated
`integration_tests` binary, even with `CARGO_BUILD_JOBS=1` and no debug info.
CI runs the committed module in the consolidated binary.

## 🔎 Why it holds

- `apply_replay_extensions` (`mcp.rs`) copies the envelope's own
  `ConnectInfo` onto the replay. With no envelope peer, it copies nothing, and
  the trust check's `is_some_and` refuses.
- `build_request` (`mcp.rs`) forwards every value of the tenant header. So
  the "two values → 400" rule still applies on the replay.
- The dispatch clone is taken after `apply_middleware` (`router.rs`), which
  adds the tenancy layer. So the replay passes through tenancy.
- The `/mcp` envelope is not refused itself. The refusal is on the replay,
  which is the only request that reaches a handler.

## 📡 Variant sweep

- `tenancy.header_name` / tenant-header readers outside `tenancy.rs`: only the
  MCP wiring (`router.rs`, `tenant_header`) and the MCP task-owner binding
  (`mcp.rs`, `Owner::new`), which uses it as a credential part, not as a
  tenant source.
- Other internal replays: capsule replay is a dev/CLI path. The plugin
  sandbox and middleware `oneshot` calls forward the original request. None
  builds a fresh peer.
- Not covered here: session binding through MCP (`source = "header"` +
  `any_client`, session holds `tenancy.session_key`). The replay forwards the
  session cookie, so the check should apply. That has no test yet.

## Also audited this run, no finding

- `CacheResponseLayer` keys by URI only. Excluded by its own contract and by
  the 2026-09-02, 2026-09-14 and 2026-09-21 write-ups.
- Local blob serve route (`/_blobs`) replays the stored content type on the
  app origin. With an empty `security.upload.allowed_mime_types` (the default)
  an uploaded `text/html` file is served as HTML. `docs/guide/storage.md`
  tells apps with untrusted uploads to set the allow list. With one set,
  markup is refused before storage. A hardening issue at most.
- Blob GET vs. upload signatures: a cross-protocol splice of the current
  upload format into a GET signature forces a two-digit expiry, which is
  always past.
- Admin plugin: role check runs before step-up. Token, flag and experiment
  models sit behind both.
- `/_autumn/jobs/{token}`: the token is the capability; owner binding is on
  top of it.
- Dev-only routes (mail previews, inspector) use `profile_is_dev`.
- The 2026-11-15 hot-path perf change to CSRF, session and idempotency scope
  is equivalent and tested.
