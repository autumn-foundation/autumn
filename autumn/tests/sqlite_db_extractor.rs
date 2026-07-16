//! Real `Db`-extractor proof for the `SQLite` runtime lane (issue #1614).
//!
//! The sibling `sqlite_boot_serve` test checks a connection out of the pool
//! **directly** (`pool.get()`), so it never runs the per-checkout initialization
//! SQL that the framework's [`autumn_web::db::Db`] extractor issues in
//! `Db::checkout`. That gap hid a Codex P1: `Db::checkout` unconditionally ran
//! the Postgres session GUC `SET statement_timeout = …`, which SQLite rejects —
//! turning **every** route that extracts `Db` into a 503 under `--features
//! sqlite`, even though a plain `pool.get()` worked fine.
//!
//! This test closes that gap by driving the **actual** `Db` extractor: a minimal
//! Axum app whose handler takes `Db` and runs a real query through it, served
//! over HTTP against a `sqlite:` target. It must answer **200** (not 503),
//! proving the checkout hot path no longer issues Postgres-only initialization
//! SQL on the SQLite backend.
//!
//! Run it explicitly (never via a members-enable edge — that would trip the
//! feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features sqlite --test sqlite_db_extractor
//! ```
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{Db, DbState, RuntimeConnection, create_pool};
use autumn_web::reexports::{axum, diesel, diesel_async};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use tower::ServiceExt as _; // for `oneshot`

/// The runtime pool type. Under `--features sqlite` `RuntimeConnection` resolves
/// to `SyncConnectionWrapper<SqliteConnection>`.
type SqlitePool = Pool<RuntimeConnection>;

/// Minimal app state exposing the SQLite pool to the `Db` extractor.
#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
}

impl DbState for AppState {
    fn pool(&self) -> Option<&SqlitePool> {
        Some(&self.pool)
    }
}

#[derive(diesel::QueryableByName)]
struct Greeting {
    #[diesel(sql_type = diesel::sql_types::Text)]
    message: String,
}

/// Answers `/greet` by reading the seeded row **through the real `Db`
/// extractor** — so this handler only runs if `Db::checkout` succeeded on the
/// SQLite backend (i.e. it did not issue Postgres-only initialization SQL).
async fn greet(mut db: Db) -> Result<String, StatusCode> {
    let rows: Vec<Greeting> = diesel::sql_query("SELECT message FROM greetings WHERE id = 1")
        .load(&mut *db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    rows.into_iter()
        .next()
        .map(|g| g.message)
        .ok_or(StatusCode::NOT_FOUND)
}

#[tokio::test]
async fn db_extractor_serves_a_sqlite_backed_request_with_200() {
    // A tempfile-backed database so the seed and the served request observe the
    // same data regardless of how deadpool recycles connections.
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("extractor.db");
    let url = format!("sqlite://{}", db_path.display());

    // Build the real SQLite pool through the public `create_pool` path.
    let config = DatabaseConfig {
        url: Some(url),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via the build_sqlite_pool path")
        .expect("a url is configured");

    // Apply a minimal schema and seed a row on a pooled connection.
    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query("CREATE TABLE greetings (id INTEGER PRIMARY KEY, message TEXT NOT NULL)")
            .execute(&mut *conn)
            .await
            .expect("create table on sqlite");
        diesel::sql_query(
            "INSERT INTO greetings (id, message) VALUES (1, 'hello via Db extractor')",
        )
        .execute(&mut *conn)
        .await
        .expect("seed row on sqlite");
    }

    // A minimal app whose handler extracts `Db` — this is the path that used to
    // 503 under SQLite because `Db::checkout` ran `SET statement_timeout`.
    let app: Router = Router::new()
        .route("/greet", get(greet))
        .with_state(AppState { pool });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/greet")
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("router serves the request");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the real `Db`-extractor route must be 200 under SQLite, not 503 from a \
         Postgres-only `SET statement_timeout` in Db::checkout"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(
        &body[..],
        b"hello via Db extractor",
        "response body is the row SELECTed through the Db extractor"
    );
}
