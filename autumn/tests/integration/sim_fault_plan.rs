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
//! | `fail_job_execution_fires_on_exactly_the_target_ordinal` | AC2 — the job-execution effect class fails deterministically, targetable by ordinal |
//! | `fail_job_targets_a_named_job_by_its_own_ordinal` | AC2 — per-target ordinals are counted per job name, not globally |
//! | `fault_plan_composes_with_a_user_job_interceptor` | AC1 — the plan drives faults through the existing `interceptor.rs` traits, composing with (never clobbering) a user interceptor and the always-on job recorder |
//! | `fault_timing_is_gated_by_the_injected_clock` | AC3 — the fault window is read from the app's injected `ClockSource`, so no wall-clock read leaks in |
//! | `outcome_is_serializable_and_round_trips` | AC4 — the run produces a structured, serializable outcome record |
//! | `same_seed_replays_a_byte_identical_outcome_100_times` | the issue's success metric — 100 consecutive replays of one seed, byte-identical |
//! | `worked_example_charge_card_fails_before_the_fix_and_passes_after` | AC5 — one scenario failing before a fix and passing after |
//!
//! Every test that drives jobs runs under the process-global job runtime lock
//! (`job::global_job_runtime_test_lock`) and starts from a cleared global job
//! client, exactly like `sim_retry_storm.rs` — the `#[job]` enqueue path is
//! process-global, so two job-backed tests running concurrently would otherwise
//! interleave their executions and destroy the ordinal determinism these tests
//! exist to prove. For the same reason ordinal determinism is only claimed
//! under the sim's paused, single-threaded runtime with one worker.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::{FaultEffect, FaultOutcome, FaultPlan, Sim};
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
/// half below then fails on `charged.contains(&1)`, printing the deterministic
/// `AUTUMN_SIM_SEED=…` replay line that reruns the identical schedule.
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
