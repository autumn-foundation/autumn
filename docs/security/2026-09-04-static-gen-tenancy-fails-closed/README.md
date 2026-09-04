# `#[static_get]` × multi-tenancy: negative result — fails closed (2026-09-04)

**Class:** hypothesis for cross-tenant read through a documented feature
composition, with no app-level analogue
**Surface:** `autumn_web::static_gen` (`render_static_routes` / `autumn build`,
and ISR's `regenerate_page`) × `[tenancy] enabled = true`
**Entry point investigated:** `#[static_get]` handler reading `autumn_web::tenancy::Tenant`
(or any `CURRENT_TENANT`-scoped repository query) rendered by `autumn build`
or an ISR background regeneration
**Status:** **negative result** — no fix required. Regression test committed:
`autumn/src/app.rs::tests::static_get_route_reading_tenant_fails_closed_for_every_tenancy_source`

## 🎯 Surface

Autumn's static generation (`#[static_get]`, SSG/ISR) writes one file per URL
*path* to a single, process-wide `dist/manifest.json` + `dist/` tree
(`autumn/src/static_gen/`). Autumn's multi-tenancy (`[tenancy] enabled = true`,
`docs/guide/...`) resolves a tenant per *request* from a header, subdomain,
session key, or JWT claim (`autumn/src/tenancy.rs`), and scopes queries to it
via the `CURRENT_TENANT` task-local.

Both features are documented, first-class, and independently unremarkable.
Composing them is where the question lives: the static-file cache key is the
URL path alone, with no tenant dimension. If an app pre-renders a page whose
content is tenant-scoped (e.g. a per-tenant public storefront homepage reading
`Tenant` or a `#[repository]` query under `CURRENT_TENANT`), *something* has to
decide which tenant's data gets frozen into that one shared file — and every
future request to that path, from every tenant, would be served whatever got
baked in.

## 🕵️ Threat model (hypothesis)

> Against an app that follows Autumn's documented multi-tenancy guide and pre-
> renders a `#[static_get]` route that reads tenant-scoped state, an attacker
> who is an ordinary authenticated (or even anonymous) user of **tenant B**
> might be able to read **tenant A**'s data — captured at whatever moment
> `autumn build` or an ISR background regeneration last ran — because the
> `dist/` cache key is the URL path only and carries no tenant identity. The
> app author did nothing the documentation told them not to do: nothing in
> `docs/guide/middleware.md`'s `static_gate` section, or anywhere else,
> mentions multi-tenancy as a hazard for static generation — only "auth
> state" and "personalized content" are called out, and subdomain-based
> tenancy in particular has no `static_gate`-shaped mitigation (a gate can
> redirect or reject, but a subdomain-scoped page needs *different bytes* per
> tenant at the *same path*, which no gate can produce).

If true, this would be exactly the framework bug class the review process
calls out by name: "a cache key missing a principal ... serves user A's
response to user B ... a framework bug class that has no app-level
analogue."

## 🧪 Reproduction attempt

Both of Autumn's static-render entry points build a **bare synthetic
request** — a path/URI only, no `Host`, no tenant header, no session cookie,
no `Authorization` header — and send it through the *full* application
router (same middleware stack a live request gets), then treat any non-2xx
response as a hard build failure:

- `autumn/src/static_gen/build.rs::render_static_routes` (`autumn build`),
  line ~293-311: `Request::builder().uri(&url).body(Body::empty())`, then
  `if !response.status().is_success() { return Err(BuildError::NonSuccessStatus { .. }) }`.
- `autumn/src/static_gen/middleware.rs::regenerate_page` (ISR background
  regeneration), same shape, same non-2xx-is-an-error rule.

Every `[tenancy] source` in `autumn/src/tenancy.rs::extract_tenant_from_parts_inner`
rejects (400/401/503) when its required signal is absent, rather than
resolving `None`/a default tenant and continuing:

| `source` | required signal | outcome when absent |
|---|---|---|
| `header` | the configured tenant header | 400 `Missing required tenant header` |
| `subdomain` | `Host` (or `ResolvedClientIdentity`) | 400 `Missing Host header for subdomain tenancy` |
| `session` | a `tenant_id` session key | 401 `Tenant ID missing from session key` (or 500 if `SessionLayer` isn't installed) |
| `jwt` | a Bearer `Authorization` header | 401 `Missing Authorization header for JWT tenancy` |

So a `#[static_get]` (or ISR) handler that extracts `Tenant` (or hits a
`CURRENT_TENANT`-scoped query, which resolves through the same extractor's
fallback path) never runs successfully during a build/regeneration: the
extractor rejects before the handler body executes, the response is non-2xx,
`render_static_routes`/`regenerate_page` treats that as a build error, and
**no file is ever written**. There is no tenant whose data could leak,
because no tenant's render ever succeeds.

Committed as a regression test rather than left as an unverified claim —
per source, so a future change that makes any one of them fail *open* is
caught immediately:

```
cargo test -p autumn-web --lib \
  app::tests::static_get_route_reading_tenant_fails_closed_for_every_tenancy_source
```

```
running 1 test
  Route /storefront -> 1 page(s)
  Rendering /storefront ...
  Route /storefront -> 1 page(s)
  Rendering /storefront ...
  Route /storefront -> 1 page(s)
  Rendering /storefront ...
  Route /storefront -> 1 page(s)
  Rendering /storefront ...
test app::tests::static_get_route_reading_tenant_fails_closed_for_every_tenancy_source ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 5575 filtered out; finished in 0.60s
```

(Full run log: `after.txt`. There is no `trunk-failure.txt` for this entry —
unlike a fix PR, a negative result's test is written to assert the *correct*
behavior and is expected to pass immediately; see the "Negative result"
outcome in Warden's process.)

## 🔎 Why this holds (root cause of the fail-closed behavior)

- `render_static_routes` / `regenerate_page` never had a "which tenant" input
  to begin with — they were built to render tenant-*agnostic* public pages
  (about pages, blog posts, docs), so they simply don't forge one.
- `Tenant::from_request_parts` (`tenancy.rs`) has a fast path that trusts an
  already-resolved `CURRENT_TENANT` task-local, but falls back to
  `extract_tenant_from_parts` when that's absent (as it always is on a bare
  synthetic request) — and every arm of that function fails closed rather
  than defaulting.
- The `tenancy` middleware itself (which normally runs ahead of the extractor
  on a live request) is likewise scoped by the tenancy `source`'s own
  extraction — there is no separate "middleware defaults, extractor
  double-checks" path that could disagree.

## 📡 Blast radius / variant sweep

- All four `[tenancy] source` values swept in one test (`header`,
  `subdomain`, `session`, `jwt`) — see the table above.
- Both static-render entry points share the same request-construction shape
  (bare URI, `Body::empty()`, no headers) and the same non-2xx-is-a-build-
  error contract, so the finding covers `autumn build` and ISR
  regeneration identically; not re-verified as a second test since the code
  path (`router.oneshot(..)` → status check) is textually identical between
  `build.rs` and `middleware.rs`.
- Not investigated further here: a *live* dynamic (non-static) route that
  mixes `#[static_get]`-style caching with tenancy is a different shape (the
  app would have to hand-roll its own cross-tenant cache, which Autumn does
  not provide a primitive for) — out of scope for this boundary.
- `docs/guide/middleware.md`'s `static_gate` section already documents the
  general "same HTML for every visitor regardless of auth state" property
  and its mitigation for *authentication*; it does not mention tenancy by
  name. Given the fail-closed result above, no doc change is required for
  correctness, but a maintainer may still want a one-line cross-reference
  from that section to this ledger entry so the next person doesn't have to
  re-derive "why doesn't `#[static_get]` + subdomain tenancy work" from
  scratch — left as a suggestion, not made here, since it's a docs call, not
  a security fix.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo test -p autumn-web --lib app::tests::static_get_route_reading_tenant_fails_closed_for_every_tenancy_source` — passes (above, `after.txt`).
- `cargo clippy -p autumn-web --all-targets -- -D warnings` and the
  cross-package `--lib --tests` compile of the consolidated
  `integration_tests` binary (`./scripts/pre-push-check.sh`'s mirror) were
  **attempted but not completed** in this session: this sandbox is severely
  CPU-throttled (a `rustc`/`clippy-driver` process accumulated only ~4
  minutes of CPU time over 45+ minutes of wall time), and re-compiling
  `autumn-macros` and the 230-module consolidated test binary from a clean
  clippy-metadata cache did not finish within a reasonable session budget.
  This is an environment-resource limitation, not a result — a maintainer
  or CI should still run the full gate before merge. Mitigating factors for
  the risk this leaves open: the change is a single new `#[tokio::test]`
  added inside an existing `#[cfg(test)] mod tests` block in
  `autumn/src/app.rs`, following the file's own established pattern
  (`i18n_bundle_layer_is_applied_to_static_route_rendering`, same file,
  immediately above) byte-for-byte in structure; it adds no new public
  API, changes no signature, and touches no production code path — so
  `check-panic-gate.sh`, `check-determinism-gate.sh`,
  `check-feature-combinations.sh`, `check-semver.sh`, and `cargo deny` are
  not implicated by its shape either way.

## 📜 Compatibility

No behavior change. No `CHANGELOG.md` entry beyond noting the new regression
test (added under `## [Unreleased]`).
