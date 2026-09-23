### Security

- **`autumn-billing` now folds the ambient resolved tenant into the identity
  it keys its billing store on:** `SessionUser`/`Entitled<R>` (and therefore
  `checkout`, `portal`, `subscription`) read the session's stored user id and
  handed it, unqualified, to `customer_by_user`/`.with_user()` against
  `BillingPlugin`'s one shared store — which always resolves the app's
  primary connection pool, never a per-shard one, regardless of how many
  tenants or shards the app has. `docs/guide/sharding.md`'s own resharding
  runbook documents that a sharded table's primary key is a **shard-local**
  `BIGSERIAL` — every shard hands out its own `1, 2, 3, …` independently — so
  a `#[repository(tenant_scoped, sharded)]` `User` model on two different
  tenants routinely produces the identical numeric id. An app combining
  `BillingPlugin` with tenancy, exactly as each feature's own guide
  documents independently, could have one tenant's user read (and, through
  the hosted Stripe portal, potentially manage) **another tenant's
  subscription** the instant both users' ids collided. `session_user_id` now
  folds `CURRENT_TENANT` into the identity unconditionally ahead of the
  session's own id. Apps without tenancy enabled compute the same identity as
  before — `CURRENT_TENANT` resolves to `None` in both cases.
  **Breaking:** for tenancy-enabled apps only, `Customer.user_id`, and
  therefore whatever `BillingHooks::recipient_for` receives, is now
  `{tenant}\u{1}{user_id}` rather than the bare session id. See the
  [migration guide](docs/migrations/next.md#autumn-billing-customeruser_id-is-tenant-scoped-under-tenancy).
  The default
  `recipient_for` strips the tenant prefix before parsing, so it is
  unaffected; a custom override needs the same one-line change. See
  `docs/security/2026-09-23-billing-cross-tenant-identity-collision/`.
