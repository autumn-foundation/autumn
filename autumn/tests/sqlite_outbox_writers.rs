//! Framework writers on the transactional outbox (issue #3062): durable
//! event listeners, job enqueue and webhook dispatch.
//!
//! Each writer puts its work in `autumn_outbox` on the transaction
//! connection. Nothing runs before the relay drains, and a rollback runs
//! nothing.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_outbox_writers`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::sync::atomic::{AtomicU32, Ordering};

use autumn_web::config::OutboxConfig;
use autumn_web::events::Events;
use autumn_web::outbox::{self, Outbox};
use autumn_web::prelude::*;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sim::Sim;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::test::TestApp;
use autumn_web::webhook_outbound::{
    InMemoryOutboundWebhookStore, OutboundWebhookPlugin, WebhookOutboundManager,
    WebhookSubscription, WebhookSubscriptionStatus,
};
use autumn_web::{event, job, jobs, listener, listeners};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use scoped_futures::ScopedFutureExt as _;
use serde::{Deserialize, Serialize};

type SqlitePool = Pool<autumn_web::db::RuntimeConnection>;

#[derive(Default)]
struct Counters {
    durable: AtomicU32,
    sync: AtomicU32,
    shipped: AtomicU32,
}

fn counters(state: &AppState) -> std::sync::Arc<Counters> {
    state.extension::<Counters>().expect("counters installed")
}

#[event]
struct OrderPlaced {
    order_id: i64,
}

#[listener(OrderPlaced, durable)]
async fn reserve_stock(state: AppState, _event: OrderPlaced) -> AutumnResult<()> {
    counters(&state).durable.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[listener(OrderPlaced)]
async fn log_order(state: AppState, _event: OrderPlaced) -> AutumnResult<()> {
    counters(&state).sync.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShipArgs {
    order_id: i64,
}

#[job]
async fn ship_order(state: AppState, args: ShipArgs) -> AutumnResult<()> {
    assert_eq!(args.order_id, 7);
    counters(&state).shipped.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

async fn substrate() -> SqliteSubstrate {
    let substrate = SqliteSubstrate::new().expect("substrate");
    outbox::ensure_schema(&substrate.pool())
        .await
        .expect("outbox tables");
    substrate
}

fn app(pool: SqlitePool) -> TestApp {
    TestApp::new()
        .with_db(pool)
        .with_outbox(OutboxConfig::default())
        .listeners(listeners![reserve_stock, log_order])
        .jobs(jobs![ship_order])
        .state_initializer(|state| state.insert_extension(Counters::default()))
}

/// Run `write` in a transaction that commits (`commit = true`) or rolls back.
async fn in_tx<F>(state: &AppState, commit: bool, write: F)
where
    F: for<'c> FnOnce(
            Outbox,
            &'c mut autumn_web::db::RuntimeConnection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'c>,
        > + Send,
{
    let outbox = Outbox::new(state);
    let mut conn = state.pool().unwrap().get().await.unwrap();
    let result: Result<(), AutumnError> = autumn_web::db::scoped_transaction(&mut *conn, |conn| {
        async move {
            write(outbox, conn).await?;
            if commit {
                Ok(())
            } else {
                Err(AutumnError::bad_request_msg("roll back"))
            }
        }
        .scope_boxed()
    })
    .await;
    assert_eq!(result.is_ok(), commit);
}

async fn sent_rows(pool: &SqlitePool) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = pool.get().await.unwrap();
    diesel::sql_query("SELECT COUNT(*) AS n FROM autumn_outbox WHERE dispatched_at IS NOT NULL")
        .get_result::<Count>(&mut conn)
        .await
        .unwrap()
        .n
}

#[tokio::test(start_paused = true)]
async fn durable_listener_runs_from_the_outbox_after_commit_only() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let substrate = substrate().await;
    let mut sim = Sim::from_seed(1);
    sim.build(app(substrate.pool()));
    let state = sim.client().state().clone();

    in_tx(&state, false, |outbox, conn| {
        Box::pin(async move { outbox.publish(conn, &OrderPlaced { order_id: 1 }).await })
    })
    .await;
    in_tx(&state, true, |outbox, conn| {
        Box::pin(async move { outbox.publish(conn, &OrderPlaced { order_id: 2 }).await })
    })
    .await;

    let counters = counters(&state);
    assert_eq!(
        AtomicU32::load(&counters.sync, Ordering::SeqCst),
        2,
        "sync listeners run at once"
    );
    assert_eq!(
        AtomicU32::load(&counters.durable, Ordering::SeqCst),
        0,
        "durable waits for the relay"
    );

    sim.run_to_idle().await;
    assert_eq!(
        AtomicU32::load(&counters.durable, Ordering::SeqCst),
        1,
        "only the commit runs"
    );
    assert_eq!(sent_rows(&substrate.pool()).await, 1);
}

#[post("/orders")]
async fn place_order(mut db: Db, events: Events) -> AutumnResult<&'static str> {
    db.tx(|conn| {
        async move {
            events
                .publish_in_tx(conn, OrderPlaced { order_id: 3 })
                .await
        }
        .scope_boxed()
    })
    .await?;
    Ok("ok")
}

#[tokio::test(start_paused = true)]
async fn events_publish_in_tx_from_a_handler_runs_the_listener_after_the_relay() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let substrate = substrate().await;
    let mut sim = Sim::from_seed(4);
    sim.build(app(substrate.pool()).routes(routes![place_order]));
    sim.client().post("/orders").send().await.assert_ok();
    let state = sim.client().state().clone();
    assert_eq!(
        AtomicU32::load(&counters(&state).durable, Ordering::SeqCst),
        0
    );

    sim.run_to_idle().await;
    assert_eq!(
        AtomicU32::load(&counters(&state).durable, Ordering::SeqCst),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn job_is_enqueued_from_the_outbox_after_commit_only() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let substrate = substrate().await;
    let mut sim = Sim::from_seed(2);
    sim.build(app(substrate.pool()));
    let state = sim.client().state().clone();

    for commit in [false, true] {
        in_tx(&state, commit, |outbox, conn| {
            Box::pin(async move {
                outbox
                    .enqueue_job(conn, "ship_order", &ShipArgs { order_id: 7 })
                    .await
                    .map(drop)
            })
        })
        .await;
    }
    assert_eq!(
        AtomicU32::load(&counters(&state).shipped, Ordering::SeqCst),
        0
    );

    sim.run_to_idle().await;
    assert_eq!(
        AtomicU32::load(&counters(&state).shipped, Ordering::SeqCst),
        1
    );
    assert_eq!(sent_rows(&substrate.pool()).await, 1);
}

#[tokio::test(start_paused = true)]
async fn webhook_dispatch_from_the_outbox_survives_a_second_relay_send() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let substrate = substrate().await;
    let pool = substrate.pool();
    let store = std::sync::Arc::new(InMemoryOutboundWebhookStore::new());

    let mut builder = app(pool.clone())
        .plugin(OutboundWebhookPlugin::new(store.clone()).with_initial_backoff_ms(1));
    let mock = builder
        .http_mock("http://mock-receiver/hooks")
        .post("/hooks")
        .respond_with(200, serde_json::json!({ "ok": true }));
    let mut sim = Sim::from_seed(3);
    sim.build(builder);
    let state = sim.client().state().clone();
    store
        .create_subscription(WebhookSubscription {
            id: "sub-1".to_owned(),
            target_url: "http://mock-receiver/hooks".to_owned(),
            event_topics: vec!["order.paid".to_owned()],
            secret: "whsec_test_secret_with_32_bytes_ok!!".to_owned(),
            status: WebhookSubscriptionStatus::Active,
            consecutive_failures: 0,
        })
        .await
        .unwrap();

    let manager = state
        .extension::<WebhookOutboundManager>()
        .expect("manager installed");
    for commit in [false, true] {
        let manager = manager.clone();
        let state_for_tx = state.clone();
        in_tx(&state, commit, move |_, conn| {
            Box::pin(async move {
                manager
                    .dispatch_in_tx(
                        &state_for_tx,
                        conn,
                        "order.paid",
                        &serde_json::json!({ "id": 9 }),
                    )
                    .await
                    .map(drop)
            })
        })
        .await;
    }
    sim.run_to_idle().await;
    mock.expect_called(1);
    let logs = store.get_delivery_logs().await.unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].request_headers["webhook-id"], logs[0].id);

    // The relay sends the message again (as after a crash before its mark).
    let mut conn = pool.get().await.unwrap();
    diesel::sql_query("UPDATE autumn_outbox SET dispatched_at = NULL")
        .execute(&mut conn)
        .await
        .unwrap();
    drop(conn);
    sim.run_to_idle().await;
    mock.expect_called(1);
    assert_eq!(store.get_delivery_logs().await.unwrap().len(), 1);
    job::clear_global_job_client();
}

mod sql_webhook_store {
    use std::collections::HashMap;

    use autumn_web::webhook_outbound::{
        OutboundWebhookHandler as _, SqlOutboundWebhookStore, WebhookDeliveryLog,
        WebhookSubscription, WebhookSubscriptionStatus,
    };
    use chrono::TimeZone as _;

    use super::substrate;

    fn subscription(id: &str) -> WebhookSubscription {
        WebhookSubscription {
            id: id.to_owned(),
            target_url: "https://receiver.example/hooks".to_owned(),
            event_topics: vec!["order.paid".to_owned(), "order.refunded".to_owned()],
            secret: "whsec_test_secret_with_32_bytes_ok!!".to_owned(),
            status: WebhookSubscriptionStatus::Active,
            consecutive_failures: 0,
        }
    }

    fn log(id: &str, status: Option<u16>, error: Option<&str>) -> WebhookDeliveryLog {
        WebhookDeliveryLog {
            id: id.to_owned(),
            subscription_id: "sub-1".to_owned(),
            topic: "order.paid".to_owned(),
            payload: "{\"id\":1}".to_owned(),
            request_headers: HashMap::from([("webhook-id".to_owned(), id.to_owned())]),
            response_status: status,
            response_body: status.map(|_| "body".to_owned()),
            elapsed_ms: 12,
            attempt: 1,
            max_attempts: 5,
            is_dlq: false,
            last_error: error.map(str::to_owned),
            timestamp: chrono::Utc.timestamp_millis_opt(1_700_000_000_123).unwrap(),
        }
    }

    #[tokio::test]
    async fn sql_store_round_trips_and_survives_a_new_store() {
        let substrate = substrate().await;
        let store = SqlOutboundWebhookStore::new(substrate.pool());
        store.ensure_schema().await.unwrap();
        store.ensure_schema().await.unwrap();
        store
            .create_subscription(subscription("sub-1"))
            .await
            .unwrap();
        let mut other = subscription("sub-2");
        other.event_topics = vec!["user.created".to_owned()];
        store.create_subscription(other).await.unwrap();

        let entry = log("log-1", None, None);
        store.log_delivery(entry.clone()).await.unwrap();

        // A new store on the same database (a restart) reads the same rows.
        let restarted = SqlOutboundWebhookStore::new(substrate.pool());
        let subs = restarted.get_subscriptions("order.paid").await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0], subscription("sub-1"));
        assert_eq!(
            restarted.get_delivery_log("log-1").await.unwrap(),
            Some(entry)
        );
        assert_eq!(restarted.get_delivery_log("nope").await.unwrap(), None);
        assert_eq!(restarted.get_delivery_logs(10).await.unwrap().len(), 1);
    }

    /// A successful replay reactivates a `Failed` subscription in the same
    /// transaction as its log. A crash after the log cannot keep it failed.
    #[tokio::test]
    async fn sql_store_success_log_reactivates_a_failed_subscription() {
        let substrate = substrate().await;
        let store = SqlOutboundWebhookStore::new(substrate.pool());
        store.ensure_schema().await.unwrap();
        let mut failed = subscription("sub-1");
        failed.status = WebhookSubscriptionStatus::Failed;
        failed.consecutive_failures = 50;
        store.create_subscription(failed).await.unwrap();
        let mut disabled = subscription("sub-2");
        disabled.status = WebhookSubscriptionStatus::Disabled;
        store.create_subscription(disabled).await.unwrap();

        store
            .log_delivery(log("ok", Some(200), None))
            .await
            .unwrap();
        let mut other = log("ok-2", Some(200), None);
        other.subscription_id = "sub-2".to_owned();
        store.log_delivery(other).await.unwrap();

        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.status, WebhookSubscriptionStatus::Active);
        assert_eq!(sub.consecutive_failures, 0);
        let sub = store.get_subscription("sub-2").await.unwrap().unwrap();
        assert_eq!(
            sub.status,
            WebhookSubscriptionStatus::Disabled,
            "an operator disable stays"
        );
    }

    /// A 2xx log is final. A late failure of a duplicate job does not
    /// overwrite it or count as a failure.
    #[tokio::test]
    async fn sql_store_success_log_is_final() {
        let substrate = substrate().await;
        let store = SqlOutboundWebhookStore::new(substrate.pool());
        store.ensure_schema().await.unwrap();
        store
            .create_subscription(subscription("sub-1"))
            .await
            .unwrap();
        store.log_delivery(log("l", Some(200), None)).await.unwrap();

        store
            .log_delivery(log("l", Some(500), Some("500")))
            .await
            .unwrap();
        let mut pending = log("l", None, None);
        pending.attempt = 2;
        store.log_delivery(pending).await.unwrap();

        let stored = store.get_delivery_log("l").await.unwrap().unwrap();
        assert_eq!(stored.response_status, Some(200));
        assert_eq!(stored.attempt, 1);
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.consecutive_failures, 0);
    }

    /// Two duplicate jobs that fail one attempt count one failure. A later
    /// DLQ move of that attempt is still stored.
    #[tokio::test]
    async fn sql_store_counts_a_repeated_failure_once() {
        let substrate = substrate().await;
        let store = SqlOutboundWebhookStore::new(substrate.pool());
        store.ensure_schema().await.unwrap();
        store
            .create_subscription(subscription("sub-1"))
            .await
            .unwrap();
        let mut failed = log("l", Some(500), Some("500"));
        store.log_delivery(failed.clone()).await.unwrap();
        store.log_delivery(failed.clone()).await.unwrap();
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.consecutive_failures, 1);

        failed.is_dlq = true;
        store.log_delivery(failed).await.unwrap();
        assert!(
            store.get_delivery_log("l").await.unwrap().unwrap().is_dlq,
            "the DLQ move is stored"
        );
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(
            sub.consecutive_failures, 1,
            "a DLQ move is not a new failure"
        );

        // A 2xx of the same attempt replaces a failure.
        store
            .log_delivery(log("l2", Some(500), Some("500")))
            .await
            .unwrap();
        store
            .log_delivery(log("l2", Some(200), None))
            .await
            .unwrap();
        let stored = store.get_delivery_log("l2").await.unwrap().unwrap();
        assert_eq!(stored.response_status, Some(200));
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.consecutive_failures, 0);
    }

    /// A failure of an older attempt is stale and ignored. A replay reset
    /// (out of the DLQ, back to attempt 1) still applies.
    #[tokio::test]
    async fn sql_store_ignores_a_stale_attempt() {
        let substrate = substrate().await;
        let store = SqlOutboundWebhookStore::new(substrate.pool());
        store.ensure_schema().await.unwrap();
        store
            .create_subscription(subscription("sub-1"))
            .await
            .unwrap();
        let mut newer = log("l", None, None);
        newer.attempt = 2;
        store.log_delivery(newer).await.unwrap();
        store
            .log_delivery(log("l", Some(500), Some("500")))
            .await
            .unwrap();
        let stored = store.get_delivery_log("l").await.unwrap().unwrap();
        assert_eq!((stored.attempt, stored.response_status), (2, None));
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.consecutive_failures, 0);

        let mut dead = log("dead", Some(500), Some("500"));
        dead.attempt = 3;
        dead.is_dlq = true;
        store.log_delivery(dead).await.unwrap();
        store.log_delivery(log("dead", None, None)).await.unwrap();
        let stored = store.get_delivery_log("dead").await.unwrap().unwrap();
        assert_eq!(
            (stored.attempt, stored.is_dlq),
            (1, false),
            "a replay reset applies"
        );
    }

    #[tokio::test]
    async fn sql_store_counts_failures_like_the_in_memory_store() {
        let substrate = substrate().await;
        let store = SqlOutboundWebhookStore::new(substrate.pool());
        store.ensure_schema().await.unwrap();
        store
            .create_subscription(subscription("sub-1"))
            .await
            .unwrap();

        for n in 0..49 {
            store
                .log_delivery(log(&format!("f{n}"), Some(500), Some("500")))
                .await
                .unwrap();
        }
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.consecutive_failures, 49);
        assert_eq!(sub.status, WebhookSubscriptionStatus::Active);

        // `replace_delivery_log` is not an outcome.
        store
            .replace_delivery_log(log("r", None, Some("enqueue failed")))
            .await
            .unwrap();
        assert_eq!(
            store
                .get_subscription("sub-1")
                .await
                .unwrap()
                .unwrap()
                .consecutive_failures,
            49
        );

        store
            .log_delivery(log("f49", None, Some("timeout")))
            .await
            .unwrap();
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.status, WebhookSubscriptionStatus::Failed);
        assert!(
            store
                .get_subscriptions("order.paid")
                .await
                .unwrap()
                .is_empty()
        );

        store.reactivate_failed_subscription("sub-1").await.unwrap();
        let sub = store.get_subscription("sub-1").await.unwrap().unwrap();
        assert_eq!(sub.status, WebhookSubscriptionStatus::Active);
        assert_eq!(sub.consecutive_failures, 0);

        store
            .log_delivery(log("f50", Some(502), None))
            .await
            .unwrap();
        store
            .log_delivery(log("ok", Some(200), None))
            .await
            .unwrap();
        assert_eq!(
            store
                .get_subscription("sub-1")
                .await
                .unwrap()
                .unwrap()
                .consecutive_failures,
            0
        );

        let mut dead = log("dead", Some(500), Some("500"));
        dead.is_dlq = true;
        store.replace_delivery_log(dead).await.unwrap();
        let dlq = store.get_dlq_logs().await.unwrap();
        assert_eq!(dlq.len(), 1);
        assert_eq!(dlq[0].id, "dead");
    }
}
