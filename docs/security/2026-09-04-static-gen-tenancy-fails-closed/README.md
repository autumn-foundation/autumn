# `#[static_get]` × multi-tenancy: negative result — fails closed (2026-09-04)

**Class:** hypothesis for cross-tenant read through a documented feature
composition, with no app-level analogue
**Surface:** `autumn_web::static_gen` (`render_static_routes` / `autumn build`,
and ISR's `regenerate_page`) × `[tenancy] enabled = true`
**Entry point investigated:** `#[static_get]` handler reading `autumn_web::tenancy::Tenant`
rendered by `autumn build` or an ISR background regeneration, on a route
exempted from `tenancy_middleware` via `[tenancy].public_paths` (so the
extractor's own fallback resolution runs, not just the middleware's earlier
call to the same function)
**Status:** **negative result** for the hypothesis as stated (a fresh
build/regeneration never bakes cross-tenant data into `dist/`) — no fix
required there. A narrower, related limitation was surfaced during review
(see "Known limitation" below) and is documented, not fixed: it requires an
operational build/serve config mismatch to trigger, not a code path this
framework can reach on its own. Regression tests committed:
`autumn/src/app.rs::tests::static_get_route_reading_tenant_fails_closed_for_every_tenancy_source`
and `autumn/src/app.rs::tests::failed_rebuild_leaves_preexisting_static_file_untouched`

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
`Tenant`), *something* has to decide which tenant's data gets frozen into that
one shared file — and every future request to that path, from every tenant,
would be served whatever got baked in.

A `#[repository(tenant_scoped)]` query is a related but distinct case, not
exercised by the test in this entry: `repository.rs` (`__autumn_m2m_tenant_scope`,
~line 853) already documents and codifies the same fail-closed contract for a
tenant-scoped repository used with `CURRENT_TENANT` unset — "the same 'no
tenant context was established' failure the derived queries raise, so the
[surface] fails closed rather than writing unscoped" — and
`repository_commit_hooks.rs` carries matching comments. Taken at face value
this covers the repository-query shape of the hypothesis too, but it wasn't
independently re-verified with a build/ISR-shaped repro here; a maintainer
who wants that confirmed should ask for it as a follow-up.

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

So a `#[static_get]` (or ISR) handler that extracts `Tenant` never runs
successfully during a build/regeneration: extraction rejects before the
handler produces a body, the response is non-2xx, `render_static_routes`/
`regenerate_page` treats that as a build error, and **no file is ever
written**. There is no tenant whose data could leak, because no tenant's
render ever succeeds.

**Which call site actually rejects, and why the test pins the route to
`public_paths`.** On an ordinary (non-public) route, `tenancy_middleware`
runs first and calls `extract_tenant_from_parts` itself, ahead of the
handler — so a naive test would only prove the *middleware* fails closed,
and would keep passing even if `Tenant::from_request_parts`'s own fallback
call to the same function later started resolving a default tenant instead
(exactly the regression this entry exists to catch, and exactly the shape
that matters for an app that lists the route in `[tenancy].public_paths` —
common for a public storefront page — while its handler still reads
`Tenant` directly. Note this is unrelated to the `#[public]` macro
attribute: per `docs/guide/route-auth-coverage.md`, `#[public]` is a
compile-time route-audit marker only and injects no runtime behavior; it
does not touch `tenancy_middleware` at all. Only listing a path in
`public_paths` makes the middleware skip tenant resolution — an earlier
draft of this entry conflated the two, corrected after Codex's review
flagged it). Codex's automated review on the PR caught this gap in the
first version of the test. The fix: the test config lists
`/storefront` in `config.tenancy.public_paths`, so `is_public_path` makes
`tenancy_middleware` skip tenant resolution entirely (`return
next.run(request).await` with `CURRENT_TENANT` never `.scope()`'d) and the
request reaches `tenant_page`. There, `CURRENT_TENANT.try_with(..)` returns
`Err` (no scope was ever entered — a different outcome from `Ok(None)`,
which is what a *resolved-but-absent* tenant would look like), so the `if
let Ok(Some(tenant_id))` fast path in `Tenant::from_request_parts` does not
match, and the extractor falls through to its own
`extract_tenant_from_parts` call — the exact code path the test is meant to
pin.

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

(Full run log: `after.txt`, captured from the *pre-Codex-fix* version of the
test — identical in setup and assertions except for the `public_paths` line
described below, which does not change the router's behavior for these
synthetic requests: `tenancy_middleware` itself calls the same
`extract_tenant_from_parts` and gets the same rejection either way, so the
build/ISR outcome — and this log — is unaffected. This session's sandbox
was too CPU-throttled to complete a fresh compile after the fix (see
Verification); the reasoning for why the fixed version still passes is laid
out below and rests on reading `is_public_path`, `tenancy_middleware`, and
`Tenant::from_request_parts` directly, not on a re-run. There is no
`trunk-failure.txt` for this entry — unlike a fix PR, a negative result's
test is written to assert the *correct* behavior and is expected to pass
immediately; see the "Negative result" outcome in Warden's process.)

## 🔎 Why this holds (root cause of the fail-closed behavior)

- `render_static_routes` / `regenerate_page` never had a "which tenant" input
  to begin with — they were built to render tenant-*agnostic* public pages
  (about pages, blog posts, docs), so they simply don't forge one.
- `Tenant::from_request_parts` (`tenancy.rs`) has a fast path that trusts an
  already-resolved `CURRENT_TENANT` task-local, but falls back to its own
  call to `extract_tenant_from_parts` when that's absent (`CURRENT_TENANT.try_with`
  returns `Err` — no scope was ever entered, as on the `public_paths`-exempted
  synthetic request the test drives) — and every arm of that function fails
  closed rather than defaulting.
- `tenancy_middleware` (which normally runs ahead of the extractor on a live,
  non-public route) calls the very same `extract_tenant_from_parts` function
  itself before ever reaching the handler — there is no separate "middleware
  defaults, extractor double-checks" path that could disagree between the two
  call sites, which is also why exempting the route from the middleware (via
  `public_paths`) is what it takes to prove the *extractor's* copy of the
  check independently.

## ⚠️ Known limitation surfaced by review: a failed rebuild doesn't invalidate a pre-existing file

Both tests above start from an **empty** `dist/`, which only proves a fresh
build/regeneration never *writes* cross-tenant data. Codex's review on the
PR correctly pointed out that this says nothing about a
`dist/<route>/index.html` that **already exists** from an earlier,
successful render:

- `render_static_routes` (`build.rs`) stages every route into a sibling
  `dist.staging` directory and only removes/replaces the real `dist_dir`
  once **every** route in the build has rendered successfully (the loop over
  `results` returns on the first `Err`, before the "remove old dist, rename
  staging → dist" swap ever runs). On failure, `dist_dir` — including
  anything already in it — is never touched.
- ISR's `regenerate_page` (`static_gen/middleware.rs`) has the same shape: it
  returns `Err` on a non-2xx response before ever calling
  `std::fs::write`/`rename`, so a route whose regeneration keeps failing
  keeps serving its last successfully-rendered file forever. This is the
  documented stale-while-revalidate contract working as designed — the
  whole point of ISR is that a broken rebuild doesn't take the page down.

Added `failed_rebuild_leaves_preexisting_static_file_untouched` to prove
this directly: seed `dist/storefront/index.html` with a sentinel value (as
if an earlier build had captured it), point tenancy at the same
always-fails-closed config as the test above, call `render_static_routes`
again, and assert both that the rebuild still fails **and** that the
sentinel file is byte-for-byte unchanged afterward.

**Why this doesn't reopen the cross-tenant hypothesis.** Nothing in Autumn
ever writes a tenant-scoped response into `dist/` without a resolved
tenant — that is exactly what the sweep above rules out for every
`[tenancy] source` and both render entry points. So there is no code path
*within this framework* that bakes the wrong tenant's data into that
sentinel file to begin with. The only realistic way a tenant-mismatched
file lands in `dist/` is operational: building with a different
`[tenancy]` config than the one that serves requests (e.g. `autumn build`
run once in CI with tenancy disabled or pointed at a different source than
production uses). That is a build/serve consistency problem for the
operator, not a bug in Autumn's request handling.

**What this does leave on the table.** Once a tenant-mismatched file exists
in `dist/` by whatever means, this test shows Autumn has no mechanism to
detect, invalidate, or expire it — a subsequent tenant-resolution failure
preserves it indefinitely, with no operator-visible signal beyond a
`tracing::warn!`/`tracing::error!` log line for ISR (`autumn build` at
least fails the whole CI step loudly). A maintainer may want to consider,
as a separate hardening item (not a security fix, since no attacker action
is involved): surfacing a stronger signal — a metric, an actuator flag, or
a build-time check that a `#[static_get]` route's rendered `Content-Type`/
response doesn't silently vary across `[tenancy]` configurations — for
detecting this class of build/serve drift. Not implemented here; this is a
suggestion for follow-up, matching the "findings, not fixes, for
operational hazards" boundary in Warden's process.

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
- `cargo test -p autumn-web --lib app::tests::static_get_route_reading_tenant_fails_closed_for_every_tenancy_source` — passes pre-fix (above, `after.txt`). The automated Codex reviewer on PR #2505 then made two passes: first, that the pre-fix version only proved `tenancy_middleware` fails closed, not the `Tenant` extractor's own fallback call on a route exempted from that middleware (the shape that matters for a route listed in `[tenancy].public_paths` whose handler still reads `Tenant`) — fixed by adding `/storefront` to `config.tenancy.public_paths` in the test. Second, that the fix's own write-up incorrectly implied `#[public]` (a compile-time route-audit marker with no runtime effect, per `docs/guide/route-auth-coverage.md`) was equivalent to `public_paths` membership — corrected throughout this entry to reference `public_paths` only. See the reproduction section above for why the fixed test still passes by inspection. A fresh confirming run could not be completed in this session (see next bullet).
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
