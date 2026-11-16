//! A hedge that learns its own delay.
//!
//! [`HedgePolicy`] takes a fixed delay, and the advice is "pick it near the
//! p95 of the call". That number drifts: a cache warms, a region gets busy, a
//! dependency ships a slow release. [`AdaptiveHedge`] keeps the most recent
//! `window` latencies and hedges at their `quantile`, clamped to
//! `[floor, ceiling]`. Until `warm_up` samples are in, it uses `initial`.
//!
//! Each call records how long it took to answer. When a hedge wins, the time
//! recorded is the hedged answer, which is never shorter than the delay that
//! started it, so hedged calls cannot drag the learned delay down.
//!
//! Time comes from `tokio::time`, so a paused test clock replays it exactly.
//!
//! ```
//! use std::time::Duration;
//! use autumn_web::hedge::adaptive::AdaptiveHedge;
//!
//! # async fn demo() {
//! let hedge = AdaptiveHedge::new(Duration::from_millis(50));
//! let body = hedge.run(|| true, |_attempt| async { "ok" }).await;
//! assert_eq!(body, "ok");
//! assert_eq!(hedge.samples(), 1);
//! # }
//! ```

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::{HedgePolicy, hedged};

/// The default number of recent latencies kept.
pub const DEFAULT_WINDOW: usize = 256;
/// The default quantile hedged at.
pub const DEFAULT_QUANTILE: f64 = 0.95;
/// The default number of samples before the learned delay is used.
pub const DEFAULT_WARM_UP: usize = 20;

/// A [`HedgePolicy`] whose delay follows a quantile of recent latencies.
/// See the [module docs](self).
#[derive(Debug)]
pub struct AdaptiveHedge {
    initial: Duration,
    floor: Duration,
    ceiling: Duration,
    quantile: f64,
    window: usize,
    warm_up: usize,
    max_hedges: u32,
    samples: Mutex<VecDeque<Duration>>,
}

impl AdaptiveHedge {
    /// Hedge once, at `initial` until warmed up, then at the p95 of the last
    /// [`DEFAULT_WINDOW`] calls. The learned delay is never under 1 ms.
    #[must_use]
    pub const fn new(initial: Duration) -> Self {
        Self {
            initial,
            floor: Duration::from_millis(1),
            ceiling: Duration::MAX,
            quantile: DEFAULT_QUANTILE,
            window: DEFAULT_WINDOW,
            warm_up: DEFAULT_WARM_UP,
            max_hedges: 1,
            samples: Mutex::new(VecDeque::new()),
        }
    }

    /// Hedge at `quantile` of recent latencies. Clamped to `[0, 1]`; a NaN
    /// becomes the default.
    #[must_use]
    pub const fn with_quantile(mut self, quantile: f64) -> Self {
        self.quantile = if quantile.is_nan() {
            DEFAULT_QUANTILE
        } else {
            quantile.clamp(0.0, 1.0)
        };
        self
    }

    /// Keep the last `window` latencies (at least 1).
    #[must_use]
    pub fn with_window(mut self, window: usize) -> Self {
        self.window = window.max(1);
        self
    }

    /// Use `initial` until `warm_up` samples are in. A warm-up past the window
    /// ends once the window is full.
    #[must_use]
    pub const fn with_warm_up(mut self, warm_up: usize) -> Self {
        self.warm_up = warm_up;
        self
    }

    /// Keep the learned delay within `[floor, ceiling]`. A `ceiling` under
    /// `floor` is raised to it.
    #[must_use]
    pub fn with_bounds(mut self, floor: Duration, ceiling: Duration) -> Self {
        self.floor = floor;
        self.ceiling = ceiling.max(floor);
        self
    }

    /// Allow up to `max_hedges` extra copies, as
    /// [`HedgePolicy::with_max_hedges`].
    #[must_use]
    pub const fn with_max_hedges(mut self, max_hedges: u32) -> Self {
        self.max_hedges = max_hedges;
        self
    }

    /// Add one observed latency, dropping the oldest past the window.
    pub fn record(&self, latency: Duration) {
        let mut samples = self.samples.lock().unwrap_or_else(PoisonError::into_inner);
        if samples.len() >= self.window {
            samples.pop_front();
        }
        samples.push_back(latency);
    }

    /// How many latencies are held now.
    #[must_use]
    pub fn samples(&self) -> usize {
        self.samples
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The delay the next call hedges at.
    #[must_use]
    pub fn delay(&self) -> Duration {
        let mut sorted: Vec<Duration> = {
            let samples = self.samples.lock().unwrap_or_else(PoisonError::into_inner);
            if samples.len() < self.warm_up.clamp(1, self.window) {
                return self.initial;
            }
            samples.iter().copied().collect()
        };
        sorted.sort_unstable();
        // Nearest rank: the smallest sample with at least `quantile` of the
        // samples at or below it.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "the quantile is in [0, 1] and the window fits in f64"
        )]
        let rank = (self.quantile * sorted.len() as f64).ceil() as usize;
        let at = sorted[rank.clamp(1, sorted.len()) - 1];
        at.clamp(self.floor, self.ceiling)
    }

    /// The [`HedgePolicy`] for the next call.
    #[must_use]
    pub fn policy(&self) -> HedgePolicy {
        HedgePolicy::new(self.delay()).with_max_hedges(self.max_hedges)
    }

    /// Run a [`hedged`] call at the learned delay and record how long it took
    /// to answer.
    pub async fn run<T, Fut>(&self, allow: impl FnMut() -> bool, make: impl FnMut(u32) -> Fut) -> T
    where
        Fut: Future<Output = T>,
    {
        let start = tokio::time::Instant::now();
        let out = hedged(self.policy(), allow, make).await;
        self.record(start.elapsed());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn cold_hedge_uses_the_initial_delay() {
        let hedge = AdaptiveHedge::new(MS(40)).with_warm_up(3);
        hedge.record(MS(1));
        hedge.record(MS(2));
        assert_eq!(hedge.delay(), MS(40));
    }

    #[test]
    fn warm_hedge_uses_the_quantile() {
        let hedge = AdaptiveHedge::new(MS(40)).with_warm_up(1);
        for ms in 1..=100 {
            hedge.record(MS(ms));
        }
        assert_eq!(hedge.delay(), MS(95));
        let median = AdaptiveHedge::new(MS(40))
            .with_warm_up(1)
            .with_quantile(0.5);
        for ms in 1..=100 {
            median.record(MS(ms));
        }
        assert_eq!(median.delay(), MS(50));
    }

    #[test]
    fn window_forgets_old_latencies() {
        let hedge = AdaptiveHedge::new(MS(40))
            .with_warm_up(1)
            .with_window(10)
            .with_quantile(1.0);
        hedge.record(MS(900));
        for _ in 0..10 {
            hedge.record(MS(5));
        }
        assert_eq!(hedge.samples(), 10);
        assert_eq!(hedge.delay(), MS(5));
    }

    #[test]
    fn warm_up_past_the_window_still_learns() {
        let hedge = AdaptiveHedge::new(MS(40)).with_window(10);
        for _ in 0..10 {
            hedge.record(MS(7));
        }
        assert_eq!(hedge.delay(), MS(7));
    }

    #[test]
    fn learned_delay_is_clamped() {
        let hedge = AdaptiveHedge::new(MS(40))
            .with_warm_up(1)
            .with_bounds(MS(10), MS(100));
        hedge.record(MS(1));
        assert_eq!(hedge.delay(), MS(10));
        hedge.record(MS(5_000));
        hedge.record(MS(5_000));
        assert_eq!(hedge.delay(), MS(100));
    }

    #[test]
    fn odd_settings_are_tamed() {
        let hedge = AdaptiveHedge::new(MS(40))
            .with_quantile(f64::NAN)
            .with_window(0)
            .with_bounds(MS(10), MS(1))
            .with_warm_up(0);
        hedge.record(MS(3));
        hedge.record(MS(30));
        assert_eq!(hedge.samples(), 1);
        assert_eq!(hedge.delay(), MS(10));
        assert_eq!(hedge.policy().max_hedges(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn run_learns_from_the_calls_it_makes() {
        let hedge = AdaptiveHedge::new(MS(500)).with_warm_up(5);
        for _ in 0..5 {
            hedge
                .run(
                    || true,
                    |_| async {
                        tokio::time::sleep(MS(20)).await;
                    },
                )
                .await;
        }
        assert_eq!(hedge.delay(), MS(20));

        // A call that stalls is now rescued after 20 ms, not 500 ms.
        let start = tokio::time::Instant::now();
        let winner = hedge
            .run(
                || true,
                |n| async move {
                    tokio::time::sleep(if n == 0 { MS(10_000) } else { MS(5) }).await;
                    n
                },
            )
            .await;
        assert_eq!(winner, 1);
        assert_eq!(start.elapsed(), MS(25));
    }

    #[tokio::test(start_paused = true)]
    async fn hedged_wins_never_pull_the_delay_below_itself() {
        let hedge = AdaptiveHedge::new(MS(50))
            .with_warm_up(1)
            .with_quantile(0.0);
        hedge.record(MS(50));
        for _ in 0..20 {
            hedge
                .run(
                    || true,
                    |n| async move {
                        tokio::time::sleep(if n == 0 { MS(10_000) } else { MS(1) }).await;
                    },
                )
                .await;
        }
        assert!(hedge.delay() >= MS(50), "{:?}", hedge.delay());
    }
}
