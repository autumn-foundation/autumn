//! Postgres testcontainer suite for `DbBillingStore`. `#[ignore]`d: needs Docker.
//!
//! Starts Postgres, applies the crate's embedded `MIGRATIONS` through
//! diesel's `MigrationHarness`, and runs the shared store contract
//! (`tests/cases/store_contract.rs`, included by path) against a
//! `DbBillingStore` over a `Pool<AsyncPgConnection>` — the type
//! `autumn_web::RuntimeConnection` resolves to in the default build.
//!
//! Run with `cargo test -p autumn-billing --test mirror_db -- --ignored`.
//! The memory-store cases inside the contract module also compile into this
//! binary and run without Docker.

#[path = "cases/store_contract.rs"]
mod store_contract;

use autumn_billing::{BillingError, BillingStore, DbBillingStore, ProviderId};
use autumn_web::reexports::diesel_migrations::MigrationHarness;
use diesel::Connection;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Start Postgres and return its connection URL. The container must stay
/// alive for the test.
async fn start_postgres() -> (String, ContainerAsync<Postgres>) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        container,
    )
}

/// Apply every pending crate migration through the sync harness, off the
/// async runtime.
async fn migrate(url: &str) {
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut conn = diesel::PgConnection::establish(&url).expect("sync connection");
        conn.run_pending_migrations(autumn_billing::MIGRATIONS)
            .expect("apply billing migrations");
    })
    .await
    .expect("migration task");
}

fn build_pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager).max_size(5).build().expect("pool")
}

async fn setup() -> (Pool<AsyncPgConnection>, ContainerAsync<Postgres>) {
    let (url, container) = start_postgres().await;
    migrate(&url).await;
    (build_pool(&url), container)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn contract_suite_on_postgres() {
    let (pool, _container) = setup().await;
    let store = DbBillingStore::new(pool);
    store_contract::run_contract(&store).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn migrations_are_idempotent_and_revertible() {
    let (url, _container) = start_postgres().await;
    migrate(&url).await;
    // A second run applies nothing and does not fail.
    migrate(&url).await;
    let pool = build_pool(&url);
    let store = DbBillingStore::new(pool.clone());
    assert_eq!(store.applied_event_count().await.unwrap(), 0);

    let revert_url = url.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = diesel::PgConnection::establish(&revert_url).expect("sync connection");
        conn.revert_all_migrations(autumn_billing::MIGRATIONS)
            .expect("revert billing migrations");
    })
    .await
    .expect("revert task");
    assert!(
        matches!(
            store.applied_event_count().await,
            Err(BillingError::Store(_))
        ),
        "the ledger table is gone after down.sql"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn unparseable_stored_status_is_a_store_error() {
    let (pool, _container) = setup().await;
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "INSERT INTO billing_customers (id, provider, provider_customer_id, created_at, updated_at) \
         VALUES ('bad-cust', 'stripe', 'cus_bad', '2024-01-01 00:00:00', '2024-01-01 00:00:00')",
    )
    .execute(&mut conn)
    .await
    .expect("seed customer");
    diesel::sql_query(
        "INSERT INTO billing_subscriptions \
         (id, customer_id, provider_subscription_id, status, quantity, cancel_at_period_end, \
          last_event_at, created_at, updated_at) \
         VALUES ('bad-sub', 'bad-cust', 'sub_bad', 'bogus', 1, 0, \
          '2024-01-01 00:00:00', '2024-01-01 00:00:00', '2024-01-01 00:00:00')",
    )
    .execute(&mut conn)
    .await
    .expect("seed subscription");
    drop(conn);

    let store = DbBillingStore::new(pool);
    let err = store
        .subscription_by_provider_id(&ProviderId::new("sub_bad"))
        .await
        .expect_err("a status that does not parse is an error");
    assert!(matches!(err, BillingError::Store(_)), "{err:?}");
    assert!(err.to_string().contains("bogus"), "{err}");
}
