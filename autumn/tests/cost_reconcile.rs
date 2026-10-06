//! Isolated integration test: per-tenant cost reconciles with process CPU
//! (issue #1720).
//!
//! Two tenants send CPU-bound requests, then enqueue CPU-bound jobs. The sum
//! of their metered CPU time must agree with the CPU time that the process
//! used, within 10%. The allocated bytes come from an `allocation-counter`
//! probe.
//!
//! The layers outside the `CostLayer` and the test client also use CPU. The
//! test measures that cost on a route that does nothing, and takes it out of
//! the process CPU. Then a slow (debug or coverage) build does not move the
//! ratio.
//!
//! This is its own binary for two reasons. It reads process-wide CPU time, so
//! no other test can run in the same process. And `allocation-counter`
//! installs a counting global allocator. See CLAUDE.md, isolated tests.
//!
//! Unix only: it reads process CPU time with `getrusage`.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::cost::{AllocationProbe, CostAccountant};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use nix::sys::resource::{UsageWho, getrusage};
use nix::time::{ClockId, clock_gettime};

/// The tolerance from the issue: metered CPU within 10% of process CPU.
const TOLERANCE: f64 = 0.10;

/// Bytes that each request allocates.
const ALLOC_BYTES: usize = 256 * 1024;

struct CountingProbe;

impl AllocationProbe for CountingProbe {
    fn measure(&self, poll: &mut dyn FnMut()) -> u64 {
        allocation_counter::measure(poll).bytes_total
    }
}

fn thread_cpu() -> Duration {
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID)
        .map(Duration::from)
        .expect("thread CPU clock")
}

fn process_cpu() -> Duration {
    let usage = getrusage(UsageWho::RUSAGE_SELF).expect("getrusage");
    let user = usage.user_time();
    let system = usage.system_time();
    let micros = |t: nix::sys::time::TimeVal| {
        u64::try_from(t.tv_sec()).unwrap_or(0) * 1_000_000 + u64::try_from(t.tv_usec()).unwrap_or(0)
    };
    Duration::from_micros(micros(user) + micros(system))
}

/// Spin until this thread used `millis` of CPU, then allocate a buffer.
fn burn(millis: u64) -> usize {
    let target = thread_cpu() + Duration::from_millis(millis);
    let mut acc = 0_u64;
    while thread_cpu() < target {
        for i in 0..1_000_u64 {
            acc = std::hint::black_box(acc.wrapping_mul(31).wrapping_add(i));
        }
    }
    let buffer = std::hint::black_box(vec![1_u8; ALLOC_BYTES]);
    buffer.len() + usize::from(acc == 0)
}

#[get("/heavy")]
async fn heavy() -> String {
    burn(60).to_string()
}

#[get("/light")]
async fn light() -> String {
    burn(20).to_string()
}

#[get("/noop")]
async fn noop() -> &'static str {
    "ok"
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BurnArgs {
    millis: u64,
}

/// A background job that burns CPU. Its cost goes to the tenant that
/// enqueued it.
#[job(name = "cost_reconcile_burn")]
async fn cost_reconcile_burn(_state: AppState, args: BurnArgs) -> AutumnResult<()> {
    std::hint::black_box(burn(args.millis));
    Ok(())
}

#[post("/enqueue-heavy")]
async fn enqueue_heavy() -> AutumnResult<&'static str> {
    CostReconcileBurnJob::enqueue(BurnArgs { millis: 60 }).await?;
    Ok("queued")
}

#[post("/enqueue-light")]
async fn enqueue_light() -> AutumnResult<&'static str> {
    CostReconcileBurnJob::enqueue(BurnArgs { millis: 20 }).await?;
    Ok("queued")
}

/// Requests that each tenant sends.
const ROUNDS: u32 = 10;

#[tokio::test(flavor = "current_thread")]
async fn per_tenant_cost_reconciles_with_process_cpu() {
    let mut config = AutumnConfig::default();
    config.cost.enabled = true;
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);

    let client = TestApp::new()
        .config(config)
        .routes(routes![heavy, light, noop, enqueue_heavy, enqueue_light])
        .jobs(jobs![cost_reconcile_burn])
        .state_initializer(|state| {
            let probe: Arc<dyn AllocationProbe> = Arc::new(CountingProbe);
            state.insert_extension(probe);
        })
        .build();
    let accountant = client
        .state()
        .extension::<CostAccountant>()
        .map(|a| (*a).clone())
        .expect("cost.enabled installs an accountant");

    // Warm up: the first request pays one-time setup that no tenant caused.
    client
        .get("/light")
        .header("x-tenant-id", "warmup")
        .send()
        .await
        .assert_ok();

    // The CPU that a request uses outside the layer: process CPU minus
    // metered CPU, on a route that does nothing.
    let metered_before = accountant.snapshot().total.cpu_micros;
    let before = process_cpu();
    for _ in 0..2 * ROUNDS {
        client
            .get("/noop")
            .header("x-tenant-id", "baseline")
            .send()
            .await
            .assert_ok();
    }
    let noop_process = process_cpu().saturating_sub(before);
    let noop_metered =
        Duration::from_micros(accountant.snapshot().total.cpu_micros - metered_before);
    let outside_layer = noop_process.saturating_sub(noop_metered);

    let metered_before = accountant.snapshot().total.cpu_micros;
    let before = process_cpu();
    for _ in 0..ROUNDS {
        client
            .get("/heavy")
            .header("x-tenant-id", "acme")
            .send()
            .await
            .assert_ok();
        client
            .get("/light")
            .header("x-tenant-id", "globex")
            .send()
            .await
            .assert_ok();
    }
    let process = process_cpu()
        .saturating_sub(before)
        .saturating_sub(outside_layer);

    let snapshot = accountant.snapshot();
    let acme = snapshot.tenants["acme"];
    let globex = snapshot.tenants["globex"];
    let metered = Duration::from_micros(snapshot.total.cpu_micros - metered_before);

    #[allow(clippy::cast_precision_loss)]
    let ratio = metered.as_secs_f64() / process.as_secs_f64();
    println!(
        "metered {metered:?}, process {process:?} (outside the layer: {outside_layer:?}), \
         ratio {ratio:.3}"
    );
    assert!(
        (1.0 - TOLERANCE..=1.0 + TOLERANCE).contains(&ratio),
        "metered CPU {metered:?} must be within 10% of process CPU {process:?} (ratio {ratio:.3})"
    );
    assert_eq!(
        Duration::from_micros(acme.cpu_micros + globex.cpu_micros),
        metered,
        "the tenant totals add up to the metered total"
    );

    // acme does three times the work of globex.
    #[allow(clippy::cast_precision_loss)]
    let share = acme.cpu_micros as f64 / globex.cpu_micros as f64;
    assert!(
        (2.5..=3.5).contains(&share),
        "acme/globex CPU share {share:.2}"
    );

    // Each request allocates its buffer, and the probe sees it.
    let floor = u64::from(ROUNDS) * ALLOC_BYTES as u64;
    assert!(acme.allocated_bytes >= floor, "{acme:?}");
    assert!(globex.allocated_bytes >= floor, "{globex:?}");

    // Slice 2: background jobs. Each tenant enqueues jobs from a request.
    // The job CPU goes to the enqueuing tenant, and the request and job CPU
    // together reconcile with process CPU.
    let metered_before = metered_all(&accountant);
    let before = process_cpu();
    for _ in 0..ROUNDS {
        client
            .post("/enqueue-heavy")
            .header("x-tenant-id", "acme")
            .send()
            .await
            .assert_ok();
        client
            .post("/enqueue-light")
            .header("x-tenant-id", "globex")
            .send()
            .await
            .assert_ok();
    }
    for _ in 0..2_000 {
        if accountant.snapshot().jobs.total.runs >= u64::from(2 * ROUNDS) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let process = process_cpu()
        .saturating_sub(before)
        .saturating_sub(outside_layer);
    let jobs = accountant.snapshot().jobs;
    assert_eq!(jobs.total.runs, u64::from(2 * ROUNDS), "{jobs:?}");
    let metered = Duration::from_micros(metered_all(&accountant) - metered_before);

    #[allow(clippy::cast_precision_loss)]
    let ratio = metered.as_secs_f64() / process.as_secs_f64();
    println!("with jobs: metered {metered:?}, process {process:?}, ratio {ratio:.3}");
    assert!(
        (1.0 - TOLERANCE..=1.0 + TOLERANCE).contains(&ratio),
        "metered CPU {metered:?} with jobs must be within 10% of process CPU {process:?} \
         (ratio {ratio:.3})"
    );

    #[allow(clippy::cast_precision_loss)]
    let share = jobs.tenants["acme"].cpu_micros as f64 / jobs.tenants["globex"].cpu_micros as f64;
    assert!(
        (2.5..=3.5).contains(&share),
        "acme/globex job CPU share {share:.2}"
    );
}

/// Metered CPU of requests and job runs, in microseconds.
fn metered_all(accountant: &CostAccountant) -> u64 {
    let snapshot = accountant.snapshot();
    snapshot.total.cpu_micros + snapshot.jobs.total.cpu_micros
}

/// Success-metric probe (issue #1720): metering adds at most 2% to the p99
/// latency of a request that does 1 ms of CPU work.
///
/// Ignored: wall-clock results depend on the machine. Run it manually:
/// `cargo test -p autumn-web --release --test cost_reconcile -- --ignored`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "timing-sensitive; run manually in release mode"]
async fn metering_adds_at_most_two_percent_to_p99() {
    use autumn_web::middleware::CostLayer;
    use tower::ServiceExt as _;

    /// Requests in one round. The rounds alternate between the two routers,
    /// so drift on the machine hits both.
    const N: usize = 400;
    const ROUNDS: usize = 10;

    async fn handler() -> String {
        burn_micros(1_000).to_string()
    }

    async fn p99_micros(router: &axum::Router) -> u128 {
        let mut samples = Vec::with_capacity(N);
        for _ in 0..N {
            let req = axum::http::Request::builder()
                .uri("/work")
                .body(axum::body::Body::empty())
                .expect("request");
            let start = std::time::Instant::now();
            router.clone().oneshot(req).await.expect("response");
            samples.push(start.elapsed().as_micros());
        }
        samples.sort_unstable();
        samples[N * 99 / 100]
    }

    let bare = axum::Router::new().route("/work", axum::routing::get(handler));
    let metered = bare
        .clone()
        .layer(CostLayer::new(CostAccountant::new(10), false));

    // Warm-up.
    p99_micros(&bare).await;
    p99_micros(&metered).await;
    let mut bare_p99 = Vec::with_capacity(ROUNDS);
    let mut metered_p99 = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        bare_p99.push(p99_micros(&bare).await);
        metered_p99.push(p99_micros(&metered).await);
    }
    bare_p99.sort_unstable();
    metered_p99.sort_unstable();
    let bare = bare_p99[ROUNDS / 2];
    let metered = metered_p99[ROUNDS / 2];

    #[allow(clippy::cast_precision_loss)]
    let overhead = metered as f64 / bare as f64 - 1.0;
    println!(
        "median p99: bare {bare} us, metered {metered} us, overhead {:.2}%",
        overhead * 100.0
    );
    assert!(
        overhead <= 0.02,
        "metering adds {:.2}% to p99, more than 2%",
        overhead * 100.0
    );
}

/// Spin until this thread used `micros` of CPU.
fn burn_micros(micros: u64) -> u64 {
    let target = thread_cpu() + Duration::from_micros(micros);
    let mut acc = 0_u64;
    while thread_cpu() < target {
        for i in 0..100_u64 {
            acc = std::hint::black_box(acc.wrapping_mul(31).wrapping_add(i));
        }
    }
    acc
}
