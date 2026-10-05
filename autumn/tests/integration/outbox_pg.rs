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

        let log = log.lock().unwrap();
        assert_eq!(log.len(), 200, "each message is handled once");
        let mut sent: HashMap<String, Vec<String>> = HashMap::new();
        for (aggregate, id) in log.iter() {
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
