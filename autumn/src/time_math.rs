//! Saturating time / duration arithmetic for request-path modules.
//!
//! `std` and `jiff` both expose *panicking* operators for the everyday
//! "deadline = now + ttl" shape:
//!
//! * `Instant + Duration` panics when the sum is not representable by the
//!   platform clock (`Duration::MAX`, `Duration::from_secs(u64::MAX)`, …).
//! * `Timestamp + SignedDuration` panics when the sum leaves
//!   `[Timestamp::MIN, Timestamp::MAX]`.
//! * A `u64` second count does not fit `SignedDuration::from_secs`'s `i64`.
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

use jiff::{SignedDuration, Timestamp};

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

/// Compute an expiry [`tokio::time::Instant`] for `ttl`, saturating instead of
/// panicking on overflow.
///
/// The tokio-timeline twin of [`saturating_deadline`], with the identical clamp
/// policy. It exists because a deadline whose counterparty is
/// `tokio::time::sleep` must live on tokio's timeline (and is virtualized for
/// free under a paused runtime), while `tokio::time::Instant` is a distinct type
/// from `std::time::Instant` and cannot reuse the same function.
#[must_use]
// Only the `redis` job worker calls this today. It stays ungated so other tokio-timeline
// deadlines reuse the clamp policy instead of re-deriving it.
#[cfg_attr(not(feature = "redis"), allow(dead_code))]
pub fn saturating_tokio_deadline(now: tokio::time::Instant, ttl: Duration) -> tokio::time::Instant {
    now.checked_add(ttl).unwrap_or_else(|| {
        now.checked_add(Duration::from_secs(SATURATING_DEADLINE_HORIZON_SECS))
            .unwrap_or(now)
    })
}

/// Add `delta` to `now`, clamping to [`Timestamp`]'s representable range
/// instead of panicking.
///
/// `Timestamp + SignedDuration` panics on overflow. When the sum leaves
/// `[Timestamp::MIN, Timestamp::MAX]` we saturate *in the direction of the
/// delta's sign*, so a huge positive TTL becomes "never expires" and a huge
/// negative offset becomes "the beginning of time" — both of which preserve
/// the comparison the caller was about to make.
pub fn saturating_ts_add(now: Timestamp, delta: SignedDuration) -> Timestamp {
    now.checked_add(delta).unwrap_or_else(|_| {
        if delta.is_negative() {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        }
    })
}

/// Build a [`SignedDuration`] from a `u64` second count, clamping to
/// [`SignedDuration::MAX`] instead of wrapping.
///
/// Counts above `i64::MAX` do not fit a `SignedDuration`'s seconds. Pair
/// this with [`saturating_ts_add`] at the addition site.
pub fn saturating_duration_secs(secs: u64) -> SignedDuration {
    i64::try_from(secs).map_or(SignedDuration::MAX, SignedDuration::from_secs)
}

/// Kani proofs over the saturating time helpers. Run with
/// `cargo kani -p autumn-web --lib`.
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Any second count gives a `SignedDuration`, never a panic. Counts that
    /// fit an `i64` are exact; the rest clamp to `SignedDuration::MAX`.
    #[kani::proof]
    fn duration_secs_never_panics() {
        let secs: u64 = kani::any();
        let delta = saturating_duration_secs(secs);
        assert!(!delta.is_negative());
        if let Ok(exact) = i64::try_from(secs) {
            assert_eq!(delta.as_secs(), exact);
            assert_eq!(delta.subsec_nanos(), 0);
        } else {
            assert_eq!(delta, SignedDuration::MAX);
        }
    }
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
    fn saturating_tokio_deadline_matches_the_std_clamp_policy() {
        // Exact for ordinary TTLs, clamped for a pathological one: the redis maintenance
        // interval comes from app config, and an absurd value must not crash the worker (#1611).
        let now = tokio::time::Instant::now();
        assert_eq!(
            saturating_tokio_deadline(now, Duration::from_secs(60)),
            now + Duration::from_secs(60)
        );
        assert_eq!(saturating_tokio_deadline(now, Duration::ZERO), now);
        for extreme in [Duration::MAX, Duration::from_secs(u64::MAX)] {
            let deadline = saturating_tokio_deadline(now, extreme);
            assert!(
                deadline > now + Duration::from_secs(3_600),
                "a clamped deadline must still be far in the future"
            );
        }
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
    fn saturating_ts_add_is_exact_for_ordinary_deltas() {
        let now = Timestamp::UNIX_EPOCH;
        assert_eq!(
            saturating_ts_add(now, SignedDuration::from_secs(86_400)),
            now + SignedDuration::from_secs(86_400)
        );
        assert_eq!(
            saturating_ts_add(now, SignedDuration::from_secs(-86_400)),
            now - SignedDuration::from_secs(86_400)
        );
    }

    #[test]
    fn saturating_ts_add_clamps_in_the_direction_of_the_delta() {
        let now = Timestamp::UNIX_EPOCH;
        assert_eq!(saturating_ts_add(now, SignedDuration::MAX), Timestamp::MAX);
        assert_eq!(saturating_ts_add(now, SignedDuration::MIN), Timestamp::MIN);
        assert_eq!(
            saturating_ts_add(Timestamp::MAX, SignedDuration::from_secs(1)),
            Timestamp::MAX
        );
        assert_eq!(
            saturating_ts_add(Timestamp::MIN, SignedDuration::from_secs(-1)),
            Timestamp::MIN
        );
    }

    #[test]
    fn saturating_duration_secs_is_exact_below_the_ceiling() {
        assert_eq!(saturating_duration_secs(0), SignedDuration::ZERO);
        assert_eq!(
            saturating_duration_secs(86_400),
            SignedDuration::from_secs(86_400)
        );
        assert_eq!(
            saturating_duration_secs(i64::MAX.cast_unsigned()),
            SignedDuration::from_secs(i64::MAX)
        );
    }

    #[test]
    fn saturating_duration_secs_clamps_instead_of_wrapping() {
        for extreme in [u64::MAX, i64::MAX.cast_unsigned() + 1] {
            assert_eq!(saturating_duration_secs(extreme), SignedDuration::MAX);
        }
    }
}
