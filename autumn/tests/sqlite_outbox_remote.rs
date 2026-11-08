//! Outbox delivery to another service, with inbox dedupe on the receiver.
//!
//! Two in-process apps on two `SQLite` databases. App A ("core") routes the
//! `deal.won` topic to app B ("marketing") through
//! `[outbox.destinations.marketing]`. A `SimNet` host serves B's router, so
//! A's relay POSTs to B in process.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_outbox_remote`.

#![cfg(all(feature = "sqlite", feature = "test-support", feature = "http-client"))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use autumn_web::config::{OutboxConfig, OutboxDestinationConfig, OutboxInboxConfig};
use autumn_web::outbox::{self, Outbox};
use autumn_web::prelude::*;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{Sim, SimNet};
use autumn_web::test::TestApp;
use autumn_web::{event, listener, listeners};

use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};
use scoped_futures::ScopedFutureExt as _;

type SqlitePool = Pool<autumn_web::db::RuntimeConnection>;

const SECRET: &str = "marketing-inbox-secret-0123456789abcdef";
const TOPIC: &str = "deal.won";

#[event(name = "deal.won")]
struct DealWon {
    deal_id: i64,
    seq: i64,
}

/// Fail the first run of the message with this `seq` (0: none).
#[derive(Default)]
struct FailOnce(AtomicI64);

/// The effect in B: one row in B's own database.
#[listener(DealWon, durable)]
async fn record_deal(state: AppState, event: DealWon) -> AutumnResult<()> {
    if let Some(fail) = state.extension::<FailOnce>()
        && fail
            .0
            .compare_exchange(event.seq, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        return Err(AutumnError::service_unavailable_msg("listener fails once"));
    }
    let message_id = outbox::current_message_id().unwrap_or_default();
    let mut conn = state.pool().expect("database").get().await?;
    diesel::sql_query("INSERT INTO won_deals (deal_id, seq, message_id) VALUES (?, ?, ?)")
        .bind::<diesel::sql_types::BigInt, _>(event.deal_id)
        .bind::<diesel::sql_types::BigInt, _>(event.seq)
        .bind::<diesel::sql_types::Text, _>(message_id)
        .execute(&mut conn)
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
    Ok(())
}

async fn database() -> SqliteSubstrate {
    let substrate = SqliteSubstrate::new().expect("substrate");
    outbox::ensure_schema(&substrate.pool())
        .await
        .expect("outbox tables");
    substrate
        .pool()
        .get()
        .await
        .unwrap()
        .batch_execute(
            "CREATE TABLE won_deals (row INTEGER PRIMARY KEY AUTOINCREMENT, \
             deal_id BIGINT NOT NULL, seq BIGINT NOT NULL, message_id TEXT NOT NULL)",
        )
        .await
        .unwrap();
    substrate
}

fn core_config() -> OutboxConfig {
    let mut destinations = BTreeMap::new();
    destinations.insert(
        "marketing".to_owned(),
        OutboxDestinationConfig {
            url: "http://marketing/_autumn/inbox".to_owned(),
            signing_secret: Some(SECRET.to_owned()),
            topics: vec![TOPIC.to_owned()],
        },
    );
    OutboxConfig {
        initial_backoff_ms: 10,
        max_backoff_ms: 10,
        destinations,
        ..OutboxConfig::default()
    }
}

fn marketing_config() -> OutboxConfig {
    OutboxConfig {
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
        inbox: OutboxInboxConfig {
            enabled: true,
            signing_secret: Some(SECRET.to_owned()),
            ..OutboxInboxConfig::default()
        },
        ..OutboxConfig::default()
    }
}

/// App B. `with_listener = false` is an old release that does not know
/// the topic yet.
fn marketing(pool: SqlitePool, with_listener: bool) -> TestApp {
    // Core calls B by the host name `marketing`. CSRF is on: the inbox is
    // exempt, because it checks its own signature.
    let mut config = autumn_web::config::AutumnConfig::default();
    config.profile = Some("test".to_owned());
    config.security.csrf.enabled = true;
    config
        .security
        .trusted_hosts
        .hosts
        .push("marketing".to_owned());
    let app = TestApp::new()
        .config(config)
        .with_db(pool)
        .with_outbox(marketing_config())
        .state_initializer(|state| state.insert_extension(FailOnce::default()));
    if with_listener {
        app.listeners(listeners![record_deal])
    } else {
        app
    }
}

fn core(pool: SqlitePool) -> TestApp {
    TestApp::new().with_db(pool).with_outbox(core_config())
}

/// Write one `deal.won` message on A in a transaction that commits or rolls back.
async fn write_deal(state: &AppState, deal_id: i64, seq: i64, commit: bool) {
    let outbox = Outbox::new(state);
    let mut conn = state.pool().unwrap().get().await.unwrap();
    let result: Result<(), AutumnError> = autumn_web::db::scoped_transaction(&mut *conn, |conn| {
        async move {
            outbox
                .write(
                    conn,
                    &format!("deal:{deal_id}"),
                    TOPIC,
                    &DealWon { deal_id, seq },
                )
                .await?;
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

#[derive(diesel::QueryableByName, Debug, PartialEq, Eq)]
struct Won {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    deal_id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    seq: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    message_id: String,
}

async fn won(pool: &SqlitePool) -> Vec<Won> {
    let mut conn = pool.get().await.unwrap();
    diesel::sql_query("SELECT deal_id, seq, message_id FROM won_deals ORDER BY row")
        .load::<Won>(&mut conn)
        .await
        .unwrap()
}

#[derive(diesel::QueryableByName)]
struct Row {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    attempts: i32,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    last_error: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    dispatched_at: Option<i64>,
}

async fn core_rows(pool: &SqlitePool) -> Vec<Row> {
    let mut conn = pool.get().await.unwrap();
    diesel::sql_query(
        "SELECT id, attempts, last_error, dispatched_at FROM autumn_outbox \
         WHERE topic = 'deal.won' ORDER BY seq",
    )
    .load::<Row>(&mut conn)
    .await
    .unwrap()
}

/// Drain B's outbox until nothing is left, so B's listeners run.
async fn drain_marketing(state: &AppState) {
    while outbox::drain(state, 100).await.unwrap() > 0 {}
}

#[tokio::test(start_paused = true)]
async fn commit_in_core_delivers_exactly_one_effect_in_marketing_across_a_redelivery() {
    let core_db = database().await;
    let marketing_db = database().await;
    let marketing = marketing(marketing_db.pool(), true).build();
    let marketing_state = marketing.state().clone();

    let net = SimNet::new().host("marketing", marketing.into_router());
    let mut sim = Sim::from_seed(11);
    sim.net(net.clone());
    sim.build(core(core_db.pool()));
    let core_state = sim.client().state().clone();

    write_deal(&core_state, 1, 1, false).await;
    write_deal(&core_state, 2, 1, true).await;
    sim.run_to_idle().await;
    drain_marketing(&marketing_state).await;

    let rows = core_rows(&core_db.pool()).await;
    assert_eq!(rows.len(), 1, "the rollback wrote no message");
    assert!(
        rows[0].dispatched_at.is_some(),
        "B acknowledged the message: {:?}",
        rows[0].last_error
    );
    let effects = won(&marketing_db.pool()).await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].deal_id, 2);
    assert_eq!(
        effects[0].message_id, rows[0].id,
        "the listener sees the producer's message id"
    );

    // Forced redelivery: A loses the acknowledgement and sends again.
    core_db
        .pool()
        .get()
        .await
        .unwrap()
        .batch_execute("UPDATE autumn_outbox SET dispatched_at = NULL WHERE topic = 'deal.won'")
        .await
        .unwrap();
    sim.run_to_idle().await;
    drain_marketing(&marketing_state).await;

    let rows = core_rows(&core_db.pool()).await;
    assert!(rows[0].dispatched_at.is_some(), "B acknowledges the copy");
    assert_eq!(
        won(&marketing_db.pool()).await.len(),
        1,
        "B drops the copy: still exactly one effect"
    );
    assert_eq!(
        net.events().len(),
        2,
        "A sent the message twice over the network"
    );
}

#[tokio::test(start_paused = true)]
async fn unknown_topic_is_retried_until_a_receiver_release_knows_it() {
    let core_db = database().await;
    let marketing_db = database().await;
    // An old marketing release: no listener for `deal.won`.
    let old = marketing(marketing_db.pool(), false).build();
    let net = SimNet::new().host("marketing", old.into_router());
    let mut sim = Sim::from_seed(12);
    sim.net(net.clone());
    sim.build(core(core_db.pool()));
    let core_state = sim.client().state().clone();

    write_deal(&core_state, 3, 1, true).await;
    sim.run_to_idle().await;
    let rows = core_rows(&core_db.pool()).await;
    assert!(rows[0].dispatched_at.is_none(), "the message stays pending");
    assert!(rows[0].attempts >= 1);
    assert!(
        rows[0]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("503"),
        "a retryable 503: {:?}",
        rows[0].last_error
    );

    // The new marketing release knows the topic.
    let new = marketing(marketing_db.pool(), true).build();
    let new_state = new.state().clone();
    let _ = net.clone().host("marketing", new.into_router());
    sim.advance(Duration::from_secs(1)).await;
    sim.run_to_idle().await;
    drain_marketing(&new_state).await;

    assert!(core_rows(&core_db.pool()).await[0].dispatched_at.is_some());
    assert_eq!(won(&marketing_db.pool()).await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn order_per_aggregate_is_kept_across_the_hop_and_failures() {
    let core_db = database().await;
    let marketing_db = database().await;
    let marketing = marketing(marketing_db.pool(), true).build();
    let marketing_state = marketing.state().clone();
    // B's listener fails the first run of seq 2.
    marketing_state
        .extension::<FailOnce>()
        .unwrap()
        .0
        .store(2, Ordering::SeqCst);

    let net = SimNet::new().host("marketing", marketing.into_router());
    let mut sim = Sim::from_seed(13);
    sim.net(net.clone());
    sim.build(core(core_db.pool()));
    let core_state = sim.client().state().clone();

    // B is down for the first sends.
    net.partition("marketing");
    for seq in 1..=4 {
        write_deal(&core_state, 7, seq, true).await;
        write_deal(&core_state, 8, seq, true).await;
    }
    sim.run_to_idle().await;
    assert!(
        core_rows(&core_db.pool())
            .await
            .iter()
            .all(|row| row.dispatched_at.is_none()),
        "nothing is sent while B is down"
    );

    net.heal("marketing");
    sim.advance(Duration::from_secs(1)).await;
    sim.run_to_idle().await;
    drain_marketing(&marketing_state).await;

    let effects = won(&marketing_db.pool()).await;
    assert_eq!(effects.len(), 8, "{effects:?}");
    for deal in [7, 8] {
        let seqs: Vec<i64> = effects
            .iter()
            .filter(|won| won.deal_id == deal)
            .map(|won| won.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4], "deal {deal} keeps its order");
    }
}

/// A signed request to B's inbox. `secret` signs it.
fn inbox_request(
    client: &autumn_web::test::TestClient,
    secret: Option<&str>,
    topic: &str,
    id: &str,
) -> autumn_web::test::RequestBuilder {
    let body = serde_json::json!({
        "id": id,
        "topic": topic,
        "aggregate": "deal:9",
        "created_at": "2026-01-01T00:00:00Z",
        "payload": { "deal_id": 9, "seq": 1 },
    })
    .to_string();
    let mut request = client
        .post(outbox::INBOX_PATH)
        .header("content-type", "application/json");
    if let Some(secret) = secret {
        let timestamp = autumn_web::time::ambient_now().timestamp();
        request = request.header(
            outbox::SIGNATURE_HEADER,
            &outbox::inbox_signature(secret, timestamp, &body),
        );
    }
    request.body(body)
}

#[tokio::test]
async fn inbox_rejects_a_bad_signature_and_an_unknown_topic_as_retryable() {
    let marketing_db = database().await;
    let client = marketing(marketing_db.pool(), true).build();

    inbox_request(&client, None, TOPIC, "m-1")
        .send()
        .await
        .assert_status(400);
    inbox_request(
        &client,
        Some("another-secret-0123456789abcdef"),
        TOPIC,
        "m-1",
    )
    .send()
    .await
    .assert_status(401);
    // A changed body breaks the signature.
    let timestamp = autumn_web::time::ambient_now().timestamp();
    client
        .post(outbox::INBOX_PATH)
        .header(
            outbox::SIGNATURE_HEADER,
            &outbox::inbox_signature(SECRET, timestamp, "{}"),
        )
        .body("{\"id\":\"m-1\"}")
        .send()
        .await
        .assert_status(401);
    // An unknown topic: 503, which the relay retries.
    inbox_request(&client, Some(SECRET), "deal.lost", "m-2")
        .send()
        .await
        .assert_status(503);

    // A good request, then its copy: both acknowledged, one handoff.
    inbox_request(&client, Some(SECRET), TOPIC, "m-3")
        .send()
        .await
        .assert_status(200);
    let copy = inbox_request(&client, Some(SECRET), TOPIC, "m-3")
        .send()
        .await;
    copy.assert_status(200);
    assert!(copy.text().contains("duplicate"), "{}", copy.text());
    drain_marketing(client.state()).await;
    assert_eq!(won(&marketing_db.pool()).await.len(), 1);
    assert!(
        won(&marketing_db.pool())
            .await
            .iter()
            .all(|w| w.message_id == "m-3"),
        "nothing ran for the rejected requests"
    );
}
