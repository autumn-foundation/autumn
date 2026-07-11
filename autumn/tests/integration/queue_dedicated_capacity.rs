//! Dedicated per-queue capacity contract test for issue #1623 (AC5).
//!
//! Proves the headline guarantee: with one queue saturated by slow jobs, a job
//! enqueued to a queue with **dedicated** capacity (`reserved` slots) is claimed
//! promptly instead of waiting behind the flood.
//!
//! Topology: `jobs.workers = 2`, and the `critical` queue reserves `1` slot. A
//! flood on `bulk` may therefore occupy at most `1` (shared) slot, leaving the
//! other worker free for `critical`'s reserved capacity. A `critical` job
//! enqueued while `bulk` is saturated must start almost immediately.
//!
//! Requires Docker (testcontainers Redis) and is marked `#[ignore]`. Run:
//!
//! ```text
//! cargo test -p autumn-web --features redis,db \
//!   --test integration_tests queue_dedicated_capacity -- --ignored
//! ```
//!
//! Gated on `#[cfg(feature = "redis")]` so it always compiles.

#![cfg(feature = "redis")]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::config::{JobConfig, JobQueue, JobQueuesConfig, JobRedisConfig};
use autumn_web::job::{self, JobInfo};
use autumn_web::{AppState, AutumnResult};
use serde_json::Value;
use tokio::time::{sleep, timeout};

static BULK_STARTED: AtomicUsize = AtomicUsize::new(0);
static CRITICAL_STARTED: AtomicUsize = AtomicUsize::new(0);

fn bulk_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        BULK_STARTED.fetch_add(1, Ordering::SeqCst);
        // Slow: hold the shared slot long enough to demonstrate the flood.
        sleep(Duration::from_millis(800)).await;
        Ok(())
    })
}

fn critical_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        CRITICAL_STARTED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

fn bulk_job_info() -> JobInfo {
    let mut info = JobInfo::new("cap_bulk", 1, 10, bulk_handler);
    info.queue = "bulk".to_string();
    info
}

fn critical_job_info() -> JobInfo {
    let mut info = JobInfo::new("cap_critical", 1, 10, critical_handler);
    info.queue = "critical".to_string();
    info
}

/// Redis config with 2 workers and a `critical` queue reserving 1 dedicated slot.
fn dedicated_capacity_config(url: &str) -> JobConfig {
    JobConfig {
        backend: "redis".to_owned(),
        workers: 2,
        queues: JobQueuesConfig::weighted_specs(vec![
            JobQueue {
                name: "critical".to_string(),
                weight: 1,
                concurrency: None,
                // One slot dedicated to `critical` that `bulk` can never take.
                reserved: Some(1),
            },
            JobQueue {
                name: "bulk".to_string(),
                weight: 1,
                concurrency: None,
                reserved: None,
            },
        ]),
        redis: JobRedisConfig {
            url: Some(url.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// AC5: a job on a dedicated-capacity queue is claimed promptly despite a flood.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reserved_queue_is_served_promptly_under_flood() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::redis::Redis as RedisImage;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    BULK_STARTED.store(0, Ordering::SeqCst);
    CRITICAL_STARTED.store(0, Ordering::SeqCst);

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let url = format!("redis://127.0.0.1:{port}");
    let config = dedicated_capacity_config(&url);

    let state = AppState::for_test().with_profile("dev");
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(
        vec![bulk_job_info(), critical_job_info()],
        &state,
        &shutdown,
        &config,
        true,
    )
    .expect("worker runtime should start");

    // Saturate `bulk` with a flood of slow jobs.
    for _ in 0..8 {
        job::enqueue("cap_bulk", serde_json::json!({}))
            .await
            .expect("enqueue bulk job");
    }

    // Let the flood get claimed and occupy its (single, shared) slot.
    let bulk_running = timeout(Duration::from_secs(5), async {
        loop {
            if BULK_STARTED.load(Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(bulk_running.is_ok(), "bulk flood should start executing");

    // Now enqueue a single `critical` job while `bulk` is still flooding.
    job::enqueue("cap_critical", serde_json::json!({}))
        .await
        .expect("enqueue critical job");

    // It must be served from `critical`'s reserved slot almost immediately —
    // well before the 800ms bulk jobs would free the shared slot.
    let critical_started = timeout(Duration::from_millis(600), async {
        loop {
            if CRITICAL_STARTED.load(Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        critical_started.is_ok(),
        "critical job with dedicated capacity must be claimed promptly, not wait behind the flood"
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
