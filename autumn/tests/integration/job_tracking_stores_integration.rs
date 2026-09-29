//! Docker-gated integration tests for the Redis/Postgres tracked-job stores
//! (issue #1373): prove `enqueue_tracked` actually persists through the
//! backend selected by `jobs.backend`, not silently through the in-memory
//! fallback, and that the configured TTL expires records on each backend.
//!
//! Both tests require Docker and are marked `#[ignore]`. Run them explicitly
//! with:
//!
//! ```text
//! cargo test -p autumn-web --features redis,db --test job_tracking_stores_integration -- --ignored
//! ```

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use autumn_web::config::JobConfig;
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnResult};
use serde_json::Value;

fn noop_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move { Ok(()) })
}

fn noop_job_info() -> JobInfo {
    JobInfo {
        version: 1,
        name: "noop".to_string(),
        max_attempts: 1,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: noop_handler,
    }
}

#[cfg(feature = "redis")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn redis_backend_persists_tracked_job_and_expires_it() {
    use redis::AsyncCommands as _;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::redis::Redis as RedisImage;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let redis_url = format!("redis://127.0.0.1:{port}");

    let config = JobConfig {
        backend: "redis".to_owned(),
        redis: autumn_web::config::JobRedisConfig {
            url: Some(redis_url.clone()),
            ..Default::default()
        },
        tracking: autumn_web::config::JobTrackingConfig {
            ttl_secs: 1,
            ..Default::default()
        },
        ..Default::default()
    };

    let state = AppState::for_test().with_profile("dev");
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(vec![noop_job_info()], &state, &shutdown, &config, true)
        .expect("start redis-backed job runtime");

    let handle = job::enqueue_tracked("noop", serde_json::json!({}))
        .await
        .expect("enqueue_tracked");

    // Query Redis directly, bypassing the JobTrackingStore abstraction, to
    // prove the config-selected backend really is Redis — not the in-memory
    // fallback, which would leave nothing here to find.
    // Through the guard, not `redis::Client::open`: this file is outside the
    // `src/` scan in `autumn_web::redis_tls`, and a bare call left here is a
    // copy-paste source for the very pattern that scan exists to eradicate
    // (#2172).
    let client = autumn_web::redis_tls::open_client(&redis_url).expect("open redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to redis");
    let key = format!(
        "{}:tracking:{}",
        config.redis.key_prefix,
        autumn_web::auth::hash_api_token(&handle.token)
    );

    let payload: String = conn
        .get(&key)
        .await
        .expect("tracked record should be persisted in redis");
    let record: Value = serde_json::from_str(&payload).expect("stored record is valid JSON");
    assert!(
        record["status"].is_string(),
        "expected a status field: {record}"
    );

    // The 1s TTL configured above (rather than the 86400s default) should
    // expire the key via Redis's own EX mechanism.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let exists: bool = conn.exists(&key).await.expect("check key existence");
    assert!(!exists, "tracked record should have expired via redis TTL");

    shutdown.cancel();
    job::clear_global_job_client();
}

#[cfg(feature = "db")]
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn postgres_backend_persists_tracked_job_and_expires_it() {
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    const CREATE_AUTUMN_JOBS: &str =
        include_str!("../../migrations/20260513000000_create_job_queue/up.sql");
    // The base `create_job_queue` table predates several additive `autumn_jobs`
    // columns the store's enqueue path writes (trace context, uniqueness /
    // concurrency keys, the pending-unique-key, and — critically — the named
    // `queue` column added by `add_queue_to_jobs`). Seed the same additive
    // migrations the production runner applies on top of the base table so the
    // schema matches production at enqueue time; otherwise enqueue fails with
    // `column "queue" of relation "autumn_jobs" does not exist`.
    const ADD_TRACE_CONTEXT_TO_JOBS: &str =
        include_str!("../../migrations/20260519000000_add_trace_context_to_jobs/up.sql");
    const ADD_JOB_UNIQUENESS_CONCURRENCY: &str =
        include_str!("../../migrations/20260610000000_add_job_uniqueness_concurrency/up.sql");
    const ADD_PENDING_UNIQUE_KEY_TO_JOBS: &str =
        include_str!("../../migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql");
    const ADD_QUEUE_TO_JOBS: &str =
        include_str!("../../migrations/20260628000000_add_queue_to_jobs/up.sql");
    const CREATE_JOB_TRACKING: &str =
        include_str!("../../migrations/20260702000000_create_job_tracking/up.sql");

    #[derive(diesel::QueryableByName)]
    struct RecordRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        record: String,
    }

    #[derive(diesel::QueryableByName)]
    struct ExpiredRow {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        expired: bool,
    }

    #[derive(diesel::QueryableByName)]
    struct RemainingSecsRow {
        #[diesel(sql_type = diesel::sql_types::Double)]
        remaining_secs: f64,
    }

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("build pool");

    {
        let mut conn = pool.get().await.expect("get connection");
        // Apply the migration `up.sql` scripts through the simple-query
        // protocol (`batch_execute`), matching the production migration runner:
        // these files contain multiple statements, which the prepared/extended
        // query path (`sql_query(..).execute(..)`) rejects with "cannot insert
        // multiple commands into a prepared statement".
        conn.batch_execute(CREATE_AUTUMN_JOBS)
            .await
            .expect("apply create_job_queue migration");
        // Bring `autumn_jobs` up to the full production schema (in migration
        // order) so every column the enqueue path writes — including `queue` —
        // exists before `enqueue_tracked` runs.
        conn.batch_execute(ADD_TRACE_CONTEXT_TO_JOBS)
            .await
            .expect("apply add_trace_context_to_jobs migration");
        conn.batch_execute(ADD_JOB_UNIQUENESS_CONCURRENCY)
            .await
            .expect("apply add_job_uniqueness_concurrency migration");
        conn.batch_execute(ADD_PENDING_UNIQUE_KEY_TO_JOBS)
            .await
            .expect("apply add_pending_unique_key_to_jobs migration");
        conn.batch_execute(ADD_QUEUE_TO_JOBS)
            .await
            .expect("apply add_queue_to_jobs migration");
        // Proves the migration in this PR actually creates the table the
        // store depends on — not just that hand-written SQL happens to work.
        conn.batch_execute(CREATE_JOB_TRACKING)
            .await
            .expect("apply create_job_tracking migration");
    }

    // 10s, not 1s: `PgJobTrackingStore::update` (see the comment below) only
    // applies a lifecycle write while `expires_at > now` — it silently
    // no-ops once the row is already expired. At ttl_secs: 1, a
    // `mark_running`/`settle_success` write delayed past one second by
    // ordinary Docker-CI-runner scheduler or database contention would find
    // its own write vetoed, leaving `status` stuck at "pending" forever and
    // turning the poll loop below into a second, load-dependent flake
    // (caught on review, PR #2867, before this ever ran organically). 10s
    // gives real job dispatch — normally sub-second even under load —
    // comfortable room to land before the row's own TTL could veto it.
    let config = JobConfig {
        backend: "postgres".to_owned(),
        tracking: autumn_web::config::JobTrackingConfig {
            ttl_secs: 10,
            ..Default::default()
        },
        ..Default::default()
    };

    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(vec![noop_job_info()], &state, &shutdown, &config, true)
        .expect("start postgres-backed job runtime");

    let handle = job::enqueue_tracked("noop", serde_json::json!({}))
        .await
        .expect("enqueue_tracked");

    let key = autumn_web::auth::hash_api_token(&handle.token);
    let mut conn = pool.get().await.expect("get connection");
    // Cast to ::TEXT to match the store's own read path (job_tracking.rs); this
    // strips the binary JSONB header so `record` is proper JSON text, not a
    // 0x01-prefixed blob that serde_json::from_str would reject.
    let row =
        diesel::sql_query("SELECT record::TEXT AS record FROM autumn_job_tracking WHERE key = $1")
            .bind::<diesel::sql_types::Text, _>(&key)
            .get_result::<RecordRow>(&mut *conn)
            .await
            .expect("tracked record should be persisted in postgres");
    let record: Value = serde_json::from_str(&row.record).expect("stored record is valid JSON");
    assert!(
        record["status"].is_string(),
        "expected a status field: {record}"
    );

    // `PgJobTrackingStore::update` (job_tracking.rs) unconditionally rewrites
    // `expires_at` to `now + ttl` on every lifecycle write, not only on
    // enqueue — `mark_running` and `settle_success` both go through it. A
    // fixed sleep measured from the read above races the job runtime's own
    // dispatch: if `mark_running` or `settle_success` lands after the read
    // but before the sleep elapses, it pushes `expires_at` back out and the
    // TTL check below can see a record that hasn't expired yet, even though
    // the *original* enqueue-time expiry already passed
    // (docs/ci-health/quarantine-ledger.md, "record should be past its
    // configured TTL", 1/50 same-commit rerun rate — confirmed by CI-native
    // rerun campaign). Refreshing `expires_at` on every lifecycle write is
    // deliberate, correct store behavior (a job still being worked on
    // shouldn't expire out from under it), so the fix is in this test: await
    // the job's own terminal status — the point after which nothing will
    // touch `expires_at` again for this key — before starting the TTL clock,
    // instead of guessing a sleep long enough to outrun a write whose timing
    // this test doesn't control.
    // Bounded to 8s — under the 10s TTL above, so a completion this slow
    // still leaves margin before the row could expire out from under a
    // still-in-flight `mark_running`/`settle_success` write.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let row = diesel::sql_query(
            "SELECT record::TEXT AS record FROM autumn_job_tracking WHERE key = $1",
        )
        .bind::<diesel::sql_types::Text, _>(&key)
        .get_result::<RecordRow>(&mut *conn)
        .await
        .expect("tracked record should still exist while polling for completion");
        let record: Value = serde_json::from_str(&row.record).expect("stored record is valid JSON");
        if record["status"] == "succeeded" || record["status"] == "failed" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "noop job never reached a terminal tracked status within 8s: {record}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The job is terminal, so `mark_running`/`settle_success` have made
    // their last write for this key and nothing else will touch
    // `expires_at` again. Rather than guess how much of the configured TTL
    // that last write already consumed, ask Postgres directly how long is
    // left on the row's *actual* `expires_at` (whichever write set it last)
    // and sleep exactly that plus a small margin — deterministic regardless
    // of how much of the 10s TTL the completion wait above used up. Lazy
    // expiry means the row itself isn't deleted, only ignored on read.
    let remaining_secs = diesel::sql_query(
        "SELECT GREATEST(EXTRACT(EPOCH FROM (expires_at - NOW())), 0)::FLOAT8 AS remaining_secs \
         FROM autumn_job_tracking WHERE key = $1",
    )
    .bind::<diesel::sql_types::Text, _>(&key)
    .get_result::<RemainingSecsRow>(&mut *conn)
    .await
    .expect("row should still exist to compute remaining TTL")
    .remaining_secs;
    tokio::time::sleep(Duration::from_secs_f64(remaining_secs + 0.3)).await;
    let expired = diesel::sql_query(
        "SELECT (expires_at <= NOW()) AS expired FROM autumn_job_tracking WHERE key = $1",
    )
    .bind::<diesel::sql_types::Text, _>(&key)
    .get_result::<ExpiredRow>(&mut *conn)
    .await
    .expect("row should still exist")
    .expired;
    assert!(expired, "record should be past its configured TTL");

    shutdown.cancel();
    job::clear_global_job_client();
}
