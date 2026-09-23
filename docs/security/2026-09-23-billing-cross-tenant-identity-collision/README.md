# 2026-09-23 — autumn-billing's session identity never carries the tenant

**Class:** cross-tenant read (and, via the hosted billing portal, potential
write) through an authenticated surface, via a plugin identity key missing
the tenant
**Surface:** `autumn_billing::gate::{session_user_id, user_id_in}`, consumed
by `SessionUser` (`routes.rs`: `checkout`, `portal`, `subscription`) and
`Entitled<R>` (`gate.rs`)
**Entry point:** `GET {prefix}/subscription`, `POST {prefix}/portal`,
`POST {prefix}/checkout`, and any app handler guarded by `Entitled<R>`
**Affected:** `autumn-billing` every release through 0.7.0, for any app that
combines `BillingPlugin` with Autumn's tenancy feature
**Status:** fixed — `autumn-billing/src/gate.rs`, `hooks.rs`, `model.rs`

## 🎯 Surface

`autumn-billing`'s quick start (`autumn-billing/src/lib.rs`,
`docs/guide/billing.md`) has an app mount `BillingPlugin` and gate a route
with `Entitled<R>`, or call the plugin's own `checkout`/`portal`/
`subscription` routes, all authenticated the ordinary way: a session cookie
resolved through `state.auth_session_key()`. Every one of those entry points
funnels through one function, `gate::session_user_id` → `user_id_in`, which
reads that session value and hands it straight to the billing store as the
customer's identity (`customer_by_user`, `CustomerUpsert::with_user`) with no
other qualification.

`BillingPlugin::resolve_store` always resolves `autumn_web::db::DbState::pool(state)`
— the app's one primary/control connection pool — never a per-shard one
(`autumn-billing/src/lib.rs:281`), so the `billing_customers` mirror lives in
one physical table regardless of how many tenants or shards the app has.

## 🕵️ Threat model

Against an app that follows Autumn's own documented, ordinary patterns —
mounts `BillingPlugin` exactly as its own quick start shows, **and**
separately turns on Autumn's tenancy feature with a `#[repository(tenant_scoped,
sharded)]` `User` model, exactly as `docs/guide/sharding.md` documents — an
attacker who is an **ordinary authenticated principal of tenant B** can
obtain **tenant A's billing state**: their subscription status and plan
entitlements (`GET /subscription`), and a **live hosted Stripe billing
portal session for tenant A's customer** (`POST /portal`, from which the
attacker could view tenant A's payment methods and invoices, and cancel
tenant A's subscription) — simply by being the second tenant to reach the
billing routes right after a shard-local id it happens to share with an
already-paying tenant A user. The app author did nothing the documentation
told them not to do: nothing in `docs/guide/billing.md` mentions tenancy at
all, and nothing in `docs/guide/sharding.md` mentions billing.

**Mechanism:** `docs/guide/sharding.md`'s own resharding runbook states that
"the destination's PK sequence" after a shard move must be reset "so the next
insert there won't collide with a shard-local `BIGSERIAL` id" — i.e. each
shard runs its own independent `BIGSERIAL` sequence, starting at 1. A
`#[repository(tenant_scoped, sharded)]` `User` model's row id is therefore
only unique **within its own shard/tenant**, not across the app. Two
different tenants' ordinary signup flows routinely produce a `User` with
`id = 7` on each shard — this is not a rare coincidence, it is the guaranteed
outcome of two shards both starting their sequence at 1. `session_user_id`
stringifies that id and hands it, unqualified, to a store shared by every
tenant, so tenant A's "7" and tenant B's "7" collide into a single
`billing_customers` row the instant either one starts a checkout.

## 🧪 Reproduction

Two new tests in `autumn-billing/tests/cases/routes.rs`, run through the real
HTTP entry points (`TestClient`, never a hand-built store row for the ACT
phase):

```
cargo test -p autumn-billing --test integration -- \
  subscription_does_not_leak_across_tenants_sharing_a_shard_local_user_id \
  portal_does_not_hand_a_hosted_session_to_another_tenants_customer \
  --nocapture
```

- `subscription_does_not_leak_across_tenants_sharing_a_shard_local_user_id`:
  tenant "acme"'s user "7" completes a real checkout and the provider's
  webhook confirms an active Pro subscription (the ordinary
  `docs/guide/billing.md` flow, run end to end). A **separate** app instance
  for tenant "widgets", sharing only the same billing store (modelling the
  one shared control-plane database two shards' web tiers both talk to),
  logs in its own, entirely unrelated user — who also happens to be "7" — and
  calls `GET /billing/subscription`.
- `portal_does_not_hand_a_hosted_session_to_another_tenants_customer`: same
  setup, but tenant "widgets"' user calls `POST /billing/portal`.

**Failure on trunk** (full output in `trunk-failure.txt`, committed in the
prior RED commit on this branch):

```
thread 'cases::routes::subscription_does_not_leak_across_tenants_sharing_a_shard_local_user_id' panicked at autumn-billing/tests/cases/routes.rs:791:9:
assertion `left == right` failed: tenant widgets' own unrelated user must not inherit tenant acme's paid subscription just because their shard-local user ids happen to collide as "7": {"entitled":true,...}
  left: Bool(true)
 right: false
```

```
thread 'cases::routes::portal_does_not_hand_a_hosted_session_to_another_tenants_customer' panicked at autumn-billing/tests/cases/routes.rs:820:9:
assertion `left != right` failed: tenant widgets must not be handed a hosted portal session for tenant acme's Stripe customer just because their shard-local user ids collide as "7" (got a redirect to Some("https://portal.fake/cus_seeded_7"))
  left: 303
 right: 303
```

Tenant widgets' brand-new, never-paying user was handed tenant acme's
`entitled: true` Pro subscription, and a live redirect into tenant acme's
Stripe billing portal.

**Green after the fix** — see `after.txt`.

## 🔎 Root cause

`autumn-billing/src/gate.rs`'s `user_id_in` (feeding `session_user_id`,
consumed by `SessionUser` and `Entitled<R>`) read the session's stored
identity and returned it verbatim, with no tenant component — unlike
`autumn/src/idempotency.rs`'s storage key, `autumn/src/security/rate_limit.rs`'s
bucket key, and `autumn-macros::cached`'s cache key, which all already fold
the ambient `CURRENT_TENANT` task-local into their storage identity for
exactly this reason. `autumn-billing` had no equivalent, and nothing in its
own docs disclosed the gap the way `autumn_web::auth::impersonation`
explicitly documents its own "same-tenant only" limitation with a real
`ImpersonationPolicy` escape hatch (see Blast radius).

## 🩹 Fix

`gate.rs`'s `user_id_in` now folds `CURRENT_TENANT` into the identity via a
new `scope_identity_to_tenant`: `{tenant}\u{1}{user_id}` when a tenant is in
scope (a `\u{1}` control byte — never contained in a resolved tenant id or an
ordinary application user id — so the two components can never be
misattributed), unchanged (`user_id` verbatim) when tenancy is disabled or
the route is tenancy-exempt. Every billing store read and write already
funnels through this one function, so `checkout`, `portal`, `subscription`,
and `Entitled<R>` are all fixed together, and a non-tenant app's stored
identity — and its `billing_customers` rows — is byte-identical to before.

A plain separator rather than a hash (unlike `#[cached]`'s tenant-folding,
which hashes) was deliberate: `Customer.user_id` is also handed to
`BillingHooks::recipient_for`, documented as "the application user id" and
default-implemented as `user_id.parse::<i64>()`. Hashing would have made the
original id unrecoverable and silently broken every tenancy-enabled app's
notifications. The default `recipient_for` now strips the tenant prefix
(splitting on the last separator) before parsing, so it is unchanged in
behavior; see Compatibility for what a custom hook implementation needs.

## ✅ Verification

```
cargo test -p autumn-billing --test integration          # 164 passed, 0 failed
cargo test -p autumn-billing --test mirror_db             # 24 passed, 3 ignored (Docker)
cargo test -p autumn-billing --test dunning_close_scan_profile
cargo test -p autumn-billing --test dunning_rearm_pending_profile
cargo fmt -p autumn-billing -- --check
cargo clippy -p autumn-billing --all-targets -- -D warnings
cargo check -p autumn-billing --all-targets
./scripts/check-panic-gate.sh          # 83 request-path modules gated
./scripts/check-determinism-gate.sh    # 20 modules gated
./scripts/check-plugin-surface.sh      # plugin API contract unchanged
./scripts/check-changelog-fragments.sh # 34 fragments parse; CHANGELOG.md untouched
./scripts/check-migration-guides.sh    # breaking entry links its guide
```

`./scripts/pre-push-check.sh`'s full-workspace `cargo test --workspace --no-run`
step hit this sandbox's disk allowance mid-build (compiling every example,
benchmark and plugin from a cold cache): the linker crashed with `Bus error`
on an unrelated example's test binary (`reddit-clone`'s
`commentable_pg_integration`) after the filesystem read 0 bytes free — an
environment resource limit, not a compile error in this change (`reddit-clone`
does not depend on `autumn-billing`). The narrower gates above cover the same
ground for a change confined to one leaf crate: `cargo check -p autumn-billing
--all-targets` (the cross-package compile-break class `pre-push-check.sh`
exists for) and the full `autumn-billing` test/clippy/fmt suite, both clean.

**CI caught a real gap in this fix**, not a flake: the "Migration guide
coverage" check's `check-docs-symbols.sh` gate failed on the migration
guide's own suggested diff for a custom `recipient_for` override, because it
referenced `autumn_billing::gate::TENANT_IDENTITY_SEPARATOR` — a
`pub(crate)` constant a downstream app cannot actually name. The migration
guide's advice would not have compiled for the exact reader it was written
for. Fixed by adding a public `autumn_billing::gate::strip_tenant_scope`
helper (used by the default `recipient_for` implementation too, replacing
its own inline `rsplit`) and pointing the migration guide's diff at that
instead of the raw separator, which also avoids exposing the separator
character as public API. Verified: `./scripts/check-docs-symbols.sh` now
reports 0 defects, and the full `autumn-billing` test/fmt/clippy suite
above stayed green after the change.

Re-attack attempts after the fix: reran both reproduction tests with the
tenant scope removed entirely (single-tenant mode) to confirm the identity
is byte-identical to pre-fix (no behavior change for non-tenant apps); reran
with the two tenants swapped (widgets pays, acme is the unrelated user) to
confirm the fix is symmetric, not an artifact of seeding order.

## 📡 Blast radius

Swept the same shape ("a framework or plugin identity/cache/rate-limit key
built from a session or request value that is not guaranteed unique across
tenants") across the codebase:

| Site | Result |
| --- | --- |
| `autumn/src/idempotency.rs` | Already folds `CURRENT_TENANT` (2026-09-02 finding) |
| `autumn/src/security/rate_limit.rs` | Already folds `CURRENT_TENANT` (2026-09-09 finding) |
| `autumn-macros::cached` (`#[cached]`) | Already folds `CURRENT_TENANT` (2026-09-05 finding) |
| `autumn/src/cache/fragment.rs` (`cache_fragment*`) | Already folds `CURRENT_TENANT` (2026-09-21 finding) |
| `autumn_web::auth::impersonation::begin_impersonation` | Explicitly documented "same-tenant only", with a real `ImpersonationPolicy` hook for multi-tenant apps — not a silent gap, no fix needed |
| `autumn-admin-plugin` (`feature_flags.rs`, `experiments.rs`) | Manage flag/experiment **definitions** only, no per-user assignment state keyed by a session identity — not applicable |
| `autumn/src/experiments.rs` (`experiment_bucket`, A/B assignment) | Keyed by `actor_id` with no tenant component, so two tenants' colliding ids land in the same experiment bucket — but this is analytics bucketing, not an authorization or data-access decision, so it does not clear the severity floor. Flagged here for a human to file as a follow-up hardening issue if desired; not fixed in this PR |
| `autumn-storage-s3` | No session/user identity handling at all — key naming is entirely the app's own responsibility |
| `autumn-billing`'s own `notify.rs` (`BillingHooks::recipient_for`) | Depends on the same `Customer.user_id` this PR changes — fixed in the same commit (default impl now strips the tenant prefix) |

Affected versions: every `autumn-billing` release through 0.7.0, for the
(likely rare, since `docs/guide/billing.md` never mentions tenancy) case of
an app combining `BillingPlugin` with tenancy.

## 📜 Compatibility

- **Behavior change, tenancy-enabled apps only:** `Customer.user_id`, and
  therefore the string `BillingHooks::recipient_for` receives, is now
  `{tenant}\u{1}{user_id}` rather than the bare session id. Both
  `Customer.user_id`'s doc comment and `BillingHooks::recipient_for`'s are
  updated to say so.
- The **default** `recipient_for` implementation is unaffected: it now
  strips the tenant prefix before parsing, so `user_id.parse::<i64>()`
  succeeds exactly as before.
- A custom `BillingHooks::recipient_for` override that assumed the bare
  session id needs the same one-line change: call the new
  `autumn_billing::gate::strip_tenant_scope(user_id)` before parsing — but
  only if the app runs `BillingPlugin` under tenancy, which
  `docs/guide/billing.md` never documented as supported in the first place.
- No config default changed, no route status code or response shape
  changed, no existing public function signature changed. The only new
  public API is `autumn_billing::gate::strip_tenant_scope`, the recovery
  half of the identity scoping — the separator itself
  (`TENANT_IDENTITY_SEPARATOR`) stays `pub(crate)`.
- Non-tenant apps (tenancy disabled, the overwhelming majority of
  `autumn-billing` users per the docs) see byte-identical behavior.
- `CHANGELOG.md`: fragment added at `changelog.d/billing-tenant-scoped-identity.md`.

## 🗂 Ledger

This directory: `trunk-failure.txt` (the RED run, committed in the prior
commit on this branch), `after.txt` (the GREEN run). No `queries.txt` or
`manifest-*.json`: the reproduction runs against `MemoryBillingStore`
(the plugin's own store-agnostic contract, matched by `store_contract.rs`
against both the memory and Postgres backends) through `TestClient`, not
against `autumn routes audit` or a live query witness — the vulnerability is
in application-level identity scoping, not in a query missing a tenant
predicate at the SQL layer (the store already binds whatever identity string
it is given correctly; the identity string itself was the bug).
