//! Issue #3062: a crash between the business commit and the relay dispatch
//! loses nothing. After restart, an inbox-deduplicating consumer applies the
//! event exactly once.
//!
//! The test sweeps a crash over every await of "commit, then relay". At each
//! point it kills the app, restarts it on the same database, lets the dead
//! relay's claim expire, and drains. The ledger must then hold one row per
//! committed order. Some crash points fall after the consumer committed and
//! before the relay marked the message sent; there the relay sends again and
//! the inbox drops the copy.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support" --test sim_outbox_crash`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use autumn_web::config::OutboxConfig;
use autumn_web::outbox::{self, Inbox, Outbox, OutboxMessage};
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{Sim, crash_at};
use autumn_web::test::TestApp;
use autumn_web::{AppState, AutumnError, AutumnResult};

use diesel::sql_types::Text;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use scoped_futures::ScopedFutureExt as _;

type SqlitePool = Pool<autumn_web::db::RuntimeConnection>;

const LEASE: Duration = Duration::from_secs(30);

async fn setup(substrate: &SqliteSubstrate) {
    outbox::ensure_schema(&substrate.pool())
        .await
        .expect("outbox tables");
    let mut conn = substrate.pool().get().await.expect("connection");
    diesel::sql_query("CREATE TABLE orders (id TEXT PRIMARY KEY)")
        .execute(&mut conn)
        .await
        .expect("orders table");
    diesel::sql_query(
        "CREATE TABLE ledger (id INTEGER PRIMARY KEY AUTOINCREMENT, order_id TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("ledger table");
}

async fn count(pool: &SqlitePool, table: &str) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = pool.get().await.expect("connection");
    diesel::sql_query(format!("SELECT COUNT(*) AS n FROM {table}"))
        .get_result::<Count>(&mut conn)
        .await
        .expect("count")
        .n
}

/// The business transaction: the order row and its outbox message commit
/// together.
async fn place_order(state: &AppState, order_id: &str) -> AutumnResult<()> {
    let outbox = Outbox::new(state);
    let order_id = order_id.to_owned();
    let mut conn = state.pool().expect("pool").get().await?;
    autumn_web::db::scoped_transaction(&mut *conn, |conn| {
        async move {
            diesel::sql_query("INSERT INTO orders (id) VALUES (?)")
                .bind::<Text, _>(&order_id)
                .execute(conn)
                .await?;
            outbox
                .write(
                    conn,
                    &format!("order:{order_id}"),
                    "order.placed",
                    &serde_json::json!({ "order_id": order_id }),
                )
                .await?;
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}

/// The consumer: the inbox record and the ledger row commit together.
async fn post_to_ledger(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let order_id = message.payload["order_id"]
        .as_str()
        .expect("order_id")
        .to_owned();
    let mut conn = state.pool().expect("pool").get().await?;
    autumn_web::db::scoped_transaction(&mut *conn, |conn| {
        async move {
            if Inbox::new("ledger").seen(conn, &message.id).await? {
                return Ok(());
            }
            diesel::sql_query("INSERT INTO ledger (order_id) VALUES (?)")
                .bind::<Text, _>(order_id)
                .execute(conn)
                .await?;
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}

/// Drops its future on a thread outside the runtime.
///
/// `crash_at` can drop the operation at a `SQLite` await. diesel-async then
/// waits for the statement in flight with `block_in_place`, which panics on
/// the current-thread sim runtime. Outside the runtime the wait is allowed.
/// The statement finishes, as one the database already received would.
struct DropOffRuntime<F: Send>(Option<Pin<Box<F>>>);

impl<F: Future + Send> Future for DropOffRuntime<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        self.get_mut()
            .0
            .as_mut()
            .expect("polled after drop")
            .as_mut()
            .poll(cx)
    }
}

impl<F: Send> Drop for DropOffRuntime<F> {
    fn drop(&mut self) {
        if let Some(future) = self.0.take() {
            std::thread::scope(|scope| {
                scope.spawn(move || drop(future));
            });
        }
    }
}

fn off_runtime<F: Future + Send>(future: F) -> DropOffRuntime<F> {
    DropOffRuntime(Some(Box::pin(future)))
}

fn app(pool: SqlitePool, calls: Arc<AtomicU64>) -> TestApp {
    TestApp::new()
        .with_db(pool)
        .with_outbox(OutboxConfig {
            lease_ms: u64::try_from(LEASE.as_millis()).unwrap(),
            ..OutboxConfig::default()
        })
        .outbox_handler("order.placed", move |state, message| {
            calls.fetch_add(1, Ordering::SeqCst);
            post_to_ledger(state, message)
        })
}

#[tokio::test(start_paused = true)]
async fn sim_outbox_crash_between_commit_and_dispatch_delivers_exactly_once() {
    let mut lost_before_dispatch = false;
    let mut copy_dropped_by_inbox = false;
    let mut index = 0;
    loop {
        let substrate = SqliteSubstrate::new().expect("substrate");
        setup(&substrate).await;
        let pool = substrate.pool();
        let calls = Arc::new(AtomicU64::new(0));

        let mut sim = Sim::from_seed(0x3062);
        sim.build(app(pool.clone(), calls.clone()));
        let state = sim.client().state().clone();
        let outcome = crash_at(
            index,
            off_runtime(async {
                place_order(&state, "o-1").await.expect("commit");
                outbox::drain(&state, 100).await.expect("relay")
            }),
        )
        .await;
        drop(state);

        if let Some(sent) = outcome.completed() {
            assert_eq!(sent, 1, "no crash: the relay sends the message once");
            assert_eq!(count(&pool, "ledger").await, 1);
            break;
        }

        let committed = count(&pool, "orders").await;
        let posted_before = count(&pool, "ledger").await;
        let calls_before = AtomicU64::load(&calls, Ordering::SeqCst);

        sim.crash_and_restart(app(pool.clone(), calls.clone()));
        sim.advance(LEASE).await;
        sim.run_to_idle().await;

        assert_eq!(
            count(&pool, "ledger").await,
            committed,
            "crash at await {index}: each committed order is posted exactly once"
        );
        lost_before_dispatch |= committed == 1 && calls_before == 0;
        copy_dropped_by_inbox |=
            posted_before == 1 && AtomicU64::load(&calls, Ordering::SeqCst) > 1;

        index += 1;
        assert!(index < 200, "the operation completes within 200 awaits");
    }

    assert!(
        lost_before_dispatch,
        "a crash point sits between the commit and the relay dispatch"
    );
    assert!(
        copy_dropped_by_inbox,
        "a crash point sits after the consumer and before the relay mark, so the inbox drops a copy"
    );
}
