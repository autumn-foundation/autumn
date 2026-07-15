//! Boot + serve proof for the SQLite runtime lane (issue #1614, PR2).
//!
//! Exercises the whole PR2 chain end to end under the `sqlite` feature:
//!
//! 1. **Pool builds** — the public [`autumn_web::db::create_pool`] entry routes a
//!    `sqlite:` URL through the new `build_sqlite_pool` path and hands back a
//!    real deadpool pool over `SyncConnectionWrapper<SqliteConnection>`
//!    (`autumn_web::db::RuntimeConnection` under this feature).
//! 2. **Schema applies** — a minimal table is created and seeded on a checked-out
//!    connection, proving the pooled SQLite connection runs real DDL/DML.
//! 3. **Serves a request backed by SQLite** — a minimal Axum router holds the
//!    pool in state and a handler answers an HTTP request by `SELECT`ing the
//!    seeded row, driving one real request/response through the SQLite pool.
//!
//! This is deliberately a public-API demo (a "minimal router using the pool");
//! the crate's Postgres transactional `TestApp` harness is Postgres-only and not
//! compiled under this feature. Run it explicitly (never via a members-enable
//! edge — that would trip the feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_boot_serve
//! ```
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{axum, diesel, diesel_async};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use tower::ServiceExt as _; // for `oneshot`

/// The runtime pool type. Under `--features sqlite` `RuntimeConnection` resolves
/// to `SyncConnectionWrapper<SqliteConnection>`.
type SqlitePool = Pool<RuntimeConnection>;

#[derive(diesel::QueryableByName)]
struct Greeting {
    #[diesel(sql_type = diesel::sql_types::Text)]
    message: String,
}

/// Answers `/greet` by reading the seeded row from SQLite through the pool.
async fn greet(State(pool): State<SqlitePool>) -> Result<String, StatusCode> {
    let mut conn = pool
        .get()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let rows: Vec<Greeting> = diesel::sql_query("SELECT message FROM greetings WHERE id = 1")
        .load(&mut *conn)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    rows.into_iter()
        .next()
        .map(|g| g.message)
        .ok_or(StatusCode::NOT_FOUND)
}

#[tokio::test]
async fn sqlite_pool_boots_and_serves_a_db_backed_request() {
    // A tempfile-backed database so the seed and the served request observe the
    // same data regardless of how deadpool recycles connections.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("boot.db");
    let url = format!("sqlite://{}", db_path.display());

    // (1) Build the real SQLite pool through the public `create_pool` path.
    let config = DatabaseConfig {
        url: Some(url),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via the new build_sqlite_pool path")
        .expect("a url is configured");

    // (2) Apply a minimal schema and seed a row on a pooled connection.
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query("CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL)")
            .execute(&mut *conn)
            .await
            .expect("create table on sqlite");
        diesel::sql_query("INSERT INTO greetings (id, message) VALUES (1, 'hello from sqlite')")
            .execute(&mut *conn)
            .await
            .expect("seed row on sqlite");
    }

    // (3) A minimal router backed by the SQLite pool serves a real request.
    let app: Router = Router::new().route("/greet", get(greet)).with_state(pool);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/greet")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router serves the request");

    assert_eq!(response.status(), StatusCode::OK, "DB-backed route is 200");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(
        &body[..],
        b"hello from sqlite",
        "response body is the row SELECTed from SQLite"
    );
}
