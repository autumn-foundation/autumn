//! Capped exponential backoff with full jitter (issue #3054).
//!
//! The delay before retry `n` (0 = first retry) is a random value in
//! `[0, min(cap, base * 2^n)]`. This is "full jitter" from
//! <https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/>.
//! Clients that fail together then retry at different times.
//!
//! The outbound HTTP client and all job backends use this module. Draw the
//! randomness from [`AppState::entropy`](crate::AppState::entropy), so a
//! [`Sim`](crate::sim::Sim) seed replays the same delays.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::time::Duration;

use crate::entropy::Entropy;

/// Default cap for outbound HTTP client retries, in ms
/// (`[http.client] max_backoff_ms`): 20 s.
pub const DEFAULT_HTTP_MAX_BACKOFF_MS: u64 = 20_000;

/// Default cap for job retries, in ms (`[jobs] max_backoff_ms`): 1 hour.
pub const DEFAULT_JOB_MAX_BACKOFF_MS: u64 = 3_600_000;

/// How far a `Retry-After` hint can push a retry past its backoff.
pub const RETRY_AFTER_SLACK: Duration = Duration::from_secs(5);

/// The largest delay for retry `retry` (0 = first retry):
/// `min(cap_ms, base_ms * 2^retry)`. The math saturates and never overflows.
#[must_use]
pub const fn ceiling_ms(base_ms: u64, cap_ms: u64, retry: u32) -> u64 {
    let factor = if retry >= u64::BITS {
        u64::MAX
    } else {
        1_u64 << retry
    };
    let raw = base_ms.saturating_mul(factor);
    if raw < cap_ms { raw } else { cap_ms }
}

/// A full-jitter delay for retry `retry` (0 = first retry), in ms.
///
/// The result is in `[0, ceiling_ms(base_ms, cap_ms, retry)]`.
#[must_use]
pub fn full_jitter_ms(entropy: &dyn Entropy, base_ms: u64, cap_ms: u64, retry: u32) -> u64 {
    let ceiling = ceiling_ms(base_ms, cap_ms, retry);
    let draw = entropy.next_u64();
    // `ceiling + 1` overflows only at `u64::MAX`, where any draw is in range.
    ceiling.checked_add(1).map_or(draw, |span| draw % span)
}

/// A full-jitter delay for retry `retry` (0 = first retry).
///
/// The result is in `[0, min(cap, base * 2^retry)]`, at millisecond
/// resolution.
#[must_use]
pub fn full_jitter(entropy: &dyn Entropy, base: Duration, cap: Duration, retry: u32) -> Duration {
    Duration::from_millis(full_jitter_ms(
        entropy,
        duration_ms(base),
        duration_ms(cap),
        retry,
    ))
}

/// The wait for a server `Retry-After` hint: `backoff + min(hint, 5 s)`.
///
/// The wait is in `[backoff, backoff + 5 s]`. It is never shorter than the
/// hint (up to 5 s). The jittered `backoff` stays in the wait, so callers
/// that get the same hint do not retry at the same instant. A long hint
/// cannot hold the caller for more than [`RETRY_AFTER_SLACK`] past the
/// backoff.
#[must_use]
pub fn retry_after_wait(hint: Duration, backoff: Duration) -> Duration {
    backoff.saturating_add(hint.min(RETRY_AFTER_SLACK))
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entropy::SeededEntropy;
    use proptest::prelude::*;

    /// Entropy that always returns one value.
    #[derive(Debug)]
    struct Fixed(u64);

    impl Entropy for Fixed {
        fn next_u64(&self) -> u64 {
            self.0
        }
        fn fill_bytes(&self, dest: &mut [u8]) {
            dest.fill(0);
        }
    }

    #[test]
    fn ceiling_doubles_then_caps() {
        assert_eq!(ceiling_ms(100, 20_000, 0), 100);
        assert_eq!(ceiling_ms(100, 20_000, 1), 200);
        assert_eq!(ceiling_ms(100, 20_000, 7), 12_800);
        assert_eq!(ceiling_ms(100, 20_000, 8), 20_000);
        assert_eq!(ceiling_ms(100, 20_000, u32::MAX), 20_000);
    }

    #[test]
    fn ceiling_saturates_without_a_cap() {
        assert_eq!(ceiling_ms(u64::MAX, u64::MAX, 3), u64::MAX);
        assert_eq!(ceiling_ms(1, u64::MAX, 63), 1 << 63);
        assert_eq!(ceiling_ms(1, u64::MAX, 64), u64::MAX);
        assert_eq!(ceiling_ms(0, u64::MAX, 64), 0);
    }

    #[test]
    fn full_jitter_reaches_both_ends() {
        assert_eq!(full_jitter_ms(&Fixed(0), 100, 1_000, 0), 0);
        assert_eq!(full_jitter_ms(&Fixed(100), 100, 1_000, 0), 100);
        assert_eq!(full_jitter_ms(&Fixed(101), 100, 1_000, 0), 0);
        assert_eq!(
            full_jitter_ms(&Fixed(u64::MAX), u64::MAX, u64::MAX, 0),
            u64::MAX
        );
    }

    #[test]
    fn full_jitter_is_zero_for_a_zero_base() {
        assert_eq!(full_jitter_ms(&Fixed(77), 0, 1_000, 5), 0);
    }

    #[test]
    fn full_jitter_spreads_one_retry() {
        let entropy = SeededEntropy::new(0x3054);
        let delays: std::collections::HashSet<u64> = (0..32)
            .map(|_| full_jitter_ms(&entropy, 1_000, 60_000, 0))
            .collect();
        assert!(
            delays.len() > 16,
            "32 draws gave only {} values",
            delays.len()
        );
    }

    #[test]
    fn full_jitter_replays_from_a_seed() {
        let a = SeededEntropy::new(7);
        let b = SeededEntropy::new(7);
        for retry in 0..16 {
            assert_eq!(
                full_jitter_ms(&a, 250, 3_600_000, retry),
                full_jitter_ms(&b, 250, 3_600_000, retry)
            );
        }
    }

    #[test]
    fn full_jitter_duration_matches_ms() {
        let delay = full_jitter(
            &Fixed(150),
            Duration::from_millis(100),
            Duration::from_secs(20),
            1,
        );
        assert_eq!(delay, Duration::from_millis(150));
        let huge = full_jitter(&Fixed(u64::MAX), Duration::MAX, Duration::MAX, 0);
        assert_eq!(huge, Duration::from_millis(u64::MAX));
    }

    #[test]
    fn retry_after_wait_adds_the_hint_to_the_backoff() {
        let backoff = Duration::from_millis(80);
        assert_eq!(retry_after_wait(Duration::ZERO, backoff), backoff);
        assert_eq!(
            retry_after_wait(Duration::from_secs(2), backoff),
            Duration::from_millis(2_080)
        );
        assert_eq!(
            retry_after_wait(Duration::from_secs(3_600), backoff),
            backoff + RETRY_AFTER_SLACK
        );
        assert_eq!(
            retry_after_wait(Duration::MAX, Duration::MAX),
            Duration::MAX
        );
    }

    proptest! {
        /// AC2 (#3054): the delay is in `[0, cap]` for every attempt, with no
        /// overflow.
        #[test]
        fn delay_is_within_zero_and_cap(
            seed in any::<u64>(),
            base in any::<u64>(),
            cap in any::<u64>(),
            retry in any::<u32>(),
        ) {
            let entropy = SeededEntropy::new(seed);
            let delay = full_jitter_ms(&entropy, base, cap, retry);
            prop_assert!(delay <= cap);
            prop_assert!(delay <= ceiling_ms(base, cap, retry));
        }

        /// The ceiling never decreases as the retry number grows.
        #[test]
        fn ceiling_is_monotonic(base in any::<u64>(), cap in any::<u64>(), retry in 0_u32..200) {
            prop_assert!(ceiling_ms(base, cap, retry) <= ceiling_ms(base, cap, retry + 1));
        }

        /// The `Duration` form stays in `[0, cap]` too.
        #[test]
        fn duration_delay_is_within_cap(
            seed in any::<u64>(),
            base_ms in any::<u64>(),
            cap_ms in any::<u64>(),
            retry in any::<u32>(),
        ) {
            let entropy = SeededEntropy::new(seed);
            let cap = Duration::from_millis(cap_ms);
            let delay = full_jitter(&entropy, Duration::from_millis(base_ms), cap, retry);
            prop_assert!(delay <= cap);
        }

        /// The wait is in `[backoff, backoff + 5 s]` and is never shorter
        /// than a hint of up to 5 s.
        #[test]
        fn hinted_wait_is_in_window(hint_ms in any::<u64>(), backoff_ms in any::<u64>()) {
            let backoff = Duration::from_millis(backoff_ms);
            let hint = Duration::from_millis(hint_ms);
            let wait = retry_after_wait(hint, backoff);
            prop_assert!(wait >= backoff);
            prop_assert!(wait >= hint.min(RETRY_AFTER_SLACK));
            prop_assert!(wait <= backoff.saturating_add(RETRY_AFTER_SLACK));
        }
    }
}
