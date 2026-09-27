# 2026-09-15 — `SandboxedPlugin::render_slot`'s independent `CURRENT_TENANT` capture (negative result)

## 🎯 Surface

`autumn_web::plugin_sandbox::plugin::SandboxedPlugin::render_slot`
(`autumn/src/plugin_sandbox/plugin.rs:279-330`) × the KV capability
(`autumn/src/plugin_sandbox/capability/kv.rs`'s `namespaced_key`). Entry
point investigated: an app that declares a render slot with
`RenderSlots::declaring(...)`, mounts a sandboxed plugin granted the
`render` capability for that slot (exactly the pattern
`plugin_sandbox/slots.rs`'s own module docs show), and the plugin's render
hook makes a `kv-set`/`kv-get` capability call while it runs.

## 🕵️ Threat model (hypothesis)

Against an app that follows this documented pattern, a plugin author who
controls what one tenant's sandboxed plugin does inside its render hook
could read or overwrite another tenant's KV-backed state — a per-tenant
cart, a per-tenant counter, anything a render hook uses `kv` for — if
`render_slot`'s own tenant capture ever diverged from the one
`SandboxedPlugin::serve` uses. The app author would have done nothing
wrong: `render_slot` is called from ordinary request-handling code, inside
the same tenancy-middleware-scoped task as everything else in the request.

This traces directly to the 2026-09-14 Keystone cross-tenant-key-derivation
findings memo
(`docs/reports/2026-09-14-keystone-tenant-scoped-key-gap.md`), which named
this exact gap while auditing four unrelated derived-key builders: *"every
`render_slot` call in \[`plugin_sandbox_capabilities.rs`\]... runs outside a
`with_tenant` scope... `render_slot`'s capture is a real, currently
uncovered gap this memo did not know about until this review caught it."*
That memo did not claim the capture was broken — only that no existing test
would notice if it were. This report is the targeted follow-up it called
for.

## 🧪 Reproduction attempt → negative result

Added `RENDER_KV_WRITE` to `autumn/src/plugin_sandbox/test_guests.rs`: the
only guest in the corpus that issues a `kv-set` capability call *from
inside a render exchange* (every other render guest in the corpus only
ever answers with a fragment, so the capability channel's `call`/reply loop
was never exercised from the render side at all).

Test:
`autumn/tests/integration/plugin_sandbox_capabilities.rs`'s
`the_tenant_a_render_hooks_kv_write_lands_under_is_the_callers_own` — the
`render_slot` sibling of the existing
`the_tenant_a_mounted_plugin_binds_to_is_the_requests_own` (the `serve`-side
proof). Calls `plugin.render_slot(...)` twice, once inside
`with_tenant("alpha", ...)` and once inside `with_tenant("beta", ...)`, with
no tenant baked into the plugin's own `CapabilityServices`, and asserts the
backing `MemoryKvStore` ends up with two keys, one correctly tagged per
tenant.

```
cargo test -p autumn-web --test integration_tests --features "plugin-sandbox,test-support" \
  the_tenant_a_render_hooks_kv_write_lands_under_is_the_callers_own -- --nocapture
```

Result: **pass** — no cross-tenant collision. See `after.txt` for the full
run (`plugin-kv:shop:alpha:cart` and `plugin-kv:shop:beta:cart`, one key
each, in the store).

Also ran the full `plugin_sandbox_capabilities` module
(23 tests) to confirm the new guest and test did not disturb any existing
coverage: `full-suite-run.txt`, all green.

## 🔎 Root cause of the fail-safe behavior

`render_slot` (`plugin.rs:291-298`) reads `CURRENT_TENANT` and folds it into
`CapabilityServices` with the same shape `serve` uses (`plugin.rs:579-587`):

```rust
let services = CapabilityServices {
    tenant: crate::tenancy::CURRENT_TENANT.try_with(Clone::clone).ok().flatten(),
    ..self.services.clone()
};
```

Rust struct-update syntax resolves the explicit `tenant:` field before
applying `..self.services.clone()`, regardless of source order, so the
ambient task-local always wins over whatever tenant an embedder baked into
`self.services` at construction time — exactly the property the
`the_tenant_a_mounted_plugin_binds_to_is_the_requests_own` test's own
comment calls out for `serve`: *"the tenant is read from the tenancy
middleware's task-local and *overwrites* whatever an embedder set."* That
comment, and its proof, simply never extended to `render_slot`'s own,
separate read.

`services` then flows unmodified into
`crate::plugin_sandbox::capability::CapabilityRuntime::new(&self.manifest,
services)` inside `SandboxHost::execute` (`host.rs:1699`) for *both*
exchange kinds (`Exchange::Request` and `Exchange::Render` share the same
`execute` path), and `kv::perform` (`kv.rs:113-121`) derives the physical
key from `runtime.tenant()` — so a capability call made from inside a
render hook is namespaced by the same tenant a request handler's own calls
would be, with no separate code path to diverge.

## 🩹 Fix

None — no bug found. Regression test and guest added as above, registered
in the existing `#[cfg(all(feature = "plugin-sandbox", feature =
"test-support"))] mod plugin_sandbox_capabilities;` block in
`autumn/tests/integration/mod.rs` (no new module declaration needed — the
test lives in the file already registered there).

Confirmed the test is not vacuous: temporarily reverted `render_slot`'s
`CapabilityServices` construction to `self.services.clone()` (dropping the
`CURRENT_TENANT` override — a one-line change) and reran the same test.
It failed, on the exact assertion this report is about:

```
assertion `left == right` failed: one key per tenant, not one shared: ["plugin-kv:shop:-:cart"]
  left: 1
 right: 2
```

Both `alpha` and `beta` collided into the single untenanted key
(`tenant_segment(None)` is `"-"`). See `non-vacuous-check.txt`. The
production code was restored immediately after (verified `git diff` is
empty for `autumn/src/plugin_sandbox/plugin.rs`); only the test and guest
additions are committed.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo test -p autumn-web --test integration_tests --features "plugin-sandbox,test-support" the_tenant_a_render_hooks_kv_write_lands_under_is_the_callers_own` — 1/1 pass (`after.txt`).
- `cargo test -p autumn-web --test integration_tests --features "plugin-sandbox,test-support" plugin_sandbox_capabilities` — 23/23 pass, no regressions (`full-suite-run.txt`).
- Non-vacuousness check above: the same test fails loudly, on the intended
  assertion, when the capture it proves is removed (`non-vacuous-check.txt`).

## 📡 Blast radius

- Swept every `CURRENT_TENANT` read in `plugin_sandbox/`:
  `grep -rn "CURRENT_TENANT" autumn/src/plugin_sandbox` finds exactly two
  acquisition sites, `plugin.rs:293` (`render_slot`) and `plugin.rs:580`
  (`serve`) — both now have dedicated, passing, cross-tenant behavioral
  coverage. No third site exists to check.
  `plugin_sandbox/capability/kv.rs`'s `namespaced_key` itself takes tenant
  as an explicit parameter rather than reading the task-local, so it has no
  acquisition site of its own to sweep (consistent with the Keystone
  memo's observation about this subsystem).
- Feature-independent within its own gate: `plugin-sandbox` is a
  non-default feature, but nothing about the capture depends on any other
  feature combination — `render_slot` and `serve` share one `execute` path
  regardless of which optional backends (`kv`, `db`, `jobs`, `http-outbound`)
  a given manifest grants.
- Did not find a third caller of `render_slot` or a caller that constructs
  `CapabilityServices` for it outside `SandboxedPlugin` itself
  (`grep -rn "render_slot" autumn/src` — only the definition and its one
  caller in `slots.rs`), so there is no additional embedding surface this
  report missed.

## 📜 Compatibility

No behavior change, no CHANGELOG entry (test-only addition, matching this
repo's convention for negative-result commits — see
`docs/security/2026-09-06-idempotency-token-principal/` and
`docs/security/2026-09-10-mcp-secured-guard-dispatch/`).

## 🗂 Ledger

This directory. `after.txt` has the full green run,
`non-vacuous-check.txt` has the synthetic-regression run proving the test
is load-bearing, `full-suite-run.txt` has the full module run showing no
collateral breakage.
