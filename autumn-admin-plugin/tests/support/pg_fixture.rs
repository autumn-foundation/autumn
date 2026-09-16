//! A Postgres fixture for the admin-model integration tests (issue #2108).
//!
//! Include it with `#[path = "support/pg_fixture.rs"] mod pg_fixture;`. Cargo
//! builds `tests/*.rs` and `tests/<dir>/main.rs` as test binaries. This file is
//! neither, so it never becomes one.
//!
//! The fixture starts a testcontainers Postgres by default. Set
//! `AUTUMN_ADMIN_TEST_PG_URL` to use a server that already runs. Use this for a
//! developer with no Docker, or for a CI `services:` block. Each call then
//! makes its own database, so the tests stay isolated either way.
//!
//! The external-URL path drops its database again when the fixture drops. It
//! also drops a database of the same name BEFORE it makes one: process ids
//! repeat, so a name can come back on a long-lived server.

use diesel::connection::SimpleConnection;
use diesel::{Connection, PgConnection};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use std::time::Duration;
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
    /// `(admin URL, database name)` for the external-URL path, which owns the
    /// database it made. `None` for the container path: the container goes.
    owned: Option<(String, String)>,
}

impl Drop for PgFixture {
    fn drop(&mut self) {
        let Some((admin_url, name)) = self.owned.take() else {
            return;
        };
        // `WITH (FORCE)` (Postgres 13+) ends the sessions the pool still holds.
        // Best effort: a failure here must not mask the test's own result.
        if let Ok(mut admin) = PgConnection::establish(&admin_url) {
            let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
        }
    }
}

/// Give each external-URL test its own database name.
static NEXT_DB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Replace the database name in a `postgres://host/name?query` URL.
///
/// The query string is kept. A server that needs `?sslmode=require` must still
/// get it, or the new database connects with a confusing failure.
fn with_database(url: &str, database: &str) -> String {
    let (head, query) = url.split_once('?').map_or((url, ""), |(h, q)| (h, q));
    let (base, _) = head.rsplit_once('/').unwrap_or((head, ""));
    if query.is_empty() {
        format!("{base}/{database}")
    } else {
        format!("{base}/{database}?{query}")
    }
}

/// Build a pool of `max_size` connections on `url`.
///
/// A deadpool checkout has no timeout unless the pool also gets a runtime
/// handle, and `deadpool` is not a dependency of this crate. Callers that build
/// a `max_size(1)` pool therefore wrap the read under test in [`within`], so an
/// overlapping checkout fails the test instead of hanging CI.
pub fn pool_for(url: &str, max_size: usize) -> Pool<::autumn_web::RuntimeConnection> {
    let manager = AsyncDieselConnectionManager::<::autumn_web::RuntimeConnection>::new(url);
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .expect("pool")
}

/// Run `fut` with a deadline, and name what timed out.
///
/// A one-connection pool turns an overlapping checkout into a wait with no end.
/// `experiments.rs`'s history page drops its connection before the
/// grouped-aggregate count for exactly that reason. Without a deadline, a
/// regression there stops the job on its global timeout with no message.
#[allow(dead_code, reason = "not every test binary uses a one-connection pool")]
pub async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), fut)
        .await
        .unwrap_or_else(|_elapsed| {
            panic!("{what} did not finish in 20s — an overlapping pool checkout?")
        })
}

/// Start Postgres, run `schema_sql`, and return a fixture on the new database.
pub async fn setup(schema_sql: &str) -> PgFixture {
    let (url, container, owned) = if let Ok(base) = std::env::var("AUTUMN_ADMIN_TEST_PG_URL") {
        let n = NEXT_DB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("autumn_admin_test_{}_{n}", std::process::id());
        let mut admin = PgConnection::establish(&base).expect("connect to the given server");
        // A process id repeats. Clear a stale database of this name first.
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .expect("drop a stale test database");
        admin
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .expect("create the test database");
        (
            with_database(&base, &name),
            None,
            Some((base.clone(), name)),
        )
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
            None,
        )
    };

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute(schema_sql).expect("create the schema");

    let pool = pool_for(&url, 5);
    PgFixture {
        pool,
        url,
        container,
        owned,
    }
}

// ── Session time-zone helpers ────────────────────────────────────────────────
//
// The timestamp guards for issue #2108 all need the same thing: a connection
// that is NOT on UTC, and proof that it stayed that way across the read under
// test. diesel-async sets every new connection to UTC (`set_config_options` in
// `diesel_async::pg`), so the zone has to be set on a pooled connection, and
// the pool has to hold exactly one.

/// The zone these guards use. In January it is five hours behind UTC, so a
/// time-zone-sensitive read of `2024-01-15 12:34:56+00` returns 07:34:56.
#[allow(dead_code, reason = "not every test binary checks the time zone")]
pub const PROBE_ZONE: &str = "America/New_York";

/// One row of `SELECT current_setting('TimeZone')`.
#[derive(diesel::QueryableByName)]
struct ZoneRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    zone: String,
}

/// Move the pool's single connection off UTC, and prove it took.
#[allow(dead_code, reason = "not every test binary checks the time zone")]
pub async fn pin_session_off_utc(pool: &Pool<::autumn_web::RuntimeConnection>) {
    use diesel_async::RunQueryDsl;

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!("SET TIME ZONE '{PROBE_ZONE}'"))
        .execute(&mut conn)
        .await
        .expect("set the session time zone");
    drop(conn);
    assert_session_off_utc(pool).await;
}

/// Prove the pool still hands out the non-UTC session.
///
/// Call it AFTER the read under test. The guard rests on a one-connection pool
/// and on deadpool not resetting session settings. If either changes, the read
/// silently lands on a fresh UTC connection and the guard stops guarding.
#[allow(dead_code, reason = "not every test binary checks the time zone")]
pub async fn assert_session_off_utc(pool: &Pool<::autumn_web::RuntimeConnection>) {
    use diesel_async::RunQueryDsl;

    let mut conn = pool.get().await.expect("conn");
    let zone = diesel::sql_query("SELECT current_setting('TimeZone') AS zone")
        .get_result::<ZoneRow>(&mut conn)
        .await
        .expect("read the session time zone")
        .zone;
    assert_eq!(
        zone, PROBE_ZONE,
        "the read under test must run on a non-UTC session"
    );
}
