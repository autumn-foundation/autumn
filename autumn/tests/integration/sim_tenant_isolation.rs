//! Issue #3072, AC1: one tenant that floods requests and jobs does not raise
//! the p99 latency of other tenants beyond a bound.
//!
//! The app has one shared upstream: `UPSTREAM_SLOTS` requests at a time,
//! `SERVICE` each, then a FIFO queue. The `noisy` tenant sends more requests
//! than the upstream can serve and enqueues a burst of slow jobs. Three quiet
//! tenants send a few requests and jobs.
//!
//! With `tenancy.max_concurrent_requests` and `[jobs.tenants]`, the quiet
//! tenants' p99 stays near the service time. Without them, the quiet
//! tenants wait behind the noisy tenant's queue (the contrast test).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::job;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use axum::http::StatusCode;
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// Upstream concurrency.
const UPSTREAM_SLOTS: usize = 8;
/// Upstream service time for one request.
const SERVICE: Duration = Duration::from_millis(10);
/// The noisy tenant sends one request each `NOISY_ARRIVAL` (1000 rps, which
/// is 1.25x the upstream capacity of 800 rps).
const NOISY_ARRIVAL: Duration = Duration::from_millis(1);
/// Each quiet tenant sends one request each `QUIET_ARRIVAL`.
const QUIET_ARRIVAL: Duration = Duration::from_millis(50);
/// Each quiet tenant enqueues one job each `QUIET_JOB_EVERY` requests.
const QUIET_JOB_EVERY: u32 = 5;
const QUIET_TENANTS: [&str; 3] = ["quiet-a", "quiet-b", "quiet-c"];
/// How long the load runs.
const END: Duration = Duration::from_secs(3);
/// The noisy tenant enqueues this many jobs at the start: more than the
/// workers can run before `END`.
const NOISY_JOBS: usize = 300;
/// One job's run time.
const JOB_RUN: Duration = Duration::from_millis(50);
/// Job workers.
const WORKERS: usize = 4;
/// The bound on the quiet tenants' request p99: 3x the service time.
const REQUEST_P99_BOUND: Duration = Duration::from_millis(30);
/// The bound on the quiet tenants' job p99 (enqueue to finish): 3x the run
/// time.
const JOB_P99_BOUND: Duration = Duration::from_millis(150);

struct Upstream {
    slots: Semaphore,
}

/// One job: its tenant, when it was enqueued, and when it finished.
type JobRecord = (String, Instant, Option<Instant>);

thread_local! {
    /// Each `#[sim_test]` has its own thread, so this state is per test.
    static UPSTREAM: RefCell<Option<Arc<Upstream>>> = const { RefCell::new(None) };
    /// Job id -> (tenant, enqueued at, finished at).
    static JOBS: RefCell<HashMap<u64, JobRecord>> = RefCell::new(HashMap::new());
}

fn reset() {
    UPSTREAM.with(|u| *u.borrow_mut() = None);
    JOBS.with(|j| j.borrow_mut().clear());
}

fn upstream() -> Arc<Upstream> {
    UPSTREAM.with(|u| {
        Arc::clone(u.borrow_mut().get_or_insert_with(|| {
            Arc::new(Upstream {
                slots: Semaphore::new(UPSTREAM_SLOTS),
            })
        }))
    })
}

#[get("/work")]
async fn work() -> &'static str {
    let upstream = upstream();
    let _slot = upstream.slots.acquire().await.expect("semaphore open");
    tokio::time::sleep(SERVICE).await;
    "done"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkArgs {
    id: u64,
}

#[job(name = "sim_tenant_isolation_work")]
async fn sim_tenant_isolation_work(_state: AppState, args: WorkArgs) -> AutumnResult<()> {
    tokio::time::sleep(JOB_RUN).await;
    JOBS.with(|j| {
        if let Some(entry) = j.borrow_mut().get_mut(&args.id) {
            entry.2 = Some(Instant::now());
        }
    });
    Ok(())
}

async fn enqueue_for(tenant: &str, id: u64) {
    JOBS.with(|j| {
        j.borrow_mut()
            .insert(id, (tenant.to_owned(), Instant::now(), None));
    });
    autumn_web::tenancy::with_tenant(tenant.to_owned(), async {
        SimTenantIsolationWorkJob::enqueue(WorkArgs { id })
            .await
            .expect("enqueue");
    })
    .await;
}

fn config(isolated: bool) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.server.max_concurrent_requests = Some(400);
    config.tenancy.enabled = true;
    config.tenancy.source = "header".into();
    config.tenancy.header_name = "x-tenant-id".into();
    config.jobs.workers = WORKERS;
    if isolated {
        config.tenancy.max_concurrent_requests = 4;
        config.jobs.tenants.max_concurrent = 2;
        config.jobs.tenants.lanes = 4;
        config.jobs.tenants.lanes_per_tenant = 2;
    }
    config
}

struct Done {
    tenant: &'static str,
    latency: Duration,
    status: StatusCode,
}

/// What the quiet tenants saw.
struct Run {
    requests: Vec<Done>,
    /// Quiet-tenant job latencies, enqueue to finish.
    quiet_jobs: Vec<Duration>,
    noisy_shed: usize,
}

fn p99(mut latencies: Vec<Duration>) -> Duration {
    assert!(!latencies.is_empty(), "no samples");
    latencies.sort();
    let index = (latencies.len() * 99).div_ceil(100).saturating_sub(1);
    latencies[index]
}

impl Run {
    /// The p99 of quiet requests. A shed quiet request counts as
    /// `Duration::MAX`: a `503` is not isolation.
    fn quiet_request_p99(&self) -> Duration {
        p99(self
            .requests
            .iter()
            .filter(|d| d.tenant != "noisy")
            .map(|d| {
                if d.status == StatusCode::OK {
                    d.latency
                } else {
                    Duration::MAX
                }
            })
            .collect())
    }

    fn quiet_job_p99(&self) -> Duration {
        p99(self.quiet_jobs.clone())
    }
}

async fn drive(sim: &Sim) -> Run {
    reset();
    let client = sim.client();
    let start = Instant::now();

    let mut next_id = 0_u64;
    for _ in 0..NOISY_JOBS {
        enqueue_for("noisy", next_id).await;
        next_id += 1;
    }

    let mut pending = FuturesUnordered::new();
    let mut requests = Vec::new();
    let mut next_noisy = start;
    let mut next_quiet = start;
    let mut quiet_ticks = 0_u32;
    loop {
        let elapsed = Instant::now().duration_since(start);
        if elapsed >= END && pending.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            Some(d) = pending.next(), if !pending.is_empty() => requests.push(d),
            () = tokio::time::sleep_until(next_quiet), if elapsed < END => {
                for tenant in QUIET_TENANTS {
                    if quiet_ticks.is_multiple_of(QUIET_JOB_EVERY) {
                        enqueue_for(tenant, next_id).await;
                        next_id += 1;
                    }
                    pending.push(send(client, tenant));
                }
                quiet_ticks += 1;
                next_quiet += QUIET_ARRIVAL;
            }
            () = tokio::time::sleep_until(next_noisy), if elapsed < END => {
                pending.push(send(client, "noisy"));
                next_noisy += NOISY_ARRIVAL;
            }
        }
    }
    // Let the jobs that are left finish. A plain sleep lets the paused
    // runtime fire each job's timer in turn; one `advance` jump would not.
    tokio::time::sleep(Duration::from_secs(60)).await;

    let quiet_jobs = JOBS.with(|j| {
        j.borrow()
            .values()
            .filter(|(tenant, _, _)| tenant != "noisy")
            .map(|(tenant, enqueued, finished)| {
                finished
                    .unwrap_or_else(|| panic!("a job of {tenant} did not finish"))
                    .duration_since(*enqueued)
            })
            .collect()
    });
    let noisy_shed = requests
        .iter()
        .filter(|d| d.tenant == "noisy" && d.status == StatusCode::SERVICE_UNAVAILABLE)
        .count();
    Run {
        requests,
        quiet_jobs,
        noisy_shed,
    }
}

async fn send(client: &autumn_web::test::TestClient, tenant: &'static str) -> Done {
    let sent = Instant::now();
    let response = client
        .get("/work")
        .header("x-tenant-id", tenant)
        .send()
        .await;
    Done {
        tenant,
        latency: Instant::now().duration_since(sent),
        status: response.status,
    }
}

fn app(isolated: bool) -> TestApp {
    TestApp::new()
        .routes(routes![work])
        .jobs(jobs![sim_tenant_isolation_work])
        .config(config(isolated))
}

#[sim_test]
async fn sim_noisy_tenant_does_not_raise_quiet_tenants_p99(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    sim.build(app(true));
    let run = drive(&sim).await;

    let request_p99 = run.quiet_request_p99();
    let job_p99 = run.quiet_job_p99();
    println!(
        "isolated: quiet request p99 {request_p99:?}, quiet job p99 {job_p99:?}, \
         noisy shed {}",
        run.noisy_shed
    );
    assert!(
        request_p99 <= REQUEST_P99_BOUND,
        "quiet request p99 {request_p99:?} > {REQUEST_P99_BOUND:?} (seed={:#x})",
        sim.seed
    );
    assert!(
        job_p99 <= JOB_P99_BOUND,
        "quiet job p99 {job_p99:?} > {JOB_P99_BOUND:?} (seed={:#x})",
        sim.seed
    );
    assert!(run.noisy_shed > 0, "the noisy tenant must hit its bulkhead");
}

/// The contrast: without the bulkheads, the same load breaks both bounds.
/// Thus the bulkheads, not the load shape, keep the quiet p99 low.
#[sim_test]
async fn sim_without_bulkheads_noisy_tenant_raises_quiet_p99(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    sim.build(app(false));
    let run = drive(&sim).await;

    let request_p99 = run.quiet_request_p99();
    let job_p99 = run.quiet_job_p99();
    println!("shared: quiet request p99 {request_p99:?}, quiet job p99 {job_p99:?}");
    assert!(
        request_p99 > REQUEST_P99_BOUND,
        "without a bulkhead, quiet request p99 {request_p99:?} should exceed \
         {REQUEST_P99_BOUND:?} (seed={:#x})",
        sim.seed
    );
    assert!(
        job_p99 > JOB_P99_BOUND,
        "without tenant job slots, quiet job p99 {job_p99:?} should exceed \
         {JOB_P99_BOUND:?} (seed={:#x})",
        sim.seed
    );
}

/// A tenant whose lane is not the first idle worker's lane still runs at
/// once. A push wakes every idle worker, not only one.
#[sim_test]
async fn sim_a_lone_job_runs_on_whichever_lane_serves_its_tenant(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset();
    let mut config = config(false);
    config.jobs.workers = 2;
    config.jobs.tenants.lanes = 2;
    config.jobs.tenants.lanes_per_tenant = 1;
    sim.build(
        TestApp::new()
            .jobs(jobs![sim_tenant_isolation_work])
            .config(config),
    );
    // Let both workers go idle.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // One tenant on each lane, each enqueued alone into an idle runtime.
    let on_lane = |lane: u16| {
        (0..1000)
            .map(|i| format!("lane-tenant-{i}"))
            .find(|t| autumn_web::bulkhead::shuffle_shard(t, 2, 1) == [lane])
            .expect("a tenant on each lane")
    };
    for (id, lane) in [(0_u64, 0_u16), (1, 1)] {
        let tenant = on_lane(lane);
        enqueue_for(&tenant, id).await;
        tokio::time::sleep(JOB_RUN * 2).await;
        let finished = JOBS.with(|j| j.borrow().get(&id).and_then(|e| e.2));
        assert!(
            finished.is_some(),
            "the job of {tenant} (lane {lane}) did not run (seed={:#x})",
            sim.seed
        );
    }
}
