//! Integration tests for transaction isolation levels + serialization-failure
//! retry (issue #1202).
//!
//! Requires Docker (testcontainers). Run with:
//!
//!     cargo test -p autumn --test integration_tests --features db,test-support \
//!         -- --ignored tx_isolation_retry

#[cfg(all(feature = "db", feature = "test-support"))]
mod tx_isolation_retry_tests {
    use autumn_web::db::{Db, TX_RETRIES_TOTAL, TxOptions};
    use autumn_web::test::TestDb;
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;
    use scoped_futures::ScopedFutureExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    // `RunQueryDsl` brings a `.load` method into scope that collides with
    // `AtomicU64::load`; read the counter through a fully-qualified call.
    fn retries_total() -> u64 {
        AtomicU64::load(&TX_RETRIES_TOTAL, Ordering::Relaxed)
    }

    // ── Schema ─────────────────────────────────────────────────

    diesel::table! {
        oncall_doctors (id) {
            id -> Int8,
            name -> Text,
            on_call -> Bool,
        }
    }

    static SETUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    async fn setup(db: &TestDb) {
        SETUP
            .get_or_init(|| async {
                db.execute_sql(
                    "CREATE TABLE IF NOT EXISTS oncall_doctors (
                        id BIGSERIAL PRIMARY KEY,
                        name TEXT NOT NULL,
                        on_call BOOLEAN NOT NULL
                    )",
                )
                .await;
            })
            .await;
    }

    async fn reset_two_on_call(db: &TestDb) {
        db.execute_sql("TRUNCATE oncall_doctors RESTART IDENTITY")
            .await;
        db.execute_sql(
            "INSERT INTO oncall_doctors (name, on_call) VALUES ('Alice', true), ('Bob', true)",
        )
        .await;
    }

    async fn on_call_count(db: &TestDb) -> i64 {
        let mut conn = db.pool().get().await.expect("checkout");
        oncall_doctors::table
            .filter(oncall_doctors::on_call.eq(true))
            .count()
            .get_result(&mut *conn)
            .await
            .expect("count")
    }

    /// One participant in the write-skew race: read how many doctors are on
    /// call, and — only if the invariant (>= 1 on call) would still hold —
    /// take *this* doctor off call. Two of these running concurrently under a
    /// weak isolation level can both observe "2 on call" and both go off,
    /// leaving nobody on call (the anomaly).
    async fn try_go_off_call(
        mut db: Db,
        doctor_id: i64,
        opts: TxOptions,
        barrier: Arc<tokio::sync::Barrier>,
    ) -> autumn_web::AutumnResult<()> {
        // Sync on the FIRST attempt only, so both transactions do their SELECT
        // before either commits. Retried attempts must not wait (their partner
        // has already finished) or they would deadlock.
        let first_attempt = Arc::new(AtomicBool::new(true));

        db.tx_with(opts, move |conn| {
            let barrier = barrier.clone();
            let first_attempt = first_attempt.clone();
            async move {
                let count: i64 = oncall_doctors::table
                    .filter(oncall_doctors::on_call.eq(true))
                    .count()
                    .get_result(conn)
                    .await?;

                if first_attempt.swap(false, Ordering::SeqCst) {
                    barrier.wait().await;
                }

                if count >= 2 {
                    diesel::update(oncall_doctors::table.find(doctor_id))
                        .set(oncall_doctors::on_call.eq(false))
                        .execute(conn)
                        .await?;
                }
                Ok::<_, autumn_web::AutumnError>(())
            }
            .scope_boxed()
        })
        .await
    }

    async fn run_race(db: &TestDb, opts: TxOptions) {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let db1 = Db::connect_for_test(&db.pool()).await.expect("db1");
        let db2 = Db::connect_for_test(&db.pool()).await.expect("db2");

        let (b1, b2) = (barrier.clone(), barrier.clone());
        let (o1, o2) = (opts, opts);
        let t1 = tokio::spawn(async move { try_go_off_call(db1, 1, o1, b1).await });
        let t2 = tokio::spawn(async move { try_go_off_call(db2, 2, o2, b2).await });

        // If either task hits a non-retryable error before reaching the
        // barrier, its partner would otherwise block on `barrier.wait()`
        // forever. Bound the whole race so an unexpected failure surfaces as a
        // clean test failure instead of an indefinite CI hang.
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let _ = t1.await.expect("task1 join");
            let _ = t2.await.expect("task2 join");
        })
        .await
        .expect(
            "write-skew race did not complete within 30s — likely one participant hit a \
             non-retryable error before reaching the barrier, leaving its partner blocked",
        );
    }

    // ── The falsifiable success-metric test ────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn read_committed_allows_write_skew_serializable_prevents_it() {
        let db = TestDb::shared().await;
        setup(db).await;

        // --- READ COMMITTED: the write-skew anomaly occurs (nobody on call). ---
        reset_two_on_call(db).await;
        run_race(db, TxOptions::read_committed()).await;
        assert_eq!(
            on_call_count(db).await,
            0,
            "under READ COMMITTED the write-skew anomaly leaves nobody on call"
        );

        // --- SERIALIZABLE + automatic retry: invariant provably preserved. ---
        reset_two_on_call(db).await;
        let retries_before = retries_total();
        run_race(db, TxOptions::serializable()).await;
        let retries_after = retries_total();

        assert!(
            on_call_count(db).await >= 1,
            "under SERIALIZABLE at least one doctor must remain on call — with zero app-side retry code"
        );
        assert!(
            retries_after > retries_before,
            "a 40001 serialization failure should have been retried automatically"
        );
    }

    // ── Savepoint helper ───────────────────────────────────────

    diesel::table! {
        savepoint_rows (id) {
            id -> Int8,
            tag -> Text,
        }
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn savepoint_inner_rollback_preserves_outer_writes() {
        let db = TestDb::shared().await;
        db.execute_sql(
            "CREATE TABLE IF NOT EXISTS savepoint_rows (id BIGSERIAL PRIMARY KEY, tag TEXT NOT NULL)",
        )
        .await;
        db.execute_sql("TRUNCATE savepoint_rows RESTART IDENTITY")
            .await;

        let mut outer = Db::connect_for_test(&db.pool()).await.expect("db");
        outer
            .tx(|conn| {
                async move {
                    diesel::insert_into(savepoint_rows::table)
                        .values(savepoint_rows::tag.eq("outer-before"))
                        .execute(conn)
                        .await?;

                    // Inner savepoint that rolls back — must NOT undo the outer row.
                    let inner: Result<(), autumn_web::AutumnError> =
                        autumn_web::db::savepoint(conn, |conn| {
                            async move {
                                diesel::insert_into(savepoint_rows::table)
                                    .values(savepoint_rows::tag.eq("inner-doomed"))
                                    .execute(conn)
                                    .await?;
                                Err(autumn_web::AutumnError::internal_server_error_msg(
                                    "roll back the savepoint",
                                ))
                            }
                            .scope_boxed()
                        })
                        .await;
                    assert!(inner.is_err(), "inner savepoint should have rolled back");

                    diesel::insert_into(savepoint_rows::table)
                        .values(savepoint_rows::tag.eq("outer-after"))
                        .execute(conn)
                        .await?;

                    Ok::<_, autumn_web::AutumnError>(())
                }
                .scope_boxed()
            })
            .await
            .expect("outer tx commits");

        let mut conn = db.pool().get().await.expect("checkout");
        let tags: Vec<String> = savepoint_rows::table
            .select(savepoint_rows::tag)
            .order(savepoint_rows::id.asc())
            .load(&mut *conn)
            .await
            .expect("load tags");

        assert_eq!(
            tags,
            vec!["outer-before".to_string(), "outer-after".to_string()],
            "the two outer writes commit; the rolled-back savepoint write is gone"
        );
    }
}
