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

// ---------------------------------------------------------------------------
// AC2: a per-queue `concurrency` cap holds the queue below the process total.
// ---------------------------------------------------------------------------

static CAPPED_RUNNING: AtomicUsize = AtomicUsize::new(0);
static CAPPED_PEAK: AtomicUsize = AtomicUsize::new(0);
static CAPPED_COMPLETED: AtomicUsize = AtomicUsize::new(0);

/// The cap under test, and the worker count it must stay below.
const BULK_CAP: usize = 2;
const CAP_WORKERS: usize = 4;
const CAP_JOB_COUNT: usize = 8;

fn capped_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        let running = CAPPED_RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        CAPPED_PEAK.fetch_max(running, Ordering::SeqCst);
        // Long enough that, uncapped, all four workers would overlap here.
        sleep(Duration::from_millis(250)).await;
        CAPPED_RUNNING.fetch_sub(1, Ordering::SeqCst);
        CAPPED_COMPLETED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

fn capped_job_info() -> JobInfo {
    let mut info = JobInfo::new("cap_capped_bulk", 1, 10, capped_handler);
    info.queue = "bulk".to_string();
    info
}

/// Redis config with 4 workers where `bulk` is capped at 2 concurrent jobs.
fn concurrency_cap_config(url: &str) -> JobConfig {
    JobConfig {
        backend: "redis".to_owned(),
        workers: CAP_WORKERS,
        queues: JobQueuesConfig::weighted_specs(vec![JobQueue {
            name: "bulk".to_string(),
            weight: 1,
            concurrency: Some(BULK_CAP),
            reserved: None,
        }]),
        redis: JobRedisConfig {
            url: Some(url.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// AC2: *"An operator can cap a queue's concurrency below the process total, so
/// a bulk queue can never occupy more than its configured share of slots."*
///
/// Four workers, a `bulk` queue capped at two, and a backlog deep enough that an
/// uncapped pool would run four at once. The observed peak must never exceed the
/// cap — and every job must still complete, so the cap throttles the queue
/// rather than deadlocking it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrency_cap_bounds_in_flight_jobs_below_the_worker_count() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::redis::Redis as RedisImage;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    CAPPED_RUNNING.store(0, Ordering::SeqCst);
    CAPPED_PEAK.store(0, Ordering::SeqCst);
    CAPPED_COMPLETED.store(0, Ordering::SeqCst);

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let url = format!("redis://127.0.0.1:{port}");

    let state = AppState::for_test().with_profile("dev");
    let shutdown = tokio_util::sync::CancellationToken::new();
    job::start_runtime(
        vec![capped_job_info()],
        &state,
        &shutdown,
        &concurrency_cap_config(&url),
        true,
    )
    .expect("worker runtime should start");

    for _ in 0..CAP_JOB_COUNT {
        job::enqueue("cap_capped_bulk", serde_json::json!({}))
            .await
            .expect("enqueue capped job");
    }

    let drained = timeout(Duration::from_secs(30), async {
        loop {
            if CAPPED_COMPLETED.load(Ordering::SeqCst) >= CAP_JOB_COUNT {
                break;
            }
            // Sample the peak while the backlog drains, not only at the end.
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "a capped queue must still drain its whole backlog, not deadlock ({} of {CAP_JOB_COUNT} \
         completed)",
        CAPPED_COMPLETED.load(Ordering::SeqCst),
    );

    let peak = CAPPED_PEAK.load(Ordering::SeqCst);
    assert!(
        peak <= BULK_CAP,
        "`bulk` is capped at {BULK_CAP} concurrent job(s) but {peak} ran at once \
         (process has {CAP_WORKERS} workers)",
    );
    // Guard against the cap "passing" because the queue never got going: with a
    // backlog of 8 and a cap of 2, at least 2 must have overlapped.
    assert!(
        peak >= 2,
        "expected the capped queue to use its full allowance of {BULK_CAP}, saw a peak of {peak} \
         — the test would not distinguish a cap from a stalled queue",
    );

    shutdown.cancel();
    job::clear_global_job_client();
}

// ---------------------------------------------------------------------------
// Success metric: p95 enqueue-to-start latency on a queue with dedicated
// capacity stays within 2x its unloaded baseline while another queue floods.
// ---------------------------------------------------------------------------

static METRIC_LAST_START: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);
static METRIC_CRITICAL_RAN: AtomicUsize = AtomicUsize::new(0);
static METRIC_FLOOD_RAN: AtomicUsize = AtomicUsize::new(0);

/// Samples per phase. Enough for a p95 to mean something without making the
/// flood phase run long.
const METRIC_SAMPLES: usize = 20;

/// Flood depth. Deep enough that `bulk`'s single shared slot (300ms per job)
/// cannot drain it while the loaded phase samples, so the phase really is
/// "one queue fully saturated by slow jobs".
const METRIC_FLOOD_JOBS: usize = 40;

/// Absolute floor on the allowed loaded p95, so the assertion does not become
/// impossibly tight when the unloaded baseline is near zero. Ordinary scheduler
/// and Redis round-trip noise lives well under this; the starvation the issue
/// describes is seconds of flood, orders of magnitude above it.
const METRIC_FLOOR: Duration = Duration::from_millis(250);

fn metric_critical_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    // Stamp the start instant before anything else so the sample measures
    // enqueue-to-start, not enqueue-to-finish.
    *METRIC_LAST_START
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::time::Instant::now());
    METRIC_CRITICAL_RAN.fetch_add(1, Ordering::SeqCst);
    Box::pin(async move { Ok(()) })
}

fn metric_flood_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        METRIC_FLOOD_RAN.fetch_add(1, Ordering::SeqCst);
        sleep(Duration::from_millis(300)).await;
        Ok(())
    })
}

fn metric_jobs() -> Vec<JobInfo> {
    let mut critical = JobInfo::new("metric_critical", 1, 10, metric_critical_handler);
    critical.queue = "critical".to_string();
    let mut flood = JobInfo::new("metric_flood", 1, 10, metric_flood_handler);
    flood.queue = "bulk".to_string();
    vec![critical, flood]
}

/// Enqueue one `critical` job and return how long it took to *start*.
async fn sample_critical_latency() -> Duration {
    let before = METRIC_CRITICAL_RAN.load(Ordering::SeqCst);
    *METRIC_LAST_START
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    let enqueued_at = std::time::Instant::now();
    job::enqueue("metric_critical", serde_json::json!({}))
        .await
        .expect("enqueue critical job");
    let ran = timeout(Duration::from_secs(20), async {
        loop {
            if METRIC_CRITICAL_RAN.load(Ordering::SeqCst) > before {
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(ran.is_ok(), "a critical job never started within 20s");
    let started_at = METRIC_LAST_START
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .expect("handler stamps its start instant before bumping the counter");
    started_at.saturating_duration_since(enqueued_at)
}

/// p95 by nearest-rank: the smallest sample at or above the 95th percentile.
fn p95(mut samples: Vec<Duration>) -> Duration {
    assert!(!samples.is_empty(), "no samples");
    samples.sort_unstable();
    let rank = (samples.len() as f64 * 0.95).ceil() as usize;
    let index = rank.saturating_sub(1).min(samples.len() - 1);
    samples[index]
}

/// The issue's Success Metric: *"With one queue fully saturated by slow jobs,
/// p95 enqueue-to-start latency for jobs on a queue with dedicated capacity
/// stays within 2x its unloaded baseline."*
///
/// Both phases run against the same process and the same Redis, and the baseline
/// is measured *in this run* rather than hard-coded, so the bound calibrates
/// itself to whatever machine CI hands us instead of encoding one box's timings.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reserved_queue_p95_latency_stays_within_twice_its_unloaded_baseline() {
    use testcontainers::runners::AsyncRunner as _;
    use testcontainers_modules::redis::Redis as RedisImage;

    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    METRIC_CRITICAL_RAN.store(0, Ordering::SeqCst);
    METRIC_FLOOD_RAN.store(0, Ordering::SeqCst);

    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    let url = format!("redis://127.0.0.1:{port}");

    let state = AppState::for_test().with_profile("dev");
    let shutdown = tokio_util::sync::CancellationToken::new();
    // Same topology as the AC5 test: 2 workers, 1 slot dedicated to `critical`.
    job::start_runtime(
        metric_jobs(),
        &state,
        &shutdown,
        &dedicated_capacity_config(&url),
        true,
    )
    .expect("worker runtime should start");

    // Phase 1 — unloaded baseline.
    let mut baseline = Vec::with_capacity(METRIC_SAMPLES);
    for _ in 0..METRIC_SAMPLES {
        baseline.push(sample_critical_latency().await);
    }
    let baseline_p95 = p95(baseline);

    // Phase 2 — saturate `bulk` with slow jobs and keep it saturated for the
    // whole sampling window (each flood job holds its slot for 300ms).
    for _ in 0..METRIC_FLOOD_JOBS {
        job::enqueue("metric_flood", serde_json::json!({}))
            .await
            .expect("enqueue flood job");
    }
    let flooding = timeout(Duration::from_secs(10), async {
        loop {
            if METRIC_FLOOD_RAN.load(Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(flooding.is_ok(), "the flood should start executing");

    let mut loaded = Vec::with_capacity(METRIC_SAMPLES);
    for _ in 0..METRIC_SAMPLES {
        loaded.push(sample_critical_latency().await);
    }
    let loaded_p95 = p95(loaded);

    // The flood must still have been in flight throughout, or the "loaded"
    // phase measured an idle queue and the metric means nothing. `bulk` gets one
    // shared slot and each job holds it for 300ms, so an undrained backlog at
    // this point means the queue was saturated for the whole sampling window.
    let flood_started = METRIC_FLOOD_RAN.load(Ordering::SeqCst);
    assert!(
        (1..METRIC_FLOOD_JOBS).contains(&flood_started),
        "the flood should have been saturating `bulk` throughout the loaded phase, but \
         {flood_started} of {METRIC_FLOOD_JOBS} flood jobs had started — 0 means it never \
         got going, {METRIC_FLOOD_JOBS} means the backlog drained and the tail of the \
         phase was measured idle",
    );

    let allowed = (baseline_p95 * 2).max(METRIC_FLOOR);
    assert!(
        loaded_p95 <= allowed,
        "p95 enqueue-to-start on the dedicated-capacity queue was {loaded_p95:?} under flood \
         but the unloaded baseline p95 was {baseline_p95:?} (budget {allowed:?})",
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
