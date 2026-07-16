//! Startup-migration proof for the `SQLite` runtime lane (issue #1614, PR3).
//!
//! PR2 built the `SQLite` deadpool pool + boot/serve but *gated* registered
//! startup migrations (a `sqlite://` target with `.migrations(...)` failed
//! fast). PR3 replaces that gate with a working `SQLite` migration path. This
//! test exercises the whole chain the way an app does:
//!
//! 1. **Registered migrations apply** — an [`EmbeddedMigrations`] set (exactly
//!    the type [`AppBuilder::migrations`](autumn_web) stores) is applied to a
//!    `sqlite://` file target through the new
//!    [`autumn_web::migrate::run_pending_sqlite`] path — diesel's
//!    `MigrationHarness` on a `SqliteConnection`, with **no** Postgres advisory
//!    lock.
//! 2. **The migrated schema is visible to the runtime pool** — the public
//!    [`autumn_web::db::create_pool`] entry builds the real `SQLite` runtime
//!    pool over the *same* database file, and a checked-out connection reads and
//!    writes the migration-created `widgets` table.
//! 3. **A DB-backed request serves** — a minimal Axum router holds the pool in
//!    state and answers an HTTP request by `INSERT`ing then `SELECT`ing from
//!    `widgets`, driving one real request/response through the migrated schema.
//!
//! A **file** target (tempfile) is deliberate: an in-memory `SQLite` database is
//! private per connection, so migrations applied on the (separate) migration
//! connection would be invisible to the pool. File targets share the database.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_migrations
//! ```
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations, run_pending_sqlite};
use autumn_web::reexports::{axum, diesel, diesel_async};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use tower::ServiceExt as _; // for `oneshot`

/// The app-registered migration set — the identical `EmbeddedMigrations` type
/// `.migrations(MIGRATIONS)` stores. It creates the `widgets` table.
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/sqlite_migrations");

/// The runtime pool type. Under `--features sqlite` `RuntimeConnection` resolves
/// to `SyncConnectionWrapper<SqliteConnection>`.
type SqlitePool = Pool<RuntimeConnection>;

#[derive(diesel::QueryableByName)]
struct Widget {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// Answers `POST /widgets` by writing a row into the migration-created table and
/// reading it back — proving the migrated schema is live through the pool.
async fn create_and_read_widget(State(pool): State<SqlitePool>) -> Result<String, StatusCode> {
    let mut conn = pool
        .get()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    diesel::sql_query("INSERT INTO widgets (id, name) VALUES (1, 'sprocket')")
        .execute(&mut *conn)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows: Vec<Widget> = diesel::sql_query("SELECT name FROM widgets WHERE id = 1")
        .load(&mut *conn)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    rows.into_iter()
        .next()
        .map(|w| w.name)
        .ok_or(StatusCode::NOT_FOUND)
}

#[tokio::test]
async fn registered_migrations_apply_to_sqlite_and_serve() {
    // A tempfile-backed database so the migration connection and every pooled
    // connection observe the same `widgets` table.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("migrate.db");
    let url = format!("sqlite://{}", db_path.display());

    // (1) Apply the registered migrations through the new SQLite path (no
    //     advisory lock). This is the behavior PR2 rejected with a fail-fast
    //     gate; PR3 makes it work.
    let result =
        run_pending_sqlite(&url, MIGRATIONS).expect("registered migrations apply on sqlite");
    assert_eq!(
        result.applied.len(),
        1,
        "exactly the one registered migration is applied (got {:?})",
        result.applied
    );

    // Re-running is a no-op: the migration is already recorded.
    let again =
        run_pending_sqlite(&url, MIGRATIONS).expect("re-running pending migrations is a no-op");
    assert!(
        again.applied.is_empty(),
        "second run applies nothing (got {:?})",
        again.applied
    );

    // (2) Build the real SQLite runtime pool over the same file via the public
    //     `create_pool` entry, and confirm the migrated `widgets` table exists
    //     on a checked-out pooled connection.
    let config = DatabaseConfig {
        url: Some(url),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        // If the migration had not applied, this SELECT would error ("no such
        // table: widgets").
        let rows: Vec<Widget> = diesel::sql_query("SELECT name FROM widgets WHERE 1 = 0")
            .load(&mut *conn)
            .await
            .expect("the migrated `widgets` table is visible to the runtime pool");
        assert!(rows.is_empty(), "no rows seeded yet");
    }

    // (3) A minimal router backed by the SQLite pool serves a DB-backed request
    //     against the migrated schema.
    let app: Router = Router::new()
        .route("/widgets", post(create_and_read_widget))
        .with_state(pool);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/widgets")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router serves the request");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "DB-backed route against the migrated schema is 200"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(
        &body[..],
        b"sprocket",
        "response body is the row written to and read from the migrated `widgets` table"
    );
}

/// A **private** in-memory target with registered migrations is rejected up
/// front with an actionable error (issue #1614 follow-up). Each `:memory:`
/// connection is its own empty database, so migrations applied on the throwaway
/// migration connection could never reach the runtime pool — surfacing the
/// error beats silently applying to a database the pool never sees.
#[test]
fn private_in_memory_target_with_registered_migrations_is_rejected() {
    for url in [
        "sqlite::memory:",
        ":memory:",
        "sqlite://:memory:",
        "file::memory:",
    ] {
        let err = run_pending_sqlite(url, MIGRATIONS)
            .expect_err("private in-memory + registered migrations must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("in-memory"),
            "error names the in-memory problem (got {msg:?})"
        );
        assert!(
            msg.contains("file-backed") && msg.contains("cache=shared"),
            "error gives the file-backed / shared-cache remedy (got {msg:?})"
        );
    }
}

/// A **shared-cache** in-memory target (`file::memory:?cache=shared`) is shared
/// across connections within one process, so it retains migrations for the
/// runtime pool and is NOT rejected — the migration applies exactly like a
/// file-backed target. (The private-vs-shared classification itself is unit
/// tested in `db::sqlite_target_is_private_in_memory_classifies_targets`.)
#[test]
fn shared_cache_in_memory_target_is_not_rejected() {
    let result = run_pending_sqlite("file::memory:?cache=shared", MIGRATIONS)
        .expect("shared-cache in-memory retains migrations for the pool and is not rejected");
    assert_eq!(
        result.applied.len(),
        1,
        "the one registered migration applies on a shared-cache in-memory target (got {:?})",
        result.applied
    );
}
