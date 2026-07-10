//! Integration tests for #1262: automatic capture of a request's SQL query
//! list and the `TestResponse` query-count / N+1 assertions built on it.
//!
//! Capture requires **zero** manual interceptor wiring: driving a request with
//! `TestClient` against a real Postgres pool (`with_db`) is enough for
//! `TestResponse::query_count` / `assert_max_queries` / `assert_no_n_plus_one`
//! to see every statement the handler issued.
//!
//! Requires Docker (testcontainers), so both tests are `#[ignore]`d and run in
//! CI with:
//!
//!     cargo test -p autumn-web --test integration_tests -- --include-ignored query_count

#[cfg(all(feature = "db", feature = "test-support"))]
mod query_count_tests {
    use autumn_web::prelude::*;
    use autumn_web::test::{TestApp, TestDb};
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;

    diesel::table! {
        qc_posts (id) {
            id -> Int8,
            title -> Text,
        }
    }

    #[derive(Debug, Queryable, Selectable)]
    #[diesel(table_name = qc_posts)]
    struct Post {
        pub id: i64,
        pub title: String,
    }

    /// Issues exactly one application query: a single `SELECT` over all posts.
    #[get("/posts-flat")]
    async fn posts_flat(mut db: Db) -> AutumnResult<Json<usize>> {
        let posts = qc_posts::table
            .select(Post::as_select())
            .load(&mut *db)
            .await?;
        Ok(Json(posts.len()))
    }

    /// Classic N+1: one `SELECT` for the list, then one `SELECT` per row. The
    /// per-row statement normalizes to a single template repeated once per row.
    #[get("/posts-n-plus-one")]
    async fn posts_n_plus_one(mut db: Db) -> AutumnResult<Json<usize>> {
        let posts = qc_posts::table
            .select(Post::as_select())
            .load(&mut *db)
            .await?;
        let mut total = 0usize;
        for p in &posts {
            let one: Post = qc_posts::table
                .filter(qc_posts::id.eq(p.id))
                .select(Post::as_select())
                .first(&mut *db)
                .await?;
            total += one.title.len();
        }
        Ok(Json(total))
    }

    static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    async fn setup(db: &TestDb) {
        SETUP
            .get_or_init(|| async {
                db.execute_sql(
                    "CREATE TABLE IF NOT EXISTS qc_posts (
                        id BIGSERIAL PRIMARY KEY,
                        title TEXT NOT NULL
                    )",
                )
                .await;
                db.execute_sql("DELETE FROM qc_posts").await;
                db.execute_sql(
                    "INSERT INTO qc_posts (title) VALUES ('a'),('bb'),('ccc'),('dddd'),('eeeee')",
                )
                .await;
            })
            .await;
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn flat_handler_query_count_and_max_queries() {
        let db = TestDb::shared().await;
        setup(db).await;

        let client = TestApp::new()
            .routes(routes![posts_flat])
            .with_db(db.pool())
            .build();

        let resp = client.get("/posts-flat").send().await;
        resp.assert_ok();

        // The housekeeping `SET statement_timeout` at checkout is excluded, so
        // the flat handler's single SELECT is the only counted query.
        assert_eq!(
            resp.query_count(),
            1,
            "flat handler issues exactly one application query, got: {:?}",
            resp.queries()
        );
        resp.assert_max_queries(1).assert_no_n_plus_one();
    }

    /// Finding-2 regression: with `observability.server_timing` enabled the
    /// router installs `ServerTimingLayer`, whose inner `REQUEST_DB_TIMINGS`
    /// scope previously SHADOWED the harness's outer capturing accumulator —
    /// so `take_captured()` came back empty and query assertions silently
    /// passed at zero. With `scope_inner` reusing the active outer accumulator,
    /// capture still works AND the `Server-Timing` header is emitted: the flat
    /// handler's single SELECT is both counted for the header and captured for
    /// the assertions.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn server_timing_enabled_still_captures_queries() {
        use autumn_web::config::AutumnConfig;

        let db = TestDb::shared().await;
        setup(db).await;

        let mut config = AutumnConfig {
            profile: Some("test".into()),
            ..AutumnConfig::default()
        };
        config.security.csrf.enabled = false;
        // Force the Server-Timing layer on (off by default in the test profile).
        config.observability.server_timing = Some(true);

        let client = TestApp::new()
            .config(config)
            .routes(routes![posts_flat])
            .with_db(db.pool())
            .build();

        let resp = client.get("/posts-flat").send().await;
        resp.assert_ok();

        // The Server-Timing layer is active: it must have stamped the header,
        // including the `db` metric for the one counted query.
        let server_timing = resp
            .header("server-timing")
            .expect("server_timing enabled must emit a Server-Timing header")
            .to_owned();
        assert!(
            server_timing.contains("db;dur="),
            "the db metric should surface the counted query: {server_timing:?}"
        );

        // Crucially, query CAPTURE must still work despite the layer's inner
        // scope — this is the regression the reuse fix guards.
        assert_eq!(
            resp.query_count(),
            1,
            "server_timing must not zero out captured queries, got: {:?}",
            resp.queries()
        );
        resp.assert_max_queries(1).assert_no_n_plus_one();
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn n_plus_one_handler_trips_assertion() {
        let db = TestDb::shared().await;
        setup(db).await;

        let client = TestApp::new()
            .routes(routes![posts_n_plus_one])
            .with_db(db.pool())
            .build();

        let resp = client.get("/posts-n-plus-one").send().await;
        resp.assert_ok();

        // 1 list query + 5 per-row queries = 6 counted statements.
        assert_eq!(
            resp.query_count(),
            6,
            "captured queries: {:?}",
            resp.queries()
        );

        // The default dev threshold is 5; the per-row template repeats 5 times,
        // so `assert_no_n_plus_one` must fire, naming the request.
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resp.assert_no_n_plus_one();
        }))
        .expect_err("the N+1 handler must trip assert_no_n_plus_one");
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        assert!(
            msg.contains("GET /posts-n-plus-one"),
            "failure names the request: {msg}"
        );
        assert!(msg.contains("5 times"), "failure reports the count: {msg}");
    }
}
