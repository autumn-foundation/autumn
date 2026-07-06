//! Integration tests for database statement timeouts and slow-query telemetry.
//!
//! **Requires Docker** to be running.

#[cfg(feature = "db")]
mod db_telemetry_tests {
    use autumn_web::db::StatementTimeout;
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use std::time::Duration;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;
    use tokio::sync::OnceCell;

    struct SharedDb {
        _container: testcontainers::ContainerAsync<Postgres>,
        pool: Pool<AsyncPgConnection>,
        _url: String,
    }

    static SHARED_DB: OnceCell<SharedDb> = OnceCell::const_new();

    async fn shared_db() -> &'static SharedDb {
        SHARED_DB
            .get_or_init(|| async {
                let container = Postgres::default()
                    .start()
                    .await
                    .expect("failed to start Postgres container");

                let host = container.get_host().await.unwrap();
                let port = container.get_host_port_ipv4(5432).await.unwrap();
                let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

                let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
                let pool = Pool::builder(manager)
                    .max_size(5)
                    .build()
                    .expect("failed to build pool");

                SharedDb {
                    _container: container,
                    pool,
                    _url: url,
                }
            })
            .await
    }

    async fn setup() -> Pool<AsyncPgConnection> {
        shared_db().await.pool.clone()
    }

    // Endpoints for testing

    #[get("/sleep-default")]
    async fn sleep_default(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {
        // Sleep in Postgres
        diesel::sql_query("SELECT pg_sleep(1)")
            .execute(&mut *db)
            .await?;
        Ok(Json(serde_json::json!({"status": "ok"})))
    }

    #[get("/sleep-override")]
    async fn sleep_override(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {
        // Sleep in Postgres
        diesel::sql_query("SELECT pg_sleep(1)")
            .execute(&mut *db)
            .await?;
        Ok(Json(serde_json::json!({"status": "ok"})))
    }

    #[get("/quick-query")]
    async fn quick_query(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {
        diesel::sql_query("SELECT 1").execute(&mut *db).await?;
        Ok(Json(serde_json::json!({"status": "ok"})))
    }

    #[get("/sleep-extreme")]
    async fn sleep_extreme(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {
        diesel::sql_query("SELECT pg_sleep(0.1)")
            .execute(&mut *db)
            .await?;
        Ok(Json(serde_json::json!({"status": "ok"})))
    }

    // Helper to build test client with config
    fn build_client(
        pool: Pool<AsyncPgConnection>,
        statement_timeout: Option<Duration>,
        slow_threshold: Duration,
    ) -> autumn_web::test::TestClient {
        let mut config = autumn_web::config::AutumnConfig::default();
        config.database.statement_timeout = statement_timeout;
        config.database.slow_query_threshold = slow_threshold;

        let mut routes = routes![sleep_default, sleep_override, quick_query, sleep_extreme];
        for r in &mut routes {
            if r.name == "sleep_override" {
                r.handler = r.handler.clone().layer(::axum::Extension(StatementTimeout(
                    Duration::from_millis(100),
                )));
            } else if r.name == "sleep_extreme" {
                r.handler = r.handler.clone().layer(::axum::Extension(StatementTimeout(
                    Duration::from_millis(u64::MAX)
                        .checked_add(Duration::from_millis(50))
                        .unwrap(),
                )));
            }
        }

        // Custom route extensions for the override path
        TestApp::new()
            .routes(routes)
            .config(config)
            .with_db(pool)
            .build()
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn test_global_statement_timeout() {
        let pool = setup().await;
        // Global statement timeout = 200ms. Query sleeps for 1s -> should cancel.
        let client = build_client(
            pool,
            Some(Duration::from_millis(200)),
            Duration::from_millis(500),
        );

        let resp = client.get("/sleep-default").send().await;
        resp.assert_status(503)
            .assert_json::<serde_json::Value, _>(|prob| {
                assert_eq!(prob["code"], "autumn.query_timeout");
                assert_eq!(prob["type"], "https://autumn.dev/problems/query-timeout");
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn test_route_scoped_timeout_override() {
        let pool = setup().await;
        // Global statement timeout = None. Route override = 100ms. Query sleeps for 1s -> should cancel.
        let client = build_client(pool, None, Duration::from_millis(500));

        let resp = client.get("/sleep-override").send().await;
        resp.assert_status(503)
            .assert_json::<serde_json::Value, _>(|prob| {
                assert_eq!(prob["code"], "autumn.query_timeout");
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn test_slow_query_telemetry_logging_and_metrics() {
        let pool = setup().await;
        // Slow query threshold = 50ms. SELECT pg_sleep(0.1) should trigger slow query metrics/logs.
        let client = build_client(pool, None, Duration::from_millis(50));

        // Sleep for 100ms
        let resp = client.get("/sleep-default").send().await;
        resp.assert_ok();

        // Get metrics snapshot
        let metrics_resp = client.get("/actuator/metrics").send().await;
        metrics_resp
            .assert_ok()
            .assert_json::<serde_json::Value, _>(|metrics| {
                // Ensure db_queries are tracked
                let queries = &metrics["db_queries"];
                assert!(queries.is_object());
                // The key should have route and query operation / name, e.g. "GET /sleep-default SELECT pg_sleep" or similar
                let found_sleep = queries
                    .as_object()
                    .unwrap()
                    .iter()
                    .any(|(k, _v)| k.contains("GET /sleep-default"));
                assert!(
                    found_sleep,
                    "Expected a metrics entry for GET /sleep-default"
                );
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn test_extreme_statement_timeout_clamping() {
        let pool = setup().await;
        let client = build_client(pool, None, Duration::from_millis(500));
        let resp = client.get("/sleep-extreme").send().await;
        resp.assert_ok();
    }
}
