use super::*;

#[test]
fn sqlite_placeholders_follow_bind_order() {
    assert_eq!(
        to_sqlite_placeholders("a = $1 AND b <= $2 AND c = $10 AND d = '$x'"),
        "a = ? AND b <= ? AND c = ? AND d = '$x'"
    );
}

#[test]
fn claim_sql_locks_rows_only_on_postgres() {
    let sql = claim_sql("'t'");
    let locks = sql.contains("FOR UPDATE SKIP LOCKED");
    assert_eq!(locks, cfg!(not(feature = "sqlite")), "{sql}");
    assert!(
        sql.contains("NOT EXISTS"),
        "the claim keeps aggregate order"
    );
}

#[test]
fn retry_delay_doubles_with_jitter_and_a_cap() {
    let config = OutboxConfig {
        initial_backoff_ms: 1_000,
        max_backoff_ms: 5_000,
        ..OutboxConfig::default()
    };
    let entropy = crate::entropy::SeededEntropy::new(3);
    for (attempt, full) in [(1, 1_000), (2, 2_000), (3, 4_000), (4, 5_000), (40, 5_000)] {
        for _ in 0..50 {
            let delay = retry_delay_ms(&config, attempt, &entropy);
            assert!(
                (full / 2..=full).contains(&delay),
                "attempt {attempt}: {delay} not in [{}, {full}]",
                full / 2
            );
        }
    }
}

#[test]
fn retry_delay_never_exceeds_a_small_max() {
    let config = OutboxConfig {
        initial_backoff_ms: 300_000,
        max_backoff_ms: 1_000,
        ..OutboxConfig::default()
    };
    let entropy = crate::entropy::SeededEntropy::new(5);
    for attempt in 1..5 {
        assert!(retry_delay_ms(&config, attempt, &entropy) <= 1_000);
    }
}

#[test]
#[should_panic(expected = "reserved")]
fn reserved_topic_prefix_is_refused() {
    OutboxHandlers::default().insert("autumn.mine", |_, _| async { Ok(()) });
}

#[test]
#[should_panic(expected = "must not be empty")]
fn empty_topic_is_refused() {
    OutboxHandlers::default().insert("", |_, _| async { Ok(()) });
}

#[test]
fn topic_characters_are_checked() {
    for good in ["order.placed", "billing:v2", "a_b-c"] {
        assert!(is_valid_topic(good), "{good}");
    }
    for bad in ["", "a b", "x'y", "price$1", "a\\b", "t\u{0}"] {
        assert!(!is_valid_topic(bad), "{bad:?}");
    }
}

#[test]
fn claim_sql_keeps_the_topic_list_as_written() {
    let sql = claim_sql("'a.b', 'c'");
    assert!(sql.contains("IN ('a.b', 'c')"), "{sql}");
    assert!(
        sql.contains("attempts = attempts + 1"),
        "a claim counts an attempt"
    );
}

#[test]
fn relay_lists_built_in_and_app_topics() {
    let mut handlers = OutboxHandlers::default();
    handlers.insert("order.placed", |_, _| async { Ok(()) });
    let relay = OutboxRelay::new(OutboxConfig::default(), handlers);
    assert!(relay.topics_in_list.contains("'order.placed'"));
    assert!(relay.topics_in_list.contains("'autumn.event'"));
    assert!(relay.topics_in_list.contains("'autumn.job'"));
}

#[test]
fn install_without_enabled_installs_nothing() {
    let state = AppState::for_test();
    install(&state, &OutboxConfig::default(), OutboxHandlers::default());
    assert!(state.extension::<OutboxRelay>().is_none());
}

#[tokio::test]
async fn drain_without_relay_is_an_error() {
    let error = drain(&AppState::for_test(), 10).await.unwrap_err();
    assert!(error.to_string().contains("outbox.enabled"), "{error}");
}

/// Tests against a real database. They run on the `SQLite` lane
/// (`cargo test -p autumn-web --features sqlite --lib`); the Postgres
/// suite is `tests/integration/outbox_pg.rs`.
#[cfg(feature = "sqlite")]
#[allow(clippy::future_not_send, reason = "test helpers run on one task")]
mod sqlite {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use chrono::TimeZone as _;
    use scoped_futures::ScopedFutureExt as _;

    use super::*;
    use crate::sim::substrate::SqliteSubstrate;
    use crate::time::TickingClock;

    struct Fixture {
        _substrate: SqliteSubstrate,
        state: AppState,
        clock: TickingClock,
    }

    async fn fixture(config: OutboxConfig, handlers: OutboxHandlers) -> Fixture {
        let substrate = SqliteSubstrate::new().expect("substrate");
        ensure_schema(&substrate.pool()).await.expect("schema");
        let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        let state = AppState::for_test()
            .with_pool(substrate.pool())
            .with_clock(Arc::new(clock.clone()))
            .with_entropy(Arc::new(crate::entropy::SeededEntropy::new(11)));
        install(
            &state,
            &OutboxConfig {
                enabled: true,
                ..config
            },
            handlers,
        );
        Fixture {
            _substrate: substrate,
            state,
            clock,
        }
    }

    async fn write(fx: &Fixture, aggregate: &str, topic: &str, payload: Value) -> String {
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        Outbox::new(&fx.state)
            .write(&mut conn, aggregate, topic, &payload)
            .await
            .unwrap()
    }

    /// Insert a row with no topic check, as a replica with other handlers can.
    async fn write_raw(fx: &Fixture, aggregate: &str, topic: &str) {
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        insert_message(
            &mut conn,
            fx.state.entropy(),
            fx.state.clock(),
            Some(aggregate),
            topic,
            &serde_json::json!({}),
        )
        .await
        .unwrap();
    }

    async fn count(fx: &Fixture, where_clause: &str) -> i64 {
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = BigInt)]
            n: i64,
        }
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        diesel::sql_query(format!(
            "SELECT COUNT(*) AS n FROM autumn_outbox WHERE {where_clause}"
        ))
        .get_result::<Count>(&mut conn)
        .await
        .unwrap()
        .n
    }

    type Log = Arc<Mutex<Vec<String>>>;

    /// A handler that logs `payload.n`, and fails while `fail_left` > 0.
    fn logging_handler(
        log: Log,
        fail_left: Arc<AtomicU32>,
    ) -> impl Fn(AppState, OutboxMessage) -> HandlerFuture + Send + Sync + 'static {
        move |_, message| {
            let log = log.clone();
            let fail_left = fail_left.clone();
            Box::pin(async move {
                let n = message.payload["n"].as_str().unwrap_or_default().to_owned();
                if fail_left
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err(AutumnError::internal_server_error_msg(format!("fail {n}")));
                }
                log.lock().unwrap().push(n);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn committed_message_is_sent_once() {
        let log = Log::default();
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", logging_handler(log.clone(), Arc::default()));
        let fx = fixture(OutboxConfig::default(), handlers).await;

        write(&fx, "a", "t", serde_json::json!({ "n": "1" })).await;
        assert_eq!(drain(&fx.state, 100).await.unwrap(), 1);
        assert_eq!(drain(&fx.state, 100).await.unwrap(), 0);
        assert_eq!(*log.lock().unwrap(), ["1"]);
        assert_eq!(count(&fx, "dispatched_at IS NOT NULL").await, 1);
    }

    #[tokio::test]
    async fn rolled_back_message_is_never_sent() {
        let log = Log::default();
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", logging_handler(log.clone(), Arc::default()));
        let fx = fixture(OutboxConfig::default(), handlers).await;

        let outbox = Outbox::new(&fx.state);
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        let result: Result<(), AutumnError> = crate::db::scoped_transaction(&mut *conn, |conn| {
            async move {
                outbox
                    .write(conn, "a", "t", &serde_json::json!({ "n": "1" }))
                    .await?;
                Err(AutumnError::bad_request_msg("business rule failed"))
            }
            .scope_boxed()
        })
        .await;
        assert!(result.is_err());
        drop(conn);

        assert_eq!(drain(&fx.state, 100).await.unwrap(), 0);
        assert_eq!(count(&fx, "1 = 1").await, 0);
    }

    #[tokio::test]
    async fn one_aggregate_is_sent_in_order_and_others_are_not_blocked() {
        let log = Log::default();
        let mut handlers = OutboxHandlers::default();
        handlers.insert(
            "t",
            logging_handler(log.clone(), Arc::new(AtomicU32::new(1))),
        );
        let fx = fixture(OutboxConfig::default(), handlers).await;

        write(&fx, "a", "t", serde_json::json!({ "n": "a1" })).await;
        write(&fx, "a", "t", serde_json::json!({ "n": "a2" })).await;
        write(&fx, "b", "t", serde_json::json!({ "n": "b1" })).await;

        // a1 fails once. a2 waits for a1; b1 does not.
        drain(&fx.state, 100).await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["b1"]);

        fx.clock.advance(Duration::from_secs(5));
        drain(&fx.state, 100).await.unwrap();
        drain(&fx.state, 100).await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["b1", "a1", "a2"]);
    }

    #[tokio::test]
    async fn failed_message_waits_for_its_backoff() {
        let log = Log::default();
        let mut handlers = OutboxHandlers::default();
        handlers.insert(
            "t",
            logging_handler(log.clone(), Arc::new(AtomicU32::new(1))),
        );
        let config = OutboxConfig {
            initial_backoff_ms: 10_000,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;

        write(&fx, "a", "t", serde_json::json!({ "n": "1" })).await;
        assert_eq!(drain(&fx.state, 100).await.unwrap(), 1);
        assert_eq!(
            count(&fx, "attempts = 1 AND last_error = 'fail 1'").await,
            1
        );

        fx.clock.advance(Duration::from_millis(4_999));
        assert_eq!(
            drain(&fx.state, 100).await.unwrap(),
            0,
            "jitter floor is 5s"
        );
        fx.clock.advance(Duration::from_millis(5_001));
        assert_eq!(drain(&fx.state, 100).await.unwrap(), 1);
        assert_eq!(*log.lock().unwrap(), ["1"]);
    }

    #[tokio::test]
    async fn last_failure_dead_letters_and_requeue_sends_again() {
        let log = Log::default();
        let fail_left = Arc::new(AtomicU32::new(2));
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", logging_handler(log.clone(), fail_left.clone()));
        let config = OutboxConfig {
            max_attempts: 2,
            initial_backoff_ms: 1,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;

        let dead_id = write(&fx, "a", "t", serde_json::json!({ "n": "a1" })).await;
        write(&fx, "a", "t", serde_json::json!({ "n": "a2" })).await;
        for _ in 0..4 {
            drain(&fx.state, 100).await.unwrap();
            fx.clock.advance(Duration::from_secs(1));
        }
        // a1 is dead after 2 attempts; a2 is no longer blocked.
        assert_eq!(*log.lock().unwrap(), ["a2"]);

        let outbox = Outbox::new(&fx.state);
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        let dead = outbox.dead_letters(&mut conn, 10).await.unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].message.id, dead_id);
        assert_eq!(dead[0].message.attempt, 2, "attempts made");
        assert_eq!(dead[0].last_error.as_deref(), Some("fail a1"));

        assert!(outbox.requeue(&mut conn, &dead_id).await.unwrap());
        assert!(!outbox.requeue(&mut conn, &dead_id).await.unwrap());
        drop(conn);
        drain(&fx.state, 100).await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["a2", "a1"]);
    }

    #[tokio::test]
    async fn claim_held_by_a_crashed_relay_expires_after_the_lease() {
        let log = Log::default();
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", logging_handler(log.clone(), Arc::default()));
        let config = OutboxConfig {
            lease_ms: 30_000,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;
        write(&fx, "a", "t", serde_json::json!({ "n": "1" })).await;

        // A relay claims the row, then its process dies.
        let relay = fx.state.extension::<OutboxRelay>().unwrap();
        let pool = fx.state.pool().unwrap().clone();
        let claimed = claim(&relay, &pool, &fx.state, "dead-relay", 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);

        assert_eq!(drain(&fx.state, 100).await.unwrap(), 0, "the lease holds");
        fx.clock.advance(Duration::from_secs(30));
        assert_eq!(drain(&fx.state, 100).await.unwrap(), 1);
        assert_eq!(*log.lock().unwrap(), ["1"]);

        // The dead relay's late mark does not touch the row again.
        mark_sent(&pool, &fx.state, "dead-relay", claimed[0].seq)
            .await
            .unwrap();
        assert_eq!(count(&fx, "dispatched_at IS NOT NULL").await, 1);
    }

    #[tokio::test]
    async fn relay_stops_a_batch_when_its_lease_ends() {
        let clock = Arc::new(Mutex::new(None::<TickingClock>));
        let mut handlers = OutboxHandlers::default();
        handlers.insert("slow", {
            let clock = clock.clone();
            move |_, _| {
                let clock = clock.lock().unwrap().clone();
                async move {
                    if let Some(clock) = clock {
                        clock.advance(Duration::from_secs(2));
                    }
                    Ok(())
                }
            }
        });
        let config = OutboxConfig {
            lease_ms: 1_000,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;
        *clock.lock().unwrap() = Some(fx.clock.clone());
        write(&fx, "a", "slow", serde_json::json!({})).await;
        write(&fx, "b", "slow", serde_json::json!({})).await;

        assert_eq!(
            drain(&fx.state, 100).await.unwrap(),
            1,
            "the lease ended after one"
        );
        assert_eq!(
            drain(&fx.state, 100).await.unwrap(),
            1,
            "the rest is claimed again"
        );
        assert_eq!(count(&fx, "dispatched_at IS NOT NULL").await, 2);
    }

    #[tokio::test]
    async fn message_without_a_handler_here_is_not_claimed() {
        let fx = fixture(OutboxConfig::default(), OutboxHandlers::default()).await;
        write_raw(&fx, "a", "deployed.later").await;
        assert_eq!(drain(&fx.state, 100).await.unwrap(), 0);
        assert_eq!(count(&fx, "attempts = 0 AND claim_token IS NULL").await, 1);
    }

    #[tokio::test]
    async fn handler_sees_its_message_id_and_a_panic_is_a_failure() {
        let seen = Arc::new(Mutex::new(None::<String>));
        let mut handlers = OutboxHandlers::default();
        handlers.insert("id", {
            let seen = seen.clone();
            move |_, _| {
                let seen = seen.clone();
                async move {
                    *seen.lock().unwrap() = current_message_id();
                    Ok(())
                }
            }
        });
        handlers.insert("boom", |_, _| async { panic!("handler bug") });
        let fx = fixture(OutboxConfig::default(), handlers).await;

        let id = write(&fx, "a", "id", serde_json::json!({})).await;
        write(&fx, "b", "boom", serde_json::json!({})).await;
        assert_eq!(drain(&fx.state, 100).await.unwrap(), 2);

        assert_eq!(seen.lock().unwrap().as_deref(), Some(id.as_str()));
        assert_eq!(current_message_id(), None);
        assert_eq!(
            count(&fx, "topic = 'boom' AND last_error LIKE '%panicked%'").await,
            1
        );
    }

    #[tokio::test]
    async fn inbox_reports_a_copy_per_consumer() {
        let fx = fixture(OutboxConfig::default(), OutboxHandlers::default()).await;
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        let ledger = Inbox::new("ledger");
        assert!(!ledger.seen(&mut conn, "m1").await.unwrap());
        assert!(ledger.seen(&mut conn, "m1").await.unwrap());
        assert!(!Inbox::new("audit").seen(&mut conn, "m1").await.unwrap());
        assert!(!ledger.seen(&mut conn, "m2").await.unwrap());
    }

    #[tokio::test]
    async fn purge_deletes_old_sent_messages_only() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", |_, _| async { Ok(()) });
        let config = OutboxConfig {
            retention_ms: 60_000,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;
        write(&fx, "a", "t", serde_json::json!({})).await;
        drain(&fx.state, 100).await.unwrap();
        write_raw(&fx, "b", "unhandled").await;

        assert_eq!(purge(&fx.state).await.unwrap(), (0, 0));
        fx.clock.advance(Duration::from_secs(61));
        assert_eq!(purge(&fx.state).await.unwrap(), (1, 0));
        assert_eq!(count(&fx, "1 = 1").await, 1, "the pending message stays");
    }

    #[tokio::test]
    async fn write_needs_the_relay_and_a_known_topic() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert("known", |_, _| async { Ok(()) });
        let fx = fixture(OutboxConfig::default(), handlers).await;
        let outbox = Outbox::new(&fx.state);
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        for topic in ["typo", "autumn.job", "autumn.mail"] {
            let error = outbox
                .write(&mut conn, "a", topic, &serde_json::json!({}))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("has no handler"),
                "{topic}: {error}"
            );
        }
        outbox
            .write(&mut conn, "a", "known", &serde_json::json!({}))
            .await
            .unwrap();

        let off = AppState::for_test().with_pool(fx.state.pool().unwrap().clone());
        let error = Outbox::new(&off)
            .enqueue_job(&mut conn, "job", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("outbox is off"), "{error}");
    }

    #[tokio::test]
    async fn a_message_that_crashes_every_relay_goes_to_dead_letters() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", |_, _| async { Ok(()) });
        let config = OutboxConfig {
            max_attempts: 2,
            lease_ms: 1_000,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;
        write(&fx, "a", "t", serde_json::json!({})).await;
        let relay = fx.state.extension::<OutboxRelay>().unwrap();
        let pool = fx.state.pool().unwrap().clone();

        // Two relays claim it and die before the handler ends.
        for token in ["crash-1", "crash-2"] {
            assert_eq!(
                claim(&relay, &pool, &fx.state, token, 10)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            fx.clock.advance(Duration::from_secs(1));
        }
        assert_eq!(drain(&fx.state, 10).await.unwrap(), 1);
        assert_eq!(
            count(
                &fx,
                "dead_at IS NOT NULL AND dispatched_at IS NULL AND attempts = 2"
            )
            .await,
            1,
            "the count stays at max_attempts"
        );
    }

    #[tokio::test]
    async fn handler_past_the_lease_fails_and_frees_the_relay() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert("hang", |_, _| std::future::pending::<AutumnResult<()>>());
        let config = OutboxConfig {
            lease_ms: 20,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;
        write(&fx, "a", "hang", serde_json::json!({})).await;
        assert_eq!(drain(&fx.state, 10).await.unwrap(), 1);
        assert_eq!(count(&fx, "last_error LIKE '%past the lease%'").await, 1);
    }

    #[tokio::test]
    async fn unhandled_rows_of_a_claim_are_given_back() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", |_, _| async { Ok(()) });
        let fx = fixture(OutboxConfig::default(), handlers).await;
        write(&fx, "a", "t", serde_json::json!({})).await;
        let relay = fx.state.extension::<OutboxRelay>().unwrap();
        let pool = fx.state.pool().unwrap().clone();
        let claimed = claim(&relay, &pool, &fx.state, "stopping", 10)
            .await
            .unwrap();
        let seqs: Vec<i64> = claimed.iter().map(|row| row.seq).collect();
        release(&pool, "stopping", &seqs).await;
        assert_eq!(count(&fx, "attempts = 0 AND claim_token IS NULL").await, 1);
        assert_eq!(
            drain(&fx.state, 10).await.unwrap(),
            1,
            "no wait for the lease"
        );
    }

    /// Release takes back the attempt of the listed rows only. A handled row
    /// that still has the token (its outcome did not save) keeps it.
    #[tokio::test]
    async fn release_skips_rows_that_are_not_listed() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", |_, _| async { Ok(()) });
        let fx = fixture(OutboxConfig::default(), handlers).await;
        write(&fx, "a", "t", serde_json::json!({})).await;
        write(&fx, "b", "t", serde_json::json!({})).await;
        let relay = fx.state.extension::<OutboxRelay>().unwrap();
        let pool = fx.state.pool().unwrap().clone();
        let mut claimed = claim(&relay, &pool, &fx.state, "tok", 10).await.unwrap();
        claimed.sort_by_key(|row| row.seq);
        release(&pool, "tok", &[claimed[1].seq]).await;
        assert_eq!(count(&fx, "attempts = 1 AND claim_token = 'tok'").await, 1);
        assert_eq!(count(&fx, "attempts = 0 AND claim_token IS NULL").await, 1);
    }

    /// A handler that panics when it makes its future (before the first
    /// poll) is a failed attempt. The relay goes on.
    #[tokio::test]
    async fn handler_that_panics_before_its_future_is_a_failure() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert(
            "sync-boom",
            |_, _| -> std::future::Ready<AutumnResult<()>> {
                panic!("handler bug before the future")
            },
        );
        handlers.insert("ok", |_, _| async { Ok(()) });
        let fx = fixture(OutboxConfig::default(), handlers).await;
        write(&fx, "a", "sync-boom", serde_json::json!({})).await;
        write(&fx, "b", "ok", serde_json::json!({})).await;
        assert_eq!(drain(&fx.state, 10).await.unwrap(), 2);
        assert_eq!(
            count(&fx, "topic = 'sync-boom' AND last_error LIKE '%panicked%'").await,
            1
        );
        assert_eq!(
            count(&fx, "topic = 'ok' AND dispatched_at IS NOT NULL").await,
            1
        );
    }

    #[tokio::test]
    async fn purge_deletes_old_inbox_entries() {
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", |_, _| async { Ok(()) });
        let config = OutboxConfig {
            retention_ms: 1,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;
        let mut conn = fx.state.pool().unwrap().get().await.unwrap();
        assert!(!Inbox::new("c").seen(&mut conn, "m").await.unwrap());
        drop(conn);
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(purge(&fx.state).await.unwrap().1, 1);
    }

    #[tokio::test]
    async fn relay_worker_sends_and_stops_on_shutdown() {
        let log = Log::default();
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", logging_handler(log.clone(), Arc::default()));
        let config = OutboxConfig {
            poll_interval_ms: 5,
            ..OutboxConfig::default()
        };
        let fx = fixture(config, handlers).await;
        let shutdown = CancellationToken::new();
        let worker =
            start_relay_worker(fx.state.clone(), shutdown.clone()).expect("relay installed");

        write(&fx, "a", "t", serde_json::json!({ "n": "1" })).await;
        for _ in 0..400 {
            if !log.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(*log.lock().unwrap(), ["1"]);

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("the worker stops")
            .unwrap();
    }

    /// A write on a shard connection puts its row in the shard. The relay
    /// drains the shard pools too.
    #[tokio::test]
    async fn relay_drains_shard_pools() {
        let shard = SqliteSubstrate::new().expect("shard substrate");
        let config = crate::config::DatabaseConfig {
            shards: vec![crate::config::ShardConfig {
                name: "s1".to_owned(),
                primary_url: shard.url().to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let shards = crate::sharding::build_shard_set(
            &config,
            vec![crate::db::DatabaseTopology::primary_only(shard.pool())],
            Arc::new(crate::sharding::HashShardRouter),
        )
        .expect("shard set");
        let main = SqliteSubstrate::new().expect("substrate");
        let log = Log::default();
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", logging_handler(log.clone(), Arc::default()));
        let state = AppState::for_test()
            .with_pool(main.pool())
            .with_shards(shards);
        install(
            &state,
            &OutboxConfig {
                enabled: true,
                ..OutboxConfig::default()
            },
            handlers,
        );
        ensure_relay_schema(&state)
            .await
            .expect("schema on every pool");

        let mut conn = shard.pool().get().await.unwrap();
        Outbox::new(&state)
            .write(&mut conn, "a", "t", &serde_json::json!({ "n": "shard" }))
            .await
            .unwrap();
        drop(conn);

        assert_eq!(drain(&state, 10).await.unwrap(), 1);
        assert_eq!(*log.lock().unwrap(), ["shard"]);

        // A full app pool does not starve the shard: each pool has its own
        // budget.
        let mut conn = main.pool().get().await.unwrap();
        for _ in 0..3 {
            Outbox::new(&state)
                .write(&mut conn, "m", "t", &serde_json::json!({ "n": "main" }))
                .await
                .unwrap();
        }
        drop(conn);
        let mut conn = shard.pool().get().await.unwrap();
        Outbox::new(&state)
            .write(&mut conn, "s", "t", &serde_json::json!({ "n": "shard" }))
            .await
            .unwrap();
        drop(conn);

        assert_eq!(drain(&state, 1).await.unwrap(), 2, "one from each pool");
        let sent = log.lock().unwrap().clone();
        assert_eq!(
            sent.iter().filter(|n| *n == "shard").count(),
            2,
            "the earlier shard message and this one: {sent:?}"
        );
        assert_eq!(
            drain(&state, 10).await.unwrap(),
            2,
            "the rest of the app pool"
        );
        assert_eq!(log.lock().unwrap().len(), 5);
    }

    /// A shard that fails (here: no outbox tables) does not stop the drain or
    /// the purge of the other pools. Only a drain where every pool fails is an error.
    #[tokio::test]
    async fn a_failed_shard_does_not_block_the_other_pools() {
        let broken = SqliteSubstrate::new().expect("shard substrate");
        let config = crate::config::DatabaseConfig {
            shards: vec![crate::config::ShardConfig {
                name: "broken".to_owned(),
                primary_url: broken.url().to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let shards = crate::sharding::build_shard_set(
            &config,
            vec![crate::db::DatabaseTopology::primary_only(broken.pool())],
            Arc::new(crate::sharding::HashShardRouter),
        )
        .expect("shard set");
        let main = SqliteSubstrate::new().expect("substrate");
        ensure_schema(&main.pool())
            .await
            .expect("schema on the app pool");
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", |_, _| async { Ok(()) });
        let state = AppState::for_test()
            .with_pool(main.pool())
            .with_shards(shards);
        install(
            &state,
            &OutboxConfig {
                enabled: true,
                ..OutboxConfig::default()
            },
            handlers,
        );
        let mut conn = main.pool().get().await.unwrap();
        Outbox::new(&state)
            .write(&mut conn, "a", "t", &serde_json::json!({}))
            .await
            .unwrap();
        drop(conn);

        assert_eq!(
            drain(&state, 10).await.unwrap(),
            1,
            "the app pool still drains"
        );
        assert!(purge(&state).await.is_ok(), "the app pool still purges");
    }

    /// With shards and no app pool, the relay still installs and drains the
    /// shards.
    #[tokio::test]
    async fn relay_runs_on_shards_without_an_app_pool() {
        let shard = SqliteSubstrate::new().expect("shard substrate");
        let config = crate::config::DatabaseConfig {
            shards: vec![crate::config::ShardConfig {
                name: "only".to_owned(),
                primary_url: shard.url().to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let shards = crate::sharding::build_shard_set(
            &config,
            vec![crate::db::DatabaseTopology::primary_only(shard.pool())],
            Arc::new(crate::sharding::HashShardRouter),
        )
        .expect("shard set");
        let mut handlers = OutboxHandlers::default();
        handlers.insert("t", |_, _| async { Ok(()) });
        let state = AppState::for_test().with_shards(shards);
        assert!(state.pool().is_none());
        install(
            &state,
            &OutboxConfig {
                enabled: true,
                ..OutboxConfig::default()
            },
            handlers,
        );
        ensure_relay_schema(&state)
            .await
            .expect("schema on the shard");
        let mut conn = shard.pool().get().await.unwrap();
        Outbox::new(&state)
            .write(&mut conn, "a", "t", &serde_json::json!({}))
            .await
            .expect("the outbox is on");
        drop(conn);

        assert_eq!(drain(&state, 10).await.unwrap(), 1);
        assert!(purge(&state).await.is_ok());
    }

    #[tokio::test]
    async fn schema_is_idempotent() {
        let fx = fixture(OutboxConfig::default(), OutboxHandlers::default()).await;
        ensure_schema(fx.state.pool().unwrap()).await.unwrap();
    }
}
