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
//! ever establishes one before a handler runs. `TestApp::perform_enqueued_jobs`
//! (`autumn/src/test.rs`) documents that it "invokes each job's registered
//! handler directly," matching the in-process worker's own dispatch path, and
//! this test proves that direct invocation carries no `CURRENT_TENANT` scope:
//! a job enqueued from a request scoped to `tenant-a-sentinel` observes
//! `CURRENT_TENANT` as `None` when it actually runs.
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
/// can assert on it after `perform_enqueued_jobs` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    /// The job has not run yet.
    NotRun,
    /// The job ran and saw no ambient tenant — the expected, safe outcome.
    NoTenant,
    /// The job ran and observed the enqueuing request's tenant leak in.
    Leaked(String),
}

static PROBED_TENANT: std::sync::Mutex<Observation> = std::sync::Mutex::new(Observation::NotRun);

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

#[post("/enqueue-probe")]
async fn enqueue_probe() -> &'static str {
    TenantLeakProbeJob::enqueue(ProbeArgs {}).await.unwrap();
    "queued"
}

#[tokio::test]
async fn job_dispatch_carries_no_ambient_tenant() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    *PROBED_TENANT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Observation::NotRun;

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

    let report = client.perform_enqueued_jobs().await;
    // Fails loudly (naming the leaked tenant id) if CURRENT_TENANT ever
    // propagates from the enqueuing request into job dispatch.
    report.assert_all_succeeded();

    assert_eq!(
        *PROBED_TENANT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Observation::NoTenant,
        "job handler must observe no ambient tenant, never tenant-a-sentinel"
    );

    job::clear_global_job_client();
}
