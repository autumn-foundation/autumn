//! Does a background job inherit the enqueuing request's tenant, or run with
//! none at all? (Warden 2026-09-16, negative result)
//!
//! Ranked attack surface #3 ("Tenancy and sharding") in Warden's charter asks
//! this exactly: "does a job inherit the enqueuing request's tenant or the
//! executing worker's?" A plausible failure mode: `#[job(...)]::enqueue(...)`
//! is called from inside a request that `tenancy_middleware` has already
//! scoped with `CURRENT_TENANT.scope(Some(tenant_id), ...)`
//! (`autumn/src/tenancy.rs`), and if the job runtime dispatched the handler
//! from *within* that same future tree (e.g. `tokio::spawn`ing the handler
//! directly off the request's task), the handler would inherit the
//! enqueuing tenant as ambient state — and, worse, a *worker* draining a
//! multi-tenant queue could inherit whichever tenant happened to scope the
//! thread that last polled it, misattributing one tenant's job to another's
//! `#[repository(tenant_scoped)]` reads and writes.
//!
//! It does not happen. `autumn/src/job.rs`, `autumn/src/scheduler.rs` and
//! `autumn-macros/src/job.rs` contain zero references to `CURRENT_TENANT` or
//! `tenancy` (`grep -ni tenant` across all three returns nothing): nothing in
//! the job runtime ever reads the enqueuing request's tenant, and nothing
//! ever establishes one before a handler runs.
//!
//! Two independent proofs, run in sequence rather than concurrently:
//!
//! 1. `TestApp::build` starts the in-process worker by default, and it drains
//!    and runs every enqueued job on its own — this is the real production
//!    dispatch path. `job_dispatch_carries_no_ambient_tenant` waits for that
//!    worker to actually run the probe job (`wait_until`, mirroring
//!    `job_recorder_integration.rs`'s own pattern) and asserts what it
//!    observed.
//! 2. `TestApp::perform_enqueued_jobs()` *separately* invokes the same
//!    registered handler again (it drains its own recorder of enqueue calls,
//!    independent of the real queue the worker already consumed), matching
//!    the framework's documented tool for asserting job-execution outcomes
//!    synchronously.
//!
//! A prior version of this test called only `perform_enqueued_jobs()` and
//! asserted a single shared static's final value. Since the in-process worker
//! *also* runs the same job concurrently and writes to the same place, that
//! was racy in a way that could mask a real leak: if the worker's dispatch
//! path leaked a tenant but `perform_enqueued_jobs()`'s direct-invocation
//! path (the *only* path this file actually exercised) did not, the clean
//! second write could silently overwrite the leaked first one before the
//! assertion ever read it — passing green while proving nothing about the
//! real worker path a production deployment actually uses. Fixed by phase 1
//! above: waiting for and asserting the worker's own run first, before phase
//! 2 resets and exercises `perform_enqueued_jobs()` on a fresh observation.
//!
//! This is the safe half of a two-part guarantee. The other half — that a
//! `#[repository(tenant_scoped)]` derived query run with no tenant context
//! fails closed with "no tenant context was established," rather than
//! silently reading unscoped or across tenants — is already proven by
//! `autumn/tests/integration/tenancy.rs::test_unscoped_query_without_context_fails`.
//! Together the two tests close the loop this file's hypothesis opened: a job
//! handler that touches a tenant-scoped repository without itself calling
//! `autumn_web::tenancy::with_tenant(..)` on a tenant id it threaded through
//! its own job args gets a loud `AutumnError`, never another tenant's rows.
//! Recording this as a regression gate: a future change that makes the job
//! runtime "convenient" by ambiently propagating the enqueuing tenant would
//! reintroduce exactly the misattribution this test rules out today.

use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::job;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::tenancy::CURRENT_TENANT;
use autumn_web::test::TestApp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ProbeArgs {}

/// What the job handler actually observed `CURRENT_TENANT` to be, so the test
/// can assert on it after a dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    /// The job has not run yet (since the observation was last reset).
    NotRun,
    /// The job ran and saw no ambient tenant — the expected, safe outcome.
    NoTenant,
    /// The job ran and observed the enqueuing request's tenant leak in.
    Leaked(String),
}

static PROBED_TENANT: std::sync::Mutex<Observation> = std::sync::Mutex::new(Observation::NotRun);

fn reset_probe() {
    *PROBED_TENANT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Observation::NotRun;
}

fn probed() -> Observation {
    PROBED_TENANT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[job(name = "tenant_leak_probe", max_attempts = 1, backoff_ms = 1)]
async fn tenant_leak_probe(_state: AppState, _args: ProbeArgs) -> AutumnResult<()> {
    let observed = CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
    let recorded = observed
        .clone()
        .map_or(Observation::NoTenant, Observation::Leaked);
    *PROBED_TENANT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = recorded;
    observed.map_or(Ok(()), |leaked| {
        Err(AutumnError::internal_server_error_msg(format!(
            "job dispatch observed ambient tenant {leaked:?}; CURRENT_TENANT leaked from \
             the enqueuing request into job execution"
        )))
    })
}

struct ProbeJobPlugin;

impl Plugin for ProbeJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![tenant_leak_probe])
    }
}

fn tenancy_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config
}

/// Requires the tenant extractor, not just a bare handler: if
/// `tenancy_middleware` ever stopped applying to this route (a config
/// regression, or the route becoming exempt), extraction itself would fail
/// closed rather than let the enqueue happen with no tenant context — which
/// would make this whole test's precondition silently vacuous (the job would
/// then correctly observe `NoTenant`, but only because there never was one).
#[post("/enqueue-probe")]
async fn enqueue_probe(tenant: autumn_web::tenancy::Tenant) -> AutumnResult<&'static str> {
    if tenant.0 != "tenant-a-sentinel" {
        return Err(AutumnError::internal_server_error_msg(format!(
            "enqueue route resolved tenant {:?}, not the expected tenant-a-sentinel; \
             this test's precondition (enqueuing under an established tenant context) \
             did not hold",
            tenant.0
        )));
    }
    TenantLeakProbeJob::enqueue(ProbeArgs {}).await.unwrap();
    Ok("queued")
}

/// Poll until `f` returns true or ~2s elapse, yielding to let the in-process
/// worker drain (mirrors `job_recorder_integration.rs`'s own helper).
async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition was not met in time");
}

#[tokio::test]
async fn job_dispatch_carries_no_ambient_tenant() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe();

    let client = TestApp::new()
        .config(tenancy_config())
        .plugin(ProbeJobPlugin)
        .routes(routes![enqueue_probe])
        .build();

    client
        .post("/enqueue-probe")
        .header("x-tenant-id", "tenant-a-sentinel")
        .send()
        .await
        .assert_ok();
    client.assert_job_enqueued("tenant_leak_probe");

    // Phase 1: the real production dispatch path. `TestApp::build` starts an
    // in-process worker that drains and runs the queue on its own; wait for
    // it to actually execute the probe rather than assuming timing.
    wait_until(|| probed() != Observation::NotRun).await;
    assert_eq!(
        probed(),
        Observation::NoTenant,
        "the in-process worker's own dispatch must not observe an ambient tenant"
    );

    // Phase 2: `perform_enqueued_jobs()` invokes the same registered handler
    // again, through its own recorder — a second, independent proof using the
    // framework's documented synchronous test helper. Reset first so this
    // phase's result cannot be confused with phase 1's (the two dispatches
    // are otherwise indistinguishable in the shared `PROBED_TENANT` slot).
    reset_probe();
    let report = client.perform_enqueued_jobs().await;
    // Fails loudly (naming the leaked tenant id) if CURRENT_TENANT ever
    // propagates from the enqueuing request into this dispatch path.
    report.assert_all_succeeded();
    assert_eq!(
        probed(),
        Observation::NoTenant,
        "perform_enqueued_jobs()'s direct handler invocation must not observe an ambient tenant"
    );

    job::clear_global_job_client();
}
