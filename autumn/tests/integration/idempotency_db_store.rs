//! Issue #3061: the database idempotency store on Postgres.
//!
//! **Requires Docker.** CI's `--ignored` sweep over the consolidated binary runs
//! these tests. The Docker-free `SQLite` twin of the crash test is
//! `autumn/tests/sim_idempotency_db.rs`.
//!
//! | Test | Proves |
//! |---|---|
//! | `db_store_lock_record_and_expiry` | the store contract: lock, owner check, record, expiry |
//! | `db_store_crash_after_commit_replays` | AC1 on Postgres: a crash after the commit replays the committed response |
//! | `db_store_fences_a_stale_lock_owner` | a handler whose lock expired cannot commit; one payment only |
//! | `db_store_recovery_point_survives_a_crash` | a multi-step handler reads the last recovery point on retry |

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::config::{AutumnConfig, IdempotencyBackend};
use autumn_web::idempotency::{
    DbIdempotencyStore, IdempotencyRecord, IdempotencyStore as _, IdempotencyTx,
};
use autumn_web::prelude::*;
use autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
use autumn_web::sim::crash_at;
use autumn_web::test::{TestApp, TestClient, TestResponse};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// The migration Autumn ships, applied as is.
const IDEMPOTENCY_UP: &str =
    include_str!("../../migrations/20261005200000_create_idempotency_keys/up.sql");

const PAYMENTS_UP: &str =
    "CREATE TABLE idem_payments (id BIGSERIAL PRIMARY KEY, amount BIGINT NOT NULL)";

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(8).build().expect("pool");
    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute(IDEMPOTENCY_UP)
        .await
        .expect("apply the idempotency migration");
    conn.batch_execute(PAYMENTS_UP)
        .await
        .expect("create payments");
    (pool, container)
}

fn record(body: &str) -> IdempotencyRecord {
    IdempotencyRecord {
        status: 201,
        headers: Vec::new(),
        body: body.as_bytes().to_vec(),
        metadata: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn db_store_lock_record_and_expiry() {
    let (pool, _container) = setup_pool().await;
    let store = DbIdempotencyStore::new(pool, Duration::from_secs(3600));
    let ttl = Duration::from_secs(60);

    assert!(store.try_lock("k", "a", ttl).await.expect("lock"));
    assert!(
        !store.try_lock("k", "b", ttl).await.expect("lock"),
        "held by a"
    );
    store.unlock("k", "b").await.expect("unlock");
    assert!(
        !store.try_lock("k", "b", ttl).await.expect("lock"),
        "b cannot unlock a"
    );
    store.unlock("k", "a").await.expect("unlock");
    assert!(
        store.try_lock("k", "b", ttl).await.expect("lock"),
        "a released it"
    );

    assert!(
        store.get("k").await.expect("get").is_none(),
        "no record yet"
    );
    store
        .set("k", record("done"), b"hash".to_vec(), ttl)
        .await
        .expect("set");
    let entry = store.get("k").await.expect("get").expect("record");
    assert_eq!(entry.record.body, b"done");
    assert_eq!(entry.body_hash, b"hash");
    assert!(
        !store.try_lock("k", "c", ttl).await.expect("lock"),
        "a completed key is not locked again"
    );

    store
        .set(
            "short",
            record("gone"),
            Vec::new(),
            Duration::from_millis(1),
        )
        .await
        .expect("set");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(store.get("short").await.expect("get").is_none(), "expired");
    assert!(
        store.try_lock("short", "d", ttl).await.expect("lock"),
        "an expired key starts over"
    );
}

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

/// Holds the handler before its transaction, so a test can let the lock expire.
#[derive(Clone)]
struct Pause(Duration);

#[post("/pay")]
async fn pay(
    State(state): State<AppState>,
    idem: IdempotencyTx,
    mut db: Db,
) -> AutumnResult<axum::response::Response> {
    let calls = state.extension::<Calls>().expect("calls installed");
    calls.add();
    if let Some(pause) = state.extension::<Pause>() {
        tokio::time::sleep(pause.0).await;
    }
    db.tx(|conn| {
        async move {
            let row: IdRow =
                diesel::sql_query("INSERT INTO idem_payments (amount) VALUES (100) RETURNING id")
                    .get_result(conn)
                    .await?;
            let body = Json(serde_json::json!({ "payment": row.id }));
            idem.commit(conn, (StatusCode::CREATED, body)).await
        }
        .scope_boxed()
    })
    .await
}

fn client(pool: &Pool<AsyncPgConnection>, calls: &Calls, pause: Option<Duration>) -> TestClient {
    let mut config = AutumnConfig::default();
    config.idempotency.backend = IdempotencyBackend::Database;
    config.idempotency.in_flight_ttl_secs = 1;
    let calls = calls.clone();
    TestApp::new()
        .config(config)
        .idempotent()
        .with_db(pool.clone())
        .routes(routes![pay, steps])
        .state_initializer(move |state| {
            state.insert_extension(calls.clone());
            if let Some(pause) = pause {
                state.insert_extension(Pause(pause));
            }
        })
        .build()
}

async fn payments(pool: &Pool<AsyncPgConnection>) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("SELECT COUNT(*) AS n FROM idem_payments")
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count")
        .n
}

async fn send(client: &TestClient, path: &str, key: &str) -> TestResponse {
    client
        .post(path)
        .header("idempotency-key", key)
        .send()
        .await
}

/// Retry until the key is not in flight; the in-flight TTL here is 1 s.
async fn retry(client: &TestClient, path: &str, key: &str) -> TestResponse {
    for _ in 0..10 {
        let response = send(client, path, key).await;
        if response.status != StatusCode::CONFLICT {
            return response;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    panic!("key {key} stayed in flight");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn db_store_crash_after_commit_replays() {
    let (pool, _container) = setup_pool().await;
    let calls = Calls::default();
    let client = client(&pool, &calls, None);

    let mut crashed_after_commit = 0;
    let mut index = 0;
    loop {
        let key = format!("pay-{index}");
        let before_rows = payments(&pool).await;

        let outcome = crash_at(index, send(&client, "/pay", &key)).await;
        if let Some(response) = outcome.completed() {
            response.assert_status(201);
            break;
        }
        // Let the server finish a statement the crash left in flight.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let committed = payments(&pool).await == before_rows + 1;
        let crashed_calls = calls.get();

        let response = retry(&client, "/pay", &key).await;
        response.assert_status(201);
        assert_eq!(payments(&pool).await, before_rows + 1, "await {index}");
        if committed {
            crashed_after_commit += 1;
            assert_eq!(response.header("x-idempotent-replayed"), Some("true"));
            assert_eq!(calls.get(), crashed_calls);
        }

        index += 1;
        assert!(index < 256, "the request completes within 256 awaits");
    }
    assert!(crashed_after_commit > 0, "a crash fell after the commit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn db_store_fences_a_stale_lock_owner() {
    let (pool, _container) = setup_pool().await;
    let calls = Calls::default();
    // The handler waits 2 s; the in-flight TTL is 1 s.
    let client = Arc::new(client(&pool, &calls, Some(Duration::from_secs(2))));

    let slow = tokio::spawn({
        let client = Arc::clone(&client);
        async move { send(&client, "/pay", "fenced").await }
    });
    // The first request holds the lock once its handler runs. Wait for that,
    // then let the 1 s lock expire.
    while calls.get() == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let second = send(&client, "/pay", "fenced").await;
    second.assert_status(201);
    let first = slow.await.expect("join");

    assert_eq!(calls.get(), 2, "both requests ran");
    assert_eq!(payments(&pool).await, 1, "only the lock owner committed");
    first.assert_status(409);
    let replay = send(&client, "/pay", "fenced").await;
    assert_eq!(replay.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(replay.text(), second.text());
}

/// Step 1 records a recovery point; step 2 commits. A crash between the steps
/// leaves the recovery point, and the retry skips step 1.
#[post("/steps")]
async fn steps(
    State(state): State<AppState>,
    idem: IdempotencyTx,
    mut db: Db,
) -> AutumnResult<axum::response::Response> {
    let calls = state.extension::<Calls>().expect("calls installed");
    calls.add();
    let point = idem.recovery_point(&mut db).await?;
    if point.is_none() {
        let idem = idem.clone();
        db.tx(|conn| {
            async move {
                diesel::sql_query("INSERT INTO idem_payments (amount) VALUES (1)")
                    .execute(conn)
                    .await?;
                idem.set_recovery_point(conn, "charged").await
            }
            .scope_boxed()
        })
        .await?;
        if let Some(pause) = state.extension::<Pause>() {
            tokio::time::sleep(pause.0).await;
        }
    }
    db.tx(|conn| {
        async move {
            idem.commit(conn, (StatusCode::CREATED, "done".to_owned()))
                .await
        }
        .scope_boxed()
    })
    .await
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn db_store_recovery_point_survives_a_crash() {
    let (pool, _container) = setup_pool().await;
    let calls = Calls::default();
    let client = client(&pool, &calls, Some(Duration::from_secs(5)));

    // Drop the request when step 1 committed, while it waits before step 2.
    {
        let request = send(&client, "/steps", "multi");
        tokio::pin!(request);
        loop {
            tokio::select! {
                _ = &mut request => panic!("the request finished before the crash"),
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if payments(&pool).await == 1 {
                        break;
                    }
                }
            }
        }
        // The request future drops at the end of this block: the crash.
    }

    let response = retry(&client, "/steps", "multi").await;
    response.assert_status(201);
    assert_eq!(payments(&pool).await, 1, "the retry skipped step 1");
    assert_eq!(calls.get(), 2);
}
