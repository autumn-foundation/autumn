//! Runtime enforcement of a grant's `rate` cap.
//!
//! The MCP dispatcher asks [`RateLimiter::check`] before each governed tool
//! call. Each grant has one token bucket. State is per process: with N
//! instances the real cap is up to N times the declared one.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};

/// A parsed `rate` string: `count` calls per `period_secs`.
///
/// `count` is a float so that every digit string the grammar accepts parses.
/// A count past [`MAX_COUNT`] is clamped: it is no cap in practice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateCap {
    pub count: f64,
    pub period_secs: u32,
}

/// The largest count a bucket holds. Beyond this a float loses whole calls.
const MAX_COUNT: f64 = 1e15;

/// Parse a `rate` string that passed [`rate_is_wellformed`](super::rate_is_wellformed).
/// `None` for anything else, so the caller can refuse.
pub fn parse_rate(rate: &str) -> Option<RateCap> {
    let (count, unit) = rate.split_once('/')?;
    let period_secs = match unit {
        "s" | "sec" => 1,
        "min" => 60,
        "hour" => 3_600,
        "day" => 86_400,
        _ => return None,
    };
    let count = count.parse::<f64>().ok().filter(|count| *count >= 1.0)?;
    let count = count.min(MAX_COUNT);
    Some(RateCap { count, period_secs })
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_refill: DateTime<Utc>,
}

/// One token bucket per grant, keyed by its name and declaration site.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Take one call from the bucket of the grant declared at `location` under
    /// `name`. Two grants with one name in different places stay apart.
    /// `Err` holds the seconds to wait.
    #[allow(clippy::significant_drop_tightening)]
    pub fn check(
        &self,
        name: &str,
        location: &str,
        cap: RateCap,
        now: DateTime<Utc>,
    ) -> Result<(), u64> {
        let burst = cap.count;
        let per_sec = burst / f64::from(cap.period_secs);
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bucket = buckets
            .entry(format!("{name}\0{location}"))
            .or_insert(Bucket {
                tokens: burst,
                last_refill: now,
            });
        // A clock that moved backwards refills nothing, and the refill time
        // stays put: moving it back would count the same seconds twice.
        let elapsed = (now - bucket.last_refill)
            .to_std()
            .map_or(0.0, |d| d.as_secs_f64());
        bucket.tokens = elapsed.mul_add(per_sec, bucket.tokens).min(burst);
        bucket.last_refill = bucket.last_refill.max(now);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let wait = ((1.0 - bucket.tokens) / per_sec).ceil().max(1.0) as u64;
            Err(wait)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).expect("valid timestamp")
    }

    const TWO_PER_MIN: RateCap = RateCap {
        count: 2.0,
        period_secs: 60,
    };

    #[test]
    fn every_well_formed_rate_parses() {
        for (rate, count, period_secs) in [
            ("10/s", 10.0, 1),
            ("5/sec", 5.0, 1),
            ("2/min", 2.0, 60),
            ("100/hour", 100.0, 3_600),
            ("1/day", 1.0, 86_400),
        ] {
            assert_eq!(parse_rate(rate), Some(RateCap { count, period_secs }));
            assert!(super::super::rate_is_wellformed(rate));
        }
    }

    #[test]
    fn a_malformed_rate_does_not_parse() {
        for rate in ["", "0/min", "min", "5/", "5/week", "-1/min"] {
            assert_eq!(parse_rate(rate), None, "{rate:?}");
        }
    }

    #[test]
    fn every_digit_string_the_grammar_accepts_parses() {
        for count in ["4294967296", "99999999999999999999999999999999"] {
            let rate = format!("{count}/min");
            assert!(super::super::rate_is_wellformed(&rate));
            let cap = parse_rate(&rate).expect("a well-formed rate parses");
            assert!(
                cap.count >= 4_294_967_296.0 && cap.count.is_finite(),
                "{cap:?}"
            );
        }
    }

    #[test]
    fn the_cap_is_refused_once_the_burst_is_spent() {
        let limiter = RateLimiter::default();
        assert_eq!(limiter.check("g", "here", TWO_PER_MIN, at(0)), Ok(()));
        assert_eq!(limiter.check("g", "here", TWO_PER_MIN, at(0)), Ok(()));
        assert_eq!(limiter.check("g", "here", TWO_PER_MIN, at(0)), Err(30));
    }

    #[test]
    fn tokens_come_back_with_time() {
        let limiter = RateLimiter::default();
        for _ in 0..2 {
            limiter
                .check("g", "here", TWO_PER_MIN, at(0))
                .expect("burst");
        }
        assert!(limiter.check("g", "here", TWO_PER_MIN, at(10)).is_err());
        assert_eq!(limiter.check("g", "here", TWO_PER_MIN, at(30)), Ok(()));
    }

    #[test]
    fn the_bucket_never_holds_more_than_the_burst() {
        let limiter = RateLimiter::default();
        limiter
            .check("g", "here", TWO_PER_MIN, at(0))
            .expect("first");
        let later = at(0) + Duration::days(1);
        assert_eq!(limiter.check("g", "here", TWO_PER_MIN, later), Ok(()));
        assert_eq!(limiter.check("g", "here", TWO_PER_MIN, later), Ok(()));
        assert!(limiter.check("g", "here", TWO_PER_MIN, later).is_err());
    }

    #[test]
    fn a_backwards_clock_refills_nothing() {
        let limiter = RateLimiter::default();
        for _ in 0..2 {
            limiter
                .check("g", "here", TWO_PER_MIN, at(100))
                .expect("burst");
        }
        assert!(limiter.check("g", "here", TWO_PER_MIN, at(0)).is_err());
    }

    #[test]
    fn a_clock_that_returns_does_not_refill_twice() {
        let limiter = RateLimiter::default();
        for _ in 0..2 {
            limiter
                .check("g", "here", TWO_PER_MIN, at(100))
                .expect("burst");
        }
        assert!(limiter.check("g", "here", TWO_PER_MIN, at(0)).is_err());
        // Back at t=100 almost no real time has passed.
        assert!(limiter.check("g", "here", TWO_PER_MIN, at(100)).is_err());
    }

    #[test]
    fn grants_with_one_name_in_two_places_do_not_share_a_bucket() {
        let limiter = RateLimiter::default();
        for _ in 0..2 {
            limiter
                .check("g", "a.rs:1", TWO_PER_MIN, at(0))
                .expect("burst");
        }
        assert!(limiter.check("g", "a.rs:1", TWO_PER_MIN, at(0)).is_err());
        assert_eq!(limiter.check("g", "b.rs:9", TWO_PER_MIN, at(0)), Ok(()));
    }

    #[test]
    fn grants_do_not_share_a_bucket() {
        let limiter = RateLimiter::default();
        for _ in 0..2 {
            limiter
                .check("a", "here", TWO_PER_MIN, at(0))
                .expect("burst");
        }
        assert!(limiter.check("a", "here", TWO_PER_MIN, at(0)).is_err());
        assert_eq!(limiter.check("b", "here", TWO_PER_MIN, at(0)), Ok(()));
    }
}
