//! Issue #3068: when upstream latency rises 5×, the adaptive limit goes down
//! within a bounded time, and the p99 latency of admitted requests stays
//! bounded.
//!
//! The upstream is a model with fixed capacity: `UPSTREAM_SLOTS` requests at
//! a time, each for `service` time. Excess requests wait in a FIFO queue, so
//! latency grows with the queue. Load is open-loop: one request every 4 ms
//! (250 rps). At first `service` is 10 ms (capacity 1000 rps). At
//! `STEP` it becomes 50 ms (capacity 200 rps), which is overload.
//!
//! A static ceiling above the capacity lets the queue grow to the ceiling,
//! and latency with it. An adaptive limit goes down to near the capacity and
//! sheds the excess.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::config::{AdmissionAlgorithm, AdmissionMode, AutumnConfig};
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use axum::http::StatusCode;
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// Upstream concurrency.
const UPSTREAM_SLOTS: usize = 10;
/// One request every 4 ms.
const ARRIVAL: Duration = Duration::from_millis(4);
/// When the upstream slows down.
const STEP: Duration = Duration::from_secs(10);
/// When the load stops.
const END: Duration = Duration::from_secs(40);
/// The adaptive limit must be at or below `CONVERGED_LIMIT` within this time
/// after `STEP`, and stay there.
const CONVERGE_WITHIN: Duration = Duration::from_secs(20);
const CONVERGED_LIMIT: u64 = 4 * UPSTREAM_SLOTS as u64;
/// The adaptive limit at start: 5× the upstream concurrency.
const INITIAL_LIMIT: usize = 5 * UPSTREAM_SLOTS;
/// The bound on admitted-request p99 latency after convergence: 3× the
/// slow service time. A static ceiling of `INITIAL_LIMIT` gives about 5×.
const P99_BOUND: Duration = Duration::from_millis(150);

/// The upstream model.
struct Upstream {
    slots: Semaphore,
    service_ms: AtomicU64,
}

thread_local! {
    /// Each `#[sim_test]` runs on its own thread and current-thread runtime,
    /// so a thread-local upstream is private to one test.
    static UPSTREAM: RefCell<Option<Arc<Upstream>>> = const { RefCell::new(None) };
}

/// A fresh upstream for this test's thread.
fn reset_upstream() {
    UPSTREAM.with(|u| *u.borrow_mut() = None);
}

fn upstream() -> Arc<Upstream> {
    UPSTREAM.with(|u| {
        Arc::clone(u.borrow_mut().get_or_insert_with(|| {
            Arc::new(Upstream {
                slots: Semaphore::new(UPSTREAM_SLOTS),
                service_ms: AtomicU64::new(10),
            })
        }))
    })
}

#[get("/work")]
async fn work() -> &'static str {
    let upstream = upstream();
    let _slot = upstream.slots.acquire().await.expect("semaphore open");
    let service = upstream.service_ms.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(service)).await;
    "done"
}

fn config(mode: AdmissionMode, algorithm: AdmissionAlgorithm, ceiling: usize) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.server.max_concurrent_requests = Some(ceiling);
    config.server.admission.mode = mode;
    config.server.admission.algorithm = algorithm;
    config.server.admission.min_limit = 8;
    // Start well above the upstream capacity, so the limit must come down.
    config.server.admission.initial_limit = INITIAL_LIMIT;
    config
}

/// One finished request.
struct Done {
    at: Duration,
    latency: Duration,
    status: StatusCode,
}

/// What the run saw.
struct Run {
    done: Vec<Done>,
    /// `(time, limit)` once per 100 ms.
    limits: Vec<(Duration, u64)>,
}

impl Run {
    /// p99 latency of admitted (`200`) requests that ended in `[from, to)`.
    fn admitted_p99(&self, from: Duration, to: Duration) -> Duration {
        let mut latencies: Vec<Duration> = self
            .done
            .iter()
            .filter(|d| d.status == StatusCode::OK && d.at >= from && d.at < to)
            .map(|d| d.latency)
            .collect();
        assert!(
            !latencies.is_empty(),
            "no admitted requests in [{from:?}, {to:?})"
        );
        latencies.sort();
        let index = (latencies.len() * 99).div_ceil(100).saturating_sub(1);
        latencies[index]
    }

    /// The highest limit seen in `[from, to)`.
    fn max_limit(&self, from: Duration, to: Duration) -> u64 {
        self.limits
            .iter()
            .filter(|(at, _)| *at >= from && *at < to)
            .map(|(_, l)| *l)
            .max()
            .unwrap_or(0)
    }

    fn shed(&self) -> usize {
        self.done
            .iter()
            .filter(|d| d.status == StatusCode::SERVICE_UNAVAILABLE)
            .count()
    }
}

/// Drive open-loop load through the app and record what happens.
async fn drive(sim: &Sim) -> Run {
    reset_upstream();
    let client = sim.client();
    let start = Instant::now();
    let mut pending = FuturesUnordered::new();
    let mut done = Vec::new();
    let mut limits = Vec::new();
    let mut next_arrival = start;
    let mut next_probe = start;
    loop {
        let now = Instant::now();
        let elapsed = now.duration_since(start);
        if elapsed >= STEP {
            upstream().service_ms.store(50, Ordering::SeqCst);
        }
        if elapsed >= END && pending.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            Some(d) = pending.next(), if !pending.is_empty() => done.push(d),
            () = tokio::time::sleep_until(next_probe), if elapsed < END => {
                let limit = client.state().metrics().snapshot().http.admission.limit;
                limits.push((next_probe.duration_since(start), limit));
                next_probe += Duration::from_millis(100);
            }
            () = tokio::time::sleep_until(next_arrival), if elapsed < END => {
                let sent = Instant::now();
                pending.push(async move {
                    let response = client.get("/work").send().await;
                    let end = Instant::now();
                    Done {
                        at: end.duration_since(start),
                        latency: end.duration_since(sent),
                        status: response.status,
                    }
                });
                next_arrival += ARRIVAL;
            }
        }
    }
    Run { done, limits }
}

/// The shared assertions for an adaptive algorithm.
fn assert_converges_and_bounds_p99(run: &Run, name: &str, seed: u64) {
    // Before the step: no shedding pressure, fast responses.
    let p99_before = run.admitted_p99(Duration::from_secs(2), STEP);
    assert!(
        p99_before <= Duration::from_millis(20),
        "{name}: p99 before the step was {p99_before:?} (seed={seed:#x})"
    );
    // The limit converges downward within CONVERGE_WITHIN and stays down.
    let settled = STEP + CONVERGE_WITHIN;
    let after = run.max_limit(settled, END);
    assert!(
        after <= CONVERGED_LIMIT,
        "{name}: limit was {after} after {settled:?}, want <= {CONVERGED_LIMIT} (seed={seed:#x})"
    );
    assert!(
        after < run.max_limit(Duration::ZERO, STEP),
        "{name}: the limit must go down"
    );
    // Admitted-request p99 stays bounded.
    let p99_after = run.admitted_p99(settled, END);
    println!(
        "{name}: limit before {}, after {after}; admitted p99 before {p99_before:?}, \
         after {p99_after:?}; shed {}",
        run.max_limit(Duration::ZERO, STEP),
        run.shed()
    );
    assert!(
        p99_after <= P99_BOUND,
        "{name}: admitted p99 was {p99_after:?} after {settled:?}, want <= {P99_BOUND:?} \
         (seed={seed:#x})"
    );
    assert!(run.shed() > 0, "{name}: overload must shed some requests");
}

#[sim_test]
async fn sim_adaptive_gradient2_converges_down_when_upstream_latency_rises_5x(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![work]).config(config(
        AdmissionMode::Adaptive,
        AdmissionAlgorithm::Gradient2,
        1000,
    )));
    let run = drive(&sim).await;
    assert_converges_and_bounds_p99(&run, "gradient2", sim.seed);
}

#[sim_test]
async fn sim_adaptive_vegas_converges_down_when_upstream_latency_rises_5x(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![work]).config(config(
        AdmissionMode::Adaptive,
        AdmissionAlgorithm::Vegas,
        1000,
    )));
    let run = drive(&sim).await;
    assert_converges_and_bounds_p99(&run, "vegas", sim.seed);
}

/// The contrast: a static ceiling at the adaptive start value
/// (`INITIAL_LIMIT`) does not meet the bound. Thus the adaptation, not the
/// ceiling, bounds the latency.
#[sim_test]
async fn sim_adaptive_static_ceiling_does_not_bound_latency(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![work]).config(config(
        AdmissionMode::Static,
        AdmissionAlgorithm::Gradient2,
        INITIAL_LIMIT,
    )));
    let run = drive(&sim).await;
    let p99 = run.admitted_p99(STEP + CONVERGE_WITHIN, END);
    println!("static {INITIAL_LIMIT}: admitted p99 after the step {p99:?}");
    assert!(
        p99 > P99_BOUND,
        "a static ceiling of {INITIAL_LIMIT} should exceed {P99_BOUND:?}, got p99 {p99:?} \
         (seed={:#x})",
        sim.seed
    );
}
