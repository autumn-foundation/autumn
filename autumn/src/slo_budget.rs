//! In-process error budgets for the `[[slo]]` objectives.
//!
//! [`crate::slo`] turns each objective into Prometheus rules, so the alerts
//! live in Prometheus. A [`BudgetTracker`] evaluates the same objective, with
//! the same [`BURN_WINDOWS`], inside the app. Feed it each request outcome and
//! ask it how fast the error budget burns:
//!
//! ```
//! use std::time::Duration;
//! use autumn_web::slo::{validate, SliKind, SloConfig};
//! use autumn_web::slo_budget::{BudgetTracker, Outcome};
//!
//! let slo = validate(&[SloConfig {
//!     name: "availability".to_owned(),
//!     objective: 99.9,
//!     sli: SliKind::Availability,
//!     route: None,
//!     threshold_ms: None,
//!     description: None,
//! }])
//! .unwrap()
//! .remove(0);
//!
//! let mut tracker = BudgetTracker::new(slo);
//! let at = Duration::from_secs(30);
//! for _ in 0..98 {
//!     tracker.record(at, Outcome { route: "/", status: 200, latency_ms: 4 });
//! }
//! tracker.record(at, Outcome { route: "/", status: 503, latency_ms: 4 });
//! tracker.record(at, Outcome { route: "/", status: 500, latency_ms: 4 });
//!
//! // 2 % errors against a 0.1 % budget is a burn rate of 20x.
//! let hour = Duration::from_hours(1);
//! assert_eq!(tracker.burn_tenths(at, hour), Some(200));
//! assert!(tracker.firing(at).iter().any(|w| w.severity == "page"));
//! ```
//!
//! The app can act on the answer while Prometheus is still scraping: shed
//! optional work, turn a feature flag off, or report on `/actuator`.
//!
//! The tracker reads no clock. Each call takes the time as a [`Duration`]
//! from an epoch the app picks, such as the boot [`std::time::Instant`]. Tests
//! and simulations replay exact timelines. Events land in one-minute buckets,
//! and buckets older than the longest window are dropped, so memory stays
//! bounded at a few thousand buckets per objective.

use std::collections::VecDeque;
use std::time::Duration;

use crate::slo::{BURN_WINDOWS, BurnWindow, PPM, Sli, Slo};

/// The width of one bucket, in seconds.
const BUCKET_SECS: u64 = 60;

/// One finished request, as an SLI sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome<'a> {
    /// The matched route pattern, for example `/api/orders/{id}`.
    pub route: &'a str,
    /// The response status code.
    pub status: u16,
    /// How long the request took, in milliseconds.
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bucket {
    index: u64,
    good: u64,
    bad: u64,
}

/// Good and bad event counts for one [`Slo`], over a sliding time range.
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    slo: Slo,
    retain_buckets: u64,
    buckets: VecDeque<Bucket>,
}

impl BudgetTracker {
    /// A tracker for `slo` with no events yet.
    #[must_use]
    pub fn new(slo: Slo) -> Self {
        let longest = BURN_WINDOWS
            .iter()
            .filter_map(|w| parse_window(w.long))
            .max()
            .unwrap_or(Duration::ZERO);
        Self {
            slo,
            retain_buckets: longest.as_secs().div_ceil(BUCKET_SECS),
            buckets: VecDeque::new(),
        }
    }

    /// The objective this tracker measures.
    #[must_use]
    pub const fn slo(&self) -> &Slo {
        &self.slo
    }

    /// Count `outcome` at time `at`. An outcome on another route, or one
    /// older than the longest burn window, is ignored.
    pub fn record(&mut self, at: Duration, outcome: Outcome<'_>) {
        let Some(good) = classify(&self.slo, &outcome) else {
            return;
        };
        let index = at.as_secs() / BUCKET_SECS;
        let newest = self.buckets.back().map_or(index, |b| b.index.max(index));
        let oldest_kept = newest.saturating_sub(self.retain_buckets);
        if index < oldest_kept {
            return;
        }
        while self.buckets.front().is_some_and(|b| b.index < oldest_kept) {
            self.buckets.pop_front();
        }
        let position = match self.buckets.binary_search_by_key(&index, |b| b.index) {
            Ok(position) => position,
            Err(position) => {
                self.buckets.insert(
                    position,
                    Bucket {
                        index,
                        good: 0,
                        bad: 0,
                    },
                );
                position
            }
        };
        let bucket = &mut self.buckets[position];
        if good {
            bucket.good += 1;
        } else {
            bucket.bad += 1;
        }
    }

    /// The burn rate over the `window` that ends at `now`, in tenths: `10`
    /// means the budget is spent exactly as fast as the objective allows.
    /// `None` when the window holds no events.
    #[must_use]
    pub fn burn_tenths(&self, now: Duration, window: Duration) -> Option<u32> {
        let (good, bad) = self.counts(now, window)?;
        let total = u128::from(good + bad);
        let scaled =
            u128::from(bad) * u128::from(PPM) * 10 / (total * u128::from(self.slo.budget_ppm()));
        Some(u32::try_from(scaled).unwrap_or(u32::MAX))
    }

    /// The share of the error budget left over the `window` that ends at
    /// `now`, in ppm: `PPM` is untouched, `0` is spent. `None` when the
    /// window holds no events.
    #[must_use]
    pub fn remaining_ppm(&self, now: Duration, window: Duration) -> Option<u32> {
        let (good, bad) = self.counts(now, window)?;
        let total = u128::from(good + bad);
        let ppm = u128::from(PPM);
        let spent = u128::from(bad) * ppm * ppm / (total * u128::from(self.slo.budget_ppm()));
        // `spent.min(ppm)` is at most `PPM`, so the difference fits in a u32.
        Some(u32::try_from(ppm - spent.min(ppm)).unwrap_or(0))
    }

    /// The [`BURN_WINDOWS`] alerts that fire at `now`. An alert fires when
    /// both its long and its short window burn faster than its factor.
    #[must_use]
    pub fn firing(&self, now: Duration) -> Vec<BurnWindow> {
        BURN_WINDOWS
            .into_iter()
            .filter(|w| {
                [w.long, w.short].into_iter().all(|span| {
                    parse_window(span).is_some_and(|span| self.exceeds(now, span, w.factor_tenths))
                })
            })
            .collect()
    }

    fn exceeds(&self, now: Duration, window: Duration, factor_tenths: u32) -> bool {
        self.counts(now, window).is_some_and(|(good, bad)| {
            let total = u128::from(good + bad);
            u128::from(bad) * u128::from(PPM) * 10
                > u128::from(factor_tenths) * u128::from(self.slo.budget_ppm()) * total
        })
    }

    /// Good and bad counts in the buckets of `(now - window, now]`.
    fn counts(&self, now: Duration, window: Duration) -> Option<(u64, u64)> {
        let last = now.as_secs() / BUCKET_SECS;
        let span = window.as_secs().div_ceil(BUCKET_SECS);
        let first = (last + 1).saturating_sub(span);
        let (good, bad) = self
            .buckets
            .iter()
            .filter(|b| (first..=last).contains(&b.index))
            .fold((0, 0), |(good, bad), b| (good + b.good, bad + b.bad));
        (good + bad > 0).then_some((good, bad))
    }

    #[cfg(test)]
    fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

/// Parse a [`BurnWindow`] span such as `5m`, `1h` or `3d`.
#[must_use]
pub fn parse_window(span: &str) -> Option<Duration> {
    let unit = match span.chars().last()? {
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return None,
    };
    let count: u64 = span[..span.len() - 1].parse().ok()?;
    count.checked_mul(unit).map(Duration::from_secs)
}

/// Whether `outcome` is a good event for `slo`. `None` when the outcome is
/// on a route the SLO does not cover.
#[must_use]
pub fn classify(slo: &Slo, outcome: &Outcome<'_>) -> Option<bool> {
    if slo.route().is_some_and(|route| route != outcome.route) {
        return None;
    }
    Some(match slo.sli {
        Sli::Availability { .. } => outcome.status < 500,
        Sli::Latency { threshold_ms, .. } => outcome.latency_ms <= threshold_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slo::{BURN_WINDOWS, PPM, Sli, Slo};

    const MIN: Duration = Duration::from_mins(1);
    const HOUR: Duration = Duration::from_hours(1);

    fn availability(route: Option<&str>) -> Slo {
        Slo {
            name: "availability".to_owned(),
            objective_ppm: 999_000,
            sli: Sli::Availability {
                route: route.map(str::to_owned),
            },
            description: None,
        }
    }

    fn latency() -> Slo {
        Slo {
            name: "orders-latency".to_owned(),
            objective_ppm: 990_000,
            sli: Sli::Latency {
                route: Some("/api/orders".to_owned()),
                threshold_ms: 250,
            },
            description: None,
        }
    }

    fn ok(route: &str) -> Outcome<'_> {
        Outcome {
            route,
            status: 200,
            latency_ms: 10,
        }
    }

    fn fail(route: &str) -> Outcome<'_> {
        Outcome {
            route,
            status: 503,
            latency_ms: 10,
        }
    }

    /// Record `good` and `bad` events spread over one minute starting at `at`.
    fn record_mix(tracker: &mut BudgetTracker, at: Duration, good: u32, bad: u32) {
        for _ in 0..good {
            tracker.record(at, ok("/"));
        }
        for _ in 0..bad {
            tracker.record(at, fail("/"));
        }
    }

    #[test]
    fn window_durations_parse() {
        assert_eq!(parse_window("5m"), Some(Duration::from_mins(5)));
        assert_eq!(parse_window("1h"), Some(HOUR));
        assert_eq!(parse_window("3d"), Some(Duration::from_hours(72)));
        assert_eq!(parse_window("30s"), None);
        assert_eq!(parse_window("h"), None);
        assert_eq!(parse_window(""), None);
        for window in BURN_WINDOWS {
            assert!(parse_window(window.long).is_some(), "{}", window.long);
            assert!(parse_window(window.short).is_some(), "{}", window.short);
        }
    }

    #[test]
    fn availability_counts_5xx_as_bad() {
        let slo = availability(None);
        assert_eq!(classify(&slo, &ok("/a")), Some(true));
        assert_eq!(
            classify(
                &slo,
                &Outcome {
                    route: "/a",
                    status: 404,
                    latency_ms: 9_999
                }
            ),
            Some(true)
        );
        assert_eq!(classify(&slo, &fail("/a")), Some(false));
    }

    #[test]
    fn latency_counts_slow_requests_as_bad_and_filters_by_route() {
        let slo = latency();
        let fast = Outcome {
            route: "/api/orders",
            status: 200,
            latency_ms: 250,
        };
        let slow = Outcome {
            route: "/api/orders",
            status: 200,
            latency_ms: 251,
        };
        let other = Outcome {
            route: "/other",
            status: 200,
            latency_ms: 9_999,
        };
        assert_eq!(classify(&slo, &fast), Some(true));
        assert_eq!(classify(&slo, &slow), Some(false));
        assert_eq!(classify(&slo, &other), None);
    }

    #[test]
    fn an_empty_window_has_no_burn_rate() {
        let tracker = BudgetTracker::new(availability(None));
        assert_eq!(tracker.burn_tenths(HOUR, HOUR), None);
        assert!(tracker.firing(HOUR).is_empty());
    }

    #[test]
    fn burn_rate_is_bad_ratio_over_budget() {
        let mut tracker = BudgetTracker::new(availability(None));
        // 1 bad in 1000 is exactly the 0.1 % budget: burn rate 1.0.
        record_mix(&mut tracker, Duration::ZERO, 999, 1);
        assert_eq!(tracker.burn_tenths(MIN, HOUR), Some(10));
        // 2 more bad in another 1000: 3 / 2000 = 1.5x.
        record_mix(&mut tracker, MIN, 998, 2);
        assert_eq!(tracker.burn_tenths(2 * MIN, HOUR), Some(15));
    }

    #[test]
    fn events_outside_the_window_do_not_count() {
        let mut tracker = BudgetTracker::new(availability(None));
        record_mix(&mut tracker, Duration::ZERO, 0, 100);
        record_mix(&mut tracker, 2 * HOUR, 1_000, 0);
        assert_eq!(tracker.burn_tenths(2 * HOUR, HOUR), Some(0));
        assert_eq!(tracker.burn_tenths(2 * HOUR, 3 * HOUR), Some(909));
    }

    #[test]
    fn unmatched_routes_are_ignored() {
        let mut tracker = BudgetTracker::new(availability(Some("/api")));
        tracker.record(Duration::ZERO, fail("/elsewhere"));
        assert_eq!(tracker.burn_tenths(MIN, HOUR), None);
        tracker.record(Duration::ZERO, fail("/api"));
        assert_eq!(tracker.burn_tenths(MIN, HOUR), Some(10_000));
    }

    #[test]
    fn a_fast_burn_fires_every_alert() {
        let mut tracker = BudgetTracker::new(availability(None));
        // An hour of a 2 % error ratio: a burn rate of 20x.
        for minute in 0..60 {
            record_mix(&mut tracker, minute * MIN, 98, 2);
        }
        let firing = tracker.firing(HOUR);
        let factors: Vec<u32> = firing.iter().map(|w| w.factor_tenths).collect();
        // Every window burns at 20x, so all three alerts fire, ticket included.
        assert_eq!(factors, vec![144, 60, 10]);
    }

    #[test]
    fn a_slow_burn_opens_only_a_ticket() {
        let mut tracker = BudgetTracker::new(availability(None));
        // Six hours at 0.2 %: a burn rate of 2x. Too slow to page.
        for minute in 0..360 {
            record_mix(&mut tracker, minute * MIN, 998, 2);
        }
        let firing = tracker.firing(6 * HOUR);
        assert_eq!(firing.len(), 1);
        assert_eq!(firing[0].severity, "ticket");
    }

    #[test]
    fn a_recovered_short_window_silences_the_page() {
        let mut tracker = BudgetTracker::new(availability(None));
        // 55 minutes of heavy errors, then 5 clean minutes.
        for minute in 0..55 {
            record_mix(&mut tracker, minute * MIN, 90, 10);
        }
        for minute in 55..60 {
            record_mix(&mut tracker, minute * MIN, 100, 0);
        }
        // The long 1h window still burns at ~92x, but the 5m window is clean,
        // so the fast page has reset, as the SRE workbook intends.
        assert!(tracker.burn_tenths(HOUR, HOUR).unwrap() > 144);
        assert!(!tracker.firing(HOUR).iter().any(|w| w.long == "1h"));
    }

    #[test]
    fn remaining_budget_drains_as_errors_land() {
        let mut tracker = BudgetTracker::new(availability(None));
        record_mix(&mut tracker, Duration::ZERO, 10_000, 0);
        assert_eq!(tracker.remaining_ppm(MIN, HOUR), Some(PPM));
        // 5 bad in 10_005 is about half of the 0.1 % budget.
        record_mix(&mut tracker, Duration::ZERO, 0, 5);
        let left = tracker.remaining_ppm(MIN, HOUR).unwrap();
        assert!((500_000..=501_000).contains(&left), "{left}");
        // Spend it all and more: the remainder floors at zero.
        record_mix(&mut tracker, Duration::ZERO, 0, 100);
        assert_eq!(tracker.remaining_ppm(MIN, HOUR), Some(0));
    }

    #[test]
    fn old_buckets_are_pruned_past_the_longest_window() {
        let mut tracker = BudgetTracker::new(availability(None));
        record_mix(&mut tracker, Duration::ZERO, 1, 0);
        record_mix(&mut tracker, 4 * 24 * HOUR, 1, 0);
        assert_eq!(tracker.bucket_count(), 1);
    }

    #[test]
    fn out_of_order_events_land_in_their_own_bucket() {
        let mut tracker = BudgetTracker::new(availability(None));
        record_mix(&mut tracker, 10 * MIN, 1, 0);
        record_mix(&mut tracker, 2 * MIN, 0, 1);
        assert_eq!(tracker.burn_tenths(10 * MIN, 5 * MIN), Some(0));
        assert_eq!(tracker.burn_tenths(10 * MIN, HOUR), Some(5_000));
    }
}
