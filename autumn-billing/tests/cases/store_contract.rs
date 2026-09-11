//! Contract suite for [`BillingStore`].
//!
//! Every property is one `pub async fn` over `&dyn BillingStore`, so the
//! same suite runs against `MemoryBillingStore` here and against
//! `DbBillingStore` in `tests/mirror_db.rs` (which includes this file by
//! path). Each property uses its own id prefix, so the whole suite can run
//! on one shared store through [`run_contract`].
//!
//! Ids are fixed strings: the store never mints ids.

#![allow(dead_code, reason = "each binary uses a subset of the suite")]

use std::time::Duration;

use autumn_billing::model::{DunningAttempt, DunningState, InvoiceStatus};
use autumn_billing::store::{CustomerUpsert, EventClaim, InvoiceUpsert, SubscriptionUpsert};
use autumn_billing::{
    BillingStore, Currency, MemoryBillingStore, Money, PlanId, ProviderId, SubscriptionStatus,
};
use chrono::{DateTime, TimeZone, Utc};

/// A fixed instant plus `secs`.
pub fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0)
        .single()
        .expect("valid timestamp")
}

const STALE: Duration = Duration::from_secs(300);

/// Insert a customer and return its local id.
async fn seed_customer(store: &dyn BillingStore, prefix: &str, user: Option<&str>) -> String {
    let mut upsert = CustomerUpsert::new(
        format!("{prefix}-cust"),
        "stripe",
        format!("cus_{prefix}"),
        at(0),
    );
    if let Some(user) = user {
        upsert = upsert.with_user(user);
    }
    store.upsert_customer(upsert).await.expect("customer").id
}

fn sub_upsert(
    prefix: &str,
    customer_id: &str,
    key: &str,
    status: SubscriptionStatus,
    occurred: i64,
) -> SubscriptionUpsert {
    SubscriptionUpsert::new(
        format!("{prefix}-sub-{key}-{occurred}"),
        customer_id,
        format!("sub_{prefix}_{key}"),
        status,
        at(occurred),
        at(1000),
    )
    .with_price("price_pro")
    .with_plan(PlanId::new("pro"))
    .with_period_end(at(occurred + 86_400))
}

fn invoice_upsert(
    prefix: &str,
    customer_id: &str,
    key: &str,
    status: InvoiceStatus,
    occurred: i64,
) -> InvoiceUpsert {
    InvoiceUpsert::new(
        format!("{prefix}-inv-{key}-{occurred}"),
        customer_id,
        format!("in_{prefix}_{key}"),
        status,
        Money::from_minor(1999, Currency::USD),
        Money::zero(Currency::USD),
        at(occurred),
        at(1000),
    )
    .with_attempt_count(1)
}

fn dunning(
    prefix: &str,
    key: &str,
    attempt: i64,
    next: i64,
    state: DunningState,
) -> DunningAttempt {
    DunningAttempt::new(
        format!("{prefix}-inv-{key}"),
        format!("{prefix}-cust"),
        attempt,
        at(next),
        state,
        at(0),
    )
}

// ── Ledger ──────────────────────────────────────────────────────────────

/// A claim is granted once; a finished claim stays a duplicate forever.
pub async fn ledger_claim_finish_and_release(store: &dyn BillingStore) {
    let id = "ledger-a-evt";
    assert_eq!(
        store.claim_event(id, "k", at(0), STALE).await.unwrap(),
        EventClaim::Claimed
    );
    assert_eq!(
        store.claim_event(id, "k", at(1), STALE).await.unwrap(),
        EventClaim::Duplicate,
        "an in-flight claim blocks redelivery"
    );
    store.release_event(id).await.unwrap();
    assert_eq!(
        store.claim_event(id, "k", at(2), STALE).await.unwrap(),
        EventClaim::Claimed,
        "a released claim is re-claimable"
    );
    store.finish_event(id, at(3)).await.unwrap();
    assert_eq!(
        store.claim_event(id, "k", at(4), STALE).await.unwrap(),
        EventClaim::Duplicate
    );
    store.release_event(id).await.unwrap();
    assert_eq!(
        store.claim_event(id, "k", at(5), STALE).await.unwrap(),
        EventClaim::Duplicate,
        "release never drops an applied row"
    );
}

/// A claim in `processing` older than `stale_after` is re-claimable.
pub async fn ledger_stale_claim_is_reclaimable(store: &dyn BillingStore) {
    let id = "ledger-b-evt";
    assert_eq!(
        store.claim_event(id, "k", at(0), STALE).await.unwrap(),
        EventClaim::Claimed
    );
    assert_eq!(
        store.claim_event(id, "k", at(299), STALE).await.unwrap(),
        EventClaim::Duplicate,
        "younger than stale_after"
    );
    assert_eq!(
        store.claim_event(id, "k", at(301), STALE).await.unwrap(),
        EventClaim::Claimed,
        "older than stale_after"
    );
    assert_eq!(
        store.claim_event(id, "k", at(302), STALE).await.unwrap(),
        EventClaim::Duplicate,
        "the re-claim refreshed claimed_at"
    );
}

/// `applied_event_count` counts finished claims only.
pub async fn ledger_applied_count(store: &dyn BillingStore) {
    let before = store.applied_event_count().await.unwrap();
    store
        .claim_event("ledger-c-1", "k", at(0), STALE)
        .await
        .unwrap();
    store
        .claim_event("ledger-c-2", "k", at(0), STALE)
        .await
        .unwrap();
    store
        .claim_event("ledger-c-3", "k", at(0), STALE)
        .await
        .unwrap();
    store.finish_event("ledger-c-1", at(1)).await.unwrap();
    store.finish_event("ledger-c-2", at(1)).await.unwrap();
    store.finish_event("ledger-c-2", at(2)).await.unwrap();
    store.finish_event("ledger-c-missing", at(2)).await.unwrap();
    assert_eq!(store.applied_event_count().await.unwrap(), before + 2);
}

// ── Customers ───────────────────────────────────────────────────────────

/// The upsert key is the provider customer id; `new_id` is used on insert only.
pub async fn customer_upsert_keyed_by_provider_id(store: &dyn BillingStore) {
    let first = store
        .upsert_customer(
            CustomerUpsert::new("cust-a-1", "stripe", "cus_cust_a", at(0)).with_email("a@x.test"),
        )
        .await
        .unwrap();
    assert_eq!(first.id, "cust-a-1");
    assert_eq!(first.provider, "stripe");
    assert_eq!(first.provider_customer_id, ProviderId::new("cus_cust_a"));
    assert_eq!(first.email.as_deref(), Some("a@x.test"));
    assert_eq!(first.user_id, None);
    assert_eq!(first.created_at, at(0));
    assert_eq!(first.updated_at, at(0));

    let again = store
        .upsert_customer(CustomerUpsert::new(
            "cust-a-2",
            "stripe",
            "cus_cust_a",
            at(5),
        ))
        .await
        .unwrap();
    assert_eq!(again.id, "cust-a-1", "the existing row is kept");
    assert_eq!(
        again.email.as_deref(),
        Some("a@x.test"),
        "None keeps the email"
    );
    assert_eq!(again.created_at, at(0));
    assert_eq!(again.updated_at, at(5));
    assert!(store.customer_by_id("cust-a-2").await.unwrap().is_none());
}

/// A user link is set once and never replaced; an email is replaced.
pub async fn customer_link_never_replaced_email_replaced(store: &dyn BillingStore) {
    let unlinked = store
        .upsert_customer(CustomerUpsert::new(
            "cust-b-1",
            "stripe",
            "cus_cust_b",
            at(0),
        ))
        .await
        .unwrap();
    assert_eq!(unlinked.user_id, None);
    let linked = store
        .upsert_customer(
            CustomerUpsert::new("cust-b-2", "stripe", "cus_cust_b", at(1)).with_user("u-b1"),
        )
        .await
        .unwrap();
    assert_eq!(linked.user_id.as_deref(), Some("u-b1"), "first link is set");
    let relinked = store
        .upsert_customer(
            CustomerUpsert::new("cust-b-3", "stripe", "cus_cust_b", at(2))
                .with_user("u-b2")
                .with_email("new@x.test"),
        )
        .await
        .unwrap();
    assert_eq!(
        relinked.user_id.as_deref(),
        Some("u-b1"),
        "link is never replaced"
    );
    assert_eq!(
        relinked.email.as_deref(),
        Some("new@x.test"),
        "email is replaced"
    );
    assert_eq!(relinked.id, "cust-b-1");
}

/// Lookups by local id, user id and provider id agree.
pub async fn customer_lookups(store: &dyn BillingStore) {
    let row = store
        .upsert_customer(
            CustomerUpsert::new("cust-c-1", "stripe", "cus_cust_c", at(0)).with_user("u-c"),
        )
        .await
        .unwrap();
    assert_eq!(
        store.customer_by_id("cust-c-1").await.unwrap(),
        Some(row.clone())
    );
    assert_eq!(
        store.customer_by_user("u-c").await.unwrap(),
        Some(row.clone())
    );
    assert_eq!(
        store
            .customer_by_provider_id(&ProviderId::new("cus_cust_c"))
            .await
            .unwrap(),
        Some(row)
    );
    assert_eq!(store.customer_by_id("cust-c-none").await.unwrap(), None);
    assert_eq!(store.customer_by_user("u-c-none").await.unwrap(), None);
    assert_eq!(
        store
            .customer_by_provider_id(&ProviderId::new("cus_cust_c_none"))
            .await
            .unwrap(),
        None
    );
}

// ── Subscriptions ───────────────────────────────────────────────────────

/// A newer event applies; an older one is stale and returns the stored row.
pub async fn subscription_newer_wins_older_stale(store: &dyn BillingStore) {
    let customer = seed_customer(store, "sub-a", None).await;
    let first = store
        .upsert_subscription(sub_upsert(
            "sub-a",
            &customer,
            "k",
            SubscriptionStatus::Trialing,
            100,
        ))
        .await
        .unwrap();
    assert!(first.is_applied());
    let first = first.into_inner();
    assert_eq!(first.id, "sub-a-sub-k-100");
    assert_eq!(first.status, SubscriptionStatus::Trialing);
    assert_eq!(first.last_event_at, at(100));
    assert_eq!(first.current_period_end, Some(at(100 + 86_400)));
    assert_eq!(first.created_at, at(1000));

    let mut newer = sub_upsert("sub-a", &customer, "k", SubscriptionStatus::Active, 200);
    newer.quantity = 3;
    newer.cancel_at_period_end = true;
    newer.now = at(2000);
    let newer = store.upsert_subscription(newer).await.unwrap();
    assert!(newer.is_applied());
    let newer = newer.into_inner();
    assert_eq!(newer.id, "sub-a-sub-k-100", "the local id is stable");
    assert_eq!(newer.status, SubscriptionStatus::Active);
    assert_eq!(newer.quantity, 3);
    assert!(newer.cancel_at_period_end);
    assert_eq!(newer.last_event_at, at(200));
    assert_eq!(newer.created_at, at(1000));
    assert_eq!(newer.updated_at, at(2000));

    let late = store
        .upsert_subscription(sub_upsert(
            "sub-a",
            &customer,
            "k",
            SubscriptionStatus::PastDue,
            150,
        ))
        .await
        .unwrap();
    assert!(!late.is_applied());
    assert_eq!(late.into_inner(), newer, "stale returns the stored row");
    assert_eq!(
        store.subscription_by_id("sub-a-sub-k-100").await.unwrap(),
        Some(newer.clone())
    );
    assert_eq!(
        store
            .subscription_by_provider_id(&ProviderId::new("sub_sub-a_k"))
            .await
            .unwrap(),
        Some(newer)
    );
    assert!(
        store
            .subscription_by_id("sub-a-sub-k-150")
            .await
            .unwrap()
            .is_none()
    );
}

/// At the same instant the higher rank wins; a lower rank is stale.
pub async fn subscription_same_instant_higher_rank_wins(store: &dyn BillingStore) {
    let customer = seed_customer(store, "sub-b", None).await;
    store
        .upsert_subscription(sub_upsert(
            "sub-b",
            &customer,
            "k",
            SubscriptionStatus::Active,
            100,
        ))
        .await
        .unwrap();
    let lower = store
        .upsert_subscription(sub_upsert(
            "sub-b",
            &customer,
            "k",
            SubscriptionStatus::Trialing,
            100,
        ))
        .await
        .unwrap();
    assert!(!lower.is_applied());
    assert_eq!(lower.into_inner().status, SubscriptionStatus::Active);
    let higher = store
        .upsert_subscription(sub_upsert(
            "sub-b",
            &customer,
            "k",
            SubscriptionStatus::PastDue,
            100,
        ))
        .await
        .unwrap();
    assert!(higher.is_applied());
    assert_eq!(higher.into_inner().status, SubscriptionStatus::PastDue);
    let same = store
        .upsert_subscription(sub_upsert(
            "sub-b",
            &customer,
            "k",
            SubscriptionStatus::PastDue,
            100,
        ))
        .await
        .unwrap();
    assert!(!same.is_applied(), "same instant and same rank is stale");
}

/// A terminal status is never left, even by a newer event.
pub async fn subscription_terminal_never_left(store: &dyn BillingStore) {
    let customer = seed_customer(store, "sub-c", None).await;
    store
        .upsert_subscription(sub_upsert(
            "sub-c",
            &customer,
            "k",
            SubscriptionStatus::Canceled,
            100,
        ))
        .await
        .unwrap();
    let revive = store
        .upsert_subscription(sub_upsert(
            "sub-c",
            &customer,
            "k",
            SubscriptionStatus::Active,
            500,
        ))
        .await
        .unwrap();
    assert!(!revive.is_applied());
    assert_eq!(revive.into_inner().status, SubscriptionStatus::Canceled);

    store
        .upsert_subscription(sub_upsert(
            "sub-c",
            &customer,
            "x",
            SubscriptionStatus::IncompleteExpired,
            100,
        ))
        .await
        .unwrap();
    let revive = store
        .upsert_subscription(sub_upsert(
            "sub-c",
            &customer,
            "x",
            SubscriptionStatus::Active,
            500,
        ))
        .await
        .unwrap();
    assert!(!revive.is_applied());
    assert_eq!(
        revive.into_inner().status,
        SubscriptionStatus::IncompleteExpired
    );
}

/// `subscriptions_for_customer` is newest `last_event_at` first and scoped
/// to the customer.
pub async fn subscriptions_for_customer_newest_first(store: &dyn BillingStore) {
    let customer = seed_customer(store, "sub-d", None).await;
    let other = seed_customer(store, "sub-d-other", None).await;
    for (key, occurred) in [("old", 100), ("new", 300), ("mid", 200)] {
        store
            .upsert_subscription(sub_upsert(
                "sub-d",
                &customer,
                key,
                SubscriptionStatus::Active,
                occurred,
            ))
            .await
            .unwrap();
    }
    store
        .upsert_subscription(sub_upsert(
            "sub-d",
            &other,
            "theirs",
            SubscriptionStatus::Active,
            400,
        ))
        .await
        .unwrap();
    let rows = store.subscriptions_for_customer(&customer).await.unwrap();
    let ids: Vec<&str> = rows.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "sub-d-sub-new-300",
            "sub-d-sub-mid-200",
            "sub-d-sub-old-100"
        ]
    );
    assert!(
        store
            .subscriptions_for_customer("sub-d-nobody")
            .await
            .unwrap()
            .is_empty()
    );
}

/// `set_subscription_status` has no ordering guard and keeps `last_event_at`.
pub async fn subscription_set_status(store: &dyn BillingStore) {
    let customer = seed_customer(store, "sub-e", None).await;
    let row = store
        .upsert_subscription(sub_upsert(
            "sub-e",
            &customer,
            "k",
            SubscriptionStatus::PastDue,
            100,
        ))
        .await
        .unwrap()
        .into_inner();
    let updated = store
        .set_subscription_status(&row.id, SubscriptionStatus::Unpaid, at(3000))
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(updated.status, SubscriptionStatus::Unpaid);
    assert_eq!(updated.updated_at, at(3000));
    assert_eq!(
        updated.last_event_at,
        at(100),
        "a local decision is not an event"
    );
    assert_eq!(
        store.subscription_by_id(&row.id).await.unwrap(),
        Some(updated)
    );
    assert!(
        store
            .set_subscription_status("sub-e-missing", SubscriptionStatus::Unpaid, at(1))
            .await
            .unwrap()
            .is_none()
    );
}

// ── Invoices ────────────────────────────────────────────────────────────

/// Guarded upsert; `subscription_id` is kept when the incoming one is `None`.
pub async fn invoice_guarded_upsert_keeps_subscription_id(store: &dyn BillingStore) {
    let customer = seed_customer(store, "inv-a", None).await;
    let mut first = invoice_upsert("inv-a", &customer, "k", InvoiceStatus::Open, 100);
    first.subscription_id = Some("inv-a-sub".to_owned());
    first.next_payment_attempt = Some(at(900));
    let first = store.upsert_invoice(first).await.unwrap();
    assert!(first.is_applied());
    let first = first.into_inner();
    assert_eq!(first.id, "inv-a-inv-k-100");
    assert_eq!(first.subscription_id.as_deref(), Some("inv-a-sub"));
    assert_eq!(first.next_payment_attempt, Some(at(900)));
    assert_eq!(first.status, InvoiceStatus::Open);
    assert_eq!(first.attempt_count, 1);

    let mut paid = invoice_upsert("inv-a", &customer, "k", InvoiceStatus::Paid, 200);
    paid.amount_paid = Money::from_minor(1999, Currency::USD);
    paid.attempt_count = 2;
    paid.now = at(2000);
    let paid = store.upsert_invoice(paid).await.unwrap();
    assert!(paid.is_applied());
    let paid = paid.into_inner();
    assert_eq!(paid.id, first.id);
    assert_eq!(
        paid.subscription_id.as_deref(),
        Some("inv-a-sub"),
        "None keeps the link"
    );
    assert_eq!(
        paid.next_payment_attempt, None,
        "None clears a provider time"
    );
    assert_eq!(paid.amount_paid, Money::from_minor(1999, Currency::USD));
    assert_eq!(paid.attempt_count, 2);
    assert_eq!(paid.last_event_at, at(200));
    assert_eq!(paid.created_at, at(1000));
    assert_eq!(paid.updated_at, at(2000));

    let stale = store
        .upsert_invoice(invoice_upsert(
            "inv-a",
            &customer,
            "k",
            InvoiceStatus::Open,
            150,
        ))
        .await
        .unwrap();
    assert!(!stale.is_applied());
    assert_eq!(stale.into_inner(), paid);

    let same_instant_higher = store
        .upsert_invoice(invoice_upsert(
            "inv-a",
            &customer,
            "j",
            InvoiceStatus::Open,
            100,
        ))
        .await
        .unwrap();
    assert!(same_instant_higher.is_applied());
    let w = store
        .upsert_invoice(invoice_upsert(
            "inv-a",
            &customer,
            "j",
            InvoiceStatus::Paid,
            100,
        ))
        .await
        .unwrap();
    assert!(w.is_applied(), "same instant, higher rank");
    let w = store
        .upsert_invoice(invoice_upsert(
            "inv-a",
            &customer,
            "j",
            InvoiceStatus::Void,
            100,
        ))
        .await
        .unwrap();
    assert!(!w.is_applied(), "same instant, lower rank");

    assert_eq!(
        store.invoice_by_id(&paid.id).await.unwrap(),
        Some(paid.clone())
    );
    assert_eq!(
        store
            .invoice_by_provider_id(&ProviderId::new("in_inv-a_k"))
            .await
            .unwrap(),
        Some(paid)
    );
    assert!(
        store
            .invoice_by_id("inv-a-missing")
            .await
            .unwrap()
            .is_none()
    );
}

/// Money round-trips exactly: a zero-exponent currency and a negative amount.
pub async fn invoice_money_round_trip(store: &dyn BillingStore) {
    let customer = seed_customer(store, "inv-b", None).await;
    let mut yen = invoice_upsert("inv-b", &customer, "jpy", InvoiceStatus::Paid, 100);
    yen.amount_due = Money::from_minor(500, Currency::JPY);
    yen.amount_paid = Money::from_minor(500, Currency::JPY);
    store.upsert_invoice(yen).await.unwrap();
    let yen = store
        .invoice_by_id("inv-b-inv-jpy-100")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(yen.amount_due, Money::from_minor(500, Currency::JPY));
    assert_eq!(yen.amount_paid, Money::from_minor(500, Currency::JPY));
    assert_eq!(yen.amount_due.currency().exponent(), 0);

    let mut credit = invoice_upsert("inv-b", &customer, "neg", InvoiceStatus::Paid, 100);
    credit.amount_due = Money::from_minor(-250, Currency::EUR);
    credit.amount_paid = Money::from_minor(0, Currency::EUR);
    store.upsert_invoice(credit).await.unwrap();
    let credit = store
        .invoice_by_id("inv-b-inv-neg-100")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(credit.amount_due, Money::from_minor(-250, Currency::EUR));
    assert_eq!(credit.amount_paid, Money::zero(Currency::EUR));
}

// ── Dunning ─────────────────────────────────────────────────────────────

/// Upsert inserts, then replaces the whole row.
pub async fn dunning_upsert_replaces(store: &dyn BillingStore) {
    store
        .upsert_dunning(dunning("dun-a", "k", 1, 10, DunningState::Pending))
        .await
        .unwrap();
    let row = store
        .dunning_by_invoice("dun-a-inv-k")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row, dunning("dun-a", "k", 1, 10, DunningState::Pending));

    let mut replaced = dunning("dun-a", "k", 2, 40, DunningState::Pending);
    replaced.subscription_id = Some("dun-a-sub".to_owned());
    replaced.updated_at = at(11);
    store.upsert_dunning(replaced.clone()).await.unwrap();
    assert_eq!(
        store.dunning_by_invoice("dun-a-inv-k").await.unwrap(),
        Some(replaced)
    );
    assert!(
        store
            .dunning_by_invoice("dun-a-missing")
            .await
            .unwrap()
            .is_none()
    );
}

/// The claim is a compare-and-set on `(state = pending, attempt)`.
pub async fn dunning_claim_is_compare_and_set(store: &dyn BillingStore) {
    store
        .upsert_dunning(dunning("dun-b", "k", 1, 10, DunningState::Pending))
        .await
        .unwrap();
    assert!(
        !store
            .claim_dunning_attempt("dun-b-inv-k", 2, at(1))
            .await
            .unwrap(),
        "wrong attempt"
    );
    assert!(
        !store
            .claim_dunning_attempt("dun-b-missing", 1, at(1))
            .await
            .unwrap(),
        "no row"
    );
    assert!(
        store
            .claim_dunning_attempt("dun-b-inv-k", 1, at(1))
            .await
            .unwrap()
    );
    let row = store
        .dunning_by_invoice("dun-b-inv-k")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, DunningState::Running);
    assert_eq!(row.updated_at, at(1));
    assert!(
        !store
            .claim_dunning_attempt("dun-b-inv-k", 1, at(2))
            .await
            .unwrap(),
        "second claim loses"
    );

    store
        .upsert_dunning(dunning("dun-b", "r", 1, 10, DunningState::Recovered))
        .await
        .unwrap();
    assert!(
        !store
            .claim_dunning_attempt("dun-b-inv-r", 1, at(1))
            .await
            .unwrap(),
        "non-pending"
    );
}

/// `open_dunning` returns pending and running rows by `next_attempt_at`.
pub async fn open_dunning_ordered_and_filtered(store: &dyn BillingStore) {
    store
        .upsert_dunning(dunning("dun-c", "late", 1, 300, DunningState::Pending))
        .await
        .unwrap();
    store
        .upsert_dunning(dunning("dun-c", "early", 1, 100, DunningState::Running))
        .await
        .unwrap();
    store
        .upsert_dunning(dunning("dun-c", "mid", 1, 200, DunningState::Pending))
        .await
        .unwrap();
    store
        .upsert_dunning(dunning(
            "dun-c",
            "recovered",
            1,
            50,
            DunningState::Recovered,
        ))
        .await
        .unwrap();
    store
        .upsert_dunning(dunning(
            "dun-c",
            "exhausted",
            1,
            60,
            DunningState::Exhausted,
        ))
        .await
        .unwrap();
    store
        .upsert_dunning(dunning("dun-c", "canceled", 1, 70, DunningState::Canceled))
        .await
        .unwrap();
    let open: Vec<String> = store
        .open_dunning()
        .await
        .unwrap()
        .into_iter()
        .filter(|d| d.invoice_id.starts_with("dun-c-"))
        .map(|d| d.invoice_id)
        .collect();
    assert_eq!(open, ["dun-c-inv-early", "dun-c-inv-mid", "dun-c-inv-late"]);
}

// ── Fix round 2 properties ──────────────────────────────────────────────

/// One customer row per user: a second provider customer for a linked user
/// returns the linked row unchanged.
pub async fn customer_one_row_per_user(store: &dyn BillingStore) {
    let first = store
        .upsert_customer(
            CustomerUpsert::new("cust-d-1", "stripe", "cus_cust_d1", at(0)).with_user("u-d"),
        )
        .await
        .unwrap();
    assert_eq!(first.id, "cust-d-1");
    // A concurrent checkout created another provider customer for the user.
    let second = store
        .upsert_customer(
            CustomerUpsert::new("cust-d-2", "stripe", "cus_cust_d2", at(1)).with_user("u-d"),
        )
        .await
        .unwrap();
    assert_eq!(second, first, "the linked row wins unchanged");
    assert!(
        store
            .customer_by_provider_id(&ProviderId::new("cus_cust_d2"))
            .await
            .unwrap()
            .is_none(),
        "the orphan provider customer is not mirrored"
    );
    assert!(store.customer_by_id("cust-d-2").await.unwrap().is_none());
    // Linking an unlinked row to an already linked user keeps one row too.
    store
        .upsert_customer(CustomerUpsert::new(
            "cust-d-3",
            "stripe",
            "cus_cust_d3",
            at(2),
        ))
        .await
        .unwrap();
    let third = store
        .upsert_customer(
            CustomerUpsert::new("cust-d-4", "stripe", "cus_cust_d3", at(3)).with_user("u-d"),
        )
        .await
        .unwrap();
    assert_eq!(third, first);
    let unlinked = store.customer_by_id("cust-d-3").await.unwrap().unwrap();
    assert_eq!(unlinked.user_id, None);
    assert_eq!(store.customer_by_user("u-d").await.unwrap(), Some(first));
}

/// The same event again (same instant, same status) is `Unchanged`, also
/// for a terminal row.
pub async fn subscription_unchanged_redelivery(store: &dyn BillingStore) {
    let customer = seed_customer(store, "sub-f", None).await;
    let first = store
        .upsert_subscription(sub_upsert(
            "sub-f",
            &customer,
            "k",
            SubscriptionStatus::Active,
            100,
        ))
        .await
        .unwrap()
        .into_inner();
    let mut again = sub_upsert("sub-f", &customer, "k", SubscriptionStatus::Active, 100);
    again.quantity = 9;
    again.now = at(2000);
    let again = store.upsert_subscription(again).await.unwrap();
    assert!(again.is_unchanged());
    assert!(!again.is_applied());
    assert_eq!(again.into_inner(), first, "nothing was written");

    store
        .upsert_subscription(sub_upsert(
            "sub-f",
            &customer,
            "k",
            SubscriptionStatus::Canceled,
            200,
        ))
        .await
        .unwrap();
    let canceled_again = store
        .upsert_subscription(sub_upsert(
            "sub-f",
            &customer,
            "k",
            SubscriptionStatus::Canceled,
            200,
        ))
        .await
        .unwrap();
    assert!(canceled_again.is_unchanged(), "terminal redelivery");
    let older = store
        .upsert_subscription(sub_upsert(
            "sub-f",
            &customer,
            "k",
            SubscriptionStatus::Active,
            150,
        ))
        .await
        .unwrap();
    assert!(!older.is_unchanged());
    assert!(!older.is_applied());
}

/// A newer snapshot without price, plan or period end keeps the stored
/// values; the other fields are replaced.
pub async fn subscription_missing_fields_keep_stored_values(store: &dyn BillingStore) {
    let customer = seed_customer(store, "sub-g", None).await;
    store
        .upsert_subscription(sub_upsert(
            "sub-g",
            &customer,
            "k",
            SubscriptionStatus::Active,
            100,
        ))
        .await
        .unwrap();
    let bare = SubscriptionUpsert::new(
        "sub-g-sub-k-200",
        &customer,
        "sub_sub-g_k",
        SubscriptionStatus::PastDue,
        at(200),
        at(2000),
    )
    .with_quantity(4);
    let row = store.upsert_subscription(bare).await.unwrap();
    assert!(row.is_applied());
    let row = row.into_inner();
    assert_eq!(row.provider_price_id, Some(ProviderId::new("price_pro")));
    assert_eq!(row.plan_id, Some(PlanId::new("pro")));
    assert_eq!(row.current_period_end, Some(at(100 + 86_400)));
    assert_eq!(row.status, SubscriptionStatus::PastDue);
    assert_eq!(row.quantity, 4);
    assert_eq!(row.last_event_at, at(200));
    assert_eq!(
        store.subscription_by_id("sub-g-sub-k-100").await.unwrap(),
        Some(row)
    );
}

/// The same invoice event again is `Unchanged`.
pub async fn invoice_unchanged_redelivery(store: &dyn BillingStore) {
    let customer = seed_customer(store, "inv-c", None).await;
    let first = store
        .upsert_invoice(invoice_upsert(
            "inv-c",
            &customer,
            "k",
            InvoiceStatus::Open,
            100,
        ))
        .await
        .unwrap()
        .into_inner();
    let mut again = invoice_upsert("inv-c", &customer, "k", InvoiceStatus::Open, 100);
    again.attempt_count = 7;
    again.now = at(2000);
    let again = store.upsert_invoice(again).await.unwrap();
    assert!(again.is_unchanged());
    assert_eq!(again.into_inner(), first, "nothing was written");
}

/// `settle_dunning` writes only from one of `from` at the expected attempt.
pub async fn settle_dunning_is_compare_and_set(store: &dyn BillingStore) {
    let open = dunning("dun-d", "k", 1, 10, DunningState::Pending);
    store.upsert_dunning(open.clone()).await.unwrap();
    let mut closed = open.clone();
    closed.state = DunningState::Canceled;
    closed.updated_at = at(5);
    assert!(
        !store
            .settle_dunning("dun-d-inv-k", 1, &[DunningState::Running], closed.clone())
            .await
            .unwrap(),
        "wrong state"
    );
    assert!(
        !store
            .settle_dunning(
                "dun-d-inv-k",
                2,
                &[DunningState::Pending, DunningState::Running],
                closed.clone()
            )
            .await
            .unwrap(),
        "wrong attempt"
    );
    assert!(
        !store
            .settle_dunning("dun-d-missing", 1, &[DunningState::Pending], closed.clone())
            .await
            .unwrap(),
        "no row"
    );
    assert_eq!(
        store.dunning_by_invoice("dun-d-inv-k").await.unwrap(),
        Some(open),
        "a refused settle writes nothing"
    );
    assert!(
        store
            .settle_dunning(
                "dun-d-inv-k",
                1,
                &[DunningState::Pending, DunningState::Running],
                closed.clone()
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store.dunning_by_invoice("dun-d-inv-k").await.unwrap(),
        Some(closed.clone())
    );
    let mut recovered = closed;
    recovered.state = DunningState::Recovered;
    assert!(
        !store
            .settle_dunning(
                "dun-d-inv-k",
                1,
                &[DunningState::Pending, DunningState::Running],
                recovered
            )
            .await
            .unwrap(),
        "a settled row is not settled again"
    );
    // The written row may carry a new attempt number.
    let running = dunning("dun-d", "n", 1, 10, DunningState::Running);
    store.upsert_dunning(running).await.unwrap();
    let next = dunning("dun-d", "n", 2, 40, DunningState::Pending);
    assert!(
        store
            .settle_dunning("dun-d-inv-n", 1, &[DunningState::Running], next.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        store.dunning_by_invoice("dun-d-inv-n").await.unwrap(),
        Some(next)
    );
}

/// `prune_events` deletes applied rows older than `before` and keeps
/// in-flight claims and newer rows. Instants lie far before every other
/// property's, so the cutoff touches this property's rows only.
pub async fn prune_events_deletes_applied_rows_before(store: &dyn BillingStore) {
    let count_before = store.applied_event_count().await.unwrap();
    for id in ["prune-a-old", "prune-a-new", "prune-a-inflight"] {
        assert_eq!(
            store
                .claim_event(id, "k", at(-1_000_000), STALE)
                .await
                .unwrap(),
            EventClaim::Claimed
        );
    }
    store
        .finish_event("prune-a-old", at(-900_000))
        .await
        .unwrap();
    store.finish_event("prune-a-new", at(50)).await.unwrap();
    assert_eq!(store.prune_events(at(-800_000)).await.unwrap(), 1);
    assert_eq!(store.applied_event_count().await.unwrap(), count_before + 1);
    assert_eq!(
        store
            .claim_event("prune-a-old", "k", at(60), STALE)
            .await
            .unwrap(),
        EventClaim::Claimed,
        "a pruned id is a new event"
    );
    assert_eq!(
        store
            .claim_event("prune-a-new", "k", at(60), STALE)
            .await
            .unwrap(),
        EventClaim::Duplicate
    );
    // Re-claimed inside `STALE` of the claim, so only the prune could have
    // freed it.
    assert_eq!(
        store
            .claim_event("prune-a-inflight", "k", at(-999_999), STALE)
            .await
            .unwrap(),
        EventClaim::Duplicate,
        "an in-flight claim is kept"
    );
    assert_eq!(store.prune_events(at(-800_000)).await.unwrap(), 0);
}

/// Run every property on one store.
pub async fn run_contract(store: &dyn BillingStore) {
    ledger_claim_finish_and_release(store).await;
    ledger_stale_claim_is_reclaimable(store).await;
    ledger_applied_count(store).await;
    customer_upsert_keyed_by_provider_id(store).await;
    customer_link_never_replaced_email_replaced(store).await;
    customer_lookups(store).await;
    subscription_newer_wins_older_stale(store).await;
    subscription_same_instant_higher_rank_wins(store).await;
    subscription_terminal_never_left(store).await;
    subscriptions_for_customer_newest_first(store).await;
    subscription_set_status(store).await;
    invoice_guarded_upsert_keeps_subscription_id(store).await;
    invoice_money_round_trip(store).await;
    dunning_upsert_replaces(store).await;
    dunning_claim_is_compare_and_set(store).await;
    open_dunning_ordered_and_filtered(store).await;
    customer_one_row_per_user(store).await;
    subscription_unchanged_redelivery(store).await;
    subscription_missing_fields_keep_stored_values(store).await;
    invoice_unchanged_redelivery(store).await;
    settle_dunning_is_compare_and_set(store).await;
    prune_events_deletes_applied_rows_before(store).await;
}

/// The suite against `MemoryBillingStore`, one test per property.
mod memory {
    use super::*;

    macro_rules! memory_case {
        ($($name:ident),* $(,)?) => {$(
            #[tokio::test]
            async fn $name() {
                let store = MemoryBillingStore::new();
                super::$name(&store).await;
            }
        )*};
    }

    memory_case!(
        ledger_claim_finish_and_release,
        ledger_stale_claim_is_reclaimable,
        ledger_applied_count,
        customer_upsert_keyed_by_provider_id,
        customer_link_never_replaced_email_replaced,
        customer_lookups,
        subscription_newer_wins_older_stale,
        subscription_same_instant_higher_rank_wins,
        subscription_terminal_never_left,
        subscriptions_for_customer_newest_first,
        subscription_set_status,
        invoice_guarded_upsert_keeps_subscription_id,
        invoice_money_round_trip,
        dunning_upsert_replaces,
        dunning_claim_is_compare_and_set,
        open_dunning_ordered_and_filtered,
        customer_one_row_per_user,
        subscription_unchanged_redelivery,
        subscription_missing_fields_keep_stored_values,
        invoice_unchanged_redelivery,
        settle_dunning_is_compare_and_set,
        prune_events_deletes_applied_rows_before,
    );

    #[tokio::test]
    async fn whole_suite_on_one_shared_store() {
        let store = MemoryBillingStore::new();
        run_contract(&store).await;
    }
}
