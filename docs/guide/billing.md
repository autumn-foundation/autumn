# Billing: Stripe subscriptions and dunning (`autumn-billing`)

`autumn-billing` turns an Autumn app into a business: hosted checkout, the
customer portal, a webhook-fed local mirror of billing state, a plan gate for
handlers, and durable retries for failed payments. Stripe ships first behind a
provider-neutral `BillingProvider` trait.

- **Crate:** `autumn-billing`
- **Issue:** [#1190](https://github.com/autumn-foundation/autumn/issues/1190)
- **Builds on:** `SignedWebhook` (verified intake), `#[job]` (durable
  retries), the notification store (#1148), `plugin_migrations` (mirror tables).

---

## 1. Install

```bash
autumn plugin add autumn-billing
```

Set the keys in the environment:

```bash
export STRIPE_SECRET_KEY=sk_live_...
export STRIPE_WEBHOOK_SECRET=whsec_...
```

Declare the webhook receiver in `autumn.toml`. `SignedWebhook` verifies the
signature and rejects replays; CSRF exempts the path because it is declared
here:

```toml
[[security.webhooks.endpoints]]
name = "billing"
path = "/billing/webhook"
provider = "stripe"
secret_env = "STRIPE_WEBHOOK_SECRET"
```

Boot fails with this exact snippet when the endpoint is missing.

## 2. Mount

```rust,ignore
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

Plans can also live in TOML:

```toml
[billing]
success_url = "/billing/success"
cancel_url = "/billing/cancel"
portal_return_url = "/account"

[[billing.plans]]
id = "pro"
name = "Pro"
price_id = "price_123"
amount_minor = 1999
currency = "USD"
interval = "month"
entitlements = ["export"]
```

The plugin registers its migration through `plugin_migrations`; the next
`autumn migrate` creates `billing_customers`, `billing_subscriptions`,
`billing_invoices`, `billing_events` and `billing_dunning`.

## 3. Routes

| Method | Path                    | Purpose                                                  |
|--------|-------------------------|----------------------------------------------------------|
| POST   | `/billing/checkout`     | `{ "plan": "pro" }` → 303 to hosted checkout (JSON: `{url}`) |
| POST   | `/billing/portal`       | 303 to the hosted billing portal                         |
| GET    | `/billing/subscription` | Current subscription + plan from the local mirror        |
| POST   | `/billing/webhook`      | Signed provider webhook receiver                         |

Checkout and portal need a logged-in session (401 otherwise). Checkout returns
409 when the user already has a live subscription. Redirect URLs come from
config only. `GET /billing/subscription` never calls the provider.

## 4. Gate a handler

```rust,ignore
use autumn_billing::prelude::*;

struct Pro;
impl PlanRequirement for Pro {
    fn rule() -> PlanRule { PlanRule::plan("pro") }
}

#[get("/reports")]
async fn reports(_pro: Entitled<Pro>) -> &'static str { "ok" }
```

`Entitled<R>` runs before the body is read: 401 without a session user, 403
without an entitled subscription. Inside a handler or a `Policy`, use the
service form:

```rust,ignore
async fn export(billing: Billing, session: Session) -> AutumnResult<&'static str> {
    let user = session.get("user_id").await.ok_or_else(|| AutumnError::unauthorized_msg("login"))?;
    billing.require(&user, &PlanRule::entitlement("export")).await.map_err(BillingError::into_autumn)?;
    Ok("csv")
}
```

Entitled means: status `active` or `trialing` (`past_due` only with
`allow_past_due`), the price maps to a catalog plan, and
`current_period_end + grace_period` is not in the past. The grace period
(default 72 h) bounds how long a dead webhook keeps a lapsed customer entitled.
Everything else is denied.

## 5. The mirror and idempotency

The provider is the source of truth. Every webhook event is claimed in
`billing_events` by its provider event id before it is applied, so a
redelivered event applies once. Upserts carry the provider event time: an older
event never overwrites newer state, a same-instant conflict resolves to the
higher-ranked status, and a canceled subscription is terminal. A failure after
the claim releases it, so the provider's redelivery applies again.

## 6. Dunning

`invoice.payment_failed` opens a row in `billing_dunning` and enqueues the
`autumn_billing_dunning_retry` job at the first retry time. The row is the
schedule; the job only carries the invoice id and reads the row, so a duplicate
run, an early run, or a restart is safe. On startup the plugin re-arms every
open row. Each retry asks the provider to collect the invoice again with a
stable idempotency key:

- paid → `recovered`, notification `billing.payment_recovered`
- declined → next retry, notification `billing.payment_failed`
- declined after the last retry → subscription `unpaid` in the mirror first,
  then `cancel_subscription` at the provider (`ExhaustionAction::MarkUnpaid`
  skips the cancel), notification `billing.dunning_exhausted`
- transport error → the job returns an error and the runtime retries with
  backoff; the schedule does not advance

Defaults: retries after 1, 3 and 5 days. Stripe's own Smart Retries would run
in parallel; turn them off in the Stripe dashboard, or keep them and mount
`DunningPolicy::disabled()` so the plugin only mirrors and notifies.

Notifications go to the in-app notification store for the customer's user id.
Map a non-numeric user id with `BillingHooks::recipient_for`.

## 7. Money

`Money { minor: i64, currency: Currency }`. Amounts are integer minor units;
`Money::from_decimal` and `to_decimal` bridge to `rust_decimal::Decimal`
exactly. The crate contains no `f32` or `f64` (a test enforces it).

## 8. Testing

`cargo test -p autumn-billing` runs every flow in process with a fake provider
and signed Stripe fixtures. `cargo test -p autumn-billing --test mirror_db --
--ignored` runs the database store against a Postgres testcontainer.
