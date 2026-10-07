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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::config::{AutumnConfig, IdempotencyBackend};
use autumn_web::db::RuntimeConnection;
use autumn_web::idempotency::{DbIdempotencyStore, IdempotencyLayer, IdempotencyTx};
use autumn_web::migrate::{EmbeddedMigrations, FRAMEWORK_MIGRATIONS, embed_migrations};
use autumn_web::prelude::*;
use autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::session::{
    MemoryStore, Session, SessionConfig, SessionLayer, SessionStore, SessionStoreError,
};
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{Sim, crash_at};
use autumn_web::sim_test;
use autumn_web::test::{TestApp, TestClient, TestResponse};

use axum::body::Body;
use axum::http::Request;
use diesel_async::{AsyncConnection as _, RunQueryDsl as _};
use tower::ServiceExt as _;

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
        .state_initializer(move |state| state.insert_extension(calls))
}

/// The number of payment rows. Takes the pool first, so the future is `Send`.
fn payments(substrate: &SqliteSubstrate) -> impl Future<Output = i64> + Send + use<> {
    let pool = substrate.pool();
    async move {
        let mut conn = pool.get().await.expect("checkout");
        diesel::sql_query("SELECT COUNT(*) AS n FROM payments")
            .get_result::<CountRow>(&mut conn)
            .await
            .expect("count payments")
            .n
    }
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

/// A session store whose save fails. It stands in for a crash between the
/// commit and the session rewrite: in both cases the final record is not
/// written.
#[derive(Clone)]
struct FailingSaveSessionStore;

impl SessionStore for FailingSaveSessionStore {
    async fn load(&self, _id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        Ok(None)
    }

    async fn save(
        &self,
        _id: &str,
        _data: HashMap<String, String>,
    ) -> Result<(), SessionStoreError> {
        Err(SessionStoreError::backend("save", "boom"))
    }

    async fn destroy(&self, _id: &str) -> Result<(), SessionStoreError> {
        Ok(())
    }
}

/// A router whose handler commits through `IdempotencyTx` and also changes
/// the session.
fn login_router<S: SessionStore + Clone>(
    substrate: &SqliteSubstrate,
    calls: &Calls,
    sessions: S,
) -> axum::Router {
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(86_400),
    ));
    let pool = substrate.pool();
    let calls = calls.clone();
    let handler = move |idem: IdempotencyTx, session: Session| {
        let pool = pool.clone();
        let calls = calls.clone();
        async move {
            calls.add();
            session.insert("user_id", "42").await;
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                diesel::sql_query("INSERT INTO payments (amount) VALUES (100)")
                    .execute(conn)
                    .await?;
                idem.commit(conn, (StatusCode::CREATED, "logged in")).await
            })
            .await
            .expect("transaction")
        }
    };
    axum::Router::new()
        .route("/login", axum::routing::post(handler))
        .layer(IdempotencyLayer::new(store))
        .layer(SessionLayer::new(sessions, SessionConfig::default()))
}

fn login(key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/login")
        .header("idempotency-key", key)
        .body(Body::empty())
        .expect("request")
}

#[sim_test]
async fn sim_idempotency_db_unfinished_session_rewrite_never_replays(sim: Sim) {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let app = login_router(&substrate, &calls, FailingSaveSessionStore);

    let first = app
        .clone()
        .oneshot(login("login"))
        .await
        .expect("infallible");
    assert!(first.status().is_server_error(), "the session save failed");
    assert_eq!(payments(&substrate).await, 1, "the payment committed");

    // Past the in-flight TTL. The stored record has no session cookie, so a
    // replay would answer "logged in" with no session.
    sim.advance(Duration::from_secs(61)).await;
    let retry = app
        .clone()
        .oneshot(login("login"))
        .await
        .expect("infallible");
    assert_eq!(
        retry.status(),
        StatusCode::CONFLICT,
        "a record without its final Set-Cookie is not replayed"
    );
    assert_eq!(calls.get(), 1, "the handler does not run again");
    assert_eq!(payments(&substrate).await, 1);
}

#[sim_test]
async fn sim_idempotency_db_session_rewrite_replays_with_cookie(_sim: Sim) {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let app = login_router(&substrate, &calls, MemoryStore::new());

    let first = app
        .clone()
        .oneshot(login("login"))
        .await
        .expect("infallible");
    assert_eq!(first.status(), StatusCode::CREATED);
    assert!(first.headers().contains_key("set-cookie"));

    let retry = app
        .clone()
        .oneshot(login("login"))
        .await
        .expect("infallible");
    assert_eq!(retry.status(), StatusCode::CREATED);
    assert_eq!(
        retry
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert!(
        retry.headers().contains_key("set-cookie"),
        "the replay carries the final session cookie"
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(payments(&substrate).await, 1);
}

#[sim_test]
async fn sim_idempotency_db_recovery_point_on_a_shard_is_an_error(_sim: Sim) {
    let primary = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    // A second database stands in for a shard: it has the table, not the key.
    let shard = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let store = Arc::new(DbIdempotencyStore::new(
        primary.pool(),
        Duration::from_secs(86_400),
    ));
    let shard_pool = shard.pool();
    let handler = move |idem: IdempotencyTx| {
        let shard_pool = shard_pool.clone();
        async move {
            let mut pooled = shard_pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            match idem.recovery_point(conn).await {
                Ok(point) => format!("{point:?}").into_response(),
                Err(error) => error.into_response(),
            }
        }
    };
    let app = axum::Router::new()
        .route("/step", axum::routing::post(handler))
        .layer(IdempotencyLayer::new(store));

    let request = Request::builder()
        .method("POST")
        .uri("/step")
        .header("idempotency-key", "step")
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("infallible");
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a missing key row is not read as \"no step committed\""
    );
}

fn step(key: &str, body: &'static str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/step")
        .header("idempotency-key", key)
        .body(Body::from(body))
        .expect("request")
}

#[sim_test]
async fn sim_idempotency_db_recovery_point_is_bound_to_the_request_body(_sim: Sim) {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(86_400),
    ));
    let pool = substrate.pool();
    // The first attempt records its step, then fails before `commit`.
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        async move {
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            match idem.recovery_point(conn).await {
                Err(error) => error.into_response(),
                Ok(Some(point)) => format!("resumed after {point}").into_response(),
                Ok(None) => {
                    let step = idem.clone();
                    conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                        step.set_recovery_point(conn, "charged").await
                    })
                    .await
                    .expect("transaction");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }
    };
    let app = axum::Router::new()
        .route("/step", axum::routing::post(handler))
        .layer(IdempotencyLayer::new(store));

    let first = app
        .clone()
        .oneshot(step("pay", "A"))
        .await
        .expect("infallible");
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let other_body = app
        .clone()
        .oneshot(step("pay", "B"))
        .await
        .expect("infallible");
    assert_eq!(
        other_body.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a step done for body A is not a step done for body B"
    );

    let same_body = app
        .clone()
        .oneshot(step("pay", "A"))
        .await
        .expect("infallible");
    assert_eq!(same_body.status(), StatusCode::OK, "body A resumes");
}

/// Multi-thread with real time, like the crash sweep: a `SQLite` query
/// cannot overlap a `Sim` clock jump on a current-thread runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_change_before_commit_holds_the_key() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(86_400),
    ));
    let pool = substrate.pool();
    // The handler changes the session, commits, then runs past the 1 s lock.
    let handler = move |idem: IdempotencyTx, session: Session| {
        let pool = pool.clone();
        async move {
            session.insert("user_id", "42").await;
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            let response = conn
                .transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                    idem.commit(conn, (StatusCode::CREATED, "logged in")).await
                })
                .await
                .expect("transaction");
            drop(pooled);
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            response
        }
    };
    let app = axum::Router::new()
        .route("/login", axum::routing::post(handler))
        .layer(IdempotencyLayer::new(store).with_in_flight_ttl(Duration::from_secs(1)))
        .layer(SessionLayer::new(
            MemoryStore::new(),
            SessionConfig::default(),
        ));

    let first = app.clone().oneshot(login("slow"));
    let retry = async {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        app.clone()
            .oneshot(login("slow"))
            .await
            .expect("infallible")
    };
    let (first, retry) = tokio::join!(first, retry);
    assert_eq!(first.expect("infallible").status(), StatusCode::CREATED);
    assert_eq!(
        retry.status(),
        StatusCode::CONFLICT,
        "the record without its Set-Cookie stays hidden past the in-flight TTL"
    );
}

/// A response TTL shorter than the in-flight TTL: a crash after the commit
/// must not let the record expire while the crash lock still hides it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_ttl_crash_after_commit_still_replays() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    // The first run commits, then hangs until the client drops it (a crash).
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            let first = calls.get() == 0;
            calls.add();
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            let response = conn
                .transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                    diesel::sql_query("INSERT INTO payments (amount) VALUES (100)")
                        .execute(conn)
                        .await?;
                    idem.commit(conn, (StatusCode::CREATED, "paid")).await
                })
                .await
                .expect("transaction");
            drop(pooled);
            if first {
                std::future::pending::<()>().await;
            }
            response
        }
    };
    let app = axum::Router::new()
        .route("/pay", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(1))
                .with_in_flight_ttl(Duration::from_secs(3)),
        );
    let pay = || {
        Request::builder()
            .method("POST")
            .uri("/pay")
            .header("idempotency-key", "short-ttl")
            .body(Body::empty())
            .expect("request")
    };

    let crashed =
        tokio::time::timeout(Duration::from_millis(500), app.clone().oneshot(pay())).await;
    assert!(
        crashed.is_err(),
        "the first request is dropped after its commit"
    );
    assert_eq!(payments(&substrate).await, 1);

    // At about 3.3 s: the 3 s crash lock has freed the key, and the 1 s
    // response TTL from the commit has passed.
    tokio::time::sleep(Duration::from_millis(2_800)).await;
    let retry = app.clone().oneshot(pay()).await.expect("infallible");
    assert_eq!(retry.status(), StatusCode::CREATED);
    assert_eq!(
        retry
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "the committed record outlives the crash lock"
    );
    assert_eq!(calls.get(), 1, "the handler does not run again");
    assert_eq!(payments(&substrate).await, 1);
}

/// On a normal release, the record lives the configured TTL from the release,
/// not the crash-lock window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_ttl_normal_release_keeps_the_configured_ttl() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            calls.add();
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                idem.commit(conn, (StatusCode::CREATED, "paid")).await
            })
            .await
            .expect("transaction")
        }
    };
    let app = axum::Router::new()
        .route("/pay", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(1))
                .with_in_flight_ttl(Duration::from_secs(3)),
        );
    let pay = || {
        Request::builder()
            .method("POST")
            .uri("/pay")
            .header("idempotency-key", "short-ttl-release")
            .body(Body::empty())
            .expect("request")
    };

    let first = app.clone().oneshot(pay()).await.expect("infallible");
    assert_eq!(first.status(), StatusCode::CREATED);

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let later = app.clone().oneshot(pay()).await.expect("infallible");
    assert_eq!(later.status(), StatusCode::CREATED);
    assert_eq!(
        later.headers().get("x-idempotent-replayed"),
        None,
        "the 1 s response TTL has passed"
    );
    assert_eq!(calls.get(), 2);
}

/// `commit` ran, then the transaction rolled back. The record is gone, so a
/// retry runs the handler again; the error response is not stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rolled_back_commit_is_not_stored() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(86_400),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    let handler = move |idem: IdempotencyTx, session: Session| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            let first = calls.get() == 0;
            calls.add();
            session.insert("user_id", "42").await;
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            let result = conn
                .transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                    let response = idem.commit(conn, (StatusCode::CREATED, "paid")).await?;
                    if first {
                        // A later step fails: the whole transaction rolls back.
                        return Err(autumn_web::AutumnError::internal_server_error_msg("boom"));
                    }
                    Ok(response)
                })
                .await;
            match result {
                Ok(response) => response,
                Err(error) => error.into_response(),
            }
        }
    };
    let app = axum::Router::new()
        .route("/login", axum::routing::post(handler))
        .layer(IdempotencyLayer::new(store))
        .layer(SessionLayer::new(
            MemoryStore::new(),
            SessionConfig::default(),
        ));

    let first = app
        .clone()
        .oneshot(login("rollback"))
        .await
        .expect("infallible");
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let retry = app
        .clone()
        .oneshot(login("rollback"))
        .await
        .expect("infallible");
    assert_eq!(
        retry.headers().get("x-idempotent-replayed"),
        None,
        "the error of a rolled-back transaction is not replayed"
    );
    assert_eq!(retry.status(), StatusCode::CREATED);
    assert_eq!(calls.get(), 2, "the handler runs again");
}

/// A response TTL shorter than the in-flight TTL: a crash after a recovery
/// point must not let the row expire while the crash lock still holds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_ttl_crash_after_recovery_point_resumes() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    // The first run records a step, then hangs until the client drops it.
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            let first = calls.get() == 0;
            calls.add();
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            if let Some(point) = idem.recovery_point(conn).await.expect("recovery point") {
                return format!("resumed after {point}");
            }
            let step = idem.clone();
            conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                step.set_recovery_point(conn, "charged").await
            })
            .await
            .expect("transaction");
            drop(pooled);
            if first {
                std::future::pending::<()>().await;
            }
            "charged again".to_owned()
        }
    };
    let app = axum::Router::new()
        .route("/step", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(1))
                .with_in_flight_ttl(Duration::from_secs(3)),
        );

    let crashed = tokio::time::timeout(
        Duration::from_millis(500),
        app.clone().oneshot(step("charge", "A")),
    )
    .await;
    assert!(
        crashed.is_err(),
        "the first request is dropped after its step"
    );

    // At about 3.3 s: the 3 s crash lock has freed the key.
    tokio::time::sleep(Duration::from_millis(2_800)).await;
    let retry = app
        .clone()
        .oneshot(step("charge", "A"))
        .await
        .expect("infallible");
    let body = axum::body::to_bytes(retry.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(
        &body[..],
        b"resumed after charged",
        "the recovery point outlives the crash lock"
    );
}

/// The release fails after a successful commit (a crash between statements).
/// The record must keep its crash-safe expiry, so a retry after the lock
/// frees still replays it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_ttl_failed_release_still_replays() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    {
        let mut conn = substrate.pool().get().await.expect("checkout");
        diesel::sql_query(
            "CREATE TRIGGER fail_release BEFORE UPDATE OF locked_by ON autumn_idempotency_keys \
             WHEN NEW.locked_by IS NULL BEGIN SELECT RAISE(ABORT, 'release fails'); END",
        )
        .execute(&mut conn)
        .await
        .expect("trigger");
    }
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            calls.add();
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                diesel::sql_query("INSERT INTO payments (amount) VALUES (100)")
                    .execute(conn)
                    .await?;
                idem.commit(conn, (StatusCode::CREATED, "paid")).await
            })
            .await
            .expect("transaction")
        }
    };
    let app = axum::Router::new()
        .route("/pay", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(1))
                .with_in_flight_ttl(Duration::from_secs(3)),
        );
    let pay = || {
        Request::builder()
            .method("POST")
            .uri("/pay")
            .header("idempotency-key", "failed-release")
            .body(Body::empty())
            .expect("request")
    };

    let first = app.clone().oneshot(pay()).await.expect("infallible");
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(payments(&substrate).await, 1);
    {
        let mut conn = substrate.pool().get().await.expect("checkout");
        diesel::sql_query("DROP TRIGGER fail_release")
            .execute(&mut conn)
            .await
            .expect("drop trigger");
    }

    // At about 3.3 s: the 3 s lock has freed the key; the 1 s TTL has passed.
    tokio::time::sleep(Duration::from_millis(3_300)).await;
    let retry = app.clone().oneshot(pay()).await.expect("infallible");
    assert_eq!(
        retry
            .headers()
            .get("x-idempotent-replayed")
            .and_then(|v| v.to_str().ok()),
        Some("true"),
        "an interrupted release keeps the record past the lock"
    );
    assert_eq!(calls.get(), 1, "the handler does not run again");
    assert_eq!(payments(&substrate).await, 1);
}

/// A record with an empty body, for store-level tests.
const fn store_record(status: u16) -> autumn_web::idempotency::IdempotencyRecord {
    autumn_web::idempotency::IdempotencyRecord {
        status,
        headers: Vec::new(),
        body: Vec::new(),
        metadata: Vec::new(),
    }
}

/// A store over a fresh substrate, with a 1 s default TTL.
fn bare_store() -> (SqliteSubstrate, DbIdempotencyStore) {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let store = DbIdempotencyStore::new(substrate.pool(), Duration::from_secs(1));
    (substrate, store)
}

/// A request outlives its lock, a retry takes the key, then the first
/// request's late `set` lands. The retry still holds the key: a third request
/// is refused, even after the short response TTL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_set_keeps_a_newer_owners_lock() {
    use autumn_web::idempotency::IdempotencyStore as _;
    let (_substrate, store) = bare_store();
    let key = "late-set";
    assert!(
        store
            .try_lock(key, "a", Duration::from_millis(300))
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        store
            .try_lock(key, "b", Duration::from_secs(10))
            .await
            .unwrap()
    );

    store
        .set(
            key,
            "a",
            store_record(201),
            Vec::new(),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        !store
            .try_lock(key, "c", Duration::from_secs(10))
            .await
            .unwrap(),
        "the retry still holds the key"
    );
    assert!(
        store.get(key).await.unwrap().is_none(),
        "a record behind a live lock is not replayed"
    );
}

/// The owner stores its record and crashes before `unlock`. The record stays
/// until one TTL after the lock frees, so a retry replays it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_then_crash_keeps_the_record_past_the_lock() {
    use autumn_web::idempotency::IdempotencyStore as _;
    let (_substrate, store) = bare_store();
    let key = "set-crash";
    assert!(
        store
            .try_lock(key, "a", Duration::from_secs(2))
            .await
            .unwrap()
    );
    store
        .set(
            key,
            "a",
            store_record(201),
            Vec::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    // No unlock: the request crashed.
    tokio::time::sleep(Duration::from_millis(2_300)).await;

    let entry = store
        .get(key)
        .await
        .unwrap()
        .expect("record replays after the lock frees");
    assert_eq!(entry.record.status, 201);
}

/// After `set` and a normal `unlock`, the record lives the configured TTL from
/// the release, not from the end of the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_then_unlock_keeps_the_configured_ttl() {
    use autumn_web::idempotency::IdempotencyStore as _;
    let (_substrate, store) = bare_store();
    let key = "set-unlock";
    assert!(
        store
            .try_lock(key, "a", Duration::from_secs(5))
            .await
            .unwrap()
    );
    store
        .set(
            key,
            "a",
            store_record(201),
            Vec::new(),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
    store.unlock(key, "a").await.unwrap();
    assert!(
        store.get(key).await.unwrap().is_some(),
        "replays after release"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        store.get(key).await.unwrap().is_none(),
        "the record expires one TTL after the release"
    );
}

/// The first request records a step and crashes. The retry takes the key and
/// crashes too, before it reads or writes anything. The third request still
/// finds the recovery point: taking over the key keeps the row one TTL past the
/// new lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_ttl_recovery_point_survives_two_crashes() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    // Run 1 records a step and hangs. Run 2 hangs at once. Run 3 resumes.
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            let run = calls.get();
            calls.add();
            if run == 1 {
                std::future::pending::<()>().await;
            }
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            if let Some(point) = idem.recovery_point(conn).await.expect("recovery point") {
                return format!("resumed after {point}");
            }
            let step = idem.clone();
            conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                step.set_recovery_point(conn, "charged").await
            })
            .await
            .expect("transaction");
            drop(pooled);
            std::future::pending::<()>().await;
            unreachable!()
        }
    };
    let app = axum::Router::new()
        .route("/step", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(1))
                .with_in_flight_ttl(Duration::from_secs(2)),
        );

    for attempt in 1..=2 {
        let crashed = tokio::time::timeout(
            Duration::from_millis(500),
            app.clone().oneshot(step("charge", "A")),
        )
        .await;
        assert!(crashed.is_err(), "attempt {attempt} is dropped");
        // The 2 s lock frees the key at about 2 s after the attempt.
        tokio::time::sleep(Duration::from_millis(1_800)).await;
    }
    assert_eq!(calls.get(), 2, "both attempts ran the handler");

    // A third run that does not find the step records it again and hangs.
    let third = tokio::time::timeout(
        Duration::from_secs(5),
        app.clone().oneshot(step("charge", "A")),
    )
    .await
    .expect("the third request resumes rather than re-running the step")
    .expect("infallible");
    let body = axum::body::to_bytes(third.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(
        &body[..],
        b"resumed after charged",
        "the recovery point outlives the second crash"
    );
}

/// A zero response TTL: once released, the record is gone at once, not kept
/// until the end of the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_ttl_record_expires_on_release() {
    use autumn_web::idempotency::IdempotencyStore as _;
    let (_substrate, store) = bare_store();
    let key = "zero-ttl";
    assert!(
        store
            .try_lock(key, "a", Duration::from_secs(5))
            .await
            .unwrap()
    );
    store
        .set(key, "a", store_record(201), Vec::new(), Duration::ZERO)
        .await
        .unwrap();
    store.unlock(key, "a").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        store.get(key).await.unwrap().is_none(),
        "a zero-TTL record is not replayed after release"
    );
    assert!(
        store
            .try_lock(key, "b", Duration::from_secs(5))
            .await
            .unwrap(),
        "the key starts over"
    );
}

/// A outlives its lock and B takes the key. B stores its response, then A's
/// late `set` arrives before B releases. B's response is the one replayed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_set_does_not_overwrite_the_owners_record() {
    use autumn_web::idempotency::IdempotencyStore as _;
    let (_substrate, store) = bare_store();
    let key = "late-overwrite";
    assert!(
        store
            .try_lock(key, "a", Duration::from_millis(300))
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        store
            .try_lock(key, "b", Duration::from_secs(10))
            .await
            .unwrap()
    );

    store
        .set(
            key,
            "b",
            store_record(201),
            Vec::new(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    store
        .set(
            key,
            "a",
            store_record(500),
            Vec::new(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    store.unlock(key, "b").await.unwrap();

    let entry = store.get(key).await.unwrap().expect("B's record");
    assert_eq!(
        entry.record.status, 201,
        "A's late set did not overwrite B's record"
    );
}

/// The layer keeps responses for 3 s; the store default is 1 s. The first
/// request records a step and crashes. A retry takes the key late, near the
/// end of the row's retention, and crashes too. The row must then live the
/// layer's 3 s past the new lock, not the store's 1 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_point_keeps_the_layer_ttl_on_takeover() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    // Run 1 records a step and hangs. Run 2 hangs at once. Run 3 resumes.
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            let run = calls.get();
            calls.add();
            if run == 1 {
                std::future::pending::<()>().await;
            }
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            if let Some(point) = idem.recovery_point(conn).await.expect("recovery point") {
                return format!("resumed after {point}");
            }
            let step = idem.clone();
            conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                step.set_recovery_point(conn, "charged").await
            })
            .await
            .expect("transaction");
            drop(pooled);
            std::future::pending::<()>().await;
            unreachable!()
        }
    };
    let app = axum::Router::new()
        .route("/step", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(3))
                .with_in_flight_ttl(Duration::from_secs(1)),
        );
    let crash = |app: axum::Router| async move {
        let dropped =
            tokio::time::timeout(Duration::from_millis(300), app.oneshot(step("charge", "A")))
                .await;
        assert!(dropped.is_err(), "the attempt is dropped");
    };

    // Run 1 at 0 s: lock to 1 s, row kept to 1 + 3 = 4 s.
    crash(app.clone()).await;
    // Run 2 at about 3.5 s: lock to 4.5 s. Kept to 4.5 + 3 = 7.5 s, not 5.5 s.
    tokio::time::sleep(Duration::from_millis(3_200)).await;
    crash(app.clone()).await;
    assert_eq!(calls.get(), 2, "both attempts ran the handler");
    // Run 3 at about 6 s, after the 5.5 s a store-default retention allows.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    let third = tokio::time::timeout(
        Duration::from_secs(5),
        app.clone().oneshot(step("charge", "A")),
    )
    .await
    .expect("the third request resumes rather than re-running the step")
    .expect("infallible");
    let body = axum::body::to_bytes(third.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(&body[..], b"resumed after charged");
}

/// A outlives its lock and B takes the key, stores its response and releases.
/// A's late `set` then finds no live lock, but B's record is still there: A's
/// stale write must not replace it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_set_after_release_keeps_the_newer_record() {
    use autumn_web::idempotency::IdempotencyStore as _;
    let (_substrate, store) = bare_store();
    let key = "late-after-release";
    assert!(
        store
            .try_lock(key, "a", Duration::from_millis(300))
            .await
            .unwrap()
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        store
            .try_lock(key, "b", Duration::from_secs(10))
            .await
            .unwrap()
    );
    store
        .set(
            key,
            "b",
            store_record(201),
            Vec::new(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    store.unlock(key, "b").await.unwrap();

    store
        .set(
            key,
            "a",
            store_record(500),
            Vec::new(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();

    let entry = store.get(key).await.unwrap().expect("B's record");
    assert_eq!(
        entry.record.status, 201,
        "A's stale write did not replace B's record"
    );
}

/// The handler outlives its 1 s lock and the row's retention, and the sweep
/// deletes the row. Its `commit` on the primary then finds no row: that is a
/// lost key (`409`), not a shard connection (`500`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_after_the_row_is_swept_is_a_conflict() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        async move {
            // Outlive the lock; the test deletes the row meanwhile.
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            match conn
                .transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                    idem.commit(conn, (StatusCode::CREATED, "paid")).await
                })
                .await
            {
                Ok(response) => response.into_response(),
                Err(error) => error.into_response(),
            }
        }
    };
    let app = axum::Router::new()
        .route("/pay", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(1))
                .with_in_flight_ttl(Duration::from_secs(1)),
        );
    let request = Request::builder()
        .method("POST")
        .uri("/pay")
        .header("idempotency-key", "swept")
        .body(Body::empty())
        .expect("request");

    let pending = tokio::spawn(app.oneshot(request));
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    {
        // What the sweep does to an expired, unlocked row.
        let mut conn = substrate.pool().get().await.expect("checkout");
        diesel::sql_query("DELETE FROM autumn_idempotency_keys")
            .execute(&mut conn)
            .await
            .expect("sweep");
    }
    let response = pending.await.expect("join").expect("infallible");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

/// The pool (one connection) is busy for longer than the 1 s lock. A's lock
/// still lasts 1 s from when A gets a connection, so B is refused right after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_ttl_starts_after_pool_checkout() {
    use autumn_web::idempotency::IdempotencyStore as _;
    let (substrate, store) = bare_store();
    let busy = substrate.pool().get().await.expect("checkout");
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        drop(busy);
    });
    assert!(
        store
            .try_lock("slow-pool", "a", Duration::from_secs(1))
            .await
            .unwrap(),
        "a gets the key once the pool frees"
    );
    release.await.expect("join");
    assert!(
        !store
            .try_lock("slow-pool", "b", Duration::from_secs(1))
            .await
            .unwrap(),
        "a's lock is still live"
    );
}

/// The handler records a step, then returns `500` (not stored), so its lock
/// is released. The recovery point then lives the 1 s TTL from the release,
/// not until the 3 s lock deadline plus the TTL: a retry at 1.5 s starts over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn released_recovery_point_keeps_the_configured_ttl() {
    let substrate = SqliteSubstrate::with_migrations(&[&FRAMEWORK_MIGRATIONS, &APP_MIGRATIONS])
        .expect("substrate");
    let calls = Calls::default();
    let store = Arc::new(DbIdempotencyStore::new(
        substrate.pool(),
        Duration::from_secs(1),
    ));
    let pool = substrate.pool();
    let handler_calls = calls.clone();
    let handler = move |idem: IdempotencyTx| {
        let pool = pool.clone();
        let calls = handler_calls.clone();
        async move {
            let first = calls.get() == 0;
            calls.add();
            let mut pooled = pool.get().await.expect("checkout");
            let conn: &mut RuntimeConnection = &mut pooled;
            if let Some(point) = idem.recovery_point(conn).await.expect("recovery point") {
                return (StatusCode::OK, format!("resumed after {point}"));
            }
            if first {
                let step = idem.clone();
                conn.transaction::<_, autumn_web::AutumnError, _>(async move |conn| {
                    step.set_recovery_point(conn, "charged").await
                })
                .await
                .expect("transaction");
                return (StatusCode::INTERNAL_SERVER_ERROR, "failed".to_owned());
            }
            (StatusCode::OK, "started over".to_owned())
        }
    };
    let app = axum::Router::new()
        .route("/step", axum::routing::post(handler))
        .layer(
            IdempotencyLayer::new(store)
                .with_ttl(Duration::from_secs(1))
                .with_in_flight_ttl(Duration::from_secs(3)),
        );

    let first = app
        .clone()
        .oneshot(step("charge", "A"))
        .await
        .expect("infallible");
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let retry = app
        .clone()
        .oneshot(step("charge", "A"))
        .await
        .expect("infallible");
    let body = axum::body::to_bytes(retry.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(
        &body[..],
        b"started over",
        "the recovery point expired with its TTL"
    );
}
