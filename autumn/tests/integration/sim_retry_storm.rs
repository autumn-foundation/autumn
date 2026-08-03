//! Sim-testing worked example (W7, issue #1797): a genuine whole-app
//! concurrency bug the deterministic simulation harness catches that a
//! conventional (real-clock) integration test cannot.
//!
//! The local job runtime's retry backoff (`execute_local_job`,
//! `autumn/src/job.rs`) computed a **pure exponential delay with no jitter**:
//! `initial_backoff_ms * 2^(attempt - 1)`. That delay is a function of the job's
//! *configuration* only — not its identity — so when several jobs in the same
//! queue fail at the same instant (a downstream dependency blips and takes
//! every in-flight job down with it), every one of them computes the *exact
//! same* delay and therefore retries at the *exact same* instant: a
//! synchronized "thundering herd" that immediately re-floods the dependency it
//! just backed off from, instead of spreading the retry load.
//!
//! A real-clock integration test cannot reproduce this on purpose: it would
//! need N real jobs to fail within the same millisecond of wall-clock time, a
//! coincidence no ordinary test schedule can force, let alone rerun
//! deterministically to prove a fix. Under the sim's virtual clock, "N jobs
//! failing at the same instant" isn't a timing fluke to hope for — it is the
//! deterministic, trivially-constructed condition this test builds directly:
//! the paused runtime lets zero real time elapse between enqueuing the jobs and
//! draining their first (failing) attempt.
//!
//! The fix (`jittered_retry_delay_ms` in `autumn/src/job.rs`) draws an
//! equal-jitter spread from the framework's injected [`autumn_web::entropy::Entropy`]
//! seam (`state.entropy()`) — the same source the [`autumn_web::entropy::Rng`]
//! extractor and deterministic `Uuid` helpers draw from — so the herd spreads
//! out using real OS entropy in production, while staying bit-for-bit
//! reproducible under a fixed `#[sim_test]` seed.
//!
//! # Observing sub-window timing under the sim
//!
//! Neither the framework's injected [`autumn_web::time::ClockSource`] nor
//! Tokio's own paused clock is useful for observing *when within* a single big
//! [`Sim::advance`] call a retry fired: both jump straight to the target
//! instant in one step before any timer runs, so every task woken by that
//! advance reads the *same* `now()` regardless of its individual deadline.
//! Distinguishing "fired at delay=520ms" from "fired at delay=980ms" therefore
//! needs the *test* to step through the window in bounded increments and
//! record which increment each retry landed in — the technique this test uses
//! (`CHECKPOINT`).
//!
//! `retries_are_not_synchronized_under_load` is the `DoD` proof for this wave:
//! reverting `jittered_retry_delay_ms` to the old unjittered
//! `backoff_ms.saturating_mul(2_u64.saturating_pow(attempt - 1))` locally and
//! rerunning this test reproduces the herd — every retry lands in the exact
//! same checkpoint bucket — and the `always!` invariant below fails, printing
//! the deterministic `AUTUMN_SIM_SEED=…` replay line. With the fix in place the
//! same seed passes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use autumn_web::app::AppBuilder;
use autumn_web::job;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::{always, sometimes};
use serde::{Deserialize, Serialize};

/// Number of jobs made to fail "simultaneously" under the sim's virtual clock.
/// Large enough that an equal-jitter spread over the ~500ms checkpointed
/// window collides into a single checkpoint bucket only with astronomically
/// low probability, while staying small enough that the drain settles well
/// inside `Sim::run_to_idle`'s bound.
const STORM_SIZE: u32 = 12;

/// The virtual-time window this test steps through in checkpoints, in
/// milliseconds elapsed since the jobs' first (failing) attempt.
///
/// `storm_probe` is configured with `backoff_ms = 1000`, so the un-jittered
/// exponential delay at attempt 1 is exactly 1000ms; equal jitter spreads the
/// jittered delay across `[500, 1000)`. These six increments (summing to
/// 1500ms, past the worst case) straddle that whole range finely enough to
/// tell "spread across several buckets" (the fix) apart from "every retry
/// lands in the last bucket" (the un-jittered bug, since 1000ms falls in
/// `[950, 1500)`).
const CHECKPOINT_STEPS_MS: [u64; 6] = [550, 100, 100, 100, 100, 550];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StormArgs {
    id: u32,
}

/// Per-job attempt counters, keyed by `StormArgs::id`. Reset before each run
/// of the test so repeated invocations (e.g. under a sweep) start clean.
static ATTEMPTS: Mutex<Option<HashMap<u32, u32>>> = Mutex::new(None);

/// Incremented by the test before draining each step in
/// [`CHECKPOINT_STEPS_MS`], so a retry that runs during that drain can record
/// which step it landed in.
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);

/// The checkpoint index (see [`CHECKPOINT`]) each job's *retry* (second)
/// attempt actually ran in, in enqueue order.
static RETRY_CHECKPOINTS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn reset_probe_state() {
    *ATTEMPTS.lock().unwrap() = Some(HashMap::new());
    RETRY_CHECKPOINTS.lock().unwrap().clear();
    CHECKPOINT.store(0, Ordering::SeqCst);
}

/// Fails its first attempt unconditionally, then succeeds — the minimal shape
/// that puts every enqueued job's retry through the backoff path being tested.
#[job(name = "storm_probe", max_attempts = 2, backoff_ms = 1000)]
async fn storm_probe(_state: AppState, args: StormArgs) -> AutumnResult<()> {
    let attempt = {
        let attempts = ATTEMPTS
            .lock()
            .unwrap()
            .as_mut()
            .expect("reset_probe_state must run first");
        let counter = attempts.entry(args.id).or_insert(0);
        *counter += 1;
        *counter
    };
    if attempt == 1 {
        Err(AutumnError::internal_server_error(std::io::Error::other(
            "forced failure (simulated downstream blip)",
        )))
    } else {
        RETRY_CHECKPOINTS
            .lock()
            .unwrap()
            .push(CHECKPOINT.load(Ordering::SeqCst));
        Ok(())
    }
}

struct StormProbeJobPlugin;

impl Plugin for StormProbeJobPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.jobs(jobs![storm_probe])
    }
}

#[sim_test]
async fn retries_are_not_synchronized_under_load(mut sim: Sim) {
    // This job path uses the process-global job client, so serialize on the
    // shared runtime lock and start from a clean client, matching every other
    // job-backed sim test in this suite.
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();
    reset_probe_state();

    sim.build(TestApp::new().plugin(StormProbeJobPlugin));

    // Enqueue every job before draining anything: on the paused runtime no
    // real time passes between these calls, so all `STORM_SIZE` first
    // attempts genuinely run "at the same virtual instant" once drained below
    // — exactly the adversarial condition a retry storm needs, constructed
    // deterministically rather than hoped for.
    for id in 0..STORM_SIZE {
        StormProbeJob::enqueue(StormArgs { id }).await.unwrap();
    }

    // Drain the first (failing) attempt for every job. Each schedules its
    // retry via a backoff timer; none of the retries are due yet.
    sim.run_to_idle().await;

    // Step through the backoff window in checkpoints (see
    // `CHECKPOINT_STEPS_MS`), recording which checkpoint each retry lands in.
    for (index, step_ms) in CHECKPOINT_STEPS_MS.into_iter().enumerate() {
        sim.advance(std::time::Duration::from_millis(step_ms)).await;
        CHECKPOINT.store(
            u32::try_from(index + 1).expect("checkpoint index fits in u32"),
            Ordering::SeqCst,
        );
        sim.run_to_idle().await;
    }

    let retry_checkpoints = RETRY_CHECKPOINTS.lock().unwrap().clone();
    assert_eq!(
        retry_checkpoints.len(),
        STORM_SIZE as usize,
        "every job must have retried and succeeded"
    );

    let first = retry_checkpoints[0];
    let spread = retry_checkpoints
        .iter()
        .any(|checkpoint| *checkpoint != first);
    sometimes!(
        spread,
        "retries observed spread across more than one checkpoint of the backoff window"
    );
    always!(
        spread,
        "all {STORM_SIZE} retries landed in the exact same checkpoint of the backoff \
         window ({:?}) — a synchronized thundering herd (seed={:#x})",
        retry_checkpoints.first(),
        sim.seed,
    );

    // Single-run non-vacuity check: the `sometimes!` target above must have
    // actually fired in this run, not merely be theoretically reachable.
    autumn_web::sim::assert_all_sometimes_satisfied();

    job::clear_global_job_client();
}
