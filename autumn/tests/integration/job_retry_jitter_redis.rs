//! Issue #3054 on the Redis job backend: jobs that fail together must not
//! retry together.
//!
//! Each job fails with a 1 h backoff and has 3 attempts. Before #3054 every
//! retry was due at `failure + 1 h`, so all scores in the `delayed` set fell
//! in the short window in which the jobs failed. Full jitter puts each one in
//! `[0, 1 h]`. A retry that is due at once fails again and waits in the set
//! for its third attempt, so the set still holds one entry per job.
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn redis_job_retries_spread_out() {
    use testcontainers::runners::AsyncRunner as _;
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
    let url = format!("redis://127.0.0.1:{port}");

    let state = AppState::for_test()
        .with_profile("dev")
        .with_entropy(autumn_web::entropy::SeededEntropy::shared(0x3054));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = JobConfig {
        backend: "redis".to_owned(),
        workers: 2,
        redis: JobRedisConfig {
            url: Some(url.clone()),
            key_prefix: KEY_PREFIX.to_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    job::start_runtime(
        vec![JobInfo::new("redis_storm_job", 3, 3_600_000, always_fails)],
        &state,
        &shutdown,
        &config,
        true,
    )
    .expect("redis job runtime starts");

    for n in 0..STORM_JOBS {
        job::enqueue("redis_storm_job", serde_json::json!({ "n": n }))
            .await
            .expect("enqueue");
    }

    let client = autumn_web::redis_tls::open_client(&url).expect("open redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("connect to redis");
    let delayed_key = format!("{KEY_PREFIX}:delayed");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let scores: Vec<(String, u64)> = loop {
        let scores: Vec<(String, u64)> = redis::cmd("ZRANGE")
            .arg(&delayed_key)
            .arg(0)
            .arg(-1)
            .arg("WITHSCORES")
            .query_async(&mut conn)
            .await
            .expect("read the delayed set");
        if scores.len() >= STORM_JOBS {
            break scores;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "every job must schedule its retry; delayed set: {scores:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let due: Vec<u64> = scores.iter().map(|(_, score)| *score).collect();
    let spread_ms = due.iter().max().unwrap() - due.iter().min().unwrap();
    assert!(
        spread_ms > 10_000,
        "{STORM_JOBS} retries are due within {spread_ms} ms of each other: they retry together"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
