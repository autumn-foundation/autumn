//! `statement_timeout` and `idle_in_transaction_session_timeout` apply inside
//! framework transactions as `SET LOCAL` (issue #3057).
//!
//! A transaction pooler (`PgBouncer` in transaction mode) drops a session
//! `SET`. A `SET LOCAL` in the transaction stays with it. Each test clears the
//! session value first, to simulate the pooler.
//!
//! Requires Docker, or set `AUTUMN_TEST_PG_URL` to use an existing Postgres.

#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod tx_local_timeouts {
    use autumn_web::db::{Db, TxOptions};
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use scoped_futures::ScopedFutureExt;
    use std::time::Duration;
    use testcontainers::ContainerAsync;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;
    use tokio::sync::OnceCell;

    const STATEMENT_TIMEOUT: Duration = Duration::from_millis(300);
    const IDLE_TIMEOUT: Duration = Duration::from_secs(2);

    struct SharedPg {
        _container: Option<ContainerAsync<Postgres>>,
        url: String,
    }

    static SHARED_PG: OnceCell<SharedPg> = OnceCell::const_new();

    async fn pg_url() -> String {
        SHARED_PG
            .get_or_init(|| async {
                if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
                    return SharedPg {
                        _container: None,
                        url,
                    };
                }
                let container = Postgres::default()
                    .start()
                    .await
                    .expect("start Postgres container");
                let host = container.get_host().await.expect("host");
                let port = container.get_host_port_ipv4(5432).await.expect("port");
                SharedPg {
                    url: format!("postgres://postgres:postgres@{host}:{port}/postgres"),
                    _container: Some(container),
                }
            })
            .await
            .url
            .clone()
    }

    async fn pool() -> Pool<AsyncPgConnection> {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(pg_url().await);
        Pool::builder(manager).max_size(4).build().expect("pool")
    }

    #[derive(diesel::QueryableByName)]
    struct Setting {
        #[diesel(sql_type = diesel::sql_types::Text)]
        value: String,
    }

    async fn show(
        conn: &mut AsyncPgConnection,
        name: &str,
    ) -> Result<String, diesel::result::Error> {
        let row: Setting = diesel::sql_query(format!("SELECT current_setting('{name}') AS value"))
            .get_result(conn)
            .await?;
        Ok(row.value)
    }

    /// Simulate a transaction pooler: the session values are gone.
    async fn clear_session_timeouts(db: &mut Db) -> AutumnResult<()> {
        use diesel_async::SimpleAsyncConnection as _;
        (**db)
            .batch_execute("SET statement_timeout = 0; SET idle_in_transaction_session_timeout = 0")
            .await?;
        Ok(())
    }

    #[get("/tx-settings")]
    async fn tx_settings(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {
        clear_session_timeouts(&mut db).await?;
        let (statement, idle) = db
            .tx(|conn| {
                async move {
                    let statement = show(conn, "statement_timeout").await?;
                    let idle = show(conn, "idle_in_transaction_session_timeout").await?;
                    Ok::<_, diesel::result::Error>((statement, idle))
                }
                .scope_boxed()
            })
            .await?;
        let after = show(&mut db, "statement_timeout").await?;
        Ok(Json(
            serde_json::json!({ "statement": statement, "idle": idle, "after": after }),
        ))
    }

    #[get("/tx-with-settings")]
    async fn tx_with_settings(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {
        clear_session_timeouts(&mut db).await?;
        let (statement, idle) = db
            .tx_with(TxOptions::serializable(), |conn| {
                async move {
                    let statement = show(conn, "statement_timeout").await?;
                    let idle = show(conn, "idle_in_transaction_session_timeout").await?;
                    Ok::<_, diesel::result::Error>((statement, idle))
                }
                .scope_boxed()
            })
            .await?;
        Ok(Json(
            serde_json::json!({ "statement": statement, "idle": idle }),
        ))
    }

    #[get("/tx-sleep")]
    async fn tx_sleep(mut db: Db) -> AutumnResult<Json<serde_json::Value>> {
        clear_session_timeouts(&mut db).await?;
        db.tx(|conn| {
            async move {
                diesel::sql_query("SELECT pg_sleep(2)")
                    .execute(conn)
                    .await?;
                Ok::<_, diesel::result::Error>(())
            }
            .scope_boxed()
        })
        .await?;
        Ok(Json(serde_json::json!({ "status": "not cancelled" })))
    }

    /// The repository path: generated code calls `scoped_transaction` on a
    /// connection of its own, not on `Db`.
    #[get("/scoped-settings")]
    async fn scoped_settings(
        axum::Extension(pool): axum::Extension<Pool<AsyncPgConnection>>,
    ) -> AutumnResult<Json<serde_json::Value>> {
        let mut conn = pool
            .get()
            .await
            .map_err(|e| AutumnError::service_unavailable_msg(e.to_string()))?;
        diesel::sql_query("SET statement_timeout = 0")
            .execute(&mut *conn)
            .await?;
        let statement =
            autumn_web::__private::scoped_transaction::<_, AutumnError, _, _>(&mut *conn, |conn| {
                async move { Ok(show(conn, "statement_timeout").await?) }.scope_boxed()
            })
            .await?;
        Ok(Json(serde_json::json!({ "statement": statement })))
    }

    async fn client(
        statement: Option<Duration>,
        idle: Option<Duration>,
    ) -> autumn_web::test::TestClient {
        let pool = pool().await;
        let mut config = autumn_web::config::AutumnConfig::default();
        config.database.statement_timeout = statement;
        config.database.idle_in_transaction_timeout = idle;
        let mut routes = routes![tx_settings, tx_with_settings, tx_sleep, scoped_settings];
        for route in &mut routes {
            if route.name == "scoped_settings" {
                route.handler = route.handler.clone().layer(axum::Extension(pool.clone()));
            }
        }
        TestApp::new()
            .routes(routes)
            .config(config)
            .with_db(pool)
            .build()
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn tx_sets_local_timeouts() {
        let client = client(Some(STATEMENT_TIMEOUT), Some(IDLE_TIMEOUT)).await;
        client
            .get("/tx-settings")
            .send()
            .await
            .assert_status(200)
            .assert_json::<serde_json::Value, _>(|body| {
                assert_eq!(body["statement"], "300ms");
                assert_eq!(body["idle"], "2s");
                // `SET LOCAL` ends with the transaction.
                assert_eq!(body["after"], "0");
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn tx_with_sets_local_timeouts() {
        let client = client(Some(STATEMENT_TIMEOUT), Some(IDLE_TIMEOUT)).await;
        client
            .get("/tx-with-settings")
            .send()
            .await
            .assert_status(200)
            .assert_json::<serde_json::Value, _>(|body| {
                assert_eq!(body["statement"], "300ms");
                assert_eq!(body["idle"], "2s");
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn statement_timeout_cancels_a_slow_query_inside_tx() {
        let client = client(Some(STATEMENT_TIMEOUT), None).await;
        client
            .get("/tx-sleep")
            .send()
            .await
            .assert_status(503)
            .assert_json::<serde_json::Value, _>(|problem| {
                assert_eq!(problem["code"], "autumn.query_timeout");
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn scoped_transaction_in_a_request_sets_local_timeout() {
        let client = client(Some(STATEMENT_TIMEOUT), None).await;
        client
            .get("/scoped-settings")
            .send()
            .await
            .assert_status(200)
            .assert_json::<serde_json::Value, _>(|body| {
                assert_eq!(body["statement"], "300ms");
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn no_timeouts_configured_sets_nothing() {
        let client = client(None, None).await;
        client
            .get("/tx-settings")
            .send()
            .await
            .assert_status(200)
            .assert_json::<serde_json::Value, _>(|body| {
                assert_eq!(body["statement"], "0");
                assert_eq!(body["idle"], "0");
            });
    }
}
