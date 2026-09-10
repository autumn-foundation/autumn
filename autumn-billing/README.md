# autumn-billing

Subscription billing for [autumn-web](https://github.com/autumn-foundation/autumn)
applications. Stripe is the first provider.

The plugin gives you:

- Hosted checkout and the customer portal (`POST /billing/checkout`, `POST /billing/portal`).
- A local **mirror** of customers, subscriptions and invoices, fed by signed webhooks.
- A **plan gate**: the `Entitled<R>` extractor and the `Billing` handle read the mirror only.
- **Dunning**: durable payment retries on a schedule you set, with in-app notifications.

The provider stays the source of truth. The mirror is what your handlers read.

## Installation

```sh
autumn plugin add autumn-billing
```

Or add the dependency by hand:

```toml
[dependencies]
autumn-billing = "0.7.0"
```

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
restart). In production the memory store is a boot error unless
`billing.allow_memory_store_in_production = true`.

The bare `.plugin(autumn_billing::BillingPlugin::new())` boots with config
from the environment only and an empty plan catalog. Registering the plugin
twice is a no-op: the framework installs each plugin once.

## Webhook endpoint

Declare the receiver in `autumn.toml`. `SignedWebhook` verifies the
signature and CSRF exempts the path. Boot fails when the entry is missing,
or when its `provider` preset is not the one the billing provider needs.

```toml
[[security.webhooks.endpoints]]
name = "billing"
path = "/billing/webhook"
provider = "stripe"
secret_env = "STRIPE_WEBHOOK_SECRET"
```

## Configuration

Every `[billing]` key with its default. Omit a key to keep the default.

```toml
[billing]
route_prefix = "/billing"        # mount point of the routes
endpoint_name = "billing"        # name of the [[security.webhooks.endpoints]] entry
success_url = "/billing/success" # redirect after a completed checkout
cancel_url = "/billing/cancel"   # redirect after an abandoned checkout
portal_return_url = "/"          # redirect when the customer leaves the portal
allow_past_due = false           # entitle `past_due` subscriptions
grace_period_hours = 72          # entitlement past `current_period_end`
allow_memory_store_in_production = false # boot without a database pool in production

[billing.dunning]
enabled = true                       # `false`: mirror and notify, never retry
retry_delays_hours = [24, 72, 120]   # delay before retry 1, 2 and 3
on_exhausted = "cancel_subscription" # or "mark_unpaid"

[billing.stripe]
secret_key = "sk_test_..."           # STRIPE_SECRET_KEY wins
webhook_secret = "whsec_..."         # STRIPE_WEBHOOK_SECRET wins
api_base = "https://api.stripe.com"  # AUTUMN_BILLING__STRIPE__API_BASE wins; https:// in production

[[billing.plans]]                    # none by default; merged with the code catalog
id = "pro"
name = "Pro"
price_id = "price_pro"
amount_minor = 1999
currency = "USD"
interval = "month"                   # "day", "week", "month" or "year"
entitlements = ["export"]
```

Load the section with `BillingConfig::from_autumn_toml("autumn.toml")`, or
build it in code: `BillingConfig::from_env().urls(..).dunning(..)`.

## Environment

Environment values win over `autumn.toml` values. Blank values are ignored.

| Variable | Purpose |
| --- | --- |
| `STRIPE_SECRET_KEY` | API key (`sk_live_…` / `sk_test_…`). Production rejects test keys. Overrides `billing.stripe.secret_key`. |
| `STRIPE_WEBHOOK_SECRET` | Webhook signing secret (`whsec_…`). Overrides `billing.stripe.webhook_secret`. |
| `AUTUMN_BILLING__STRIPE__API_BASE` | Stripe API base URL. Overrides `billing.stripe.api_base`. Production requires `https://`. |
| `AUTUMN_BILLING__ROUTE_PREFIX` | Mount point of the routes. Overrides `billing.route_prefix`. |

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

Or as `[[billing.plans]]` entries in `autumn.toml` (see Configuration).
Both sources merge into one catalog.

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

For a decision inside a handler, `Billing::current_user` reads the user id
with the app's configured auth session key:

```rust
#[get("/export")]
async fn export(billing: Billing, session: Session) -> AutumnResult<&'static str> {
    let user_id = billing
        .current_user(&session)
        .await
        .map_err(BillingError::into_autumn)?;
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
the past. Without a known period end the grace period counts from the last
event applied to the row. Every missing piece denies.

## Routes

| Method | Path | Auth | Response |
| --- | --- | --- | --- |
| POST | `/billing/checkout` | session | `303` to checkout, or JSON `{ "url", "id" }` with `Accept: application/json`. `404` unknown plan, `409` live subscription exists. |
| POST | `/billing/portal` | session | `303` to the portal, or JSON `{ "url", "id" }`. `404` when the user has no customer. |
| GET | `/billing/subscription` | session | JSON `{subscription, plan, entitled}` from the mirror. |
| POST | `/billing/webhook` | Stripe signature | JSON `{accepted, outcome, event_id}`. |

`checkout` takes `{"plan": "pro"}` as JSON or `plan=pro` as a form. Redirect
URLs come from the config only.

## Dunning

When an invoice payment fails the plugin opens a `billing_dunning` row and
schedules the retry job. The default policy runs three retries. The first
retry runs 1 day after the failure, the second 3 days after that, the third
5 days after that (days 1, 4 and 9). After the last failed retry the plugin
cancels the subscription. Every retry goes through the store row, so a
duplicate or early job run is a no-op.

```rust
let billing = BillingConfig::from_env()
    .dunning(DunningPolicy::standard().with_on_exhausted(ExhaustionAction::MarkUnpaid));
```

After a restart the plugin re-arms every pending row from the store. No
retry is lost. A retry whose provider call fails in transport keeps its
attempt number and runs again 15 minutes later; a row left `running` by a
crashed process is reclaimed after 10 minutes.

**Stripe Smart Retries**: disable them in the Stripe dashboard when the
plugin retries, or the two schedules compete. To keep Stripe's retries, use
`DunningPolicy::disabled()`: the plugin mirrors and notifies but never retries.

## Notifications

Each step writes to the in-app notification store (`Notifications`). The
recipient is `BillingHooks::recipient_for(user_id)` for the customer's
linked user. `None` suppresses the notification.

| Kind | Payload fields |
| --- | --- |
| `billing.payment_failed` | `invoice_id`, `provider_invoice_id`, `subscription_id`, `amount_due`, `attempt` (scheduled retry number; `null` when dunning is disabled), `provider_attempt_count`, `reason` |
| `billing.payment_recovered` | `invoice_id`, `provider_invoice_id`, `subscription_id`, `amount_paid` |
| `billing.dunning_exhausted` | `invoice_id`, `provider_invoice_id`, `subscription_id`, `amount_due`, `attempts`, `action` |
| `billing.subscription_canceled` | `subscription_id`, `provider_subscription_id`, `plan_id` |

Money fields serialize as `{"minor": 1999, "currency": "USD"}`.

## Database

Five tables: `billing_customers`, `billing_subscriptions`,
`billing_invoices`, `billing_events` (the idempotency ledger) and
`billing_dunning`. The migration is `20260910203829_billing_mirror`, registered
by the plugin. A partial unique index on `billing_customers.user_id` keeps
one customer per user when two checkouts race. Applied ledger rows older than
30 days are pruned at startup. Without a pool the plugin uses the in-memory
store; production refuses it unless `allow_memory_store_in_production` is set.

## Tests

```sh
cargo test -p autumn-billing
```

Postgres contract tests need Docker:

```sh
cargo test -p autumn-billing --test mirror_db -- --ignored
```
