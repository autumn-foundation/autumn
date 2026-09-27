//! Deterministic, injectable wall-clock time.
//!
//! Autumn exposes a [`Clock`] extractor so handlers can read the current time
//! through the framework's injected clock instead of calling
//! [`chrono::Utc::now`] directly. In tests, replace the clock with
//! [`FixedClock`] or [`TickingClock`] via [`crate::test::TestApp::with_clock`]
//! to control time without sleeping.
//!
//! # Quick example
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::time::Clock;
//!
//! #[get("/token-age")]
//! async fn token_age(clock: Clock) -> String {
//!     format!("now is {}", clock.now())
//! }
//! ```
//!
//! # Testing time-sensitive logic
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::test::TestApp;
//! use autumn_web::time::{Clock, TickingClock};
//! use chrono::{TimeZone, Utc};
//! use std::time::Duration;
//!
//! #[get("/token")]
//! async fn check_token(clock: Clock) -> axum::http::StatusCode {
//!     let issued = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
//!     if clock.now() < issued + chrono::Duration::days(30) {
//!         axum::http::StatusCode::OK
//!     } else {
//!         axum::http::StatusCode::UNAUTHORIZED
//!     }
//! }
//!
//! # #[tokio::main]
//! # async fn main() {
//! let issued = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
//! let client = TestApp::new()
//!     .routes(routes![check_token])
//!     .with_clock(TickingClock::starting_at(issued))
//!     .build();
//!
//! client.get("/token").send().await.assert_status(200); // valid
//! client.advance_clock(Duration::from_secs(30 * 24 * 3600)); // advance 30 days
//! client.get("/token").send().await.assert_status(401); // expired
//! # }
//! ```

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

// ── Monotonic instant ─────────────────────────────────────────────────────────

/// Process-wide origin for the real monotonic clock.
///
/// Sampled once, lazily, the first time [`ClockSource::monotonic`] runs on a
/// source that uses the default (real) implementation. Every real monotonic
/// reading is an offset from this single instant, which is what makes
/// [`MonotonicInstant`] a plain [`Duration`] and therefore constructible at an
/// arbitrary *virtual* point — something [`std::time::Instant`] can never be.
#[allow(
    clippy::disallowed_methods,
    reason = "this IS the seam: the single process-monotonic origin every real \
              MonotonicInstant is measured from. There is nothing further to \
              inject it from."
)]
static MONOTONIC_ORIGIN: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// A monotonic instant read from a [`ClockSource`], for measuring *elapsed
/// time* the way [`ClockSource::now`] measures *wall-clock time*.
///
/// # Why this exists
///
/// [`ClockSource`] models only `DateTime<Utc>`, so framework code that needed a
/// duration (request latency, query timings, task run times, uptime) reached
/// for [`std::time::Instant`] directly — off the injected seam. `Instant` is
/// opaque and cannot be constructed at an arbitrary point, and tokio's
/// `start_paused` test runtime does **not** virtualize it (only
/// [`tokio::time::Instant`] moves with the paused timer). Under a
/// [`#[sim_test]`](crate::sim_test) those measurements silently came from the
/// real machine clock: a 24-hour virtual advance read back as microseconds, and
/// two runs of the same seed disagreed.
///
/// `MonotonicInstant` is an offset from its source's own origin, so a virtual
/// clock can produce one at any point while a real clock keeps genuine
/// monotonicity.
///
/// # Guarantees
///
/// - **Monotonic in production.** [`SystemClock`] derives it from a
///   process-global [`std::time::Instant`], so a wall-clock/NTP jump — even a
///   backwards one — can never make an elapsed duration negative or absurd.
///   Comparing wall-clock timestamps has no such guarantee.
/// - **Virtual under simulation.** [`TickingClock`] derives it from the virtual
///   instant, so [`Sim::advance`](crate::sim::Sim::advance) moves it and the same
///   seed replays the same durations byte-for-byte.
/// - **Only comparable within one source.** Two `MonotonicInstant`s are relative
///   to their own clock's origin; subtracting across different clocks is
///   meaningless. [`saturating_duration_since`](Self::saturating_duration_since)
///   never panics and never underflows.
///
/// # Taking two readings
///
/// Measuring elapsed time needs a *start* and an *end*. The [`Clock`] extractor
/// is a **snapshot** taken when the extractor resolved, so
/// [`Clock::monotonic`] gives you the request-start instant and never moves —
/// calling it twice returns the same value. Take the closing reading from the
/// live source, [`AppState::monotonic`](crate::state::AppState::monotonic):
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::time::Clock;
///
/// #[get("/work")]
/// async fn handler(clock: Clock, state: AppState) -> String {
///     let start = clock.monotonic();           // request start (a snapshot)
///     // ... work ...
///     let elapsed = state.monotonic()          // live reading
///         .saturating_duration_since(start);
///     format!("took {}ms", elapsed.as_millis())
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicInstant(Duration);

impl MonotonicInstant {
    /// The origin of a monotonic clock — the zero point every reading is
    /// measured from.
    pub const ORIGIN: Self = Self(Duration::ZERO);

    /// Build a monotonic instant `since_origin` after its source's origin.
    ///
    /// Framework/clock-implementor helper: prefer [`ClockSource::monotonic`] to
    /// *read* the current instant.
    #[must_use]
    pub const fn from_origin_elapsed(since_origin: Duration) -> Self {
        Self(since_origin)
    }

    /// How far this instant sits after its source's origin.
    #[must_use]
    pub const fn since_origin(self) -> Duration {
        self.0
    }

    /// The duration from `earlier` to `self`, saturating at
    /// [`Duration::ZERO`] when `earlier` is later.
    ///
    /// This is the replacement for `end - start` / `start.elapsed()` on
    /// [`std::time::Instant`]. It never panics, and (unlike subtracting two
    /// wall-clock timestamps) it cannot go negative.
    #[must_use]
    pub const fn saturating_duration_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }

    /// The duration from `self` to `now`, saturating at [`Duration::ZERO`].
    ///
    /// Convenience mirror of [`std::time::Instant::elapsed`] for callers holding
    /// a start instant and a freshly-read `now`.
    #[must_use]
    pub const fn elapsed_at(self, now: Self) -> Duration {
        now.saturating_duration_since(self)
    }

    /// This instant advanced by `duration`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, duration: Duration) -> Option<Self> {
        match self.0.checked_add(duration) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// This instant advanced by `duration`, saturating at [`Duration::MAX`].
    ///
    /// The panic-free replacement for `Instant + Duration`, which panics when
    /// the sum is not representable on the platform clock.
    #[must_use]
    pub const fn saturating_add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration))
    }
}

// ── Clock source trait ────────────────────────────────────────────────────────

/// Source of the current wall-clock time used internally by the framework.
///
/// Production apps see [`SystemClock`] (the silent default). Tests swap it out
/// via [`crate::test::TestApp::with_clock`].
///
/// Implement this trait to supply a custom clock (e.g. from an NTP client or a
/// property-testing generator).
pub trait ClockSource: Send + Sync + 'static {
    /// Returns the current UTC instant.
    fn now(&self) -> DateTime<Utc>;

    /// Returns the current *monotonic* instant, for measuring elapsed time.
    ///
    /// The default implementation reads the real process-monotonic clock
    /// ([`std::time::Instant`] offset from a process-global origin) — exactly
    /// what framework code did before this method existed. That makes the method
    /// **fully backward compatible**: an existing downstream `impl ClockSource`
    /// keeps compiling and keeps its current behavior.
    ///
    /// **Override this whenever [`now`](Self::now) is virtual.** A clock that
    /// pins or steps wall time but leaves this at the default reports real
    /// elapsed time, which silently reintroduces nondeterminism — the exact gap
    /// [`MonotonicInstant`] exists to close. The in-tree virtual clocks
    /// ([`FixedClock`], [`TickingClock`]) override it; [`SystemClock`]
    /// deliberately does not.
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant::from_origin_elapsed(MONOTONIC_ORIGIN.elapsed())
    }
}

/// A shared clock handle is itself a clock.
///
/// Lets an already-`Arc`ed source — the one a capsule replay builds, or one a
/// test keeps a handle on to inspect afterwards — be handed to APIs that take
/// `impl ClockSource` by value, without a hand-written forwarding newtype at
/// every call site.
impl ClockSource for std::sync::Arc<dyn ClockSource> {
    fn now(&self) -> DateTime<Utc> {
        (**self).now()
    }

    fn monotonic(&self) -> MonotonicInstant {
        (**self).monotonic()
    }
}

// ── Extractor ─────────────────────────────────────────────────────────────────

/// Axum extractor that resolves the current framework time.
///
/// Use as a handler argument to get the current time through the injected clock
/// instead of calling [`chrono::Utc::now`] directly. This lets tests control
/// time via [`crate::test::TestApp::with_clock`] and
/// [`crate::test::TestClient::advance_clock`].
///
/// ```rust,ignore
/// use autumn_web::time::Clock;
///
/// async fn handler(clock: Clock) -> String {
///     format!("Current time: {}", clock.now())
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Clock(DateTime<Utc>, MonotonicInstant);

impl Clock {
    /// Returns the UTC instant captured when this extractor was resolved.
    #[must_use]
    pub const fn now(&self) -> DateTime<Utc> {
        self.0
    }

    /// Returns the *monotonic* instant captured when this extractor was
    /// resolved — i.e. the **request-start** instant, and the deterministic
    /// replacement for [`std::time::Instant::now`] as the start of an elapsed
    /// measurement.
    ///
    /// Like [`now`](Self::now) this is a **snapshot**, not a live handle:
    /// calling it twice on the same `Clock` returns the same value. Take the
    /// closing reading from the live source —
    /// [`AppState::monotonic`](crate::state::AppState::monotonic) — and subtract
    /// with [`MonotonicInstant::saturating_duration_since`]. See
    /// [`MonotonicInstant`]'s "Taking two readings" for a worked example.
    ///
    /// Under a [`#[sim_test]`](crate::sim_test) the underlying clock moves only
    /// when [`Sim::advance`](crate::sim::Sim::advance) steps virtual time, so the
    /// resulting measurement is reproducible from the seed.
    #[must_use]
    pub const fn monotonic(&self) -> MonotonicInstant {
        self.1
    }
}

impl std::ops::Deref for Clock {
    type Target = DateTime<Utc>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl axum::extract::FromRequestParts<crate::state::AppState> for Clock {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &crate::state::AppState,
    ) -> Result<Self, Self::Rejection> {
        let clock = state.clock();
        Ok(Self(clock.now(), clock.monotonic()))
    }
}

// ── System (real) clock ───────────────────────────────────────────────────────

/// Real wall-clock implementation of [`ClockSource`].
///
/// This is the default when no custom clock is configured. It delegates to
/// [`chrono::Utc::now`] and carries zero overhead compared to calling
/// `Utc::now()` directly.
///
/// It deliberately does **not** override [`ClockSource::monotonic`]: the trait's
/// default body already reads the real process-monotonic clock, so an elapsed
/// measurement taken through this clock is derived from the same
/// [`std::time::Instant`] reading it replaced — the identical monotonicity
/// guarantee, plus one relaxed atomic load (the process-origin
/// [`std::sync::LazyLock`]) and one [`Duration`] subtraction. That is a few
/// nanoseconds on top of the `Instant::now()` call itself, and it is *not* on
/// the per-request path unless a caller asks for elapsed time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl ClockSource for SystemClock {
    #[allow(
        clippy::disallowed_methods,
        reason = "this IS the seam: SystemClock is the production ClockSource, \
                  the one place the real wall clock is allowed to be read."
    )]
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

// ── Fixed clock ───────────────────────────────────────────────────────────────

/// A test clock that stays pinned to a fixed point in time.
///
/// Every call to [`ClockSource::now`] returns the same instant. Use when you
/// need a stable reference time but don't need [`crate::test::TestClient::advance_clock`].
///
/// Calling `advance_clock` when this clock is active is a safe no-op.
///
/// ```rust,ignore
/// use autumn_web::time::FixedClock;
/// use chrono::{TimeZone, Utc};
///
/// let clock = FixedClock::at(Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap());
/// ```
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(DateTime<Utc>);

impl FixedClock {
    /// Create a clock pinned to `dt`.
    #[must_use]
    pub const fn at(dt: DateTime<Utc>) -> Self {
        Self(dt)
    }
}

impl ClockSource for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }

    /// Pinned, exactly like [`now`](ClockSource::now): a clock whose wall time
    /// never moves must not report elapsed time either, or a test that pins the
    /// clock would still see real durations tick past.
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant::ORIGIN
    }
}

// ── Ticking clock ─────────────────────────────────────────────────────────────

/// A test clock that starts at a given time and can be stepped forward.
///
/// Cloning produces a handle that shares the same internal instant — a clone
/// passed to [`crate::test::TestApp::with_clock`] and a clone kept by the test
/// both observe the same time.
///
/// Advance the clock between requests via
/// [`crate::test::TestClient::advance_clock`]:
///
/// ```rust,ignore
/// use autumn_web::time::TickingClock;
/// use chrono::{TimeZone, Utc};
/// use std::time::Duration;
///
/// let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
/// let client = TestApp::new().with_clock(clock.clone()).build();
///
/// client.advance_clock(Duration::from_secs(3600)); // advance 1 hour
/// ```
#[derive(Clone, Debug)]
pub struct TickingClock {
    /// Shared current virtual instant, stepped by [`advance`](Self::advance).
    current: Arc<Mutex<DateTime<Utc>>>,
    /// The instant this clock started at. Copied (not shared) on clone, which is
    /// harmless because it is immutable — every clone agrees on the origin, so
    /// every clone reports the same [`monotonic`](ClockSource::monotonic).
    start: DateTime<Utc>,
}

impl TickingClock {
    /// Create a ticking clock starting at `dt`.
    #[must_use]
    pub fn starting_at(dt: DateTime<Utc>) -> Self {
        Self {
            current: Arc::new(Mutex::new(dt)),
            start: dt,
        }
    }

    /// Step this clock forward by `duration`.
    ///
    /// Sub-millisecond durations are truncated to zero (chrono's minimum resolution
    /// is microseconds). This method never panics.
    pub fn advance(&self, duration: std::time::Duration) {
        let mut guard = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(delta) = chrono::Duration::from_std(duration) {
            *guard += delta;
        }
    }
}

impl ClockSource for TickingClock {
    fn now(&self) -> DateTime<Utc> {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Virtual, derived from the distance the clock has been stepped from its
    /// starting instant — so elapsed time moves *only* when
    /// [`advance`](Self::advance) moves it, in exact lockstep with the wall
    /// clock.
    ///
    /// Because `advance` only ever adds, the difference is non-negative; a
    /// (currently impossible) backwards step saturates to the origin rather than
    /// panicking.
    fn monotonic(&self) -> MonotonicInstant {
        let elapsed = self
            .now()
            .signed_duration_since(self.start)
            .to_std()
            .unwrap_or(Duration::ZERO);
        MonotonicInstant::from_origin_elapsed(elapsed)
    }
}

// ── Helpers for internal framework code ──────────────────────────────────────

/// The current instant on the **real** process-monotonic timeline.
///
/// The sanctioned replacement for [`std::time::Instant::now`] in framework code
/// that genuinely has no [`ClockSource`] handle in scope (a constructor that
/// runs before the clock is installed, a free function with no state argument).
/// Equivalent to `SystemClock.monotonic()`.
///
/// Prefer a real seam wherever one is reachable —
/// [`AppState::monotonic`](crate::state::AppState::monotonic),
/// [`Clock::monotonic`], or `clock.monotonic()` on a threaded-in
/// `Arc<dyn ClockSource>` — because only those follow a virtual clock under a
/// [`#[sim_test]`](crate::sim_test). This function never does; for a reading
/// that follows a running `Sim`, use [`ambient_monotonic`].
#[must_use]
pub fn monotonic_now() -> MonotonicInstant {
    SystemClock.monotonic()
}

// ── Ambient clock ─────────────────────────────────────────────────────────────
//
// Framework code with no clock handle in scope used to read the OS clock
// directly, which a `Sim` cannot control (issue #2967). The ambient clock is
// the fix: a `Sim` installs its virtual clock for its own thread while it
// lives, and the `ambient_*` helpers read it. The sim runtime is a
// current-thread runtime, so every task it polls runs on that thread. Other
// threads, and code that runs with no `Sim`, see the system clock.

thread_local! {
    /// The clocks installed on this thread, newest last, with a liveness flag.
    static AMBIENT: std::cell::RefCell<Vec<AmbientEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// One installed ambient clock.
struct AmbientEntry {
    /// Cleared when the guard drops, on any thread.
    alive: Arc<std::sync::atomic::AtomicBool>,
    clock: Arc<dyn ClockSource>,
}

/// Removes its clock from the ambient stack when dropped.
///
/// The guard may drop on another thread (a `Sim` can move). The cleared flag
/// then makes the install thread skip the entry, and prune it on its next
/// read.
#[derive(Debug)]
pub(crate) struct AmbientGuard {
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for AmbientGuard {
    fn drop(&mut self) {
        self.alive
            .store(false, std::sync::atomic::Ordering::Release);
        // `try_with`: a guard dropped during thread teardown finds no stack.
        let _ = AMBIENT.try_with(|stack| {
            if let Ok(mut stack) = stack.try_borrow_mut() {
                stack.retain(|entry| !Arc::ptr_eq(&entry.alive, &self.alive));
            }
        });
    }
}

/// Make `clock` the ambient clock of this thread until the guard drops. The
/// newest installed clock wins, so a nested `Sim` shadows an outer one.
pub(crate) fn install_ambient(clock: Arc<dyn ClockSource>) -> AmbientGuard {
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    AMBIENT.with(|stack| {
        stack.borrow_mut().push(AmbientEntry {
            alive: Arc::clone(&alive),
            clock,
        });
    });
    AmbientGuard { alive }
}

/// Run `read` on the ambient clock, or on [`SystemClock`] when none is set.
fn with_ambient<T>(read: impl FnOnce(&dyn ClockSource) -> T) -> T {
    let clock = AMBIENT
        .try_with(|stack| {
            let mut stack = stack.try_borrow_mut().ok()?;
            // Prune entries whose guard dropped on another thread.
            stack.retain(|entry| entry.alive.load(std::sync::atomic::Ordering::Acquire));
            stack.last().map(|entry| Arc::clone(&entry.clock))
        })
        .ok()
        .flatten();
    match clock {
        Some(clock) => read(clock.as_ref()),
        None => read(&SystemClock),
    }
}

/// A [`ClockSource`] that reads the ambient clock: the running
/// [`Sim`](crate::sim::Sim)'s virtual clock on this thread, else the system
/// clock.
///
/// Under a `Sim`, wall time is the sim clock, and elapsed time
/// ([`ambient_monotonic`], [`ambient_instant`]) follows tokio's paused clock.
/// So an ambient deadline and a `tokio::time::sleep` stay on one timeline.
///
/// Use it where framework code needs a clock but has none in scope. Prefer
/// the app's injected clock ([`AppState::clock`](crate::state::AppState::clock),
/// the [`Clock`] extractor) wherever one is reachable.
#[derive(Debug, Clone, Copy, Default)]
pub struct AmbientClock;

impl ClockSource for AmbientClock {
    fn now(&self) -> DateTime<Utc> {
        ambient_now()
    }

    fn monotonic(&self) -> MonotonicInstant {
        ambient_monotonic()
    }
}

/// The ambient wall-clock time. Replaces `Utc::now()` where no clock is in
/// scope. See [`AmbientClock`].
#[must_use]
pub fn ambient_now() -> DateTime<Utc> {
    with_ambient(ClockSource::now)
}

/// The ambient monotonic instant. Replaces `Instant::now()` for elapsed time
/// where no clock is in scope. See [`AmbientClock`].
#[must_use]
pub fn ambient_monotonic() -> MonotonicInstant {
    with_ambient(ClockSource::monotonic)
}

/// The ambient monotonic instant as a [`std::time::Instant`], for code that
/// stores or passes `Instant`s. See [`AmbientClock`].
///
/// Measure with `ambient_instant().saturating_duration_since(start)`, not
/// `start.elapsed()`: `elapsed` reads the OS clock.
#[must_use]
pub fn ambient_instant() -> std::time::Instant {
    *MONOTONIC_ORIGIN + ambient_monotonic().since_origin()
}

/// The system monotonic clock as a [`std::time::Instant`], never a `Sim`'s.
///
/// For instants kept in process-global state, which outlives any `Sim`: a
/// virtual instant stored there would later be compared with real time.
pub(crate) fn system_instant() -> std::time::Instant {
    *MONOTONIC_ORIGIN + SystemClock.monotonic().since_origin()
}

/// The ambient wall-clock time as a [`std::time::SystemTime`]. Replaces
/// `SystemTime::now()` where no clock is in scope. See [`AmbientClock`].
#[must_use]
pub fn ambient_system_time() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + clock_unix_duration(&AmbientClock)
}

/// Compute the current Unix timestamp in seconds from the given clock.
///
/// Used by scheduler and storage internals instead of
/// `SystemTime::now().duration_since(UNIX_EPOCH)`.
#[must_use]
pub fn clock_unix_secs(clock: &dyn ClockSource) -> u64 {
    clock_unix_duration(clock).as_secs()
}

/// Compute the elapsed duration since the Unix epoch from the given clock.
#[must_use]
pub fn clock_unix_duration(clock: &dyn ClockSource) -> std::time::Duration {
    let now = clock.now();
    let ts = now.timestamp();
    if ts >= 0 {
        std::time::Duration::new(ts.cast_unsigned(), now.timestamp_subsec_nanos())
    } else {
        std::time::Duration::ZERO
    }
}

// ── Module-level unit tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn system_clock_returns_time_close_to_utc_now() {
        let clock = SystemClock;
        let a = clock.now();
        let b = Utc::now();
        assert!(
            (b - a).num_seconds().abs() < 1,
            "SystemClock should be within 1s of Utc::now()"
        );
    }

    #[test]
    fn fixed_clock_always_returns_same_time() {
        let pinned = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let clock = FixedClock::at(pinned);
        assert_eq!(clock.now(), pinned);
        assert_eq!(clock.now(), pinned);
    }

    #[test]
    fn ticking_clock_starts_at_given_time() {
        let start = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let clock = TickingClock::starting_at(start);
        assert_eq!(clock.now(), start);
    }

    #[test]
    fn ticking_clock_advances_correctly() {
        let start = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let clock = TickingClock::starting_at(start);
        clock.advance(std::time::Duration::from_secs(3600));
        assert_eq!(clock.now(), start + chrono::Duration::hours(1));
    }

    #[test]
    fn ticking_clock_clone_shares_state() {
        let start = Utc.with_ymd_and_hms(2025, 6, 1, 12, 0, 0).unwrap();
        let clock = TickingClock::starting_at(start);
        let clone = clock.clone();

        clock.advance(std::time::Duration::from_secs(86400));
        assert_eq!(clone.now(), start + chrono::Duration::days(1));
    }

    #[test]
    fn clock_unix_secs_uses_clock_timestamp() {
        let pinned = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let clock = FixedClock::at(pinned);
        let secs = clock_unix_secs(&clock);
        assert_eq!(secs, pinned.timestamp().cast_unsigned());
    }

    // ── Monotonic seam (issue #1797) ─────────────────────────────────────

    #[test]
    fn monotonic_saturates_instead_of_underflowing_on_inversion() {
        // The one behaviour every caller depends on: subtracting a LATER
        // instant from an earlier one yields zero rather than panicking or
        // wrapping. `Duration` has no negative representation, so an unchecked
        // subtraction here would be an arithmetic panic in production.
        let early = MonotonicInstant::from_origin_elapsed(Duration::from_secs(1));
        let late = MonotonicInstant::from_origin_elapsed(Duration::from_secs(5));
        assert_eq!(
            late.saturating_duration_since(early),
            Duration::from_secs(4)
        );
        assert_eq!(early.saturating_duration_since(late), Duration::ZERO);
        assert_eq!(early.elapsed_at(late), Duration::from_secs(4));
    }

    #[test]
    fn monotonic_add_saturates_instead_of_panicking() {
        // `Instant + Duration` panics when the sum is not representable; the
        // replacement must clamp, because these TTLs are app-supplied.
        let base = MonotonicInstant::ORIGIN;
        assert_eq!(
            base.saturating_add(Duration::from_secs(30)).since_origin(),
            Duration::from_secs(30)
        );
        assert_eq!(
            base.checked_add(Duration::MAX),
            Some(MonotonicInstant::from_origin_elapsed(Duration::MAX))
        );
        let high = MonotonicInstant::from_origin_elapsed(Duration::MAX);
        assert_eq!(high.checked_add(Duration::from_secs(1)), None);
        assert_eq!(high.saturating_add(Duration::from_secs(1)), high);
    }

    #[test]
    fn ticking_clock_monotonic_moves_in_lockstep_with_wall_time() {
        let start = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let clock = TickingClock::starting_at(start);
        assert_eq!(clock.monotonic(), MonotonicInstant::ORIGIN);

        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.monotonic().since_origin(), Duration::from_secs(90));
        assert_eq!(clock.now(), start + chrono::Duration::seconds(90));

        // A clone shares the instant, so it reports the same elapsed time — the
        // property `TestApp::with_clock` and `Sim::advance` both rely on.
        let clone = clock.clone();
        clock.advance(Duration::from_secs(10));
        assert_eq!(clone.monotonic().since_origin(), Duration::from_secs(100));
    }

    #[test]
    fn ticking_clock_monotonic_is_reproducible_across_instances() {
        // Two clocks constructed identically and stepped identically must agree
        // exactly — this is what makes a sim run replay from its seed.
        let start = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let a = TickingClock::starting_at(start);
        let b = TickingClock::starting_at(start);
        for step in [1u64, 60, 3_600, 86_400] {
            a.advance(Duration::from_secs(step));
            b.advance(Duration::from_secs(step));
        }
        assert_eq!(a.monotonic(), b.monotonic());
        assert_eq!(a.monotonic().since_origin(), Duration::from_secs(90_061));
    }

    #[test]
    fn fixed_clock_monotonic_never_advances() {
        // A clock whose wall time is pinned must not let elapsed time tick past,
        // or a test that pins the clock would still see real durations.
        let clock = FixedClock::at(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
        let first = clock.monotonic();
        std::thread::yield_now();
        assert_eq!(first, MonotonicInstant::ORIGIN);
        assert_eq!(clock.monotonic(), first);
    }

    #[test]
    fn system_clock_monotonic_is_real_and_non_decreasing() {
        let clock = SystemClock;
        let first = clock.monotonic();
        let second = clock.monotonic();
        assert!(
            second >= first,
            "the real monotonic clock must never go backwards"
        );
        // And it is the same timeline the free helper reads.
        assert!(monotonic_now() >= second);

        // The default body must read a REAL clock. Without this, a body that
        // simply returned `ORIGIN` would keep every other test in this file
        // green while silently making production elapsed-time measurements
        // always zero. Bounded spin rather than a sleep, so the test stays fast
        // and never depends on scheduler behaviour.
        let base = clock.monotonic();
        let mut advanced = base;
        for _ in 0..50_000_000u64 {
            advanced = clock.monotonic();
            if advanced > base {
                break;
            }
        }
        assert!(
            advanced > base,
            "ClockSource::monotonic's default body must advance with real time"
        );
    }

    #[test]
    fn a_custom_clock_keeps_compiling_and_gets_real_monotonic_by_default() {
        // Backward-compatibility guard: `monotonic()` ships with a default body,
        // so a downstream `impl ClockSource` that predates it still compiles and
        // still reports real process-monotonic time. If someone ever removes the
        // default body, this test stops compiling — which is the point.
        #[derive(Debug)]
        struct LegacyClock;
        impl ClockSource for LegacyClock {
            fn now(&self) -> DateTime<Utc> {
                Utc.with_ymd_and_hms(1999, 12, 31, 23, 59, 59).unwrap()
            }
        }
        let clock = LegacyClock;
        let first = clock.monotonic();
        assert!(clock.monotonic() >= first);
    }

    #[test]
    fn a_backwards_wall_clock_cannot_produce_a_negative_elapsed() {
        // A clock whose `now()` runs backwards (a crude NTP-jump model) must not
        // be able to corrupt an elapsed measurement taken through the seam.
        // `TickingClock` derives monotonic from its own start, and its
        // subtraction saturates, so the worst case is zero — never a panic.
        let start = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let clock = TickingClock::starting_at(start);
        clock.advance(Duration::from_secs(10));
        let later = clock.monotonic();
        let earlier = MonotonicInstant::from_origin_elapsed(Duration::from_secs(100));
        assert_eq!(later.saturating_duration_since(earlier), Duration::ZERO);
    }

    #[test]
    fn clock_unix_duration_zero_for_pre_epoch() {
        // Chrono timestamps before the epoch should not underflow.
        let pre_epoch = Utc.with_ymd_and_hms(1969, 12, 31, 23, 59, 59).unwrap();
        let clock = FixedClock::at(pre_epoch);
        assert_eq!(clock_unix_duration(&clock), std::time::Duration::ZERO);
    }

    #[test]
    fn ambient_clock_is_installed_nested_and_removed() {
        let pinned = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let later = Utc.with_ymd_and_hms(2021, 1, 1, 0, 0, 0).unwrap();
        let outer = install_ambient(Arc::new(FixedClock::at(pinned)));
        assert_eq!(ambient_now(), pinned);
        let inner = install_ambient(Arc::new(FixedClock::at(later)));
        assert_eq!(ambient_now(), later, "the newest clock wins");
        drop(inner);
        assert_eq!(ambient_now(), pinned);
        drop(outer);
        assert!(ambient_now() > later, "the system clock again");
    }

    #[test]
    fn ambient_guard_dropped_on_another_thread_uninstalls() {
        let pinned = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let guard = install_ambient(Arc::new(FixedClock::at(pinned)));
        assert_eq!(ambient_now(), pinned);
        std::thread::spawn(move || drop(guard)).join().unwrap();
        assert_ne!(
            ambient_now(),
            pinned,
            "the entry is skipped once its guard drops"
        );
    }
}
