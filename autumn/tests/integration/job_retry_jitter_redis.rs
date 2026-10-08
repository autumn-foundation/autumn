//! Issue #3054 on the Redis job backend: jobs that fail together must not
//! retry together.
//!
//! Each job has a 1 h backoff. Before #3054 every retry was due at
//! `failure + 1 h` (and a recovered claim at once), so all due times fell in
//! the short window in which the jobs failed. Full jitter puts each one in
//! `[0, 1 h]`. The tests read the due times from the `delayed` sorted set.
//!
//! Requires Docker. CI's `--ignored` sweep runs it (see CLAUDE.md).

#![cfg(feature = "redis")]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use autumn_web::config::{JobConfig, JobRedisConfig};
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnError, AutumnResult};
use serde_json::Value;

const STORM_JOBS: usize = 8;
const KEY_PREFIX: &str = "autumn:jitter-test";

fn always_fails(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move { Err(AutumnError::internal_server_error_msg("blip")) })
}

/// A handler that hangs, as on a dependency that never answers. The lease
/// heartbeat keeps its claim alive (#3051), so the claim expires only when
/// the worker dies.
fn hangs(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(std::future::pending())
}

/// A Redis in a container and its URL. Keep the container alive for the test.
async fn redis_server() -> (
    testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    String,
) {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::redis::Redis as RedisImage;

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    (container, format!("redis://127.0.0.1:{port}"))
}

fn config(url: &str, workers: usize, visibility_timeout_ms: u64) -> JobConfig {
    JobConfig {
        backend: "redis".to_owned(),
        workers,
        redis: JobRedisConfig {
            url: Some(url.to_owned()),
            key_prefix: KEY_PREFIX.to_owned(),
            visibility_timeout_ms,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Run `phase` on a new runtime, then shut that runtime down without waiting.
/// That stops every task it spawned, lease heartbeats included, like a
/// process kill.
fn run_then_kill<F>(phase: F)
where
    F: FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("worker runtime");
        runtime.block_on(phase());
        runtime.shutdown_background();
    })
    .join()
    .expect("worker thread");
}

/// Wait until the `processing` set holds `count` claims.
async fn wait_for_claims(url: &str, count: usize) {
    let client = autumn_web::redis_tls::open_client(url).expect("open redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to redis");
    let processing_key = format!("{KEY_PREFIX}:processing");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let claims: usize = redis::cmd("ZCARD")
            .arg(&processing_key)
            .query_async(&mut conn)
            .await
            .expect("read the processing set");
        if claims >= count {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "every job must be claimed; {claims} of {count} are"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until the `delayed` set holds `STORM_JOBS` entries, then return the
/// spread of their due times in ms.
async fn delayed_spread_ms(url: &str) -> u64 {
    let client = autumn_web::redis_tls::open_client(url).expect("open redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to redis");
    let delayed_key = format!("{KEY_PREFIX}:delayed");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let scores: Vec<(String, u64)> = redis::cmd("ZRANGE")
            .arg(&delayed_key)
            .arg(0)
            .arg(-1)
            .arg("WITHSCORES")
            .query_async(&mut conn)
            .await
            .expect("read the delayed set");
        if scores.len() >= STORM_JOBS {
            let due: Vec<u64> = scores.iter().map(|(_, score)| *score).collect();
            return due.iter().max().unwrap() - due.iter().min().unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "every job must be scheduled; delayed set: {scores:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Each job fails and has 3 attempts. A retry that is due at once fails again
/// and waits in the set for its third attempt, so the set still holds one
/// entry per job.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn redis_job_retries_spread_out() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let (_container, url) = redis_server().await;

    let state = AppState::for_test()
        .with_profile("dev")
        .with_entropy(autumn_web::entropy::SeededEntropy::shared(0x3054));
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(
        vec![JobInfo::new("redis_storm_job", 3, 3_600_000, always_fails)],
        &state,
        &shutdown,
        &config(&url, 2, 30_000),
        true,
    )
    .expect("redis job runtime starts");

    for n in 0..STORM_JOBS {
        job::enqueue("redis_storm_job", serde_json::json!({ "n": n }))
            .await
            .expect("enqueue");
    }

    let spread_ms = delayed_spread_ms(&url).await;
    assert!(
        spread_ms > 10_000,
        "{STORM_JOBS} retries are due within {spread_ms} ms of each other: they retry together"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

/// Claims that expire together must not all run again at once. Before the
/// fix the recovery pushed every job back onto its queue at once.
///
/// A claim expires only when its worker dies, since the lease heartbeat
/// renews it while the worker lives (#3051). So worker A claims every job,
/// its handlers hang, and the test kills it. Worker B recovers the claims.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn redis_recovered_claims_spread_out() {
    const VISIBILITY_MS: u64 = 500;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    let (_container, url) = redis_server().await;

    let url_a = url.clone();
    run_then_kill(move || {
        Box::pin(async move {
            let state = AppState::for_test().with_profile("dev");
            let shutdown = tokio_util::sync::CancellationToken::new();
            // One worker per job, so worker A claims every job.
            job::start_runtime(
                vec![JobInfo::new("redis_hang_job", 3, 3_600_000, hangs)],
                &state,
                &shutdown,
                &config(&url_a, STORM_JOBS, VISIBILITY_MS),
                true,
            )
            .expect("worker A starts");
            for n in 0..STORM_JOBS {
                job::enqueue("redis_hang_job", serde_json::json!({ "n": n }))
                    .await
                    .expect("enqueue");
            }
            wait_for_claims(&url_a, STORM_JOBS).await;
        })
    });
    job::clear_global_job_client();

    let state = AppState::for_test()
        .with_profile("dev")
        .with_entropy(autumn_web::entropy::SeededEntropy::shared(0x3054));
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(
        vec![JobInfo::new("redis_hang_job", 3, 3_600_000, hangs)],
        &state,
        &shutdown,
        &config(&url, 2, VISIBILITY_MS),
        true,
    )
    .expect("worker B starts");

    let spread_ms = delayed_spread_ms(&url).await;
    assert!(
        spread_ms > 10_000,
        "{STORM_JOBS} recovered claims are due within {spread_ms} ms of each other"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
