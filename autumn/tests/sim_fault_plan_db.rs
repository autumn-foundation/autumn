//! Authored fault scenarios (`FaultPlan`, issue #1680) — the Docker-free
//! database lane.
//!
//! This is the [`FaultEffect::DbCheckout`] half of the acceptance criteria, run
//! over the in-memory `SQLite` sim substrate
//! ([`SqliteSubstrate`](autumn_web::sim::substrate::SqliteSubstrate)) so it
//! needs no container and runs on every push. Its Postgres twin —
//! `autumn/tests/integration/sim_fault_plan_pg.rs` — proves the same plan
//! composes under transactional test isolation; the job-effect half lives in
//! `autumn/tests/integration/sim_fault_plan.rs`.
//!
//! | Test | Criterion |
//! |---|---|
//! | `fail_db_checkout_fires_on_exactly_the_third_checkout` | AC2 — the DB-checkout effect class fails deterministically, targetable by ordinal |
//! | `fail_db_checkout_on_targets_a_named_pool` | AC2 — the ordinal is counted on the *named pool's* own counter, and a plan naming a pool the app does not have fires nothing |
//! | `db_checkout_fault_is_captured_as_a_server_error_via_reporting` | AC4 — the 5xx the fault produced is captured through `reporting.rs` into the structured outcome |
//! | `fault_plan_composes_with_a_user_db_interceptor` | AC1 — the plan drives the fault through the existing `DbConnectionInterceptor` seam and composes with (never replaces) a user-installed one |
//! | `same_seed_replays_a_byte_identical_db_outcome_100_times` | the issue's success metric, on the DB lane |
//!
//! A **standalone** `[[test]]` binary rather than a module of the consolidated
//! `integration_tests` binary, for the same reason as `sim_chaos.rs`: that
//! binary is Postgres-typed and does not compile under `--features sqlite`. It
//! needs `test-support` for the public [`TestApp`] harness, so the file is
//! `#![cfg(all(feature = "sqlite", feature = "test-support"))]` — a default
//! `cargo test` compiles it to an empty (passing) binary. Run it with:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_fault_plan_db`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};

use autumn_web::app::AppBuilder;
use autumn_web::interceptor::{DbCheckoutContext, DbConnectionInterceptor};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{FaultEffect, FaultOutcome, FaultPlan, Sim};
use autumn_web::test::TestApp;

/// The authoring seed shared by every scenario here.
const SEED: u64 = 0x5EED;

/// Requests driven by the explicit-ordinal scenarios.
const REQUESTS: usize = 5;

/// Requests driven by the named-pool scenario: enough to show the fault firing
/// once and the checkouts either side of it succeeding.
const NAMED_POOL_REQUESTS: usize = 4;

/// Requests driven by the seed-derived replay scenario, matching the
/// `random_db_checkout_faults` range below so every drawn ordinal is reachable.
const REPLAY_REQUESTS: usize = 8;

/// Reuse the substrate lane's migration set, so the mounted app has a live
/// schema behind the `Db` extractor (`sim_chaos.rs` does the same).
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/fixtures/sim_sqlite_substrate");

/// A `Db`-extractor route: merely resolving `Db` checks out a connection, which
/// is exactly the seam the fault interceptor sits on. A fired fault turns this
/// into a 503 before the handler body runs; otherwise it returns `ok`.
#[get("/touch")]
async fn touch(_db: Db) -> &'static str {
    "ok"
}

struct TouchPlugin;

impl Plugin for TouchPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![touch])
    }
}

/// Checkouts seen by a *user-supplied* interceptor installed alongside a plan.
static USER_CHECKOUTS: AtomicU32 = AtomicU32::new(0);

/// Checkouts the user-supplied interceptor saw come back as `Err` — the
/// injected failure must reach it looking like an ordinary pool failure.
static USER_CHECKOUT_ERRORS: AtomicU32 = AtomicU32::new(0);

type CheckoutFuture<'a> = Pin<
    Box<dyn Future<Output = Result<autumn_web::db::PooledConnection, AutumnError>> + Send + 'a>,
>;

/// Counts checkouts and checkout failures, so the composition rule ("the plan
/// composes, it never replaces") is observable from outside the framework.
struct CountingDbInterceptor;

impl DbConnectionInterceptor for CountingDbInterceptor {
    fn intercept_checkout<'a>(
        &'a self,
        ctx: DbCheckoutContext,
        next: CheckoutFuture<'a>,
    ) -> CheckoutFuture<'a> {
        Box::pin(async move {
            assert_eq!(ctx.pool_name, "primary");
            USER_CHECKOUTS.fetch_add(1, Ordering::SeqCst);
            let result = next.await;
            if result.is_err() {
                USER_CHECKOUT_ERRORS.fetch_add(1, Ordering::SeqCst);
            }
            result
        })
    }
}

/// Mount a fresh migrated substrate with `plan` attached and return the sim.
fn mount(seed: u64, plan: FaultPlan) -> Sim {
    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");
    let mut sim = Sim::from_seed(seed);
    sim.build(
        TestApp::new()
            .plugin(TouchPlugin)
            .with_db(substrate.pool())
            .with_fault_plan(plan),
    );
    // The substrate pool is owned by the mounted app from here on; the local
    // handle is dropped, which is what `sim_chaos.rs` relies on too.
    sim
}

/// AC2 (database effect class): a plan naming the 3rd connection checkout fails
/// **exactly** that checkout. Five identical requests go out; only the third is
/// a 503, and the recorded [`FiredFault`](autumn_web::sim::FiredFault) names the
/// pool and the ordinal it fired on.
///
/// This is the assertion a probabilistic fault source cannot make:
/// `Chaos::db_transient_errors(p)` can say "about `p` of these fail", never
/// "the third one fails", which is the form a regression test for an
/// off-by-one retry or a checkout-budget bug has to take.
#[tokio::test(start_paused = true)]
async fn fail_db_checkout_fires_on_exactly_the_third_checkout() {
    let sim = mount(SEED, FaultPlan::from_seed(SEED).fail_db_checkout(3));

    let mut statuses = Vec::with_capacity(REQUESTS);
    for _ in 0..REQUESTS {
        statuses.push(sim.client().get("/touch").send().await.status.as_u16());
    }
    assert_eq!(
        statuses,
        vec![200, 200, 503, 200, 200],
        "only the planned 3rd checkout fails"
    );

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(outcome.seed, SEED);
    assert_eq!(
        outcome.fired.len(),
        1,
        "exactly one planned fault fired; got {:?}",
        outcome.fired
    );
    let fired = &outcome.fired[0];
    assert_eq!(fired.effect, FaultEffect::DbCheckout);
    assert_eq!(fired.ordinal, 3);
    assert_eq!(
        fired.target_ordinal, 3,
        "only one pool is in play, so the per-pool ordinal matches the global one"
    );
    assert_eq!(fired.target, "primary");

    assert!(outcome.suppressed.is_empty());
    assert!(outcome.unfired.is_empty());
    assert_eq!(
        outcome.final_state.db_checkouts,
        u64::try_from(REQUESTS).expect("request count fits in u64"),
        "every checkout is counted, fired or not"
    );
}

/// AC2 (per-target ordinals on the database lane): `fail_db_checkout_on` counts
/// its ordinal on the **named pool's own** counter, and a plan naming a pool
/// this app does not have quietly fires nothing rather than falling back to
/// "any pool".
///
/// Both halves matter. The first pins the targeting: naming `primary` and
/// ordinal 2 must fail the 2nd checkout on that pool, with `target` and
/// `target_ordinal` on the record saying so — an implementation that ignored
/// the pool name and used the global counter would pass a single-pool test by
/// accident, which is why the record is asserted field by field.
///
/// The second is the negative control, and it is the one an
/// "unrecognised name falls through to the global counter" bug fails: a plan
/// aimed at `replica` on a `primary`-only app must leave every request at 200
/// and land the entry in [`FaultOutcome::unfired`] — planned, never reached —
/// rather than silently faulting `primary` instead. `unfired` also carries the
/// pool name, so a mistyped target is visible in the outcome record instead of
/// looking like a scenario that simply did not trigger.
#[tokio::test(start_paused = true)]
async fn fail_db_checkout_on_targets_a_named_pool() {
    // ── The pool the app actually has ───────────────────────────────────
    let sim = mount(
        SEED,
        FaultPlan::from_seed(SEED).fail_db_checkout_on("primary", 2),
    );

    let mut statuses = Vec::with_capacity(NAMED_POOL_REQUESTS);
    for _ in 0..NAMED_POOL_REQUESTS {
        statuses.push(sim.client().get("/touch").send().await.status.as_u16());
    }
    assert_eq!(
        statuses,
        vec![200, 503, 200, 200],
        "only the 2nd checkout on the named pool fails"
    );

    let outcome = sim.client().fault_outcome().await;
    assert_eq!(
        outcome.fired.len(),
        1,
        "exactly one planned fault fired; got {:?}",
        outcome.fired
    );
    let fired = &outcome.fired[0];
    assert_eq!(fired.effect, FaultEffect::DbCheckout);
    assert_eq!(
        fired.target, "primary",
        "the named pool is the one that failed"
    );
    assert_eq!(
        fired.target_ordinal, 2,
        "the 2nd checkout on `primary` specifically"
    );
    assert!(outcome.suppressed.is_empty());
    assert!(
        outcome.unfired.is_empty(),
        "the named ordinal was reached; got {:?}",
        outcome.unfired
    );

    // ── A pool this app does not have ───────────────────────────────────
    let absent = mount(
        SEED,
        FaultPlan::from_seed(SEED).fail_db_checkout_on("replica", 1),
    );

    let mut absent_statuses = Vec::with_capacity(NAMED_POOL_REQUESTS);
    for _ in 0..NAMED_POOL_REQUESTS {
        absent_statuses.push(absent.client().get("/touch").send().await.status.as_u16());
    }
    assert_eq!(
        absent_statuses,
        vec![200; NAMED_POOL_REQUESTS],
        "a plan aimed at a pool the app does not have must not fault the pool it does"
    );

    let absent_outcome = absent.client().fault_outcome().await;
    assert!(
        absent_outcome.fired.is_empty(),
        "nothing may fire against an absent pool; got {:?}",
        absent_outcome.fired
    );
    assert_eq!(
        absent_outcome.unfired.len(),
        1,
        "the planned-but-never-reached entry is reported; got {:?}",
        absent_outcome.unfired
    );
    assert_eq!(absent_outcome.unfired[0].effect, FaultEffect::DbCheckout);
    assert_eq!(
        absent_outcome.unfired[0].target.as_deref(),
        Some("replica"),
        "the outcome names the pool the plan was aiming at"
    );
    assert_eq!(absent_outcome.unfired[0].ordinal, 1);
    assert_eq!(
        absent_outcome.final_state.db_checkouts,
        u64::try_from(NAMED_POOL_REQUESTS).expect("request count fits in u64"),
        "the checkouts still happened; they just were not the plan's target"
    );
}

/// AC4 (structured outcome, including which requests 5xx'd via `reporting.rs`):
/// the 503 an injected checkout failure produced is projected into
/// `FaultOutcome::server_errors` with its status, method, matched route and
/// message — and the whole record round-trips through canonical JSON, which is
/// what makes it byte-comparable across replays.
// `FaultOutcome::server_errors` is populated only when the `reporting` feature
// (a default) is compiled in; without it the field is empty by contract.
#[cfg(feature = "reporting")]
#[tokio::test(start_paused = true)]
async fn db_checkout_fault_is_captured_as_a_server_error_via_reporting() {
    let sim = mount(SEED, FaultPlan::from_seed(SEED).fail_db_checkout(1));

    sim.client().get("/touch").send().await.assert_status(503);
    sim.client().get("/touch").send().await.assert_status(200);

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(
        outcome.server_errors.len(),
        1,
        "exactly the faulted request was reported; got {:?}",
        outcome.server_errors
    );
    let reported = &outcome.server_errors[0];
    assert_eq!(reported.status, 503);
    assert_eq!(reported.method.as_deref(), Some("GET"));
    assert_eq!(reported.route.as_deref(), Some("/touch"));
    assert!(
        reported.message.contains("fault plan"),
        "the reported 5xx names the injected fault; got {:?}",
        reported.message
    );

    let json = outcome.to_json_string();
    let parsed = FaultOutcome::from_json_str(&json).expect("the outcome record round-trips");
    assert_eq!(parsed, outcome);
    assert_eq!(parsed.to_json_string(), json, "serialization is canonical");
    assert_eq!(parsed.fingerprint(), outcome.fingerprint());
}

/// AC1 (faults are driven through the existing `interceptor.rs` traits, without
/// app code changes): a plan **composes** with a user-installed
/// [`DbConnectionInterceptor`] instead of replacing it.
///
/// `with_db_interceptor` is documented as "last one wins", so an implementation
/// that installed the fault interceptor through the same slot would silently
/// drop the user's — a failure mode invisible from the outcome record alone.
/// The user interceptor must therefore still see all five checkouts, and must
/// see the injected failure surface as an ordinary checkout `Err`.
#[tokio::test(start_paused = true)]
async fn fault_plan_composes_with_a_user_db_interceptor() {
    USER_CHECKOUTS.store(0, Ordering::SeqCst);
    USER_CHECKOUT_ERRORS.store(0, Ordering::SeqCst);

    let substrate =
        SqliteSubstrate::with_migrations(&[&MIGRATIONS]).expect("migrated substrate builds");
    let mut sim = Sim::from_seed(SEED);
    sim.build(
        TestApp::new()
            .plugin(TouchPlugin)
            .with_db(substrate.pool())
            .with_db_interceptor(CountingDbInterceptor)
            .with_fault_plan(FaultPlan::from_seed(SEED).fail_db_checkout(2)),
    );

    for _ in 0..REQUESTS {
        let _ = sim.client().get("/touch").send().await;
    }

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(
        outcome.fired.len(),
        1,
        "the plan still fires with a user interceptor installed; got {:?}",
        outcome.fired
    );
    assert_eq!(outcome.fired[0].ordinal, 2);
    assert_eq!(
        USER_CHECKOUTS.load(Ordering::SeqCst),
        u32::try_from(REQUESTS).expect("request count fits in u32"),
        "the user interceptor was not replaced: it saw every checkout"
    );
    assert_eq!(
        USER_CHECKOUT_ERRORS.load(Ordering::SeqCst),
        1,
        "the injected failure surfaces to a wrapping interceptor as an ordinary Err"
    );
    assert_eq!(
        outcome.final_state.db_checkouts,
        u64::try_from(REQUESTS).expect("request count fits in u64")
    );
}

/// One iteration of the DB replay scenario: a fresh substrate, a fresh app, and
/// eight requests through a **seed-derived** schedule, returning the outcome's
/// canonical JSON.
async fn replay_once(seed: u64) -> String {
    let sim = mount(
        seed,
        FaultPlan::from_seed(seed).random_db_checkout_faults(2, 1..=8),
    );
    for _ in 0..REPLAY_REQUESTS {
        let _ = sim.client().get("/touch").send().await;
    }
    sim.client().fault_outcome().await.to_json_string()
}

/// The issue's success metric on the database lane: **a single authored fault
/// scenario, replayed 100× from the same seed, produces a byte-identical
/// outcome record 100/100 times.**
///
/// The ordinals here are drawn from the seed rather than written out, so the
/// record is only reproducible if the draw, the checkout counting, the clock
/// stamps, and the reporting capture are all deterministic. Anything that
/// leaked real time or OS entropy into the run — a `Utc::now()` on a
/// `FiredFault`, an unseeded request id folded into a `ReportedError` — shows
/// up as one differing record out of a hundred.
#[tokio::test(start_paused = true)]
async fn same_seed_replays_a_byte_identical_db_outcome_100_times() {
    let first = replay_once(SEED).await;

    let baseline =
        FaultOutcome::from_json_str(&first).expect("the replayed outcome record round-trips");
    assert!(
        !baseline.fired.is_empty(),
        "the scenario must actually inject something for the replay claim to mean anything; \
         got {first}"
    );

    for iteration in 1..100 {
        let replayed = replay_once(SEED).await;
        assert_eq!(
            replayed, first,
            "replay {iteration} of seed {SEED:#x} diverged.\n first: {first}\n  this: {replayed}"
        );
    }

    // Non-vacuity: the seed, not the literal builder call, chooses the ordinals.
    assert_ne!(
        FaultPlan::from_seed(SEED)
            .random_db_checkout_faults(2, 1..=8)
            .planned(),
        FaultPlan::from_seed(SEED + 1)
            .random_db_checkout_faults(2, 1..=8)
            .planned(),
        "seed-derived ordinals must differ between seeds"
    );
}
