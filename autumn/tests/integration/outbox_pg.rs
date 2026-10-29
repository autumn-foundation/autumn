//! Transactional outbox on Postgres (issue #3062).
//!
//! The `SQLite` path runs in `src/outbox/tests.rs` and the sim suites. These
//! tests check the Postgres SQL: `FOR UPDATE SKIP LOCKED`, aggregate order,
//! rollback, and the inbox. They share one container, so each test uses its
//! own topics.

#![cfg(feature = "db")]

#[cfg(feature = "test-support")]
mod docker {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use autumn_web::config::OutboxConfig;
    use autumn_web::outbox::{self, Inbox, Outbox, OutboxMessage};
    use autumn_web::test::TestApp;
    use autumn_web::{AppState, AutumnError};
    use diesel_async::pooled_connection::deadpool::Pool;
    use scoped_futures::ScopedFutureExt as _;

    type PgPool = Pool<autumn_web::db::RuntimeConnection>;
    type Log = Arc<Mutex<Vec<(String, String)>>>;

    async fn pool() -> PgPool {
        let pool = autumn_web::test::TestDb::shared().await.pool();
        outbox::ensure_schema(&pool).await.expect("outbox tables");
        pool
    }

    /// A test app with one logging handler for `topic`.
    fn relay(pool: &PgPool, topic: &str, log: &Log) -> AppState {
        let log = log.clone();
        TestApp::new()
            .with_db(pool.clone())
            .with_outbox(OutboxConfig::default())
            .outbox_handler(topic, move |_, message: OutboxMessage| {
                let log = log.clone();
                async move {
                    log.lock()
                        .unwrap()
                        .push((message.aggregate.clone(), message.id.clone()));
                    // Let the other relay run between claim and mark.
                    tokio::task::yield_now().await;
                    Ok(())
                }
            })
            .build()
            .state()
            .clone()
    }

    async fn write(state: &AppState, aggregate: &str, topic: &str) -> String {
        let mut conn = state.pool().unwrap().get().await.unwrap();
        Outbox::new(state)
            .write(&mut conn, aggregate, topic, &serde_json::json!({}))
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_outbox_two_relays_send_each_message_once_in_aggregate_order() {
        let pool = pool().await;
        let topic = "pg.race";
        let log = Log::default();
        let first = relay(&pool, topic, &log);
        let second = relay(&pool, topic, &log);

        let mut written: HashMap<String, Vec<String>> = HashMap::new();
        for n in 0..200 {
            let aggregate = format!("race-{}", n % 20);
            let id = write(&first, &aggregate, topic).await;
            written.entry(aggregate).or_default().push(id);
        }

        let (a, b) = tokio::join!(
            async {
                let mut total = 0;
                for _ in 0..50 {
                    total += outbox::drain(&first, 10).await.unwrap();
                }
                total
            },
            async {
                let mut total = 0;
                for _ in 0..50 {
                    total += outbox::drain(&second, 10).await.unwrap();
                }
                total
            },
        );
        assert!(a + b >= 200, "the relays handle every message");

        let log = log.lock().unwrap().clone();
        assert_eq!(log.len(), 200, "each message is handled once");
        let mut sent: HashMap<String, Vec<String>> = HashMap::new();
        for (aggregate, id) in &log {
            sent.entry(aggregate.clone()).or_default().push(id.clone());
        }
        assert_eq!(sent, written, "each aggregate is sent once, in write order");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_outbox_rolled_back_message_is_not_sent() {
        let pool = pool().await;
        let topic = "pg.rollback";
        let log = Log::default();
        let state = relay(&pool, topic, &log);

        let outbox = Outbox::new(&state);
        let mut conn = pool.get().await.unwrap();
        let result: Result<(), AutumnError> =
            autumn_web::db::scoped_transaction(&mut *conn, |conn| {
                async move {
                    outbox
                        .write(conn, "rollback", topic, &serde_json::json!({}))
                        .await?;
                    Err(AutumnError::bad_request_msg("business rule failed"))
                }
                .scope_boxed()
            })
            .await;
        assert!(result.is_err());
        drop(conn);

        assert_eq!(outbox::drain(&state, 100).await.unwrap(), 0);
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_outbox_failure_retries_then_dead_letters() {
        let pool = pool().await;
        let topic = "pg.dead";
        let state = TestApp::new()
            .with_db(pool.clone())
            .with_outbox(OutboxConfig {
                max_attempts: 2,
                initial_backoff_ms: 1,
                max_backoff_ms: 1,
                ..OutboxConfig::default()
            })
            .outbox_handler(topic, |_, _| async {
                Err(AutumnError::internal_server_error_msg("receiver down"))
            })
            .build()
            .state()
            .clone();

        let id = write(&state, "dead", topic).await;
        for _ in 0..20 {
            outbox::drain(&state, 10).await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outbox = Outbox::new(&state);
        let mut conn = pool.get().await.unwrap();
        let dead = outbox.dead_letters(&mut conn, 1_000).await.unwrap();
        let letter = dead
            .iter()
            .find(|letter| letter.message.id == id)
            .expect("the message is a dead letter");
        assert_eq!(letter.last_error.as_deref(), Some("receiver down"));
        assert!(outbox.requeue(&mut conn, &id).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_inbox_reports_a_copy() {
        let pool = pool().await;
        let mut conn = pool.get().await.unwrap();
        let inbox = Inbox::new("pg-inbox-test");
        let id = uuid::Uuid::new_v4().to_string();
        assert!(!inbox.seen(&mut conn, &id).await.unwrap());
        assert!(inbox.seen(&mut conn, &id).await.unwrap());
    }

    #[derive(diesel::QueryableByName)]
    struct IndexValid {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        valid: bool,
    }

    /// Tables made before `idx_autumn_outbox_claim` existed get the index from
    /// `ensure_schema`, built in the background (it must not delay boot).
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_outbox_ensure_schema_adds_claim_index_to_existing_tables() {
        use diesel_async::RunQueryDsl as _;

        let pool = pool().await;
        let mut conn = pool.get().await.unwrap();
        diesel::sql_query("DROP INDEX IF EXISTS idx_autumn_outbox_claim")
            .execute(&mut *conn)
            .await
            .unwrap();

        outbox::ensure_schema(&pool).await.unwrap();

        // A concurrent build is in the catalog with `indisvalid = false` first.
        let mut valid = false;
        for _ in 0..100 {
            let rows = diesel::sql_query(
                "SELECT i.indisvalid AS valid FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indexrelid \
                 WHERE c.relname = 'idx_autumn_outbox_claim'",
            )
            .load::<IndexValid>(&mut *conn)
            .await
            .unwrap();
            if rows.len() == 1 && rows[0].valid {
                valid = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            valid,
            "ensure_schema builds the claim index in the background"
        );
    }

    /// A pool of one has no connection to spare for the background build, so
    /// `ensure_schema` skips it (and still succeeds).
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_outbox_ensure_schema_skips_the_index_build_on_a_single_connection_pool() {
        use diesel_async::AsyncPgConnection;
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;

        let db = autumn_web::test::TestDb::shared().await;
        outbox::ensure_schema(&db.pool()).await.unwrap();
        let single: PgPool = Pool::builder(AsyncDieselConnectionManager::<AsyncPgConnection>::new(
            db.url(),
        ))
        .max_size(1)
        .build()
        .unwrap();
        outbox::ensure_schema(&single).await.unwrap();
    }

    /// Sent rows that the purge has not deleted yet. Their `seq` is lower than
    /// the `seq` of each pending row.
    const SENT_ROWS: i64 = 30_000;

    /// Pending rows in backoff, after the edge rows. With these rows, the
    /// planner walks `autumn_outbox_pkey` when the claim index is missing.
    /// Without them, the planner sorts the few pending rows, and the test passes
    /// with no claim index.
    const WAITING_ROWS: i64 = 500;

    #[derive(diesel::QueryableByName)]
    struct OutboxStats {
        /// Index entries that scans of `autumn_outbox_pkey` read.
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        pkey_reads: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        inserts: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        updates: i64,
    }

    /// The counters of `autumn_outbox`, when `ready` accepts them.
    ///
    /// Postgres 11 sends the counters of a connection at the end of a
    /// transaction, at most once in 500 ms. Thus this helper waits 600 ms, then
    /// runs a query on each connection of `pool` (`max_size` 2). The query sends
    /// the counters of that connection.
    async fn outbox_stats(pool: &PgPool, ready: impl Fn(&OutboxStats) -> bool) -> OutboxStats {
        use diesel_async::RunQueryDsl as _;

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let (first, second) = tokio::join!(pool.get(), pool.get());
            let (mut first, mut second) = (first.unwrap(), second.unwrap());
            diesel::sql_query("SELECT 1")
                .execute(&mut *second)
                .await
                .unwrap();
            drop(second);
            diesel::sql_query("SELECT 1")
                .execute(&mut *first)
                .await
                .unwrap();
            let stats = diesel::sql_query(
                "SELECT i.idx_tup_read AS pkey_reads, t.n_tup_ins AS inserts, \
                 t.n_tup_upd AS updates \
                 FROM pg_stat_user_tables t \
                 JOIN pg_stat_user_indexes i ON i.relid = t.relid \
                 WHERE t.relname = 'autumn_outbox' AND i.indexrelname = 'autumn_outbox_pkey'",
            )
            .get_result::<OutboxStats>(&mut *first)
            .await
            .unwrap();
            if ready(&stats) {
                return stats;
            }
        }
        panic!("the counters of autumn_outbox did not arrive");
    }

    /// The claim reads the pending rows only, not the sent rows that the purge
    /// has not deleted yet. It takes the edge rows that the fixture marks
    /// (#3203).
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_outbox_claim_reads_pending_rows_only() {
        use chrono::TimeZone as _;
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;
        use diesel_async::{
            AsyncConnection as _, AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _,
        };

        #[derive(diesel::QueryableByName)]
        struct Id {
            #[diesel(sql_type = diesel::sql_types::Text)]
            id: String,
        }

        // A database of its own: rows of other tests change the counters.
        // Do not use the shared pool: a connection stops with the runtime
        // that opened it, and other tests can get it from the pool.
        let db = autumn_web::test::TestDb::shared().await;
        let mut admin = AsyncPgConnection::establish(db.url()).await.unwrap();
        admin
            .batch_execute("DROP DATABASE IF EXISTS outbox_claim_cost")
            .await
            .unwrap();
        admin
            .batch_execute("CREATE DATABASE outbox_claim_cost")
            .await
            .unwrap();
        drop(admin);
        let (server, _) = db.url().rsplit_once('/').unwrap();
        let pool: PgPool = Pool::builder(AsyncDieselConnectionManager::<AsyncPgConnection>::new(
            format!("{server}/outbox_claim_cost"),
        ))
        .max_size(2)
        .build()
        .unwrap();
        outbox::ensure_schema(&pool).await.unwrap();

        let now = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let ms = now.timestamp_millis();
        let mut conn = pool.get().await.unwrap();
        conn.batch_execute(&format!(
            "INSERT INTO autumn_outbox \
             (id, aggregate, topic, payload, created_at, available_at, dispatched_at) \
             SELECT 'sent-' || g, 'sent-' || (g % 500), 't', '{{}}', {ms}, {ms}, {ms} \
             FROM generate_series(1, {SENT_ROWS}) g"
        ))
        .await
        .unwrap();
        conn.batch_execute(
            &include_str!("../fixtures/outbox/claim_edges.sql").replace("__NOW__", &ms.to_string()),
        )
        .await
        .unwrap();
        conn.batch_execute(&format!(
            "INSERT INTO autumn_outbox (id, aggregate, topic, payload, created_at, available_at) \
             SELECT 'waiting-' || g, 'waiting-' || g, 't', '{{}}', {ms}, {ms} + 3600000 \
             FROM generate_series(1, {WAITING_ROWS}) g"
        ))
        .await
        .unwrap();
        conn.batch_execute("ANALYZE autumn_outbox").await.unwrap();
        let expected: Vec<String> = diesel::sql_query(
            "SELECT id FROM autumn_outbox WHERE payload = '{\"claim\":true}' ORDER BY seq",
        )
        .load::<Id>(&mut *conn)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
        assert_eq!(expected.len(), 5, "the fixture marks 5 rows");
        drop(conn);
        let before = outbox_stats(&pool, |stats| stats.inserts >= SENT_ROWS + WAITING_ROWS).await;

        let log = Log::default();
        let state = TestApp::new()
            .with_db(pool.clone())
            .with_clock(autumn_web::time::FixedClock::at(now))
            .with_outbox(OutboxConfig::default())
            .outbox_handler("t", {
                let log = log.clone();
                move |_, message: OutboxMessage| {
                    let log = log.clone();
                    async move {
                        log.lock().unwrap().push((message.aggregate, message.id));
                        Ok::<_, AutumnError>(())
                    }
                }
            })
            .build()
            .state()
            .clone();
        assert_eq!(outbox::drain(&state, 100).await.unwrap(), expected.len());
        let sent: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|(_, id)| id.clone())
            .collect();
        assert_eq!(sent, expected);

        // Each sent message has two updates: the claim and the mark.
        let claimed = i64::try_from(expected.len()).unwrap();
        let after =
            outbox_stats(&pool, |stats| stats.updates - before.updates >= 2 * claimed).await;
        let reads = after.pkey_reads - before.pkey_reads;
        assert!(
            reads < SENT_ROWS / 10,
            "the relay read {reads} entries of autumn_outbox_pkey; \
             it must not walk the {SENT_ROWS} sent rows"
        );
    }
}

#[cfg(feature = "test-support")]
mod docker_webhook_store {
    use std::collections::HashMap;

    use autumn_web::webhook_outbound::{
        OutboundWebhookHandler as _, SqlOutboundWebhookStore, WebhookDeliveryLog,
        WebhookSubscription, WebhookSubscriptionStatus,
    };

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn pg_sql_webhook_store_round_trips_and_fails_after_50() {
        let pool = autumn_web::test::TestDb::shared().await.pool();
        let store = SqlOutboundWebhookStore::new(pool);
        store.ensure_schema().await.unwrap();
        let sub = WebhookSubscription {
            id: "pg-sub-1".to_owned(),
            target_url: "https://receiver.example/hooks".to_owned(),
            event_topics: vec!["pg.paid".to_owned()],
            secret: "whsec_test_secret_with_32_bytes_ok!!".to_owned(),
            status: WebhookSubscriptionStatus::Active,
            consecutive_failures: 0,
        };
        store.create_subscription(sub.clone()).await.unwrap();
        assert_eq!(store.get_subscriptions("pg.paid").await.unwrap(), vec![sub]);

        for n in 0..50 {
            let log = WebhookDeliveryLog {
                id: format!("pg-log-{n}"),
                subscription_id: "pg-sub-1".to_owned(),
                topic: "pg.paid".to_owned(),
                payload: "{}".to_owned(),
                request_headers: HashMap::new(),
                response_status: Some(500),
                response_body: None,
                elapsed_ms: 1,
                attempt: 1,
                max_attempts: 5,
                is_dlq: n == 49,
                last_error: Some("500".to_owned()),
                timestamp: chrono::Utc::now(),
            };
            store.log_delivery(log).await.unwrap();
        }
        let failed = store.get_subscription("pg-sub-1").await.unwrap().unwrap();
        assert_eq!(failed.status, WebhookSubscriptionStatus::Failed);
        assert!(
            store
                .get_dlq_logs()
                .await
                .unwrap()
                .iter()
                .any(|log| log.id == "pg-log-49")
        );
    }
}
