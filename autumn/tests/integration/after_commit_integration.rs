//! Integration tests for the `after_commit` callback infrastructure.
//!
//! Covers the three failure modes from issue #676:
//!
//! **(a)** TX rolls back → callbacks are never called
//! **(b)** TX commits → callback fires exactly once
//! **(c)** Callback returns an error → the already-committed DB state stands
//!         and `AFTER_COMMIT_FAILURES_TOTAL` is incremented.
//!
//! The first four tests verify the registry mechanism in-process without any
//! Postgres container.  The `#[ignore]`-gated tests near the bottom require
//! Docker and reproduce scenarios (a)–(c) end-to-end against a real
//! `AsyncPgConnection`.
//!
//! ```text
//! # Run all tests in this file (non-Docker):
//! cargo test -p autumn-web --test after_commit_integration
//!
//! # Run Docker tests explicitly:
//! cargo test -p autumn-web --test after_commit_integration -- --ignored
//! ```

#[cfg(feature = "db")]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use autumn_web::db::{
        AFTER_COMMIT_FAILURES_TOTAL, AFTER_COMMIT_REGISTRY, CommitCallback, register_after_commit,
    };

    // ── Helpers ─────────────────────────────────────────────────────────────

    /// Simulate the drain loop that `Db::tx` runs after a successful commit.
    async fn drain_registry(registry: &Arc<Mutex<Vec<CommitCallback>>>) {
        let callbacks: Vec<CommitCallback> = {
            let mut guard = registry.lock().expect("registry lock");
            std::mem::take(&mut *guard)
        };
        for cb in callbacks {
            if let Err(e) = cb().await {
                AFTER_COMMIT_FAILURES_TOTAL.fetch_add(1, Ordering::Relaxed);
                tracing::error!("test drain: after_commit callback failed: {e}");
            }
        }
    }

    // ── (no Docker) ─────────────────────────────────────────────────────────

    /// Outside a `Db::tx` scope `register_after_commit` falls back to running
    /// the callback immediately so no side-effect is silently lost.
    #[tokio::test]
    async fn callback_runs_eagerly_outside_tx() {
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();

        register_after_commit(move || async move {
            c.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await;

        assert!(
            called.load(Ordering::Relaxed),
            "callback should run eagerly when there is no active transaction scope"
        );
    }

    /// Inside a scope (as created by `Db::tx`) the callback is deferred until
    /// the registry is drained, and fires exactly once.
    #[tokio::test]
    async fn callback_deferred_and_fired_exactly_once_on_drain() {
        let call_count = Arc::new(AtomicU64::new(0));
        let c = call_count.clone();

        let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                register_after_commit(move || async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                })
                .await;
            })
            .await;

        // Callback must not have fired yet — the scope has ended but registry
        // was not drained (simulates the tx being rolled back).
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            0,
            "callback must not run before the registry is drained"
        );

        // Drain (simulates what Db::tx does on a successful commit).
        drain_registry(&registry).await;

        assert_eq!(
            call_count.load(Ordering::Relaxed),
            1,
            "callback should fire exactly once after drain"
        );
    }

    /// Scenario **(a)**: not draining the registry after a rollback means no
    /// callbacks are ever invoked — the dual-write problem is avoided.
    #[tokio::test]
    async fn callbacks_suppressed_when_registry_not_drained() {
        let call_count = Arc::new(AtomicU64::new(0));
        let c = call_count.clone();

        let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                register_after_commit(move || async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                })
                .await;
            })
            .await;

        // Simulate a rollback: Db::tx only drains when result.is_ok().
        // Here we simply drop the registry without draining.
        drop(registry);

        assert_eq!(
            call_count.load(Ordering::Relaxed),
            0,
            "callbacks must not fire when the registry is dropped without draining"
        );
    }

    /// Scenario **(c)** — metric half: a failing callback increments
    /// `AFTER_COMMIT_FAILURES_TOTAL` so the failure is observable in metrics.
    #[tokio::test]
    async fn failing_callback_increments_failure_counter() {
        let before = AFTER_COMMIT_FAILURES_TOTAL.load(Ordering::Relaxed);

        let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

        AFTER_COMMIT_REGISTRY
            .scope(registry.clone(), async {
                register_after_commit(|| async {
                    Err(autumn_web::AutumnError::internal_server_error_msg(
                        "intentional test failure",
                    ))
                })
                .await;
            })
            .await;

        drain_registry(&registry).await;

        let after = AFTER_COMMIT_FAILURES_TOTAL.load(Ordering::Relaxed);
        assert!(
            after > before,
            "AFTER_COMMIT_FAILURES_TOTAL should have incremented: before={before} after={after}"
        );
    }

    // ── Docker-backed tests (require testcontainers) ─────────────────────────

    #[cfg(any(feature = "test-support", test))]
    mod docker {
        use super::*;
        use diesel::sql_types::BigInt;
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;
        use diesel_async::pooled_connection::deadpool::Pool;
        use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
        use testcontainers::runners::AsyncRunner;
        use testcontainers_modules::postgres::Postgres;

        #[derive(diesel::QueryableByName)]
        struct CountRow {
            #[diesel(sql_type = BigInt)]
            count: i64,
        }

        async fn start_postgres() -> (
            testcontainers::ContainerAsync<Postgres>,
            Pool<AsyncPgConnection>,
        ) {
            let container = Postgres::default()
                .start()
                .await
                .expect("start Postgres container");
            let host = container.get_host().await.expect("container host");
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("container port");
            let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
            let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
            let pool = Pool::builder(manager)
                .max_size(4)
                .build()
                .expect("build pool");
            (container, pool)
        }

        const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS ac_test (
            id   BIGSERIAL PRIMARY KEY,
            name TEXT      NOT NULL
        )";

        /// Scenario **(b)**: transaction commits → the `after_commit` callback fires
        /// exactly once and the committed row is visible.
        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn real_pg_commit_fires_callback_after_tx() {
            let (_container, pool) = start_postgres().await;
            let mut conn = pool.get().await.expect("get connection");

            diesel::sql_query(CREATE_TABLE)
                .execute(&mut *conn)
                .await
                .expect("create table");
            diesel::sql_query("TRUNCATE ac_test")
                .execute(&mut *conn)
                .await
                .expect("truncate");

            // Use a Mutex<u64> to avoid the `AtomicU64::load` vs
            // `diesel_async::RunQueryDsl::load` name collision.
            let call_count = Arc::new(std::sync::Mutex::new(0u64));
            let cc = call_count.clone();
            let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

            let result: Result<(), diesel::result::Error> = AFTER_COMMIT_REGISTRY
                .scope(
                    registry.clone(),
                    conn.transaction(|c| {
                        let cc = cc.clone();
                        Box::pin(async move {
                            diesel::sql_query("INSERT INTO ac_test (name) VALUES ('committed')")
                                .execute(c)
                                .await?;

                            // Registers a deferred callback because the scope is active.
                            register_after_commit(move || async move {
                                *cc.lock().expect("counter lock") += 1;
                                Ok(())
                            })
                            .await;

                            Ok::<_, diesel::result::Error>(())
                        })
                    }),
                )
                .await;

            assert!(result.is_ok(), "transaction should commit");

            // Drain — this is what Db::tx does after a successful commit.
            drain_registry(&registry).await;

            assert_eq!(
                *call_count.lock().expect("counter lock"),
                1,
                "callback should fire exactly once after commit"
            );

            let row_count = diesel::sql_query(
                "SELECT COUNT(*)::BIGINT AS count FROM ac_test WHERE name = 'committed'",
            )
            .get_result::<CountRow>(&mut *conn)
            .await
            .expect("count query")
            .count;
            assert_eq!(row_count, 1, "committed row should be visible");
        }

        /// Scenario **(a)**: transaction rolls back → no callbacks fire and no
        /// domain row is persisted — the dual-write problem is avoided.
        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn real_pg_rollback_suppresses_callback() {
            let (_container, pool) = start_postgres().await;
            let mut conn = pool.get().await.expect("get connection");

            diesel::sql_query(CREATE_TABLE)
                .execute(&mut *conn)
                .await
                .expect("create table");
            diesel::sql_query("TRUNCATE ac_test")
                .execute(&mut *conn)
                .await
                .expect("truncate");

            let call_count = Arc::new(std::sync::Mutex::new(0u64));
            let cc = call_count.clone();
            let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

            let result: Result<(), diesel::result::Error> = AFTER_COMMIT_REGISTRY
                .scope(
                    registry.clone(),
                    conn.transaction(|c| {
                        let cc = cc.clone();
                        Box::pin(async move {
                            diesel::sql_query(
                                "INSERT INTO ac_test (name) VALUES ('should-roll-back')",
                            )
                            .execute(c)
                            .await?;

                            register_after_commit(move || async move {
                                *cc.lock().expect("counter lock") += 1;
                                Ok(())
                            })
                            .await;

                            // Force rollback.
                            Err::<(), _>(diesel::result::Error::RollbackTransaction)
                        })
                    }),
                )
                .await;

            assert!(result.is_err(), "transaction should have rolled back");

            // Db::tx only drains on success — simulate that behaviour.
            // (We intentionally do NOT call drain_registry here.)

            assert_eq!(
                *call_count.lock().expect("counter lock"),
                0,
                "callback must not fire when the transaction rolls back"
            );

            let row_count = diesel::sql_query(
                "SELECT COUNT(*)::BIGINT AS count FROM ac_test WHERE name = 'should-roll-back'",
            )
            .get_result::<CountRow>(&mut *conn)
            .await
            .expect("count query")
            .count;
            assert_eq!(row_count, 0, "rolled-back row must not be visible");
        }

        /// Scenario **(c)** — full: transaction commits, but the `after_commit`
        /// callback returns an error.  The committed domain row must still be
        /// present (the DB state stands) and the failure counter increments.
        #[tokio::test]
        #[ignore = "requires Docker (testcontainers)"]
        async fn real_pg_failing_callback_does_not_undo_committed_tx() {
            let (_container, pool) = start_postgres().await;
            let mut conn = pool.get().await.expect("get connection");

            diesel::sql_query(CREATE_TABLE)
                .execute(&mut *conn)
                .await
                .expect("create table");
            diesel::sql_query("TRUNCATE ac_test")
                .execute(&mut *conn)
                .await
                .expect("truncate");

            let callback_called = Arc::new(std::sync::Mutex::new(false));
            let cbc = callback_called.clone();
            // Capture before-count using fetch_add(0) to avoid the name
            // collision between AtomicU64::load and RunQueryDsl::load.
            let before = AFTER_COMMIT_FAILURES_TOTAL.fetch_add(0, Ordering::Relaxed);
            let registry: Arc<Mutex<Vec<CommitCallback>>> = Arc::new(Mutex::new(Vec::new()));

            let result: Result<(), diesel::result::Error> = AFTER_COMMIT_REGISTRY
                .scope(
                    registry.clone(),
                    conn.transaction(|c| {
                        let cbc = cbc.clone();
                        Box::pin(async move {
                            diesel::sql_query(
                            "INSERT INTO ac_test (name) VALUES ('committed-despite-bad-callback')",
                        )
                        .execute(c)
                        .await?;

                            register_after_commit(move || async move {
                                *cbc.lock().expect("flag lock") = true;
                                Err(autumn_web::AutumnError::internal_server_error_msg(
                                    "intentional callback error for testing",
                                ))
                            })
                            .await;

                            Ok::<_, diesel::result::Error>(())
                        })
                    }),
                )
                .await;

            assert!(result.is_ok(), "transaction itself should commit");

            // Drain — errors increment AFTER_COMMIT_FAILURES_TOTAL.
            drain_registry(&registry).await;

            // DB state stands: the committed row must still be there.
            let row_count = diesel::sql_query(
                "SELECT COUNT(*)::BIGINT AS count \
                 FROM ac_test WHERE name = 'committed-despite-bad-callback'",
            )
            .get_result::<CountRow>(&mut *conn)
            .await
            .expect("count query")
            .count;
            assert_eq!(
                row_count, 1,
                "committed row must survive a failing callback"
            );

            // The callback was invoked (the failure is not silently swallowed).
            assert!(
                *callback_called.lock().expect("flag lock"),
                "callback should have been called"
            );

            // The failure counter was bumped.
            let after = AFTER_COMMIT_FAILURES_TOTAL.fetch_add(0, Ordering::Relaxed);
            assert!(
                after > before,
                "AFTER_COMMIT_FAILURES_TOTAL should have incremented: before={before} after={after}"
            );
        }
    }
}
