//! A Postgres fixture for the admin-model integration tests (issue #2108).
//!
//! Include it with `#[path = "support/pg_fixture.rs"] mod pg_fixture;`. Cargo
//! discovers only `tests/*.rs`, so this file is never built as its own test
//! binary.
//!
//! The fixture starts a testcontainers Postgres by default. Set
//! `AUTUMN_ADMIN_TEST_PG_URL` to use a server that already runs instead — a
//! local `postgres://…` for a developer with no Docker, or a CI `services:`
//! block. Each call then makes its own database, so the tests stay isolated
//! either way.

use diesel::connection::SimpleConnection;
use diesel::{Connection, PgConnection};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// One ready Postgres database, plus whatever keeps it alive.
pub struct PgFixture {
    /// A pool on the new database.
    pub pool: Pool<::autumn_web::RuntimeConnection>,
    /// The URL of that database, for a test that needs its own pool.
    #[allow(dead_code, reason = "not every test binary builds a second pool")]
    pub url: String,
    #[allow(
        dead_code,
        reason = "the container is held, not read: it stops when this drops"
    )]
    container: Option<Box<testcontainers::ContainerAsync<Postgres>>>,
}

/// Give each external-URL test its own database name.
static NEXT_DB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Replace the database name in a `postgres://host/name` URL.
fn with_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').unwrap_or((url, ""));
    format!("{base}/{database}")
}

/// Build a pool of `max_size` connections on `url`.
pub fn pool_for(url: &str, max_size: usize) -> Pool<::autumn_web::RuntimeConnection> {
    let manager = AsyncDieselConnectionManager::<::autumn_web::RuntimeConnection>::new(url);
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .expect("pool")
}

/// Start Postgres, run `schema_sql`, and return a fixture on the new database.
pub async fn setup(schema_sql: &str) -> PgFixture {
    let (url, container) = if let Ok(base) = std::env::var("AUTUMN_ADMIN_TEST_PG_URL") {
        let n = NEXT_DB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("autumn_admin_test_{}_{n}", std::process::id());
        let mut admin = PgConnection::establish(&base).expect("connect to the given server");
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .expect("create the test database");
        (with_database(&base, &name), None)
    } else {
        let container = Postgres::default()
            .start()
            .await
            .expect("failed to start postgres container");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        (
            format!("postgres://postgres:postgres@{host}:{port}/postgres"),
            Some(Box::new(container)),
        )
    };

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute(schema_sql).expect("create the schema");

    let pool = pool_for(&url, 5);
    PgFixture {
        pool,
        url,
        container,
    }
}
