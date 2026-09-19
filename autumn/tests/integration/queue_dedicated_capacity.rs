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
        // Slow: hold the shared slot long enough to demonstrate the flood. The
        // gap between this and the deadline below is the whole detection band,
        // so keep it wide — the backlog is abandoned at shutdown, never drained,
        // so a longer sleep costs no wall clock.
        sleep(Duration::from_millis(3000)).await;
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
    // well before the 3s bulk jobs would free the shared slot.
    let critical_started = timeout(Duration::from_millis(1500), async {
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
// The uncapped control queue: without it, `peak == BULK_CAP` is also what a
// process with only BULK_CAP workers produces, so the test would prove nothing
// about the cap.
static FREE_COMPLETED: AtomicUsize = AtomicUsize::new(0);
// In-flight across BOTH queues, and its high-water mark. This is the control,
// not the uncapped queue's own peak: with equal weights and identical handlers,
// a perfectly valid schedule parks two jobs in each queue forever, so `free`
// alone may never exceed BULK_CAP even while all four slots are busy. What the
// control has to establish is only that the process ran more than BULK_CAP jobs
// at once — so measure exactly that.
static TOTAL_RUNNING: AtomicUsize = AtomicUsize::new(0);
static TOTAL_PEAK: AtomicUsize = AtomicUsize::new(0);

/// Enter a job: bump the shared in-flight count and record the high-water mark.
fn enter_inflight() {
    let total = TOTAL_RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
    TOTAL_PEAK.fetch_max(total, Ordering::SeqCst);
}

fn leave_inflight() {
    TOTAL_RUNNING.fetch_sub(1, Ordering::SeqCst);
}

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
        enter_inflight();
        // Long enough that, uncapped, all four workers would overlap here.
        sleep(Duration::from_millis(250)).await;
        leave_inflight();
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

fn free_handler(
    _state: AppState,
    _payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'static>> {
    Box::pin(async move {
        enter_inflight();
        sleep(Duration::from_millis(250)).await;
        leave_inflight();
        FREE_COMPLETED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

fn free_job_info() -> JobInfo {
    let mut info = JobInfo::new("cap_free", 1, 10, free_handler);
    info.queue = "free".to_string();
    info
}

/// Redis config with 4 workers where `bulk` is capped at 2 concurrent jobs.
fn concurrency_cap_config(url: &str) -> JobConfig {
    JobConfig {
        backend: "redis".to_owned(),
        workers: CAP_WORKERS,
        queues: JobQueuesConfig::weighted_specs(vec![
            JobQueue {
                name: "bulk".to_string(),
                weight: 1,
                concurrency: Some(BULK_CAP),
                reserved: None,
            },
            // Uncapped control: proves the pool really has more than `BULK_CAP`
            // slots, so `bulk`'s peak is the cap doing its job rather than the
            // process simply not having more workers to give.
            JobQueue {
                name: "free".to_string(),
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
    FREE_COMPLETED.store(0, Ordering::SeqCst);
    TOTAL_RUNNING.store(0, Ordering::SeqCst);
    TOTAL_PEAK.store(0, Ordering::SeqCst);

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
        vec![capped_job_info(), free_job_info()],
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
        job::enqueue("cap_free", serde_json::json!({}))
            .await
            .expect("enqueue uncapped control job");
    }

    let drained = timeout(Duration::from_secs(60), async {
        loop {
            if CAPPED_COMPLETED.load(Ordering::SeqCst) >= CAP_JOB_COUNT
                && FREE_COMPLETED.load(Ordering::SeqCst) >= CAP_JOB_COUNT
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        drained.is_ok(),
        "a capped queue must still drain its whole backlog, not deadlock ({} capped and {} \
         uncapped of {CAP_JOB_COUNT} each completed)",
        CAPPED_COMPLETED.load(Ordering::SeqCst),
        FREE_COMPLETED.load(Ordering::SeqCst),
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
        peak >= BULK_CAP,
        "expected the capped queue to use its full allowance of {BULK_CAP}, saw a peak of {peak} \
         — the test would not distinguish a cap from a stalled queue",
    );
    // The control: `peak <= BULK_CAP` alone is also what a process with only
    // BULK_CAP workers would produce. Total in-flight exceeding the cap is what
    // establishes the shared pool really is larger, so `bulk`'s ceiling is the
    // configured cap and not the worker count.
    //
    // Deliberately the *combined* count rather than the uncapped queue's own
    // peak: with equal weights and identical handlers, a schedule that parks two
    // jobs in each queue is entirely valid and leaves `free` at BULK_CAP too —
    // that schedule has four slots busy and proves the point, so asserting on
    // `free` alone would fail a correct run.
    let total_peak = TOTAL_PEAK.load(Ordering::SeqCst);
    assert!(
        total_peak > BULK_CAP,
        "the process never ran more than {total_peak} job(s) at once, so this run never \
         proved it had more than the {BULK_CAP} slots `bulk` is capped at — `bulk`'s peak \
         of {peak} therefore says nothing about the cap",
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

/// Baseline samples discarded before computing the baseline p95. The first
/// samples run while both workers' Redis connections are still establishing and
/// the container is warming, and `p95` over 20 samples is the second-largest —
/// so two cold samples land exactly on the p95 index and inflate the budget.
const METRIC_WARMUP_SAMPLES: usize = 5;

/// How long each flood job holds `bulk`'s single shared slot. This is the signal:
/// with the reservation removed, a `critical` job waits roughly one of these, so
/// the budget below must stay well under it or the test cannot tell a working
/// reservation from a broken one.
const METRIC_FLOOD_SLEEP: Duration = Duration::from_millis(3000);

/// Flood depth. Deep enough that `bulk`'s single shared slot cannot drain it
/// while the loaded phase samples, so the phase really is "one queue fully
/// saturated by slow jobs".
const METRIC_FLOOD_JOBS: usize = 40;

/// Absolute floor on the allowed loaded p95, so the assertion does not become
/// impossibly tight when the unloaded baseline is near zero. Ordinary scheduler
/// and Redis round-trip noise — including the 1s stale-claim maintenance tick
/// sharing this test's current-thread runtime — lives well under this, while it
/// stays far below `METRIC_FLOOD_SLEEP`, so the detection band is wide in both
/// directions.
const METRIC_FLOOR: Duration = Duration::from_millis(500);

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
        sleep(METRIC_FLOOD_SLEEP).await;
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
///
/// Integer arithmetic rather than `(len as f64 * 0.95).ceil() as usize` — the
/// float round trip buys nothing here and costs two lint suppressions.
/// `(n * 95).div_ceil(100)` is the same nearest rank, computed exactly.
fn p95(mut samples: Vec<Duration>) -> Duration {
    assert!(!samples.is_empty(), "no samples");
    samples.sort_unstable();
    let rank = (samples.len() * 95).div_ceil(100);
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

    // Phase 1 — unloaded baseline, after discarding warm-up samples.
    let mut baseline = Vec::with_capacity(METRIC_SAMPLES);
    for i in 0..(METRIC_WARMUP_SAMPLES + METRIC_SAMPLES) {
        let sample = sample_critical_latency().await;
        if i >= METRIC_WARMUP_SAMPLES {
            baseline.push(sample);
        }
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

    let allowed = (baseline_p95 * 2).max(METRIC_FLOOR);
    // A self-calibrating budget can calibrate itself right past the signal: if
    // the baseline is noisy enough that 2x it reaches one flood job, the
    // assertion below would pass even with `reserved` removed. Refuse to draw a
    // conclusion from a measurement that cannot distinguish the two.
    assert!(
        allowed < METRIC_FLOOD_SLEEP,
        "baseline p95 {baseline_p95:?} is too noisy to measure against a \
         {METRIC_FLOOD_SLEEP:?} flood job — a budget of {allowed:?} would pass even with \
         the reservation removed, so this run proves nothing",
    );
    assert!(
        loaded_p95 <= allowed,
        "p95 enqueue-to-start on the dedicated-capacity queue was {loaded_p95:?} under flood \
         but the unloaded baseline p95 was {baseline_p95:?} (budget {allowed:?})",
    );

    // Asserted *after* the metric, deliberately: a broken reservation makes each
    // sample cost about one flood job, which is long enough for the backlog to
    // drain — so checking saturation first would fail here and report the wrong
    // cause for a real regression. `bulk` gets one shared slot, so an undrained
    // backlog means the queue was saturated for the whole sampling window.
    let flood_started = METRIC_FLOOD_RAN.load(Ordering::SeqCst);
    assert!(
        (1..METRIC_FLOOD_JOBS).contains(&flood_started),
        "the flood should have been saturating `bulk` throughout the loaded phase, but \
         {flood_started} of {METRIC_FLOOD_JOBS} flood jobs had started (loaded p95 \
         {loaded_p95:?}, baseline {baseline_p95:?}) — 0 means it never got going, \
         {METRIC_FLOOD_JOBS} means the backlog drained and the tail of the phase was \
         measured idle",
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
