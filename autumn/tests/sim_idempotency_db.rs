//! Issue #3061 AC1: the idempotency record commits with the handler's mutation.
//!
//! The handler writes a payment and its response in one `Db::tx`, through
//! [`IdempotencyTx::commit`]. The test drops the request with `crash_at` at each
//! suspension point in turn, then retries with the same key. For each crash:
//!
//! - exactly one payment row exists at the end;
//! - if the payment committed before the crash, the retry replays the stored
//!   response and the handler does not run again.
//!
//! Some crash points fall after the commit and before the response reaches the
//! client. The test asserts that at least one such point was hit.
//!
//! The test uses the in-memory `SQLite` sim substrate. It needs no Docker.
//!
//! The crash sweep runs on a multi-thread runtime. diesel-async calls
//! `block_in_place` when it drops a `SQLite` query future. That panics on the
//! current-thread runtime of `#[sim_test]`. On a multi-thread runtime, each
//! crash waits for its query, so the state after a crash is known.
//!
//! The in-flight TTL is 1 s. A key that a crash left locked is free again
//! after 1 s of real time.
//!
//! The crash window after the commit is in the handler (see
//! `AFTER_COMMIT_AWAITS`). The Postgres twin also crashes in the
//! middleware's own awaits after the commit.
//!
//! A standalone `[[test]]` binary: the consolidated binary does not compile
//! under `sqlite`. Run it with:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_idempotency_db`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::config::{AutumnConfig, IdempotencyBackend};
use autumn_web::idempotency::IdempotencyTx;
use autumn_web::migrate::{EmbeddedMigrations, FRAMEWORK_MIGRATIONS, embed_migrations};
use autumn_web::prelude::*;
use autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{Sim, crash_at};
use autumn_web::sim_test;
use autumn_web::test::{TestApp, TestClient, TestResponse};

use diesel_async::RunQueryDsl as _;

const APP_MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/sim_idempotency_db");

/// Number of handler runs, shared with the test.
#[derive(Clone, Default)]
struct Calls(Arc<AtomicU64>);

impl Calls {
    fn add(&self) {
        AtomicU64::fetch_add(&self.0, 1, Ordering::SeqCst);
    }

    fn get(&self) -> u64 {
        // Qualified: `diesel_async::RunQueryDsl::load` shadows `AtomicU64::load`.
        AtomicU64::load(&self.0, Ordering::SeqCst)
    }
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// Suspension points after the commit. They stand in for a slow response
/// path. A `SQLite` query on a multi-thread runtime does not always suspend, so
/// the number of awaits before the commit varies; a wide window here makes
/// sure the sweep crashes inside it.
const AFTER_COMMIT_AWAITS: usize = 8;

/// Writes a payment and its response in one transaction.
#[post("/pay")]
async fn pay(
    State(state): State<AppState>,
    idem: IdempotencyTx,
    mut db: Db,
) -> AutumnResult<axum::response::Response> {
    let calls = state.extension::<Calls>().expect("calls installed");
    calls.add();
    let response = db
        .tx(|conn| {
            async move {
                let row: IdRow =
                    diesel::sql_query("INSERT INTO payments (amount) VALUES (100) RETURNING id")
                        .get_result(conn)
                        .await?;
                let body = Json(serde_json::json!({ "payment": row.id }));
                idem.commit(conn, (StatusCode::CREATED, body)).await
            }
            .scope_boxed()
        })
        .await?;
    for _ in 0..AFTER_COMMIT_AWAITS {
        tokio::task::yield_now().await;
    }
    Ok(response)
}

fn app(substrate: &SqliteSubstrate, calls: &Calls) -> TestApp {
    let mut config = AutumnConfig::default();
    config.idempotency.backend = IdempotencyBackend::Database;
    config.idempotency.in_flight_ttl_secs = 1;
    let calls = calls.clone();
    TestApp::new()
        .config(config)
        .idempotent()
        .with_db(substrate.pool())
        .routes(routes![pay])
        .state_initializer(move |state| state.insert_extension(calls.clone()))
}

async fn payments(substrate: &SqliteSubstrate) -> i64 {
    let mut conn = substrate.pool().get().await.expect("checkout");
    diesel::sql_query("SELECT COUNT(*) AS n FROM payments")
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count payments")
        .n
}

async fn send(client: &TestClient, key: &str) -> TestResponse {
    client
        .post("/pay")
        .header("idempotency-key", key)
        .send()
        .await
}

/// Retry until the key is not in flight. A crash can leave the lock held, so a
/// `409` waits out the 1 s in-flight TTL.
async fn retry(client: &TestClient, key: &str) -> TestResponse {
    for _ in 0..8 {
        let response = send(client, key).await;
        if response.status != StatusCode::CONFLICT {
            return response;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    panic!("key {key} stayed in flight");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_commit_replays_the_committed_response() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let client = app(&substrate, &calls).build();

    let mut crashed_after_commit = 0;
    let mut index = 0;
    loop {
        let key = format!("pay-{index}");
        let before_rows = payments(&substrate).await;
        let before_calls = calls.get();

        let outcome = crash_at(index, send(&client, &key)).await;
        if let Some(response) = outcome.completed() {
            response.assert_status(201);
            assert_eq!(payments(&substrate).await, before_rows + 1);
            break;
        }

        let committed = payments(&substrate).await == before_rows + 1;
        let crashed_calls = calls.get();
        let response = retry(&client, &key).await;
        response.assert_status(201);
        assert_eq!(
            payments(&substrate).await,
            before_rows + 1,
            "crash at await {index}: exactly one payment commits"
        );
        if committed {
            crashed_after_commit += 1;
            assert_eq!(
                response.header("x-idempotent-replayed"),
                Some("true"),
                "crash at await {index}: the retry replays the committed response"
            );
            assert_eq!(
                calls.get(),
                crashed_calls,
                "crash at await {index}: the handler does not run again"
            );
            assert_eq!(crashed_calls, before_calls + 1);
        }

        index += 1;
        assert!(index < 256, "the request completes within 256 awaits");
    }

    assert!(
        crashed_after_commit > 0,
        "at least one crash fell between the commit and the response"
    );
}

#[sim_test]
async fn sim_idempotency_db_replays_without_running_the_handler(mut sim: Sim) {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let client = sim.build(app(&substrate, &calls));

    let first = send(client, "same").await;
    first.assert_status(201);
    let second = send(client, "same").await;
    second.assert_status(201);

    assert_eq!(second.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(second.text(), first.text());
    assert_eq!(calls.get(), 1);
    assert_eq!(payments(&substrate).await, 1);
}
