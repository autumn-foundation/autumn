//! Saturating time / duration arithmetic for request-path modules.
//!
//! `std` and `chrono` both expose *panicking* operators for the everyday
//! "deadline = now + ttl" shape:
//!
//! * `Instant + Duration` panics when the sum is not representable by the
//!   platform clock (`Duration::MAX`, `Duration::from_secs(u64::MAX)`, …).
//! * `DateTime<Utc> + TimeDelta` panics via `expect("`DateTime + TimeDelta`
//!   overflowed")`.
//! * `TimeDelta::seconds` panics above `i64::MAX / 1_000` seconds.
//!
//! Every one of those inputs is routinely *configuration* (a TTL, a retry
//! window, a poll interval), which means a bad config value crashes a worker
//! or a request thread instead of degrading. The helpers here clamp to a
//! far-future horizon instead, so a pathological TTL yields an effectively
//! non-expiring deadline — the safe direction for expiry, uniqueness holds
//! and maintenance throttles alike.
//!
//! Crate-internal on purpose: these are the framework's own guard rails, not
//! a public time API.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};

/// Clamp horizon (~10 years, `10 * 365 * 24 * 3600`) used when a
/// caller-supplied TTL would overflow `Instant + Duration`. This constant is
/// itself always representable when added to a fresh `Instant`, so it can
/// never re-trigger the overflow.
pub const SATURATING_DEADLINE_HORIZON_SECS: u64 = 315_360_000;

/// Compute an expiry `Instant` for `ttl`, saturating instead of panicking on
/// overflow.
///
/// `now + ttl` panics when the sum is not representable by the platform
/// clock — a pathological `ttl` such as `Duration::MAX` or
/// `Duration::from_secs(u64::MAX)` (entirely config-influenceable) triggers
/// this. Instead of panicking we clamp the deadline to ~10 years out (far
/// enough that the entry is effectively non-expiring), falling back to `now`
/// only in the astronomically unlikely event that even the clamped horizon is
/// not representable.
pub fn saturating_deadline(now: Instant, ttl: Duration) -> Instant {
    now.checked_add(ttl).unwrap_or_else(|| {
        now.checked_add(Duration::from_secs(SATURATING_DEADLINE_HORIZON_SECS))
            .unwrap_or(now)
    })
}

/// Add `delta` to `now`, clamping to chrono's representable range instead of
/// panicking.
///
/// `DateTime<Utc> + TimeDelta` is `checked_add_signed(..).expect(..)`. When
/// the sum leaves `[DateTime::MIN_UTC, DateTime::MAX_UTC]` we saturate *in the
/// direction of the delta's sign*, so a huge positive TTL becomes "never
/// expires" and a huge negative offset becomes "the beginning of time" —
/// both of which preserve the comparison the caller was about to make.
pub fn saturating_dt_add(now: DateTime<Utc>, delta: TimeDelta) -> DateTime<Utc> {
    now.checked_add_signed(delta).unwrap_or_else(|| {
        if delta < TimeDelta::zero() {
            DateTime::<Utc>::MIN_UTC
        } else {
            DateTime::<Utc>::MAX_UTC
        }
    })
}

/// Build a [`TimeDelta`] from a `u64` second count, clamping to
/// [`TimeDelta::MAX`] instead of panicking.
///
/// `TimeDelta::seconds` panics above `i64::MAX / 1_000` seconds, so the
/// common `TimeDelta::seconds(i64::try_from(ttl_secs).unwrap_or(i64::MAX))`
/// idiom panics *precisely* on the saturating branch it was written to
/// protect. Pair this with [`saturating_dt_add`] at the addition site.
pub fn saturating_time_delta_secs(secs: u64) -> TimeDelta {
    i64::try_from(secs)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .unwrap_or(TimeDelta::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_deadline_is_exact_for_ordinary_ttls() {
        let now = Instant::now();
        assert_eq!(
            saturating_deadline(now, Duration::from_secs(60)),
            now + Duration::from_secs(60)
        );
        assert_eq!(saturating_deadline(now, Duration::ZERO), now);
    }

    #[test]
    fn saturating_deadline_clamps_instead_of_panicking() {
        let now = Instant::now();
        for extreme in [Duration::MAX, Duration::from_secs(u64::MAX)] {
            let deadline = saturating_deadline(now, extreme);
            assert!(
                deadline > now,
                "a clamped deadline must still be in the future"
            );
        }
    }

    #[test]
    fn saturating_dt_add_is_exact_for_ordinary_deltas() {
        let now = DateTime::<Utc>::UNIX_EPOCH;
        assert_eq!(
            saturating_dt_add(now, TimeDelta::seconds(86_400)),
            now + TimeDelta::seconds(86_400)
        );
        assert_eq!(
            saturating_dt_add(now, TimeDelta::seconds(-86_400)),
            now - TimeDelta::seconds(86_400)
        );
    }

    #[test]
    fn saturating_dt_add_clamps_in_the_direction_of_the_delta() {
        let now = DateTime::<Utc>::UNIX_EPOCH;
        assert_eq!(
            saturating_dt_add(now, TimeDelta::MAX),
            DateTime::<Utc>::MAX_UTC
        );
        assert_eq!(
            saturating_dt_add(now, TimeDelta::MIN),
            DateTime::<Utc>::MIN_UTC
        );
        assert_eq!(
            saturating_dt_add(DateTime::<Utc>::MAX_UTC, TimeDelta::seconds(1)),
            DateTime::<Utc>::MAX_UTC
        );
        assert_eq!(
            saturating_dt_add(DateTime::<Utc>::MIN_UTC, TimeDelta::seconds(-1)),
            DateTime::<Utc>::MIN_UTC
        );
    }

    #[test]
    fn saturating_time_delta_secs_is_exact_below_the_ceiling() {
        assert_eq!(saturating_time_delta_secs(0), TimeDelta::zero());
        assert_eq!(
            saturating_time_delta_secs(86_400),
            TimeDelta::seconds(86_400)
        );
        // The largest exactly-representable second count.
        let ceiling = i64::MAX / 1_000;
        assert_eq!(
            saturating_time_delta_secs(
                u64::try_from(ceiling).expect("the ceiling is a positive i64")
            ),
            TimeDelta::seconds(ceiling)
        );
    }

    #[test]
    fn saturating_time_delta_secs_clamps_instead_of_panicking() {
        for extreme in [
            u64::MAX,
            u64::try_from(i64::MAX).expect("i64::MAX fits in u64"),
            // One second past the `i64::MAX / 1_000` ceiling.
            u64::try_from(i64::MAX / 1_000).expect("positive") + 1,
        ] {
            assert_eq!(saturating_time_delta_secs(extreme), TimeDelta::MAX);
        }
    }
}
