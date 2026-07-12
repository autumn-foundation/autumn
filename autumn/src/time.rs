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

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

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
pub struct Clock(DateTime<Utc>);

impl Clock {
    /// Returns the UTC instant captured when this extractor was resolved.
    #[must_use]
    pub const fn now(&self) -> DateTime<Utc> {
        self.0
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
        Ok(Self(state.clock().now()))
    }
}

// ── System (real) clock ───────────────────────────────────────────────────────

/// Real wall-clock implementation of [`ClockSource`].
///
/// This is the default when no custom clock is configured. It delegates to
/// [`chrono::Utc::now`] and carries zero overhead compared to calling
/// `Utc::now()` directly.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl ClockSource for SystemClock {
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
pub struct TickingClock(Arc<Mutex<DateTime<Utc>>>);

impl TickingClock {
    /// Create a ticking clock starting at `dt`.
    #[must_use]
    pub fn starting_at(dt: DateTime<Utc>) -> Self {
        Self(Arc::new(Mutex::new(dt)))
    }

    /// Step this clock forward by `duration`.
    ///
    /// Sub-millisecond durations are truncated to zero (chrono's minimum resolution
    /// is microseconds). This method never panics.
    pub fn advance(&self, duration: std::time::Duration) {
        let mut guard = self
            .0
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
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ── Helpers for internal framework code ──────────────────────────────────────

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

    #[test]
    fn clock_unix_secs_exact_epoch() {
        let epoch = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
        let clock = FixedClock::at(epoch);
        assert_eq!(clock_unix_secs(&clock), 0);
    }

    #[test]
    fn clock_unix_duration_zero_for_pre_epoch() {
        // Chrono timestamps before the epoch should not underflow.
        let pre_epoch = Utc.with_ymd_and_hms(1969, 12, 31, 23, 59, 59).unwrap();
        let clock = FixedClock::at(pre_epoch);
        assert_eq!(clock_unix_duration(&clock), std::time::Duration::ZERO);
    }

    #[test]
    fn clock_unix_duration_exact_epoch_with_nanos() {
        // Test exact epoch with some nanos to ensure the true branch logic is exercised
        let epoch_with_nanos = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::nanoseconds(100);
        let clock = FixedClock::at(epoch_with_nanos);
        assert_eq!(clock_unix_duration(&clock), std::time::Duration::from_nanos(100));
    }

    #[test]
    fn clock_unix_duration_post_epoch() {
        let post_epoch = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 1).unwrap();
        let clock = FixedClock::at(post_epoch);
        assert_eq!(
            clock_unix_duration(&clock),
            std::time::Duration::from_secs(1)
        );
    }
}
