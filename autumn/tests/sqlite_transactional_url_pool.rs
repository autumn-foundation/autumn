//! Explicit-URL pool proof for the `SQLite` runtime test harness (issue #1614,
//! Codex P2).
//!
//! Under `--features sqlite`, `TestApp::with_transactional_db("sqlite:...")`
//! records an explicit database URL but the harness has no Postgres-style
//! transactional-rollback isolation to apply to it. The bug: the `SQLite` build
//! branch of `TestApp::build` used to discard that URL and hand back the app's
//! (absent) pool unchanged, so a `TestApp` built with an explicit `sqlite:` URL
//! ended up with NO pool — and every route using the [`Db`] extractor answered
//! **503**.
//!
//! The fix builds a plain (non-transactional) `SQLite` pool from the recorded
//! URL through the same runtime `create_pool` path production uses. This test
//! drives a real `Db`-extractor route served against a `TestApp` built with
//! `with_transactional_db("sqlite://<tempfile>")` and asserts it answers
//! **200**, not 503 — proving the explicit URL is honored with a live pool.
//!
//! Needs `test-support` for the public [`TestApp`] harness, so the file is
//! `#![cfg(all(feature = "sqlite", feature = "test-support"))]` — a default
//! `cargo test` (and a bare `--features sqlite` run) compiles it to an empty
//! (passing) binary. Run explicitly (never via a members-enable edge — that
//! would trip the feature-unification hazard):
//!
//! ```sh
//! cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_transactional_url_pool
//! ```
#![cfg(all(feature = "sqlite", feature = "test-support"))]

use autumn_web::prelude::*;
use autumn_web::reexports::diesel;
use autumn_web::test::TestApp;

use diesel::sql_types::BigInt;
use diesel_async::RunQueryDsl as _;

#[derive(diesel::QueryableByName)]
struct One {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Reads a constant through the **real `Db` extractor** — this handler only runs
/// if `Db` resolved a live pool. With no pool the extractor short-circuits with
/// 503 before the body ever executes, which is exactly the dropped-URL bug.
#[get("/ping")]
async fn ping(mut db: Db) -> AutumnResult<Json<i64>> {
    let rows: Vec<One> = diesel::sql_query("SELECT 1 AS n").load(&mut *db).await?;
    Ok(Json(rows.into_iter().next().map_or(0, |r| r.n)))
}

#[tokio::test]
async fn with_transactional_db_sqlite_url_yields_a_live_pool() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("explicit_url.db");
    let url = format!("sqlite://{}", db_path.display());

    let client = TestApp::new()
        .routes(routes![ping])
        .with_transactional_db(url)
        .build();

    client
        .get("/ping")
        .send()
        .await
        .assert_status(200)
        .assert_body_eq("1");
}
