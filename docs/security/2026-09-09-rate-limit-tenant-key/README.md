# Cross-tenant rate-limit bucket collision (2026-09-09)

**Class:** cross-tenant interference / denial of service through an
authenticated surface, via a framework rate-limit bucket key missing the
tenant
**Surface:** `autumn_web::security::rate_limit` (`Limiter::extract_key` /
`resolve_key_and_params`) × `#[throttle(key = "principal")]`
(`__check_throttle`) × `[tenancy] enabled = true`
**Entry point:** any request under `key_strategy = "authenticated_principal"`
(global limiter) or a `#[throttle(key = "principal")]`-guarded route, in an
app with multi-tenancy enabled
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped
`KeyStrategy::AuthenticatedPrincipal`
**Status:** fixed — `autumn/src/security/rate_limit.rs`

## 🕵️ Threat model

> Against an app that follows Autumn's own documented patterns — turns on
> multi-tenancy (`[tenancy] enabled = true`), stores the session identity the
> documented way (`docs/guide/authentication.md`:
> `session.insert("user_id", user.id.to_string())`), and keys its rate
> limiter on the authenticated principal (`[security.rate_limit]
> key_strategy = "authenticated_principal"`, or a stricter
> `#[throttle(key = "principal")]` on a specific route, both documented in
> `docs/guide/rate-limiting.md`) — an attacker who is an ordinary
> authenticated user of tenant A can deny service to a user of a
> *completely unrelated* tenant B, simply by exhausting their own bucket,
> whenever the two users' session-carried principal ids happen to coincide.
> The app author did nothing the documentation told them not to do.

The coincidence is not exotic. Autumn's own horizontal-sharding guide
documents that per-tenant primary keys are shard-local, not globally unique:

> \[the destination] shard-local `BIGSERIAL` id is not copied, so re-runs
> never collide on the primary key — `docs/guide/sharding.md`

`[database.shards]` routes each tenant to one of a fixed set of physical
Postgres shards by hashing the tenant id (`docs/guide/sharding.md`, "Keys,
slots, and shards"); "several tenants share a shard: still filter by
tenant" is the documented normal case. Each shard is a full, independent
Postgres database with its own sequences. The first user account
provisioned on tenant A's shard and the first user account provisioned on
tenant B's shard are both, ordinarily, `id = 1` — and the documented OAuth
generator pattern (`docs/guide/oauth.md`) stores exactly that shard-local
`users.id` as the session's `user_id`, the same value
`docs/guide/authentication.md`'s plain-login example stores.

Every *other* `tenant_scoped` operation in Autumn resolves the tenant from
the ambient `CURRENT_TENANT` task-local automatically — repository finders,
`save()`, preload, retention sweeps, and (after 2026-09-05) `#[cached]`. The
rate limiter's `AuthenticatedPrincipal` key strategy is the one place that
idiom silently didn't apply: its bucket key was built purely from the
`RateLimitPrincipal` extension's string value, with no tenant folded in
anywhere in `security/rate_limit.rs`.

## 🧪 Reproduction

Test file: `autumn/tests/integration/rate_limit_tenant_scope.rs`

- `global_limiter_principal_bucket_isolated_by_tenant` — global tower layer,
  `key_strategy = "authenticated_principal"`.
- `per_route_throttle_principal_bucket_isolated_by_tenant` —
  `#[throttle(limit = 1, per = "1s", key = "principal")]`.

Both seed two tenants (header-sourced `[tenancy]`) whose sessions carry the
*same* `user_id` value (`"shared-id"`, modeling the shard-local-id
coincidence above), spend tenant A's one-request burst, and assert that
tenant B — who has made zero requests — still gets `200`, not `429`.

Run:

```
cargo test -p autumn-web --test integration_tests --features test-support rate_limit_tenant_scope
```

See `trunk-failure.txt` (both tests FAILED against trunk — tenant B's
request came back `429`) and `after.txt` (both PASSED after the fix).

## 🔎 Root cause

`security/rate_limit.rs`:

- `Limiter::extract_key`'s `KeyStrategy::AuthenticatedPrincipal` arm built
  `format!("principal:{}", p.0)` from the `RateLimitPrincipal` extension
  alone.
- `resolve_key_and_params` (the global tower layer) used that string
  directly as the token-bucket key, only namespaced by an optional
  path-override prefix — never by tenant.
- `__check_throttle` (the per-route `#[throttle]` guard) called the same
  `extract_key` via `extract_throttle_key` and used its return value
  directly as `bucket_key` — same gap, independent call site.
- `RateLimitPrincipal` itself is populated from `session.get(auth_session_key)`
  in three more places (`RequireAuth`, `RequireApiToken`,
  `populate_rate_limit_principal`'s and `__check_throttle`'s session
  fallbacks) — none of them tenant-aware either, but all of them feed the
  same two key-construction sites above, so fixing those two closes every
  path.

No test previously exercised two tenants sharing a principal id; every
existing `rate_limit_principal.rs` / `throttle_route.rs` test used tenancy-
free, distinct principal strings.

## 🩹 Fix

Added `tenant_qualify_bucket_key(key_strategy, raw_key)` in
`rate_limit.rs`: for an `AuthenticatedPrincipal` key that actually resolved
a principal (`raw_key` starts with `"principal:"`), it reads
`CURRENT_TENANT` and — when present — rewrites the key to
`principal:tenant=<tenant>:<id>`, preserving the `"principal:"` prefix so
`key_class_label` still reports "authenticated principal". Applied at both
call sites:

```rust
// resolve_key_and_params (global tower layer)
let tenant_qualified_key = tenant_qualify_bucket_key(self.key_strategy, &raw_key);
let key = if key_ns.is_empty() { tenant_qualified_key } else { format!("{key_ns}\0{tenant_qualified_key}") };
// raw_key itself is untouched, still passed to strip_key_prefix() for the tier hook.

// __check_throttle (per-route guard)
let bucket_key = tenant_qualify_bucket_key(key_strategy, &bucket_key);
```

Deliberately does **not** touch `extract_key`'s return value or
`strip_key_prefix`: the tier-hook contract
(`with_tier_hook(|principal_id| db.get_plan(principal_id))`,
`docs/guide/rate-limiting.md` and `rate_limit.rs`'s own doc example) still
receives the bare, unqualified principal id. Only the value used to look up
the token bucket is tenant-qualified. `Ip` and `ApiToken` keys, and the
unauthenticated IP fallback, are untouched — a bearer token is already a
unique random string and the IP fallback's cross-tenant sharing (e.g. one
NAT'd network) is an existing, accepted, documented limitation, not this
bug.

## ✅ Verification

- Reproduction: both tests FAILED before the fix (`trunk-failure.txt`),
  both PASSED after (`after.txt`).
- `cargo test -p autumn-web --test integration_tests --features
  test-support -- rate_limit throttle` — 48 passed, 0 failed, including the
  pre-existing `tier_assignment_hook_selects_correct_limits` (confirms the
  tier-hook contract is unaffected) and every `rate_limit_principal.rs` /
  `throttle_route.rs` test (confirms no behavior change for tenancy-free
  apps).
- `cargo fmt --all` — clean, no diff.
- `cargo clippy -p autumn-web --features test-support --all-targets -- -D
  warnings` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` (reduced
  `CARGO_BUILD_JOBS=2` to stay under this sandbox's memory ceiling) — clean,
  0 errors.
- `cargo clippy -p autumn-web --features
  "ws,mail,offline-sync,redis,markdown,inbound-mail,inbound-mailgun,inbound-ses,storage,tls,acme"
  --lib -- -D warnings` and `--features "plugin-sandbox,test-support" --lib`
  (the two gated-feature lanes `scripts/pre-push-check.sh` mirrors from
  `ci.yml`) — both clean.
- `cargo check --workspace --all-targets` — 0 errors; every workspace
  member (all 16 example apps, both plugins, `autumn-cli`,
  `autumn-schema-core`, `autumn-edge`, `autumn-search`,
  `autumn-storage-s3`, `autumn-cache-redis`) type-checks against the
  change. Substituted for `scripts/pre-push-check.sh`'s `cargo test
  --workspace --no-run` step, which ran this sandbox's fixed per-session
  disk allowance to 0 bytes free while linking `reddit-clone`'s test
  binaries (`rustc-LLVM ERROR: IO failure on output stream: No space left
  on device`) — a sandbox resource limit hit while linking an unrelated
  example's test binaries, not a compile error in this change; `cargo
  check` gives the same cross-package type-correctness signal without the
  disk-heavy link step. The change touches no public signature (the new
  `tenant_qualify_bucket_key` helper is a private `fn`, and
  `__check_throttle`'s signature is unchanged), so the blast radius `cargo
  test --workspace --no-run` would additionally catch is already covered
  by `cargo check --workspace` plus the full `autumn-web` test/clippy runs
  above.
- `cargo test --workspace --doc` — 0 failed across every workspace crate
  including `autumn_web`.
- `./scripts/check-panic-gate.sh` and `./scripts/check-determinism-gate.sh`
  — both self-tests and gates pass unchanged (67 / 18 modules gated, same
  counts as trunk); this change touches neither a panic-gated nor a
  determinism-gated module.
- `./scripts/check-feature-combinations.sh` (needs `cargo-hack`, not
  installed in this sandbox) and `./scripts/check-semver.sh` (needs a
  pinned `1.94.1` toolchain, not installed in this sandbox) could not run
  here — both environment gaps, not failures. Substituted by hand: the fix
  adds one private `fn tenant_qualify_bucket_key` and changes no public
  signature (`__check_throttle`'s signature is byte-for-byte unchanged,
  only its body gained one line), so there is nothing for
  `check-semver.sh` to flag; the two explicit gated-feature clippy lanes
  above already cover the feature combinations `check-feature-combinations.sh`
  would otherwise sweep for this unconditionally-compiled module.
- Re-attack: confirmed a same-tenant repeat still shares its own bucket
  (the fix partitions by tenant; it does not disable per-principal
  throttling), and that two *different* tenants with *different* principal
  ids were already isolated before and after (unaffected baseline case).

## 📡 Blast radius

- Single enforcement point per consumer: every `AuthenticatedPrincipal`-keyed
  bucket lookup goes through one of the two call sites fixed
  (`resolve_key_and_params` for the global layer, `__check_throttle` for
  `#[throttle]`); no third site independently builds this key shape.
- Applies identically to both rate-limit backends (`BucketBackend::Memory`
  and, behind the `redis` feature, `BucketBackend::Redis`): the key is
  built before backend selection.
- Checked and out of scope: `KeyStrategy::Ip` and `KeyStrategy::ApiToken`
  (see "Fix" above — neither has this tenant-collision shape); named
  `#[throttle("name")]` limiters (deliberately shared by name across
  routes, not by tenant — the fix still folds tenant in per-request, so two
  tenants calling the same named limiter now get independent shares of it,
  which is the same isolation improvement, not a new behavior class).
- Affects every released `autumn-web` version that shipped
  `KeyStrategy::AuthenticatedPrincipal`.

## 📜 Compatibility

- No macro or config *input* syntax changed.
- Behavior change, recorded in `CHANGELOG.md` under `## [Unreleased]` →
  `### Security`: upgrading resets any in-flight bucket for a
  tenant-enabled `authenticated_principal`/`principal` key (new key string
  → fresh bucket) — a one-time full-bucket refill, not a correctness
  change, and not marked `**Breaking:**` since no app-visible contract
  (config shape, response shape, `with_tier_hook` signature) changed.
- No config default changed; no migration required.
