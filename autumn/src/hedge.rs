//! Hedged requests: cut tail latency by racing a second attempt.
//!
//! Start an operation. If it has not answered after `delay`, start a second
//! copy and take whichever answers first. The slower copy is dropped, which
//! cancels it. This is the "hedged request" from Dean and Barroso,
//! *The Tail at Scale*. Pick `delay` near the p95 of the call, and only about
//! 5 % of calls send a second copy while the slowest ones get a fast second
//! chance.
//!
//! Only hedge calls that are safe to run twice (reads, or writes that carry an
//! idempotency key). The `allow` hook runs before each extra copy, so a
//! retry budget or a load check can veto it
//! and keep hedging from adding load to an upstream that is already struggling.
//!
//! Time comes from `tokio::time`, so a paused test clock or a
//! [`Sim`](crate::sim::Sim) replays hedges exactly.
//!
//! ```
//! use std::time::Duration;
//! use autumn_web::hedge::{HedgePolicy, hedged};
//!
//! # async fn demo() {
//! let policy = HedgePolicy::new(Duration::from_millis(50));
//! let body = hedged(policy, || true, |_attempt| async { "ok" }).await;
//! assert_eq!(body, "ok");
//! # }
//! ```

use std::future::Future;
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;

/// When and how often to send extra copies of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HedgePolicy {
    delay: Duration,
    max_hedges: u32,
}

impl HedgePolicy {
    /// One extra copy, sent after `delay` of silence.
    #[must_use]
    pub const fn new(delay: Duration) -> Self {
        Self {
            delay,
            max_hedges: 1,
        }
    }

    /// One extra copy, sent after the observed `q` quantile of `tracker`
    /// (`0.95` is the usual pick). Until the tracker has a sample, `fallback`
    /// is the delay.
    #[must_use]
    pub fn from_tracker(
        tracker: &crate::latency_tracker::LatencyTracker,
        q: f64,
        fallback: Duration,
    ) -> Self {
        Self::new(tracker.quantile(q).unwrap_or(fallback))
    }

    /// Allow up to `max_hedges` extra copies. Each one waits a further `delay`
    /// after the last. `0` turns hedging off.
    #[must_use]
    pub const fn with_max_hedges(mut self, max_hedges: u32) -> Self {
        self.max_hedges = max_hedges;
        self
    }

    /// The silence before an extra copy starts.
    #[must_use]
    pub const fn delay(&self) -> Duration {
        self.delay
    }

    /// The most extra copies one call may start.
    #[must_use]
    pub const fn max_hedges(&self) -> u32 {
        self.max_hedges
    }
}

/// Run `make(0)`; while nothing has answered, start `make(1)`, `make(2)`, …
/// (one per `policy.delay()`, up to `policy.max_hedges()`), and return the
/// first output. Every copy still in flight is dropped.
///
/// `allow` is asked before each extra copy. When it says no, hedging stops for
/// this call and the copies already running carry on.
pub async fn hedged<T, Fut>(
    policy: HedgePolicy,
    mut allow: impl FnMut() -> bool,
    mut make: impl FnMut(u32) -> Fut,
) -> T
where
    Fut: Future<Output = T>,
{
    let mut in_flight = FuturesUnordered::new();
    in_flight.push(make(0));
    let mut started = 0;
    while started < policy.max_hedges {
        tokio::select! {
            out = in_flight.next() => {
                // `in_flight` is never empty here: it holds the first copy.
                if let Some(out) = out {
                    return out;
                }
            }
            () = tokio::time::sleep(policy.delay) => {
                if allow() {
                    started += 1;
                    in_flight.push(make(started));
                } else {
                    break;
                }
            }
        }
    }
    loop {
        if let Some(out) = in_flight.next().await {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[tokio::test(start_paused = true)]
    async fn fast_first_copy_never_hedges() {
        let made = Arc::new(AtomicU32::new(0));
        let m = made.clone();
        let out = hedged(
            HedgePolicy::new(MS(50)),
            || true,
            move |n| {
                m.fetch_add(1, Ordering::SeqCst);
                async move {
                    tokio::time::sleep(MS(10)).await;
                    n
                }
            },
        )
        .await;
        assert_eq!(out, 0);
        assert_eq!(made.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_first_copy_loses_to_the_hedge() {
        let start = tokio::time::Instant::now();
        let out = hedged(
            HedgePolicy::new(MS(50)),
            || true,
            |n| async move {
                // The first copy takes 1 s; the hedge takes 10 ms.
                tokio::time::sleep(if n == 0 { MS(1_000) } else { MS(10) }).await;
                n
            },
        )
        .await;
        assert_eq!(out, 1);
        assert_eq!(start.elapsed(), MS(60));
    }

    #[tokio::test(start_paused = true)]
    async fn denied_hedge_waits_for_the_first_copy() {
        let made = Arc::new(AtomicU32::new(0));
        let m = made.clone();
        let out = hedged(
            HedgePolicy::new(MS(50)),
            || false,
            move |n| {
                m.fetch_add(1, Ordering::SeqCst);
                async move {
                    tokio::time::sleep(MS(500)).await;
                    n
                }
            },
        )
        .await;
        assert_eq!(out, 0);
        assert_eq!(made.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hedges_are_capped_and_spaced_by_delay() {
        let made = Arc::new(AtomicU32::new(0));
        let m = made.clone();
        let start = tokio::time::Instant::now();
        let out = hedged(
            HedgePolicy::new(MS(10)).with_max_hedges(2),
            || true,
            move |n| {
                m.fetch_add(1, Ordering::SeqCst);
                async move {
                    // Only the last copy is quick.
                    tokio::time::sleep(if n == 2 { MS(5) } else { MS(10_000) }).await;
                    n
                }
            },
        )
        .await;
        assert_eq!(out, 2);
        assert_eq!(made.load(Ordering::SeqCst), 3);
        assert_eq!(start.elapsed(), MS(25));
    }

    #[test]
    fn from_tracker_uses_the_quantile_or_the_fallback() {
        let t = crate::latency_tracker::LatencyTracker::new();
        assert_eq!(HedgePolicy::from_tracker(&t, 0.95, MS(7)).delay(), MS(7));
        for _ in 0..100 {
            t.record(MS(40));
        }
        let d = HedgePolicy::from_tracker(&t, 0.95, MS(7)).delay();
        assert!(d >= MS(40) && d <= MS(50), "{d:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn zero_max_hedges_is_a_plain_call() {
        let out = hedged(
            HedgePolicy::new(MS(1)).with_max_hedges(0),
            || panic!("must not ask"),
            |n| async move {
                tokio::time::sleep(MS(100)).await;
                n
            },
        )
        .await;
        assert_eq!(out, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn losing_copy_is_dropped() {
        struct Flag(Arc<AtomicU32>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicU32::new(0));
        let d = dropped.clone();
        let out = hedged(
            HedgePolicy::new(MS(5)),
            || true,
            move |n| {
                let guard = Flag(d.clone());
                async move {
                    let _guard = guard;
                    tokio::time::sleep(if n == 0 { MS(1_000) } else { MS(1) }).await;
                    n
                }
            },
        )
        .await;
        assert_eq!(out, 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }
}
