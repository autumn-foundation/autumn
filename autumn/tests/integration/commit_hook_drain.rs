//! Integration coverage for the test-harness export
//! [`autumn_web::test::drain_ready_repository_commit_hooks`].
//!
//! Exercises the export exactly as a downstream integration test would: a
//! durable repository commit hook is staged into the real
//! `autumn_repository_commit_hooks` queue, its registered runner has a
//! persisted, observable side effect (a row in a second table), and the drain
//! export drives the real claim → run-registered-runner → ack wiring **without**
//! starting the timing-based background commit-hook worker. The whole flow is
//! deterministic: no `sleep`, no timeout-poll.
//!
//! The enqueue + runner registration go through the same low-level durable
//! surface the framework's own `tests/sqlite_commit_hook_worker.rs` uses, rather
//! than a macro `#[repository(commit_hooks = true)]` `save()`. A macro `save()`
//! auto-kicks the dispatcher, which spawns a background drain worker — exactly
//! the timing-based worker this export exists to avoid — so driving the enqueue
//! directly is what keeps the "not fired yet → drain → fired" assertions
//! deterministic. A private [`TestDb::new`] pool (not the shared one) further
//! guarantees no other test's background worker can touch this queue.
//!
//! Run with:
//!
//!     cargo test -p autumn-web --features "db,test-support" \
//!         --test integration_tests commit_hook_drain -- --ignored

#[cfg(all(feature = "db", feature = "test-support"))]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use autumn_web::AutumnError;
    use autumn_web::test::TestDb;
    use diesel_async::{RunQueryDsl, SimpleAsyncConnection};
    use serde_json::json;

    const COMMIT_HOOK_UP: &str = include_str!(
        "../../repository_commit_hook_migrations/20260515000000_create_repository_commit_hook_queue/up.sql"
    );

    // Process-unique so the globally-registered runner registry never collides
    // with another test's handler key in the same test binary.
    static KEY_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_handler_key() -> &'static str {
        let n = KEY_SEQ.fetch_add(1, Ordering::Relaxed);
        Box::leak(format!("test::commit_hook_drain::handler::{n}").into_boxed_str())
    }

    #[derive(diesel::QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    async fn count_marks(db: &TestDb) -> i64 {
        let mut conn = db.pool().get().await.expect("checkout");
        diesel::sql_query("SELECT COUNT(*) AS n FROM hook_marks")
            .get_result::<CountRow>(&mut *conn)
            .await
            .expect("count hook_marks")
            .n
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn drain_export_runs_ready_repository_commit_hook_end_to_end() {
        let db = TestDb::new().await;

        // Real durable queue schema + a second table the hook writes into as its
        // deterministically observable side effect.
        {
            let mut conn = db.pool().get().await.expect("setup connection");
            conn.batch_execute(COMMIT_HOOK_UP)
                .await
                .expect("apply commit-hook queue migration");
            conn.batch_execute(
                "CREATE TABLE IF NOT EXISTS hook_marks (\
                     id BIGSERIAL PRIMARY KEY, \
                     note TEXT NOT NULL\
                 )",
            )
            .await
            .expect("create hook_marks table");
        }

        // Register the durable runner. Its create branch writes a persisted row
        // into the second table, so a fired hook is observable across a fresh
        // connection — the strongest end-to-end assertion.
        let handler_key = unique_handler_key();
        let runner_pool = db.pool();
        autumn_web::__private::register_repository_commit_hook_runner(
            handler_key,
            move |_ctx, _record| {
                let pool = runner_pool.clone();
                async move {
                    let mut conn = pool.get().await.map_err(|error| {
                        AutumnError::internal_server_error_msg(format!("hook pool error: {error}"))
                    })?;
                    diesel::sql_query("INSERT INTO hook_marks (note) VALUES ('fired')")
                        .execute(&mut *conn)
                        .await
                        .map_err(|error| {
                            AutumnError::internal_server_error_msg(format!(
                                "hook insert failed: {error}"
                            ))
                        })?;
                    Ok(())
                }
            },
            |_ctx, _record| async { Ok(()) },
            |_ctx, _record| async { Ok(()) },
        );

        // Stage one durable hook into the real queue (no dispatcher kick, so no
        // background worker is ever spawned on this pool).
        {
            let mut conn = db.pool().get().await.expect("enqueue connection");
            autumn_web::__private::enqueue_repository_commit_hook_on_conn(
                &mut conn,
                handler_key,
                "create",
                None,
                None,
                &json!({ "op": "create" }),
                &json!({ "note": "fired" }),
            )
            .await
            .expect("enqueue durable hook");
        }

        // Precondition: the hook is queued but has NOT run — no worker exists.
        assert_eq!(
            count_marks(&db).await,
            0,
            "the commit hook must not have fired before the drain"
        );

        // Drive the real drain wiring through the export. A cap above the number
        // of enqueued hooks drains everything in one pass.
        let processed = autumn_web::test::drain_ready_repository_commit_hooks(&db.pool(), 16).await;
        assert_eq!(
            processed, 1,
            "the drain must report exactly one ready hook claimed-and-run"
        );

        // The registered runner fired end-to-end through the real claim/ack path.
        assert_eq!(
            count_marks(&db).await,
            1,
            "the commit hook's persisted side effect must exist after the drain"
        );

        // A second drain has nothing ready and is a no-op.
        let processed_again =
            autumn_web::test::drain_ready_repository_commit_hooks(&db.pool(), 16).await;
        assert_eq!(
            processed_again, 0,
            "a drained queue reports zero on re-drain"
        );
        assert_eq!(
            count_marks(&db).await,
            1,
            "re-draining must not run the already-processed hook again"
        );
    }
}
