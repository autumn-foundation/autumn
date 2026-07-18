//! Process-role worker-gating contract tests for issue #1613.
//!
//! These prove the durable-backend behavior that the in-process `local` backend
//! cannot demonstrate (a split web/worker topology is disallowed on `local`):
//!
//! - AC #1: a **web** replica (`run_workers = false`) installs the enqueue
//!   client and enqueues into the durable queue, but runs **no** worker loops,
//!   so the job is not executed until a **worker**/combined replica drains it.
//! - AC #5: a **worker** replica drains an in-flight job to completion when its
//!   shutdown token is cancelled (the worker loop only breaks between jobs, so a
//!   claimed job finishes first).
//!
//! Both require Docker (testcontainers Redis) and are marked `#[ignore]`. Run:
//!
//! ```text
//! cargo test -p autumn-web --features redis,db \
//!   --test integration_tests process_role_worker_gating -- --ignored
//! ```
//!
//! They are gated on `#[cfg(feature = "redis")]` so they always compile; they
//! need the Redis container to actually run.

#![cfg(feature = "redis")]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::config::{JobConfig, JobRedisConfig};
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnResult};
use serde_json::Value;
use tokio::time::{sleep, timeout};

static EXECUTED: AtomicUsize = AtomicUsize::new(0);

fn counting_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    EXECUTED.fetch_add(1, Ordering::SeqCst);
    Box::pin(async move { Ok(()) })
}

fn counting_job_info() -> JobInfo {
    JobInfo::new("role_counting", 1, 10, counting_handler)
}

static SLOW_STARTED: AtomicUsize = AtomicUsize::new(0);
static SLOW_COMPLETED: AtomicUsize = AtomicUsize::new(0);

fn slow_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        SLOW_STARTED.fetch_add(1, Ordering::SeqCst);
        sleep(Duration::from_millis(400)).await;
        SLOW_COMPLETED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

fn slow_job_info() -> JobInfo {
    JobInfo::new("role_slow", 1, 10, slow_handler)
}

fn redis_config(url: &str) -> JobConfig {
    JobConfig {
        backend: "redis".to_owned(),
        redis: JobRedisConfig {
            url: Some(url.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// AC #1: a web replica enqueues into the durable queue but never executes the
/// job; a worker replica against the same Redis drains and runs it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn web_role_does_not_execute_jobs_while_worker_role_does() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::redis::Redis as RedisImage;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    EXECUTED.store(0, Ordering::SeqCst);

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let url = format!("redis://127.0.0.1:{port}");
    let config = redis_config(&url);

    // Web replica: run_workers = false. Installs the enqueue client, no workers.
    let web_state = AppState::for_test().with_profile("dev");
    let web_shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(
        vec![counting_job_info()],
        &web_state,
        &web_shutdown,
        &config,
        false,
    )
    .expect("web-role job runtime should start");

    // The web replica can enqueue into the durable Redis queue.
    job::enqueue("role_counting", serde_json::json!({}))
        .await
        .expect("web replica should enqueue into the durable queue");

    // With no worker loops running, the job stays pending — never executed.
    sleep(Duration::from_millis(500)).await;
    assert_eq!(
        EXECUTED.load(Ordering::SeqCst),
        0,
        "web role must not execute enqueued jobs"
    );

    // Bring up a worker replica against the same Redis; run_workers = true.
    job::clear_global_job_client();
    let worker_state = AppState::for_test().with_profile("dev");
    let worker_shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(
        vec![counting_job_info()],
        &worker_state,
        &worker_shutdown,
        &config,
        true,
    )
    .expect("worker-role job runtime should start");

    // The durable job enqueued by the web replica is now drained and executed.
    let drained = timeout(Duration::from_secs(5), async {
        loop {
            if EXECUTED.load(Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "worker role must drain and execute the durable job the web replica enqueued"
    );

    web_shutdown.cancel();
    worker_shutdown.cancel();
    job::clear_global_job_client();
}

/// AC #5: a worker replica drains an in-flight job to completion when its
/// shutdown token is cancelled mid-flight (the loop only breaks between jobs).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn worker_role_drains_in_flight_job_on_shutdown() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::redis::Redis as RedisImage;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    SLOW_STARTED.store(0, Ordering::SeqCst);
    SLOW_COMPLETED.store(0, Ordering::SeqCst);

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let url = format!("redis://127.0.0.1:{port}");
    let config = redis_config(&url);

    let state = AppState::for_test().with_profile("dev");
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(vec![slow_job_info()], &state, &shutdown, &config, true)
        .expect("worker-role job runtime should start");

    job::enqueue("role_slow", serde_json::json!({}))
        .await
        .expect("enqueue slow job");

    // Wait until the worker has claimed and started the job (it is in-flight).
    let started = timeout(Duration::from_secs(5), async {
        loop {
            if SLOW_STARTED.load(Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(started.is_ok(), "job should start executing");
    assert_eq!(
        SLOW_COMPLETED.load(Ordering::SeqCst),
        0,
        "job should still be in-flight when we cancel"
    );

    // Cancel while the handler is mid-flight; the worker finishes it first.
    shutdown.cancel();
    let drained = timeout(Duration::from_secs(5), async {
        loop {
            if SLOW_COMPLETED.load(Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "worker must drain the in-flight job to completion on shutdown"
    );

    job::clear_global_job_client();
}
