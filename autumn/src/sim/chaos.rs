//! Deterministic fault injection for simulation tests (sim-testing W5, issue
//! #1797).
//!
//! This is the **chaos lane** of the sim harness: a small, additive, builder
//! that turns on reproducible faults — transient database checkout failures,
//! at-least-once job duplicate delivery, and wall-clock skew — so a
//! [`#[sim_test]`](crate::sim_test) can prove its code is idempotent and
//! resilient *without* a flaky, timing-dependent fault source.
//!
//! # The determinism contract
//!
//! Every fault decision is drawn from a **dedicated seeded entropy stream**,
//! independent of the app-facing [`SimRng`](crate::sim::SimRng) /
//! [`Entropy`] source an app is seeded with. The stream
//! is seeded from `seed ^ CHAOS_STREAM_SALT`, so:
//!
//! * chaos draws never perturb (and are never perturbed by) the application's
//!   own identifier stream, and
//! * the **same seed and the same chaos configuration replay the same fault
//!   schedule byte-for-byte** — under the paused, single-threaded sim runtime
//!   the hooks fire in a deterministic order (checkout #N, job #N), each drawing
//!   exactly one `u64` from the single `Mutex`-guarded stream.
//!
//! Every decision is recorded into a shared event log readable through
//! [`Sim::__chaos_events`](crate::sim::Sim::__chaos_events), which is what the W5
//! Definition-of-Done test asserts equality on across two same-seed runs.
//!
//! # Zero overhead when unused
//!
//! A default (empty) [`Chaos`] is **inactive**: [`Sim::build`](crate::sim::Sim::build)
//! installs nothing and is byte-for-byte unchanged, so every existing sim test
//! is unaffected. Chaos is opt-in via
//! [`Sim::chaos`](crate::sim::Sim::chaos).
//!
//! # Scope (W5.0)
//!
//! This wave ships the scaffolding and the three base fault kinds. Richer
//! per-fault surfaces (item 5/6/7 in the W5 stack) build additively on top of
//! this state and event log.

// The builder methods intentionally take `self` by value and clamp with runtime
// float ops, so they are not `const`-eligible; this narrowly-scoped allow keeps
// the module clean under the workspace's `nursery` lint set (matching the parent
// `sim` module's own allow) without masking real issues.
#![allow(clippy::missing_const_for_fn)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::entropy::{Entropy, SeededEntropy};
use crate::time::{ClockSource, TickingClock};

/// Salt `XOR`ed into the sim seed to derive the chaos **decision** stream, keeping
/// it independent of the app-facing entropy source (which is seeded from the raw
/// seed). An arbitrary fixed constant — its only requirement is being non-zero
/// and distinct from the skew salt.
pub(crate) const CHAOS_STREAM_SALT: u64 = 0xC7A0_5EED_C7A0_5EED;

/// Salt `XOR`ed into the sim seed to derive the one-shot **clock-skew** offset,
/// kept on its own sub-stream so enabling skew never shifts the DB/job decision
/// schedule.
const CHAOS_SKEW_SALT: u64 = 0x5C0F_F5E7_5C0F_F5E7;

/// Which chaos hook produced a [`ChaosEvent`].
///
/// `#[non_exhaustive]` so later waves can add hooks (e.g. a mail-transport or
/// channel-publish fault) without a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChaosHook {
    /// A database connection checkout through the [`crate::db::Db`] extractor.
    DbCheckout,
    /// A job delivery decision made at enqueue time.
    JobDelivery,
}

/// One recorded fault decision.
///
/// The event log is an ordered list of these, one per chaos-hook invocation,
/// exposing the reproducible fault *schedule* a run produced. Two same-seed,
/// same-config runs produce identical logs — the property the W5 `DoD` asserts.
///
/// `#[non_exhaustive]` so fields can be added without breaking readers; the
/// derived [`PartialEq`] is what the `DoD` test compares two runs on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChaosEvent {
    /// The hook that made the decision.
    pub hook: ChaosHook,
    /// The per-hook sequence number (checkout #N or job #N), starting at 0.
    pub seq: u64,
    /// Whether the fault fired for this decision.
    pub fired: bool,
}

/// A reproducible fault-injection configuration for a simulation.
///
/// Build one with the chaining setters and hand it to
/// [`Sim::chaos`](crate::sim::Sim::chaos); [`Sim::build`](crate::sim::Sim::build)
/// then installs the corresponding deterministic hooks. All fields are private
/// and the struct is `#[non_exhaustive]`, so future fault kinds are additive.
///
/// A default `Chaos` is **inactive** (`is_active` is `false`) and installs
/// nothing.
///
/// ```rust,ignore
/// use autumn_web::sim::Chaos;
/// use std::time::Duration;
///
/// let chaos = Chaos::default()
///     .db_transient_errors(0.1)          // 10% of checkouts fail transiently
///     .job_duplicate_delivery(0.2)       // 20% of jobs delivered twice
///     .clock_skew(Duration::from_secs(5)); // deterministic ≤5s wall-clock skew
/// ```
#[non_exhaustive]
#[derive(Default, Debug, Clone)]
pub struct Chaos {
    /// Probability a `RuntimeConnection` checkout returns a transient,
    /// retryable error instead of a real connection. Clamped to `[0.0, 1.0]`.
    db_transient_error_prob: f64,
    /// Probability a job is delivered (and therefore executed) twice. Clamped to
    /// `[0.0, 1.0]`.
    job_duplicate_prob: f64,
    /// When set, install a wrapping clock offset by a deterministic amount in
    /// `[0, dur]`.
    clock_skew: Option<std::time::Duration>,
}

impl Chaos {
    /// Set the probability that a database connection checkout (through the
    /// [`crate::db::Db`] extractor) returns a transient, retryable
    /// service-unavailable error instead of a real connection.
    ///
    /// `p` is clamped to `[0.0, 1.0]`. The decision for each checkout is drawn
    /// from the seeded chaos stream, so the failing checkouts are the same on
    /// every run of a given seed.
    #[must_use]
    pub fn db_transient_errors(mut self, p: f64) -> Self {
        self.db_transient_error_prob = clamp_prob(p);
        self
    }

    /// Set the probability that a job is delivered — and therefore executed —
    /// twice (an at-least-once duplicate), so a test can prove its handler is
    /// idempotent.
    ///
    /// `p` is clamped to `[0.0, 1.0]`. See the module docs for the duplicate
    /// mechanism (an enqueue-seam re-enqueue of the same `(name, payload)`).
    #[must_use]
    pub fn job_duplicate_delivery(mut self, p: f64) -> Self {
        self.job_duplicate_prob = clamp_prob(p);
        self
    }

    /// Install a clock that reports a wall-clock time offset forward by a
    /// deterministic amount in `[0, dur]`.
    ///
    /// The applied offset is drawn **once at build** from a seed-derived
    /// sub-stream (`seed ^ CHAOS_SKEW_SALT`), so it is reproducible for a given
    /// seed and independent of the DB/job decision schedule. It is a **fixed
    /// additive offset** for the whole run: [`Sim::advance`](crate::sim::Sim::advance)
    /// still steps the underlying virtual clock, and handlers reading the
    /// [`crate::time::Clock`] extractor observe `virtual_now + offset`.
    #[must_use]
    pub fn clock_skew(mut self, dur: std::time::Duration) -> Self {
        self.clock_skew = Some(dur);
        self
    }

    /// Whether this configuration installs any hook. A default `Chaos` is
    /// inactive, so [`Sim::build`](crate::sim::Sim::build) stays byte-for-byte
    /// unchanged when chaos is unused.
    pub(crate) fn is_active(&self) -> bool {
        self.db_transient_error_prob > 0.0
            || self.job_duplicate_prob > 0.0
            || self.clock_skew.is_some()
    }
}

/// Clamp a probability into `[0.0, 1.0]`, mapping `NaN` to `0.0`.
fn clamp_prob(p: f64) -> f64 {
    if p.is_nan() { 0.0 } else { p.clamp(0.0, 1.0) }
}

/// Shared runtime state backing every chaos hook for one simulation.
///
/// Holds the single seeded decision stream (a `ChaCha8` behind a `Mutex`, via
/// [`SeededEntropy`]), the per-hook sequence counters, the re-entrancy guard
/// that keeps an injected duplicate from itself being duplicated, and the
/// ordered event log.
pub(crate) struct ChaosState {
    /// The dedicated seeded decision stream (`seed ^ CHAOS_STREAM_SALT`).
    stream: Arc<dyn Entropy>,
    db_transient_error_prob: f64,
    job_duplicate_prob: f64,
    checkout_seq: AtomicU64,
    job_seq: AtomicU64,
    /// Set while an injected duplicate enqueue is in flight so its re-entrant
    /// pass through the interceptor makes no new decision (and cannot recurse).
    suppress_next_duplicate: AtomicBool,
    events: Mutex<Vec<ChaosEvent>>,
}

impl ChaosState {
    /// Build the shared state for `seed` and `chaos`.
    pub(crate) fn new(seed: u64, chaos: &Chaos) -> Arc<Self> {
        Arc::new(Self {
            stream: SeededEntropy::shared(seed ^ CHAOS_STREAM_SALT),
            db_transient_error_prob: chaos.db_transient_error_prob,
            job_duplicate_prob: chaos.job_duplicate_prob,
            checkout_seq: AtomicU64::new(0),
            job_seq: AtomicU64::new(0),
            suppress_next_duplicate: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
        })
    }

    /// Draw one `u64` from the decision stream and decide whether a fault of
    /// probability `prob` fires. Always consumes exactly one draw so the hook
    /// order maps one-to-one onto the stream.
    #[allow(clippy::cast_precision_loss)] // 53-bit mantissa: the shifted value fits f64 exactly
    fn decide(&self, prob: f64) -> bool {
        let draw = self.stream.next_u64();
        // Uniform in [0, 1): take the top 53 bits and divide by 2^53.
        let unit = (draw >> 11) as f64 / (1u64 << 53) as f64;
        unit < prob
    }

    fn record(&self, hook: ChaosHook, seq: u64, fired: bool) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ChaosEvent { hook, seq, fired });
    }

    /// A snapshot copy of the recorded fault schedule so far.
    pub(crate) fn events(&self) -> Vec<ChaosEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A [`ClockSource`] that reports the wrapped virtual clock offset forward by a
/// fixed, deterministic amount. Advancing the underlying [`TickingClock`] is
/// still reflected (they share the same instant); only a constant skew is added.
struct SkewClock {
    inner: TickingClock,
    offset: ChronoDuration,
}

impl ClockSource for SkewClock {
    fn now(&self) -> DateTime<Utc> {
        self.inner.now() + self.offset
    }
}

/// Draw the one-shot, deterministic clock-skew offset in `[0, dur]` from the
/// skew sub-stream. Saturates a pathologically large `dur` at `i64::MAX`
/// nanoseconds (≈292 years) so the chrono conversion never overflows.
fn deterministic_skew(seed: u64, dur: std::time::Duration) -> ChronoDuration {
    let max_nanos = dur.as_nanos();
    if max_nanos == 0 {
        return ChronoDuration::zero();
    }
    let stream = SeededEntropy::shared(seed ^ CHAOS_SKEW_SALT);
    let draw = u128::from(stream.next_u64());
    // Inclusive of `dur`: pick uniformly in [0, max_nanos].
    let picked = draw % (max_nanos.saturating_add(1));
    // `nanoseconds` takes an i64; saturate rather than wrap for an absurd `dur`.
    let nanos = i64::try_from(picked).unwrap_or(i64::MAX);
    ChronoDuration::nanoseconds(nanos)
}

/// Install the active chaos hooks onto `app` and return it, ready to
/// [`build`](crate::test::TestApp::build).
///
/// Wires the (skewed or plain) virtual clock, the job duplicate-delivery
/// interceptor (ungated), and — under the `db` feature — the transient-checkout
/// interceptor, all sharing `state`.
pub(crate) fn install(
    app: crate::test::TestApp,
    chaos: &Chaos,
    seed: u64,
    ticking: TickingClock,
    state: Arc<ChaosState>,
) -> crate::test::TestApp {
    let mut app = match chaos.clock_skew {
        Some(dur) => app.with_clock(SkewClock {
            inner: ticking,
            offset: deterministic_skew(seed, dur),
        }),
        None => app.with_clock(ticking),
    };

    app = app.with_job_interceptor(ChaosJobInterceptor {
        state: Arc::clone(&state),
    });

    #[cfg(feature = "db")]
    {
        app = app.with_db_interceptor(ChaosDbInterceptor {
            state: Arc::clone(&state),
        });
    }

    drop(state);
    app
}

/// Fault-injecting [`crate::interceptor::JobInterceptor`] for duplicate
/// delivery.
///
/// **Mechanism.** The `next` futures at both job seams are one-shot
/// (`Pin<Box<dyn Future>>`) and cannot be re-awaited, so a duplicate is produced
/// by re-enqueueing the same `(name, payload)` at the **enqueue seam**: when the
/// decision fires, the interceptor awaits the real enqueue, then calls
/// [`crate::job::enqueue`] again with the same arguments. The worker therefore
/// delivers — and the handler executes — the logical job twice, faithfully
/// modelling an at-least-once broker. A re-entrancy guard
/// ([`ChaosState::suppress_next_duplicate`]) makes the injected copy skip the
/// decision on its own pass through the interceptor, so duplication is bounded
/// (exactly one extra delivery) and never recurses. `intercept_execute` is a
/// pass-through.
struct ChaosJobInterceptor {
    state: Arc<ChaosState>,
}

impl crate::interceptor::JobInterceptor for ChaosJobInterceptor {
    fn intercept_enqueue<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        Box::pin(async move {
            // The injected duplicate's own pass: enqueue once, make no decision,
            // and do not recurse.
            if self
                .state
                .suppress_next_duplicate
                .swap(false, Ordering::SeqCst)
            {
                return next.await;
            }

            let seq = self.state.job_seq.fetch_add(1, Ordering::SeqCst);
            let fired = self.state.decide(self.state.job_duplicate_prob);
            self.state.record(ChaosHook::JobDelivery, seq, fired);

            let res = next.await;
            if fired && res.is_ok() {
                self.state
                    .suppress_next_duplicate
                    .store(true, Ordering::SeqCst);
                // Re-enqueue the same logical job. The re-entrant pass sees the
                // suppress flag and makes no further decision.
                if crate::job::enqueue(name, payload.clone()).await.is_err() {
                    self.state
                        .suppress_next_duplicate
                        .store(false, Ordering::SeqCst);
                }
            }
            res
        })
    }

    fn intercept_execute<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        next
    }
}

/// Fault-injecting [`crate::interceptor::DbConnectionInterceptor`] for transient
/// checkout failures. When the decision fires it returns the same
/// `service_unavailable` shape a real pool-exhaustion checkout returns (a
/// plausibly-retryable error), otherwise it defers to the real checkout.
#[cfg(feature = "db")]
struct ChaosDbInterceptor {
    state: Arc<ChaosState>,
}

#[cfg(feature = "db")]
impl crate::interceptor::DbConnectionInterceptor for ChaosDbInterceptor {
    fn intercept_checkout<'a>(
        &'a self,
        _ctx: crate::interceptor::DbCheckoutContext,
        next: std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                    > + Send
                    + 'a,
            >,
        >,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let seq = self.state.checkout_seq.fetch_add(1, Ordering::SeqCst);
            let fired = self.state.decide(self.state.db_transient_error_prob);
            self.state.record(ChaosHook::DbCheckout, seq, fired);
            if fired {
                Err(crate::AutumnError::service_unavailable_msg(
                    "chaos: injected transient database checkout error",
                ))
            } else {
                next.await
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Chaos, ChaosHook, ChaosState, clamp_prob, deterministic_skew};

    #[test]
    fn default_chaos_is_inactive() {
        assert!(!Chaos::default().is_active());
    }

    #[test]
    fn any_configured_fault_activates() {
        assert!(Chaos::default().db_transient_errors(0.01).is_active());
        assert!(Chaos::default().job_duplicate_delivery(0.01).is_active());
        assert!(
            Chaos::default()
                .clock_skew(std::time::Duration::from_secs(1))
                .is_active()
        );
    }

    #[test]
    fn probabilities_are_clamped() {
        assert!((clamp_prob(-1.0) - 0.0).abs() < f64::EPSILON);
        assert!((clamp_prob(2.0) - 1.0).abs() < f64::EPSILON);
        assert!((clamp_prob(f64::NAN) - 0.0).abs() < f64::EPSILON);
        // A zero probability leaves the config inactive on that lane.
        assert!(!Chaos::default().db_transient_errors(0.0).is_active());
    }

    // The fast, DB-free determinism signal: the decision stream is a pure
    // function of the seed. Same seed ⇒ identical fired-schedule; a different
    // seed ⇒ (very likely) a different one.
    #[test]
    fn decision_stream_is_seed_deterministic() {
        let chaos = Chaos::default().db_transient_errors(0.5);
        let draw = |seed: u64| {
            let state = ChaosState::new(seed, &chaos);
            (0..64).map(|_| state.decide(0.5)).collect::<Vec<bool>>()
        };
        assert_eq!(
            draw(42),
            draw(42),
            "same seed must replay the same schedule"
        );
        assert_ne!(
            draw(42),
            draw(43),
            "different seeds should (overwhelmingly likely) diverge"
        );
    }

    // Extremes never draw a fault outside the obvious bound, and always/never
    // faults are exact regardless of the stream.
    #[test]
    fn decide_extremes_are_exact() {
        let state = ChaosState::new(7, &Chaos::default());
        assert!((0..32).all(|_| state.decide(1.0)), "p=1.0 always fires");
        assert!((0..32).all(|_| !state.decide(0.0)), "p=0.0 never fires");
    }

    #[test]
    fn recorded_events_snapshot_in_order() {
        let state = ChaosState::new(1, &Chaos::default());
        state.record(ChaosHook::DbCheckout, 0, true);
        state.record(ChaosHook::JobDelivery, 0, false);
        let events = state.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].hook, ChaosHook::DbCheckout);
        assert!(events[0].fired);
        assert_eq!(events[1].hook, ChaosHook::JobDelivery);
        assert!(!events[1].fired);
    }

    #[test]
    fn clock_skew_offset_is_deterministic_and_bounded() {
        let dur = std::time::Duration::from_secs(5);
        let a = deterministic_skew(99, dur);
        let b = deterministic_skew(99, dur);
        assert_eq!(a, b, "same seed ⇒ same skew offset");
        assert!(a >= chrono::Duration::zero());
        assert!(
            a <= chrono::Duration::from_std(dur).unwrap(),
            "offset stays within [0, dur]"
        );
        // A zero window yields no skew.
        assert_eq!(
            deterministic_skew(99, std::time::Duration::ZERO),
            chrono::Duration::zero()
        );
    }
}
