//! Authored, seeded fault scenarios (`FaultPlan`, issue #1680) — the DB-free
//! job lane.
//!
//! `Chaos` (issue #1797, W5) injects faults *probabilistically*: "fail roughly
//! one checkout in ten". That is the right shape for a sweep, and the wrong
//! shape for a regression test — "roughly" cannot express "the bug reproduces
//! when the **second** attempt of `charge_card` fails", which is what a
//! regression test for a retry bug has to say. [`FaultPlan`] is the authored
//! counterpart: a plan names the exact effect *ordinals* that must fail, so a
//! scenario is a fixed schedule rather than a distribution, and a run of it
//! produces a structured, serializable [`FaultOutcome`] a test can assert on
//! field by field.
//!
//! This module is the job-effect half of the acceptance criteria; the database
//! half lives in `autumn/tests/sim_fault_plan_db.rs` (sqlite substrate,
//! Docker-free) and `autumn/tests/integration/sim_fault_plan_pg.rs` (Postgres).
//! What each test here proves:
//!
//! | Test | Criterion |
//! |---|---|
//! | `a_test_app_without_a_plan_is_unchanged` | AC1/AC5 control — an app with no plan attached behaves exactly as before, and the worked example's charge is only lost *because* of the plan |
//! | `fail_job_execution_fires_on_exactly_the_target_ordinal` | AC2 — the job-execution effect class fails deterministically, targetable by ordinal |
//! | `fail_job_targets_a_named_job_by_its_own_ordinal` | AC2 — per-target ordinals are counted per job name, not globally |
//! | `fault_plan_composes_with_a_user_job_interceptor` | AC1 — the plan drives faults through the existing `interceptor.rs` traits, composing with (never clobbering) a user interceptor and the always-on job recorder |
//! | `fault_plan_composes_with_an_active_chaos_lane` | AC1 — an authored plan and a probabilistic `Chaos` lane coexist; neither clobbers the other's interceptor or clock |
//! | `fault_timing_is_gated_by_the_injected_clock` | AC3 — the fault window is read from the app's injected `ClockSource`, so no wall-clock read leaks in |
//! | `outcome_is_serializable_and_round_trips` | AC4 — the run produces a structured, serializable outcome record |
//! | `same_seed_replays_a_byte_identical_outcome_100_times` | the issue's success metric — 100 consecutive replays of one seed, byte-identical |
//! | `worked_example_charge_card_fails_before_the_fix_and_passes_after` | AC5 — one scenario failing before a fix and passing after |
//! | `a_multi_worker_config_is_rejected_at_build_time` | the determinism guards in `TestApp::build` refuse a config that would make ordinals unreplayable |
//! | `a_sampled_reporting_config_is_rejected_at_build_time` | ditto, for the sampler that would randomly drop 5xx out of the outcome |
//! | `a_failure_capture_config_is_rejected_at_build_time` | ditto, for capsule persistence that reporting awaits before any reporter runs |
//!
//! Every test that drives jobs runs under the process-global job runtime lock
//! (`job::global_job_runtime_test_lock`) and starts from a cleared global job
//! client, exactly like `sim_retry_storm.rs` — the `#[job]` enqueue path is
//! process-global, so two job-backed tests running concurrently would otherwise
//! interleave their executions and destroy the ordinal determinism these tests
//! exist to prove. For the same reason ordinal determinism is only claimed
//! under the sim's paused, single-threaded runtime with one worker.
//!
//! The lock is actually broader than "drives jobs": `sim.build`/`TestApp::build`
//! always runs `initialize_job_runtime`, which unconditionally clears the
//! process-global job client before deciding whether this app has any jobs to
//! (re)install. So even a job-less test's `sim.build` clears state a
//! concurrently-running, job-driving test depends on — `a_stalled_user_reporter_
//! does_not_starve_the_fault_projection` builds no jobs but still takes the lock
//! for exactly this reason (a gap here surfaced as a spurious "job runtime is
//! not initialized" failure in whichever job-driving test happened to be
//! mid-flight).

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::job;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
#[cfg(feature = "reporting")]
use autumn_web::reporting::{ErrorEvent, ErrorReporter, ReportFuture};
use autumn_web::sim::{Chaos, FaultEffect, FaultOutcome, FaultPlan, Sim};
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// The seed every authored scenario in this module is built from. Explicit
/// ordinals do not consume it; `random_job_execution_faults` does, which is
/// what the 100× replay test below pins down.
const SEED: u64 = 0x5EED;

/// How far virtual time is stepped between drain passes so a retry whose
/// jittered backoff is drawn from `backoff_ms = 10` comes due. Generous by two
/// orders of magnitude: the point is to release the timer, not to measure it.
const RETRY_WINDOW: Duration = Duration::from_millis(100);

/// The fixed simulation epoch every `Sim` clock starts at (`autumn/src/sim.rs`,
/// `SIM_EPOCH_UNIX_SECS`). `FiredFault::at` is read from that injected clock,
/// so a fault fired before any [`Sim::advance`] is stamped exactly here.
fn sim_epoch() -> DateTime<Utc> {
    Utc.timestamp_opt(1_577_836_800, 0)
        .single()
        .expect("the sim epoch is a valid UTC instant")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeArgs {
    id: u32,
}

/// Ids the `fault_plan_probe` handler actually ran for, in execution order.
///
/// A fired fault drops the handler future without polling it (the fault
/// interceptor sits innermost), so an id missing here is an execution the plan
/// suppressed — the handler-side witness that the injected failure is real.
static PROBE_RUNS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Number of `fault_plan_other` handler runs — the second job name used to
/// prove per-name ordinal counting.
static OTHER_RUNS: AtomicU32 = AtomicU32::new(0);

/// Ids the worked example's charge handlers committed, in execution order.
static CHARGED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Execute-seam passes seen by the *user-supplied* job interceptor.
static USER_EXECUTES: AtomicU32 = AtomicU32::new(0);

/// Execute-seam passes the user-supplied interceptor saw return `Err` — the
/// injected failure must reach it looking like an ordinary handler error.
static USER_OBSERVED_ERRORS: AtomicU32 = AtomicU32::new(0);

/// Clear every shared probe witness so repeated runs (a sweep, the 100×
/// replay loop) each start from a known state.
fn reset_probe_state() {
    PROBE_RUNS
        .lock()
        .expect("probe ledger is not poisoned")
        .clear();
    CHARGED
        .lock()
        .expect("charge ledger is not poisoned")
        .clear();
    OTHER_RUNS.store(0, Ordering::SeqCst);
    USER_EXECUTES.store(0, Ordering::SeqCst);
    USER_OBSERVED_ERRORS.store(0, Ordering::SeqCst);
}

/// Always succeeds. Every failure these tests observe is injected by the plan
/// at the `intercept_execute` seam, so the handler stays a pure witness and the
/// drain stays deterministic under the paused runtime (no I/O, no real timers).
#[job(name = "fault_plan_probe", max_attempts = 3, backoff_ms = 10)]
async fn fault_plan_probe(_state: AppState, args: ProbeArgs) -> AutumnResult<()> {
    PROBE_RUNS
        .lock()
        .expect("probe ledger is not poisoned")
        .push(args.id);
    Ok(())
}

/// A second job name, interleaved with `fault_plan_probe`, so a per-name
/// ordinal and the global ordinal are provably different numbers.
#[job(name = "fault_plan_other", max_attempts = 3, backoff_ms = 10)]
async fn fault_plan_other(_state: AppState, _args: ProbeArgs) -> AutumnResult<()> {
    OTHER_RUNS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// The worked example's **pre-fix** job: a single attempt, so one injected
/// failure loses the charge permanently. See
/// `worked_example_charge_card_fails_before_the_fix_and_passes_after`.
#[job(name = "charge_card_v1", max_attempts = 1, backoff_ms = 10)]
async fn charge_card_v1(_state: AppState, args: ProbeArgs) -> AutumnResult<()> {
    CHARGED
        .lock()
        .expect("charge ledger is not poisoned")
        .push(args.id);
    Ok(())
}

/// The worked example's **post-fix** job: byte-for-byte the same handler as
/// [`charge_card_v1`], differing only in `max_attempts = 3`.
#[job(name = "charge_card_v2", max_attempts = 3, backoff_ms = 10)]
async fn charge_card_v2(_state: AppState, args: ProbeArgs) -> AutumnResult<()> {
    CHARGED
        .lock()
        .expect("charge ledger is not poisoned")
        .push(args.id);
    Ok(())
}

struct FaultProbeJobPlugin;

impl Plugin for FaultProbeJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![fault_plan_probe, fault_plan_other])
    }
}

struct ChargeCardJobPlugin;

impl Plugin for ChargeCardJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![charge_card_v1, charge_card_v2])
    }
}

/// A do-nothing route, so the build-time guard tests at the bottom of this
/// module can mount an app that registers **no jobs** — the guards run before
/// any job runtime starts, so those tests need neither the process-global job
/// runtime lock nor a drain.
#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

/// Counts execute-seam passes and how many of them came back `Err`.
///
/// Installed via [`TestApp::with_job_interceptor`] alongside a plan, so the
/// composition rule ("the plan composes, it never replaces") is observable from
/// the outside: this interceptor must still run, and must see the plan's
/// injected error come back out of `next`.
struct CountingJobInterceptor;

type JobFuture<'a> = Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'a>>;

impl autumn_web::interceptor::JobInterceptor for CountingJobInterceptor {
    fn intercept_enqueue<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a serde_json::Value,
        next: JobFuture<'a>,
    ) -> JobFuture<'a> {
        next
    }

    fn intercept_execute<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a serde_json::Value,
        next: JobFuture<'a>,
    ) -> JobFuture<'a> {
        Box::pin(async move {
            USER_EXECUTES.fetch_add(1, Ordering::SeqCst);
            let result = next.await;
            if result.is_err() {
                USER_OBSERVED_ERRORS.fetch_add(1, Ordering::SeqCst);
            }
            result
        })
    }
}

/// Drain every job the runtime can run now, then step virtual time far enough
/// (several times over) for any jittered retry backoff to come due and drain
/// again. `run_to_idle` deliberately does not fast-forward to a future timer —
/// that is [`Sim::advance`]'s job — so a retry needs this pairing.
async fn settle(sim: &Sim) {
    sim.run_to_idle().await;
    for _ in 0..4 {
        sim.advance(RETRY_WINDOW).await;
        sim.run_to_idle().await;
    }
}

/// The control for everything below: an app built **without** a plan is
/// unchanged by this feature.
///
/// Two claims, both of which a regression in `TestApp::build`'s wiring would
/// break silently. First, the no-plan path stays inert: no ledger is created
/// ([`TestClient::fault_ledger`](autumn_web::test::TestClient::fault_ledger)
/// returns `None`), the job runs exactly once with no injected failure, and the
/// always-on job recorder still records the enqueue — i.e. attaching the fault
/// machinery to `build` did not perturb apps that never opt in.
///
/// Second, it is the control for the AC5 worked example
/// (`worked_example_charge_card_fails_before_the_fix_and_passes_after`): the
/// *same* `charge_card_v1` job, `max_attempts = 1` and all, records its charge
/// when no plan is attached. Without this, "the charge is lost" down there
/// would be equally consistent with a broken job as with a working fault
/// injector; here it pins the loss on the plan.
#[sim_test]
async fn a_test_app_without_a_plan_is_unchanged(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    // ── No plan, the probe job ──────────────────────────────────────────
    sim.build(TestApp::new().plugin(FaultProbeJobPlugin));

    FaultPlanProbeJob::enqueue(ProbeArgs { id: 7 })
        .await
        .expect("enqueue via the in-process job runtime");
    settle(&sim).await;

    assert!(
        sim.client().fault_ledger().is_none(),
        "no plan was attached, so no ledger exists to snapshot"
    );
    assert_eq!(
        *PROBE_RUNS.lock().expect("probe ledger is not poisoned"),
        vec![7],
        "the job ran exactly once, unfaulted: no plan means no injected failure and no retry"
    );
    assert_eq!(
        sim.client().enqueued_jobs().len(),
        1,
        "the always-on job recorder is untouched by the no-plan path"
    );

    // ── No plan, the worked example's pre-fix charge job ────────────────
    let mut control_sim = Sim::from_seed(sim.seed);
    job::clear_global_job_client();
    reset_probe_state();
    control_sim.build(TestApp::new().plugin(ChargeCardJobPlugin));

    ChargeCardV1Job::enqueue(ProbeArgs { id: 0 })
        .await
        .expect("enqueue the pre-fix charge job");
    settle(&control_sim).await;

    assert!(control_sim.client().fault_ledger().is_none());
    assert_eq!(
        *CHARGED.lock().expect("charge ledger is not poisoned"),
        vec![0],
        "AC5 control: with no plan attached, `charge_card_v1` charges normally — so the \
         lost charge in the worked example is caused by the plan, not by the job"
    );

    job::clear_global_job_client();
}

/// AC2 (job effect class): a plan naming the 2nd job execution fails **exactly**
/// that execution — not the 1st, not the 3rd — and the injected failure is an
/// ordinary attempt failure, so the runtime's own retry policy carries the job
/// to success on the next attempt.
///
/// Three jobs are enqueued before anything is drained, so under the paused
/// single-threaded runtime all three first attempts run back-to-back at the
/// virtual epoch: executions 1, 2, 3. Only execution 2 is planned, so exactly
/// one `FiredFault` is recorded, at the epoch (`elapsed_ms == 0`), and the
/// fourth execution is the retry the injected failure caused.
#[sim_test]
async fn fail_job_execution_fires_on_exactly_the_target_ordinal(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    sim.build(
        TestApp::new()
            .plugin(FaultProbeJobPlugin)
            .with_fault_plan(FaultPlan::from_seed(SEED).fail_job_execution(2)),
    );

    for id in 0..3 {
        FaultPlanProbeJob::enqueue(ProbeArgs { id })
            .await
            .expect("enqueue via the in-process job runtime");
    }
    settle(&sim).await;

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(outcome.seed, SEED, "the outcome carries the authoring seed");
    assert_eq!(
        outcome.fired.len(),
        1,
        "exactly one planned fault fired; got {:?}",
        outcome.fired
    );
    let fired = &outcome.fired[0];
    assert_eq!(fired.effect, FaultEffect::JobExecution);
    assert_eq!(fired.ordinal, 2, "the 2nd global job execution failed");
    assert_eq!(
        fired.target_ordinal, 2,
        "only one job name runs, so the per-name ordinal matches the global one"
    );
    assert_eq!(fired.target, "fault_plan_probe");
    assert_eq!(
        fired.at,
        sim_epoch(),
        "the fault is stamped from the injected virtual clock, still at the sim epoch"
    );
    assert_eq!(fired.elapsed_ms, 0, "no virtual time had elapsed yet");

    assert!(
        outcome.suppressed.is_empty(),
        "no clock window is configured, so nothing can be suppressed"
    );
    assert!(
        outcome.unfired.is_empty(),
        "the single planned ordinal was reached"
    );
    assert!(
        outcome.server_errors.is_empty(),
        "this scenario drives no HTTP requests, so reporting.rs sees no 5xx"
    );

    assert_eq!(
        outcome.final_state.job_executions, 4,
        "3 attempts + 1 retry"
    );
    assert_eq!(outcome.final_state.job_executions_failed, 1);
    assert_eq!(outcome.final_state.job_executions_succeeded, 3);

    let mut ran = PROBE_RUNS
        .lock()
        .expect("probe ledger is not poisoned")
        .clone();
    ran.sort_unstable();
    assert_eq!(
        ran,
        vec![0, 1, 2],
        "every job still completed: the faulted attempt never reached the handler, and its \
         retry did"
    );

    job::clear_global_job_client();
}

/// AC2 (per-target ordinals): `fail_job` counts the ordinal on the **named
/// job's own** counter, which is a different number from the global one as soon
/// as two job names interleave.
///
/// Enqueue order `probe, other, probe, other` drains as executions
/// `probe#1 (global 1), other#1 (global 2), probe#2 (global 3), other#2 (global 4)`,
/// so `fail_job("fault_plan_probe", 2)` must fire at global ordinal 3 and
/// target ordinal 2. A globally-counted implementation would fire on
/// `other#1` instead, which is exactly what this asserts against.
#[sim_test]
async fn fail_job_targets_a_named_job_by_its_own_ordinal(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    sim.build(
        TestApp::new()
            .plugin(FaultProbeJobPlugin)
            .with_fault_plan(FaultPlan::from_seed(SEED).fail_job("fault_plan_probe", 2)),
    );

    for id in 0..2 {
        FaultPlanProbeJob::enqueue(ProbeArgs { id })
            .await
            .expect("enqueue the probe job");
        FaultPlanOtherJob::enqueue(ProbeArgs { id })
            .await
            .expect("enqueue the other job");
        // Drain between pairs so the interleaving is the enqueue interleaving,
        // not whatever order a batch happens to drain in.
        sim.run_to_idle().await;
    }
    settle(&sim).await;

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(
        outcome.fired.len(),
        1,
        "exactly one planned fault fired; got {:?}",
        outcome.fired
    );
    let fired = &outcome.fired[0];
    assert_eq!(fired.effect, FaultEffect::JobExecution);
    assert_eq!(
        fired.target, "fault_plan_probe",
        "the named job is the one that failed"
    );
    assert_eq!(
        fired.target_ordinal, 2,
        "the 2nd execution of `fault_plan_probe` specifically"
    );
    assert_eq!(
        fired.ordinal, 3,
        "which is the 3rd execution overall, because `fault_plan_other` ran in between"
    );

    assert_eq!(
        OTHER_RUNS.load(Ordering::SeqCst),
        2,
        "the untargeted job name is untouched"
    );
    assert_eq!(outcome.final_state.job_executions_failed, 1);
    assert!(outcome.unfired.is_empty());

    job::clear_global_job_client();
}

/// AC1 (drives faults through the existing `interceptor.rs` traits, without app
/// code changes): attaching a plan **composes** with a user-supplied
/// [`autumn_web::interceptor::JobInterceptor`] and with the always-on job
/// recorder, rather than replacing either.
///
/// `with_job_interceptor` is documented as "last one wins", so a naive
/// implementation that installed the fault interceptor the same way would
/// silently drop the user's. Three things must therefore hold at once: the
/// user interceptor sees every execute-seam pass, it observes the injected
/// error coming back out of `next` (so the fault looks like a genuine handler
/// failure to anything wrapping it), and `enqueued_jobs()` still records the
/// enqueues.
#[sim_test]
async fn fault_plan_composes_with_a_user_job_interceptor(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    sim.build(
        TestApp::new()
            .plugin(FaultProbeJobPlugin)
            .with_job_interceptor(CountingJobInterceptor)
            .with_fault_plan(FaultPlan::from_seed(SEED).fail_job_execution(1)),
    );

    for id in 0..2 {
        FaultPlanProbeJob::enqueue(ProbeArgs { id })
            .await
            .expect("enqueue the probe job");
    }
    settle(&sim).await;

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(
        outcome.fired.len(),
        1,
        "the plan still fires with a user interceptor installed; got {:?}",
        outcome.fired
    );
    assert_eq!(outcome.fired[0].ordinal, 1);

    assert_eq!(
        USER_EXECUTES.load(Ordering::SeqCst),
        3,
        "the user interceptor was not replaced: it saw 2 first attempts + 1 retry"
    );
    assert_eq!(
        USER_OBSERVED_ERRORS.load(Ordering::SeqCst),
        1,
        "the injected failure surfaces to a wrapping interceptor as an ordinary Err"
    );
    assert_eq!(
        outcome.final_state.job_executions, 3,
        "the ledger and the user interceptor agree on the number of execute passes"
    );

    assert_eq!(
        sim.client().enqueued_jobs().len(),
        2,
        "the always-on job recorder still captures both enqueues (a retry is re-submitted \
         inside the runtime, not re-enqueued)"
    );

    job::clear_global_job_client();
}

/// AC1 (composition, the other direction): an authored plan and an **active
/// `Chaos` lane** coexist on the same app.
///
/// This is the composition case a user interceptor cannot cover, because
/// `Sim::build` installs chaos itself: when [`Chaos`] is active it calls
/// `with_clock` **and** `with_job_interceptor` on the `TestApp` on its way to
/// `build()`, both of which are last-one-wins slots. So the two lanes are
/// racing for the same two seams, and a wiring bug in either direction is
/// plausible: the plan's interceptor could be dropped in favour of the chaos
/// one, or the plan could take over the clock the skew wrapper installed.
///
/// `clock_skew` is chosen because it activates chaos without needing a
/// database, and because it is the fault class that owns the clock — the seam
/// the plan reads its window and its `FiredFault` stamps from. All three
/// claims are asserted at once: the plan's fault fires, the retry it triggers
/// still carries the job to success, and the chaos lane still recorded its own
/// (non-firing) enqueue decision.
#[sim_test]
async fn fault_plan_composes_with_an_active_chaos_lane(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    sim.chaos(Chaos::default().clock_skew(Duration::from_secs(1)));
    sim.build(
        TestApp::new()
            .plugin(FaultProbeJobPlugin)
            .with_fault_plan(FaultPlan::from_seed(SEED).fail_job_execution(1)),
    );

    FaultPlanProbeJob::enqueue(ProbeArgs { id: 0 })
        .await
        .expect("enqueue the probe job");
    settle(&sim).await;

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(
        outcome.fired.len(),
        1,
        "the plan's interceptor survived chaos installing its own; got {:?}",
        outcome.fired
    );
    assert_eq!(outcome.fired[0].ordinal, 1);
    assert_eq!(outcome.fired[0].target, "fault_plan_probe");
    assert_eq!(outcome.final_state.job_executions_failed, 1);
    assert_eq!(
        outcome.final_state.job_executions_succeeded, 1,
        "the retry the injected failure caused still ran to success under the skewed clock"
    );
    assert_eq!(
        *PROBE_RUNS.lock().expect("probe ledger is not poisoned"),
        vec![0],
        "the handler ran exactly once — on the retry, not on the faulted attempt"
    );

    assert!(
        !sim.__chaos_events().is_empty(),
        "the chaos lane still recorded its enqueue decision: attaching a plan did not \
         clobber the chaos job interceptor"
    );

    job::clear_global_job_client();
}

/// Enqueue one probe, drain it, advance virtual time by `advance_by`, then
/// enqueue and drain a second — so the plan's 2nd job execution happens at a
/// known point on the **injected** clock. Returns that run's outcome.
///
/// Assumes the caller already holds `job::global_job_runtime_test_lock()`.
async fn run_window_scenario(sim: &mut Sim, advance_by: Duration) -> FaultOutcome {
    job::clear_global_job_client();
    reset_probe_state();

    sim.build(
        TestApp::new().plugin(FaultProbeJobPlugin).with_fault_plan(
            FaultPlan::from_seed(SEED)
                .fail_job_execution(2)
                .only_between(Duration::from_secs(5), Duration::from_secs(10)),
        ),
    );

    FaultPlanProbeJob::enqueue(ProbeArgs { id: 0 })
        .await
        .expect("enqueue the first probe");
    sim.run_to_idle().await;

    sim.advance(advance_by).await;

    FaultPlanProbeJob::enqueue(ProbeArgs { id: 1 })
        .await
        .expect("enqueue the second probe");
    settle(sim).await;

    let outcome = sim.client().fault_outcome().await;
    job::clear_global_job_client();
    outcome
}

/// AC3 (timing driven through the injected `Clock`): a plan's `only_between`
/// window is evaluated against the app's injected [`autumn_web::time::ClockSource`],
/// not wall time — under the sim that clock only moves when
/// [`Sim::advance`] moves it, so "inside the window" and "outside the window"
/// are both constructed exactly, with zero real time elapsed either way.
///
/// The same plan (`fail_job_execution(2).only_between(5s, 10s)`) is run twice.
/// Advancing 6s before the second execution puts it inside the window: the
/// fault fires, stamped at the sim epoch + 6s with `elapsed_ms == 6000` — the
/// value a leaked `Utc::now()` / `Instant::now()` could not produce. Advancing
/// 11s instead puts the very same execution past the window: the ordinal is
/// still consumed, but the fault is suppressed rather than fired.
#[sim_test]
async fn fault_timing_is_gated_by_the_injected_clock(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;

    // ── Inside the window ───────────────────────────────────────────────
    let inside = run_window_scenario(&mut sim, Duration::from_secs(6)).await;

    assert_eq!(
        inside.fired.len(),
        1,
        "the 2nd execution happened inside [5s, 10s); got {:?}",
        inside.fired
    );
    let fired = &inside.fired[0];
    assert_eq!(fired.effect, FaultEffect::JobExecution);
    assert_eq!(fired.ordinal, 2);
    assert_eq!(
        fired.at,
        sim_epoch() + chrono::Duration::seconds(6),
        "the wall-clock stamp comes from the injected virtual clock"
    );
    assert_eq!(
        fired.elapsed_ms, 6000,
        "elapsed is measured on the injected monotonic clock, which started at 0 on build"
    );
    assert!(inside.suppressed.is_empty());

    // ── Past the window ─────────────────────────────────────────────────
    // A fresh sim (and therefore a fresh clock and ledger) driven identically
    // except for the size of the advance.
    let mut late_sim = Sim::from_seed(sim.seed);
    let outside = run_window_scenario(&mut late_sim, Duration::from_secs(11)).await;

    assert!(
        outside.fired.is_empty(),
        "past the window nothing may fire; got {:?}",
        outside.fired
    );
    assert_eq!(
        outside.suppressed.len(),
        1,
        "the ordinal is still consumed, and the suppression is recorded"
    );
    assert_eq!(outside.suppressed[0].ordinal, 2);
    assert_eq!(outside.suppressed[0].elapsed_ms, 11_000);
    assert!(
        outside.unfired.is_empty(),
        "suppression *consumes* the ordinal: the planned fault was reached and then held \
         back by the window, which is a different outcome from never being reached at all; \
         got {:?}",
        outside.unfired
    );
    assert_eq!(
        outside.final_state.job_executions_failed, 0,
        "a suppressed fault never fails an attempt"
    );

    job::clear_global_job_client();
}

/// AC4 (structured, serializable outcome): a scenario run produces a
/// [`FaultOutcome`] whose canonical JSON round-trips back to an equal value
/// with an equal fingerprint, and which names the three things a regression
/// test asserts on — which faults fired, which requests 5xx'd, and the final
/// state.
#[sim_test]
async fn outcome_is_serializable_and_round_trips(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    sim.build(
        TestApp::new().plugin(FaultProbeJobPlugin).with_fault_plan(
            FaultPlan::from_seed(SEED)
                .fail_job_execution(1)
                // Never reached: proves `unfired` is part of the record too.
                .fail_job("fault_plan_probe", 99),
        ),
    );

    FaultPlanProbeJob::enqueue(ProbeArgs { id: 0 })
        .await
        .expect("enqueue the probe job");
    settle(&sim).await;

    let outcome = sim.client().fault_outcome().await;
    let json = outcome.to_json_string();

    let parsed = FaultOutcome::from_json_str(&json).expect("the outcome record round-trips");
    assert_eq!(parsed, outcome, "deserializing yields an equal record");
    assert_eq!(
        parsed.to_json_string(),
        json,
        "serialization is canonical: re-encoding is byte-identical"
    );
    assert_eq!(parsed.fingerprint(), outcome.fingerprint());

    for field in [
        "\"seed\"",
        "\"fired\"",
        "\"server_errors\"",
        "\"final_state\"",
    ] {
        assert!(
            json.contains(field),
            "the outcome JSON must carry {field}; got {json}"
        );
    }

    assert_eq!(outcome.fired.len(), 1);
    assert_eq!(
        outcome.unfired.len(),
        1,
        "the ordinal-99 entry was planned but never reached; got {:?}",
        outcome.unfired
    );
    assert_eq!(outcome.unfired[0].ordinal, 99);
    assert_eq!(
        outcome.unfired[0].target.as_deref(),
        Some("fault_plan_probe")
    );

    job::clear_global_job_client();
}

/// One iteration of the replay scenario: build a fresh app with the plan,
/// enqueue six jobs, drain them (retries included), and return the outcome's
/// canonical JSON.
///
/// The plan mixes an explicit ordinal with two **seed-derived** ones, so the
/// schedule this returns is a function of the seed rather than of the literal
/// builder calls — which is what makes the replay claim meaningful.
async fn replay_once(seed: u64) -> String {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    let mut sim = Sim::from_seed(seed);
    sim.build(
        TestApp::new().plugin(FaultProbeJobPlugin).with_fault_plan(
            FaultPlan::from_seed(seed)
                .random_job_execution_faults(2, 1..=6)
                .fail_job("fault_plan_probe", 5),
        ),
    );

    for id in 0..6 {
        FaultPlanProbeJob::enqueue(ProbeArgs { id })
            .await
            .expect("enqueue the probe job");
    }
    settle(&sim).await;

    let json = sim.client().fault_outcome().await.to_json_string();
    job::clear_global_job_client();
    json
}

/// The issue's success metric: **a single authored fault scenario, replayed
/// 100× from the same seed, produces a byte-identical outcome record 100/100
/// times.**
///
/// Each iteration gets its own paused current-thread runtime and its own freshly
/// built app — the same construction `#[sim_test]` performs, done in a loop
/// because one `#[sim_test]` body only ever owns a single [`Sim`] (the pattern
/// `sim_retry_storm.rs` uses for its two-run replay check). Anything that leaked
/// real time, real entropy, or cross-run state into the scenario — a
/// `Utc::now()` stamp on a `FiredFault`, OS-seeded retry jitter reordering the
/// executions, a ledger shared between builds — shows up here as one differing
/// record out of a hundred.
///
/// The second half checks the metric is not vacuous in the direction that
/// matters: the seed genuinely selects the schedule, so a different seed gives
/// a different set of planned ordinals.
#[test]
fn same_seed_replays_a_byte_identical_outcome_100_times() {
    fn run_once(seed: u64) -> String {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("failed to build paused sim runtime");
        runtime.block_on(replay_once(seed))
    }

    let first = run_once(SEED);

    let baseline =
        FaultOutcome::from_json_str(&first).expect("the replayed outcome record round-trips");
    assert!(
        !baseline.fired.is_empty(),
        "the scenario must actually inject something for the replay claim to mean anything; \
         got {first}"
    );

    for iteration in 1..100 {
        let replayed = run_once(SEED);
        assert_eq!(
            replayed, first,
            "replay {iteration} of seed {SEED:#x} diverged.\n first: {first}\n  this: {replayed}"
        );
    }

    // Non-vacuity: the *seed*, not just the literal builder calls, chooses the
    // schedule — otherwise "identical across 100 runs" would be trivially true.
    assert_ne!(
        FaultPlan::from_seed(SEED)
            .random_job_execution_faults(2, 1..=6)
            .planned(),
        FaultPlan::from_seed(SEED + 1)
            .random_job_execution_faults(2, 1..=6)
            .planned(),
        "seed-derived ordinals must differ between seeds"
    );
}

/// AC5 (worked example — a scenario that fails before a fix and passes after).
///
/// The bug: `charge_card` was registered with `max_attempts = 1`. One transient
/// failure at the execute seam — a card processor blipping for a second — and
/// the charge is silently lost, because there is no second attempt. The fix is
/// one attribute: **`max_attempts = 1` → `max_attempts = 3`**, which is exactly
/// the difference between the two otherwise byte-identical handlers this module
/// registers, [`charge_card_v1`] (before) and [`charge_card_v2`] (after).
///
/// To reproduce the pre-fix failure against the *fixed* code, revert
/// `charge_card_v2`'s attribute to `max_attempts = 1` and rerun: the "after"
/// half below then fails on
/// `assert_eq!(after.final_state.job_executions_succeeded, 1)` — the first
/// assertion that can tell the two versions apart, since with a single attempt
/// there is no retry to succeed — and, were that one removed, on
/// `assert_eq!(charged_after, vec![1])` immediately after it. Either way the
/// panic prints the deterministic `AUTUMN_SIM_SEED=…` replay line that reruns
/// the identical schedule.
///
/// The control that the *plan* (and not some defect in the job) is what loses
/// the charge is `a_test_app_without_a_plan_is_unchanged` at the top of this
/// module: it runs this same `charge_card_v1` with no plan attached and the
/// charge is recorded normally.
///
/// This is the shape a real regression test takes, and it is only expressible
/// because the plan names an *ordinal* rather than a probability: "the first
/// execution of `charge_card` fails" is a fixed, replayable condition, where
/// `Chaos::db_transient_errors(0.1)` could only say "some executions fail,
/// sometimes".
#[sim_test]
async fn worked_example_charge_card_fails_before_the_fix_and_passes_after(mut sim: Sim) {
    let _guard = job::global_job_runtime_test_lock().lock().await;

    // ── Before the fix: max_attempts = 1 ────────────────────────────────
    job::clear_global_job_client();
    reset_probe_state();
    sim.build(
        TestApp::new()
            .plugin(ChargeCardJobPlugin)
            .with_fault_plan(FaultPlan::from_seed(SEED).fail_job("charge_card_v1", 1)),
    );
    ChargeCardV1Job::enqueue(ProbeArgs { id: 0 })
        .await
        .expect("enqueue the pre-fix charge job");
    settle(&sim).await;

    let before = sim.client().fault_outcome().await;
    let charged_before = CHARGED
        .lock()
        .expect("charge ledger is not poisoned")
        .clone();

    assert_eq!(before.fired.len(), 1, "the planned fault fired");
    assert_eq!(before.fired[0].target, "charge_card_v1");
    assert_eq!(before.final_state.job_executions_failed, 1);
    assert_eq!(
        before.final_state.job_executions_succeeded, 0,
        "with max_attempts = 1 there is no second attempt"
    );
    // The resilience assertion a regression test would make is
    // `assert!(charged_before.contains(&0))`. Asserting its negation here is
    // what pins the *pre-fix* behaviour: the charge is lost.
    assert!(
        !charged_before.contains(&0),
        "pre-fix behaviour: a single injected failure loses the charge outright"
    );

    // ── After the fix: max_attempts = 3 ─────────────────────────────────
    let mut fixed_sim = Sim::from_seed(sim.seed);
    job::clear_global_job_client();
    reset_probe_state();
    fixed_sim.build(
        TestApp::new()
            .plugin(ChargeCardJobPlugin)
            .with_fault_plan(FaultPlan::from_seed(SEED).fail_job("charge_card_v2", 1)),
    );
    ChargeCardV2Job::enqueue(ProbeArgs { id: 1 })
        .await
        .expect("enqueue the post-fix charge job");
    settle(&fixed_sim).await;

    let after = fixed_sim.client().fault_outcome().await;
    let charged_after = CHARGED
        .lock()
        .expect("charge ledger is not poisoned")
        .clone();

    assert_eq!(
        after.fired.len(),
        1,
        "the identical fault fired against the fixed job; got {:?}",
        after.fired
    );
    assert_eq!(after.fired[0].target, "charge_card_v2");
    assert_eq!(after.final_state.job_executions_failed, 1);
    assert_eq!(
        after.final_state.job_executions_succeeded, 1,
        "the retry the fix enables succeeded"
    );
    assert_eq!(
        charged_after,
        vec![1],
        "post-fix behaviour: the charge is recovered by the retry, and recorded exactly once"
    );

    job::clear_global_job_client();
}

/// A plan is only replayable if the surrounding config cannot reorder what it
/// records, so `TestApp::build` refuses `jobs.workers > 1` up front rather than
/// producing a scenario that passes locally and flakes in CI.
///
/// Two concurrent workers can swap which execution is the Nth, so
/// `fail_job_execution(2)` would name a different attempt from run to run — the
/// exact failure the ordinal API exists to rule out. The guard runs before any
/// job runtime starts, which is why this app registers routes only and needs
/// neither the process-global job lock nor `#[sim_test]` (a plain paused
/// current-thread runtime composes with `#[should_panic]`, which the sim macro
/// does not).
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[should_panic(expected = "jobs.workers = 1")]
async fn a_multi_worker_config_is_rejected_at_build_time() {
    let mut config = AutumnConfig::default();
    config.jobs.workers = 2;

    let _client = TestApp::new()
        .config(config)
        .routes(routes![ping])
        .with_fault_plan(FaultPlan::from_seed(SEED).fail_job_execution(1))
        .build();
}

/// The companion guard: error-report sampling below `1.0` draws OS randomness,
/// so a sampled-out 5xx would drop out of `FaultOutcome::server_errors` at
/// random and the "byte-identical across 100 replays" claim would be false a
/// fraction of the time. `TestApp::build` refuses that config outright.
///
/// Gated on `reporting` because the guard — and the `reporting` config section
/// it reads — only exist under that feature.
#[cfg(feature = "reporting")]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[should_panic(expected = "reporting.sample_rate = 1.0")]
async fn a_sampled_reporting_config_is_rejected_at_build_time() {
    let mut config = AutumnConfig::default();
    config.reporting.sample_rate = 0.5;

    let _client = TestApp::new()
        .config(config)
        .routes(routes![ping])
        .with_fault_plan(FaultPlan::from_seed(SEED).fail_job_execution(1))
        .build();
}

/// Codex review (round 2, P2): with `failure_capture.enabled = true`, reporting
/// awaits the capsule's **blocking** persistence (directory scan, write,
/// `sync_all`) before any reporter runs, so `fault_outcome()`'s cooperative
/// settle could snapshot while that write is still in flight on slow storage
/// and silently miss a 5xx the client already observed. Capsules are
/// production evidence with no place in an authored fault scenario, so
/// `TestApp::build` refuses the combination outright.
#[cfg(feature = "reporting")]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[should_panic(expected = "failure_capture.enabled = false")]
async fn a_failure_capture_config_is_rejected_at_build_time() {
    let mut config = AutumnConfig::default();
    config.failure_capture.enabled = true;

    let _client = TestApp::new()
        .config(config)
        .routes(routes![ping])
        .with_fault_plan(FaultPlan::from_seed(SEED).fail_job_execution(1))
        .build();
}

// ── Codex review round 1: the projector must not sit behind a stalled reporter ─

/// An app-owned reporter whose future never resolves — the shape of a reporter
/// that waits on a Tokio timer under a paused simulation.
#[cfg(feature = "reporting")]
struct StallingReporter;

#[cfg(feature = "reporting")]
impl ErrorReporter for StallingReporter {
    fn report<'a>(&'a self, _event: &'a ErrorEvent) -> ReportFuture<'a> {
        Box::pin(std::future::pending())
    }
}

/// A handler that fails with a 503 so the reporting layer emits one event.
#[get("/boom")]
async fn boom() -> AutumnResult<&'static str> {
    Err(AutumnError::service_unavailable_msg("boom"))
}

/// Codex review (round 1, P2): `ReporterChain::report_all` awaits the
/// registered reporters **sequentially**, so if the plan's 5xx projector were
/// appended *after* a user reporter whose future stays pending, the projector
/// would never run and the 5xx the client observed would be missing from
/// `FaultOutcome::server_errors` — `fault_outcome()` only yields a bounded
/// number of times before snapshotting. The projector is therefore placed
/// first in the chain; this test pins that: with a never-resolving user
/// reporter installed, the observed 503 still lands in `server_errors`.
///
/// AC4 hardening — the 5xx capture must be independent of the app's own
/// reporters' progress.
#[cfg(feature = "reporting")]
#[sim_test]
async fn a_stalled_user_reporter_does_not_starve_the_fault_projection(mut sim: Sim) {
    // This app builds no jobs, but `sim.build` still runs the shared
    // `initialize_job_runtime` boot path, which unconditionally clears the
    // process-global job client before checking whether there's anything to
    // (re)install (`autumn::app::initialize_job_runtime`). Without the same
    // lock every other test in this module holds, that clear can land inside
    // another, job-driving test's window between building its app and
    // enqueueing against it, surfacing as a spurious "job runtime is not
    // initialized" failure there instead of here (observed on CI as
    // `fault_plan_composes_with_a_user_job_interceptor` failing under
    // parallel test execution).
    let _guard = job::global_job_runtime_test_lock().lock().await;
    sim.build(
        TestApp::new()
            .routes(routes![boom])
            .with_error_reporter(StallingReporter)
            .with_fault_plan(FaultPlan::from_seed(SEED)),
    );

    sim.client().get("/boom").send().await.assert_status(503);

    let outcome = sim.client().fault_outcome().await;
    assert_eq!(
        outcome.server_errors.len(),
        1,
        "the observed 503 must reach the ledger even though the app's own \
         reporter never completes; got {:?}",
        outcome.server_errors
    );
    assert_eq!(outcome.server_errors[0].status, 503);
    assert_eq!(outcome.server_errors[0].route.as_deref(), Some("/boom"));
    assert!(
        outcome.fired.is_empty() && outcome.unfired.is_empty(),
        "an empty plan plans nothing and fires nothing"
    );
}
