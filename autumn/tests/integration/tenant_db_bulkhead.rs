//! Issue #3072: a per-tenant share of the database pool. A tenant at its
//! connection cap gets `503`; another tenant is not affected.

#[cfg(all(feature = "db", feature = "test-support", not(feature = "sqlite")))]
mod tenant_db_bulkhead_tests {
    use autumn_web::config::AutumnConfig;
    use autumn_web::prelude::*;
    use autumn_web::test::{TestApp, TestDb};

    /// Holds two connections at the same time.
    #[get("/two")]
    async fn two(_first: Db, _second: Db) -> &'static str {
        "ok"
    }

    fn config(max_db_connections: usize) -> AutumnConfig {
        let mut config = AutumnConfig::default();
        config.tenancy.enabled = true;
        config.tenancy.source = "header".into();
        config.tenancy.header_name = "x-tenant-id".into();
        config.tenancy.max_db_connections = max_db_connections;
        config
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn a_tenant_over_its_connection_share_gets_503() {
        let db = TestDb::shared().await;
        let client = TestApp::new()
            .routes(routes![two])
            .with_db(db.pool())
            .config(config(1))
            .build();
        client
            .get("/two")
            .header("x-tenant-id", "noisy")
            .send()
            .await
            .assert_status(503);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn a_tenant_within_its_connection_share_is_served() {
        let db = TestDb::shared().await;
        let client = TestApp::new()
            .routes(routes![two])
            .with_db(db.pool())
            .config(config(2))
            .build();
        client
            .get("/two")
            .header("x-tenant-id", "quiet")
            .send()
            .await
            .assert_status(200);
        // The permits come back when the request ends.
        client
            .get("/two")
            .header("x-tenant-id", "quiet")
            .send()
            .await
            .assert_status(200);
    }
}
