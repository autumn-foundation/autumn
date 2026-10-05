//! Request deadlines (issue #3058).
//!
//! The inbound request timeout sets a [`Deadline`] for the handler task.
//! Framework calls read it with [`Deadline::current`]:
//!
//! - The outbound HTTP client limits each attempt to the time left. It does
//!   not start a retry that has no time left.
//! - The client sends the time left downstream in [`DEADLINE_HEADER`].
//! - `Db` limits the connection wait to the time left.
//!
//! A task that you start with `tokio::spawn` does not get the deadline. Use
//! [`Deadline::scope`] to give it one. Use [`bounded`] to limit other calls,
//! for example Redis calls.
//!
//! ```rust,no_run
//! use autumn_web::deadline::Deadline;
//!
//! # async fn demo() {
//! if let Some(deadline) = Deadline::current() {
//!     tracing::info!(remaining_ms = deadline.remaining().as_millis(), "time left");
//! }
//! # }
//! ```

// autumn-determinism-gate: read time only through `tokio::time::Instant`, so
// a paused runtime and `#[sim_test]` control it. See CONTRIBUTING.md
// "Determinism seam gate" (issue #1797).
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// The header that carries the time left, in whole milliseconds.
///
/// The value is relative, not a timestamp, so clock skew has no effect. The
/// outbound client sends it when a deadline is set. The server reads it only
/// when `server.timeouts.accept_deadline_header` is `true`, and it can only
/// make the route deadline shorter.
pub const DEADLINE_HEADER: &str = "x-autumn-deadline-ms";

tokio::task_local! {
    static CURRENT: Option<Deadline>;
}

/// A point in time after which the request has no value.
///
/// Uses [`tokio::time::Instant`], so a paused runtime and `#[sim_test]`
/// control it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// A deadline `timeout` from now. A very large `timeout` is capped.
    #[must_use]
    pub fn after(timeout: Duration) -> Self {
        Self {
            at: crate::time_math::saturating_tokio_deadline(Instant::now(), timeout),
        }
    }

    /// A deadline at `at`.
    #[must_use]
    pub const fn at(at: Instant) -> Self {
        Self { at }
    }

    /// The deadline of the current task, if one is set.
    #[must_use]
    pub fn current() -> Option<Self> {
        CURRENT.try_with(|deadline| *deadline).ok().flatten()
    }

    /// The instant of the deadline.
    #[must_use]
    pub const fn instant(self) -> Instant {
        self.at
    }

    /// The time left. Zero when the deadline has passed.
    #[must_use]
    pub fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// `true` when no time is left.
    #[must_use]
    pub fn is_expired(self) -> bool {
        self.remaining().is_zero()
    }

    /// `limit`, or the time left if that is shorter.
    #[must_use]
    pub fn clamp(self, limit: Duration) -> Duration {
        limit.min(self.remaining())
    }

    /// Run `future` with this deadline as [`current`](Self::current).
    ///
    /// If a deadline is set already, the earlier one applies. This does not
    /// stop `future` at the deadline. Use [`bounded`] for that.
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        self.scope_future(future).await
    }

    /// Like [`scope`](Self::scope), but returns a named future type.
    pub fn scope_future<F: Future>(self, future: F) -> DeadlineScope<F> {
        CURRENT.scope(Some(self.nested()), future)
    }

    /// Like [`scope`](Self::scope), for code that is not async.
    pub fn sync_scope<R>(self, f: impl FnOnce() -> R) -> R {
        CURRENT.sync_scope(Some(self.nested()), f)
    }

    /// This deadline, or the current one when that is earlier.
    fn nested(self) -> Self {
        Self::current().map_or(self, |current| current.min(self))
    }
}

/// Parse a [`DEADLINE_HEADER`] value. `None` when it is not a whole number
/// of milliseconds.
#[must_use]
pub fn parse_header(value: &http::HeaderValue) -> Option<Duration> {
    let text = value.to_str().ok()?.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().map(Duration::from_millis)
}

/// The future of [`Deadline::scope_future`].
pub type DeadlineScope<F> = tokio::task::futures::TaskLocalFuture<Option<Deadline>, F>;

/// Run `future` with no deadline, as a separate process would. A simulated
/// host uses it: only the [`DEADLINE_HEADER`] carries the deadline to it.
#[cfg(feature = "http-client")]
pub(crate) async fn unscoped<F: Future>(future: F) -> F::Output {
    CURRENT.scope(None, future).await
}

/// The error of [`bounded`]: the deadline passed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("deadline exceeded")]
pub struct DeadlineExceeded;

/// Run `future`, but stop it at the current deadline.
///
/// With no deadline set, this runs `future` to the end.
///
/// # Errors
///
/// [`DeadlineExceeded`] when the deadline passes first.
pub async fn bounded<F: Future>(future: F) -> Result<F::Output, DeadlineExceeded> {
    match Deadline::current() {
        Some(deadline) => tokio::time::timeout_at(deadline.instant(), future)
            .await
            .map_err(|_elapsed| DeadlineExceeded),
        None => Ok(future.await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn remaining_counts_down_to_zero() {
        let deadline = Deadline::after(Duration::from_secs(5));
        assert_eq!(deadline.remaining(), Duration::from_secs(5));
        assert!(!deadline.is_expired());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(deadline.remaining(), Duration::from_secs(3));
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(deadline.remaining(), Duration::ZERO);
        assert!(deadline.is_expired());
    }

    #[tokio::test(start_paused = true)]
    async fn a_huge_timeout_does_not_overflow() {
        let deadline = Deadline::after(Duration::MAX);
        assert!(deadline.remaining() > Duration::from_secs(60 * 60 * 24 * 365));
    }

    #[tokio::test(start_paused = true)]
    async fn clamp_and_min_take_the_shorter_value() {
        let deadline = Deadline::after(Duration::from_secs(5));
        assert_eq!(
            deadline.clamp(Duration::from_secs(30)),
            Duration::from_secs(5)
        );
        assert_eq!(
            deadline.clamp(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        let earlier = Deadline::after(Duration::from_secs(1));
        assert_eq!(deadline.min(earlier), earlier);
        assert_eq!(earlier.min(deadline), earlier);
    }

    #[tokio::test(start_paused = true)]
    async fn current_is_set_only_inside_a_scope() {
        assert_eq!(Deadline::current(), None);
        let deadline = Deadline::after(Duration::from_secs(5));
        let seen = deadline.scope(async { Deadline::current() }).await;
        assert_eq!(seen, Some(deadline));
        assert_eq!(deadline.sync_scope(Deadline::current), Some(deadline));
        assert_eq!(Deadline::current(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_nested_scope_keeps_the_earlier_deadline() {
        let outer = Deadline::after(Duration::from_secs(1));
        let inner = Deadline::after(Duration::from_secs(5));
        let seen = outer
            .scope(async { inner.scope(async { Deadline::current() }).await })
            .await;
        assert_eq!(seen, Some(outer), "a later deadline cannot extend");
        let seen = inner
            .scope(async { outer.scope(async { Deadline::current() }).await })
            .await;
        assert_eq!(seen, Some(outer), "an earlier deadline shortens");
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_stops_at_the_deadline() {
        let deadline = Deadline::after(Duration::from_secs(1));
        let start = Instant::now();
        let outcome = deadline
            .scope(bounded(tokio::time::sleep(Duration::from_secs(10))))
            .await;
        assert_eq!(outcome, Err(DeadlineExceeded));
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert_eq!(bounded(async { 7 }).await, Ok(7), "no deadline, no limit");
    }

    #[tokio::test(start_paused = true)]
    async fn unscoped_clears_the_deadline() {
        let deadline = Deadline::after(Duration::from_secs(1));
        let seen = deadline
            .scope(unscoped(async { Deadline::current() }))
            .await;
        assert_eq!(seen, None);
    }

    #[test]
    fn header_values_parse_as_whole_milliseconds() {
        let parse = |raw: &str| parse_header(&http::HeaderValue::from_str(raw).unwrap());
        assert_eq!(parse("1500"), Some(Duration::from_millis(1500)));
        assert_eq!(parse("0"), Some(Duration::ZERO));
        assert_eq!(parse(" 20 "), Some(Duration::from_millis(20)));
        assert_eq!(parse("-1"), None);
        assert_eq!(parse("1.5"), None);
        assert_eq!(parse("soon"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("99999999999999999999999"), None);
    }
}
