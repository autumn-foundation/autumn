# autumn-billing

Subscription billing for [autumn-web](https://github.com/autumn-foundation/autumn)
applications. Stripe is the first provider.

The plugin gives you:

- Hosted checkout and the customer portal (`POST /billing/checkout`, `POST /billing/portal`).
- A local **mirror** of customers, subscriptions and invoices, fed by signed webhooks.
- A **plan gate**: the `Entitled<R>` extractor and the `Billing` handle read the mirror only.
- **Dunning**: durable payment retries on a schedule you set, with in-app notifications.

The provider stays the source of truth. The mirror is what your handlers read.

## Mount

```rust
use autumn_billing::prelude::*;

let billing = BillingConfig::from_autumn_toml("autumn.toml")?;
let plans = PlanCatalog::new().plan(
    Plan::new("pro", "Pro", "price_123", Money::from_minor(1999, Currency::USD), BillingInterval::Month)
        .entitlement("export"),
);

autumn_web::app()
    .plugin(BillingPlugin::new().config(billing).plans(&plans))
    .run()
    .await;
```

`BillingPlugin` also accepts `.provider(..)`, `.store(..)` and `.hooks(..)`.
The default store is the database when a pool exists, else memory (lost on
restart).

## Webhook endpoint

Declare the receiver in `autumn.toml`. `SignedWebhook` verifies the
signature and CSRF exempts the path. Boot fails when the entry is missing.

```toml
[[security.webhooks.endpoints]]
name = "billing"
path = "/billing/webhook"
provider = "stripe"
secret_env = "STRIPE_WEBHOOK_SECRET"
```

## Environment

| Variable | Purpose |
| --- | --- |
| `STRIPE_SECRET_KEY` | API key (`sk_live_…` / `sk_test_…`). Production rejects test keys. |
| `STRIPE_WEBHOOK_SECRET` | Webhook signing secret (`whsec_…`). |

Environment values win over `autumn.toml` values.

## Plans

In code:

```rust
let plans = PlanCatalog::new()
    .plan(Plan::new("pro", "Pro", "price_pro", Money::from_minor(1999, Currency::USD), BillingInterval::Month)
        .entitlement("export"))
    .plan(Plan::new("team", "Team", "price_team", Money::from_minor(4999, Currency::USD), BillingInterval::Month)
        .entitlement("export")
        .entitlement("sso"));
```

Or in `autumn.toml` (merged with the code catalog):

```toml
[billing]
success_url = "/billing/success"
cancel_url = "/billing/cancel"
portal_return_url = "/account"
allow_past_due = false
grace_period_hours = 72

[[billing.plans]]
id = "pro"
name = "Pro"
price_id = "price_pro"
amount_minor = 1999
currency = "USD"
interval = "month"
entitlements = ["export"]
```

## Gate a route

```rust
struct Pro;
impl PlanRequirement for Pro {
    fn rule() -> PlanRule { PlanRule::plan("pro") }
}

#[get("/reports")]
async fn reports(_pro: Entitled<Pro>) -> &'static str { "ok" }
```

`Entitled<R>` runs before the body is read. It answers `401` without a
session user and `403` without an entitled subscription.

For a decision inside a handler:

```rust
#[get("/export")]
async fn export(billing: Billing, session: Session) -> AutumnResult<&'static str> {
    let user_id = session.get("user_id").await.unwrap_or_default();
    let view = billing
        .require(&user_id, &PlanRule::entitlement("export"))
        .await
        .map_err(BillingError::into_autumn)?;
    Ok("ok")
}
```

A user is entitled when the mirror subscription is `active` or `trialing`
(`past_due` only with `allow_past_due = true`), the price maps to a catalog
plan, and `current_period_end` plus `grace_period` (default 72h) is not in
the past. Every missing piece denies.

## Routes

| Method | Path | Auth | Response |
| --- | --- | --- | --- |
| POST | `/billing/checkout` | session | `303` to checkout, or JSON `{url, id}` with `Accept: application/json`. `404` unknown plan, `409` live subscription exists. |
| POST | `/billing/portal` | session | `303` to the portal, or JSON. `404` when the user has no customer. |
| GET | `/billing/subscription` | session | JSON `{subscription, plan, entitled}` from the mirror. |
| POST | `/billing/webhook` | Stripe signature | JSON `{accepted, outcome, event_id}`. |

`checkout` takes `{"plan": "pro"}` as JSON or `plan=pro` as a form. Redirect
URLs come from the config only.

## Dunning

When an invoice payment fails the plugin opens a `billing_dunning` row and
schedules the retry job. The default policy retries after 1, 3 and 5 days,
then cancels the subscription. Every retry goes through the store row, so a
duplicate or early job run is a no-op.

```rust
let billing = BillingConfig::from_env()
    .dunning(DunningPolicy::standard().with_on_exhausted(ExhaustionAction::MarkUnpaid));
```

After a restart the plugin re-arms every pending row from the store. No
retry is lost.

**Stripe Smart Retries**: disable them in the Stripe dashboard when the
plugin retries, or the two schedules compete. To keep Stripe's retries, use
`DunningPolicy::disabled()`: the plugin mirrors and notifies but never retries.

## Database

Five tables: `billing_customers`, `billing_subscriptions`,
`billing_invoices`, `billing_events` (the idempotency ledger) and
`billing_dunning`. The migration is `20260910000000_billing_mirror`, registered
by the plugin. Without a pool the plugin uses the in-memory store.

## Tests

```sh
cargo test -p autumn-billing
```

Postgres contract tests need Docker:

```sh
cargo test -p autumn-billing --test mirror_db -- --ignored
```
