//! An in-process error budget meter for one [`Slo`].
//!
//! [`crate::slo`] turns `[[slo]]` tables into Prometheus rules, so the burn
//! rate is known only to Prometheus. [`BurnMeter`] counts good and bad events
//! inside the process and answers the same question the generated alerts ask:
//! is this objective burning its error budget faster than a [`BurnWindow`]
//! allows? The app can act on the answer itself, for example by shedding
//! optional work or turning a feature flag off while the budget burns.
//!
//! ```
//! use std::time::{Duration, Instant};
//! use autumn_web::slo::{validate, SliKind, SloConfig, BURN_WINDOWS};
//! use autumn_web::slo_meter::BurnMeter;
//!
//! let slo = validate(&[SloConfig {
//!     name: "availability".into(),
//!     objective: 99.9,
//!     sli: SliKind::Availability,
//!     route: None,
//!     threshold_ms: None,
//!     description: None,
//! }])
//! .unwrap()
//! .remove(0);
//!
//! let meter = BurnMeter::new(&slo, Duration::from_mins(1), Duration::from_hours(1));
//! let now = Instant::now();
//! for i in 0..1_000 {
//!     meter.record_at(now, i % 50 != 0); // 2% errors
//! }
//! // A 0.1% budget burned at 2% is a burn rate of 20×.
//! assert_eq!(meter.burn_tenths_at(now, Duration::from_mins(5)), Some(200));
//! assert!(meter.firing_at(now, &BURN_WINDOWS[0]));
//! ```
//!
//! Counts sit in a ring of fixed-width buckets, so memory is set by
//! `retention / bucket` and does not grow with traffic. All ratio math is
//! integer parts per million, the same as [`crate::slo`].

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::slo::{BurnWindow, PPM, Slo};

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    /// The bucket number this slot holds, counted from the meter's origin.
    epoch: u64,
    good: u64,
    bad: u64,
}

#[derive(Debug)]
struct Ring {
    origin: Instant,
    slots: Vec<Bucket>,
}

/// Good and bad event counts over a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Events that met the objective.
    pub good: u64,
    /// Events that did not.
    pub bad: u64,
}

impl Counts {
    /// All events.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.good + self.bad
    }

    /// The bad-event ratio in parts per million, or `None` with no events.
    #[must_use]
    pub fn error_ppm(&self) -> Option<u32> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let ppm = u128::from(self.bad) * u128::from(PPM) / u128::from(total);
        Some(u32::try_from(ppm).unwrap_or(PPM))
    }
}

/// Counts an SLO's good and bad events and reports its burn rate.
///
/// It is `Sync`: share it in an `Arc` and record from every request.
#[derive(Debug)]
pub struct BurnMeter {
    budget_ppm: u32,
    bucket_nanos: u64,
    ring: Mutex<Ring>,
}

impl BurnMeter {
    /// A meter for `slo` that keeps `retention` of history in `bucket`-wide
    /// slots.
    ///
    /// Windows are rounded to whole buckets, so a 1-minute bucket measures a
    /// 5-minute window to the minute. `bucket` is raised to 1ms, and
    /// `retention` to one bucket.
    #[must_use]
    pub fn new(slo: &Slo, bucket: Duration, retention: Duration) -> Self {
        let bucket_nanos = duration_nanos(bucket).max(1_000_000);
        let slots = duration_nanos(retention).div_ceil(bucket_nanos).max(1);
        Self {
            budget_ppm: slo.budget_ppm(),
            bucket_nanos,
            ring: Mutex::new(Ring {
                origin: Instant::now(),
                slots: vec![Bucket::default(); usize::try_from(slots).unwrap_or(usize::MAX)],
            }),
        }
    }

    /// The SLO's error budget in parts per million.
    #[must_use]
    pub const fn budget_ppm(&self) -> u32 {
        self.budget_ppm
    }

    /// Record one event now. `good` is whether it met the objective.
    pub fn record(&self, good: bool) {
        self.record_at(Instant::now(), good);
    }

    /// Record one event at `now`.
    ///
    /// An instant before the meter was created counts in its first bucket.
    pub fn record_at(&self, now: Instant, good: bool) {
        let mut ring = self.lock();
        let epoch = self.epoch(&ring, now);
        let len = ring.slots.len() as u64;
        let slot = &mut ring.slots[usize::try_from(epoch % len).unwrap_or(0)];
        if slot.epoch > epoch {
            // The slot already holds a newer bucket, so this event is older
            // than the retention. Drop it rather than wipe newer counts.
            return;
        }
        if slot.epoch < epoch {
            *slot = Bucket {
                epoch,
                ..Bucket::default()
            };
        }
        if good {
            slot.good += 1;
        } else {
            slot.bad += 1;
        }
        drop(ring);
    }

    /// The counts over the `window` that ends at `now`.
    ///
    /// The window is capped at the meter's retention.
    #[must_use]
    pub fn counts_at(&self, now: Instant, window: Duration) -> Counts {
        let ring = self.lock();
        let newest = self.epoch(&ring, now);
        let len = ring.slots.len() as u64;
        let span = duration_nanos(window)
            .div_ceil(self.bucket_nanos)
            .clamp(1, len);
        let oldest = newest.saturating_sub(span - 1);
        ring.slots
            .iter()
            .filter(|b| (b.good, b.bad) != (0, 0) && (oldest..=newest).contains(&b.epoch))
            .fold(Counts::default(), |acc, b| Counts {
                good: acc.good + b.good,
                bad: acc.bad + b.bad,
            })
    }

    /// The burn rate over `window`, in tenths, or `None` with no events.
    ///
    /// `10` means the budget is spent exactly as fast as the objective
    /// allows. A zero budget (a 100% objective) burns at `u32::MAX` on any
    /// error.
    #[must_use]
    pub fn burn_tenths_at(&self, now: Instant, window: Duration) -> Option<u32> {
        let counts = self.counts_at(now, window);
        if counts.total() == 0 {
            return None;
        }
        if self.budget_ppm == 0 {
            return Some(if counts.bad == 0 { 0 } else { u32::MAX });
        }
        // bad / total / (budget_ppm / PPM), in tenths, from the counts so
        // the ratio is not rounded to whole ppm first.
        let tenths = u128::from(counts.bad) * 10 * u128::from(PPM)
            / (u128::from(self.budget_ppm) * u128::from(counts.total()));
        Some(u32::try_from(tenths).unwrap_or(u32::MAX))
    }

    /// Whether `alert` fires at `now`: both its long and its short window
    /// burn above its factor, as the generated Prometheus rule checks.
    ///
    /// A window the meter cannot parse, or one with no events, does not fire.
    #[must_use]
    pub fn firing_at(&self, now: Instant, alert: &BurnWindow) -> bool {
        // The rule fires on `bad / total > budget_ppm * factor_tenths / 10^7`.
        // Cross-multiplied, so a burn just over the factor is not rounded
        // down to it.
        let limit = u128::from(self.budget_ppm) * u128::from(alert.factor_tenths);
        let over = |window: &str| {
            parse_window(window).is_some_and(|w| {
                let counts = self.counts_at(now, w);
                counts.total() > 0
                    && u128::from(counts.bad) * 10 * u128::from(PPM)
                        > limit * u128::from(counts.total())
            })
        };
        over(alert.long) && over(alert.short)
    }

    /// [`Self::firing_at`] now.
    #[must_use]
    pub fn firing(&self, alert: &BurnWindow) -> bool {
        self.firing_at(Instant::now(), alert)
    }

    fn epoch(&self, ring: &Ring, now: Instant) -> u64 {
        duration_nanos(now.saturating_duration_since(ring.origin)) / self.bucket_nanos
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Parse a Prometheus window such as `5m`, `6h` or `3d`.
///
/// Only `s`, `m`, `h` and `d` are accepted: those are the units
/// [`crate::slo::BURN_WINDOWS`] uses.
#[must_use]
pub fn parse_window(window: &str) -> Option<Duration> {
    let unit_at = window.len().checked_sub(1)?;
    let (digits, unit) = window.split_at_checked(unit_at)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    value.checked_mul(seconds).map(Duration::from_secs)
}

fn duration_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slo::{BURN_WINDOWS, SliKind, SloConfig, validate};

    const MINUTE: Duration = Duration::from_mins(1);

    fn slo(objective: f64) -> Slo {
        validate(&[SloConfig {
            name: "availability".into(),
            objective,
            sli: SliKind::Availability,
            route: None,
            threshold_ms: None,
            description: None,
        }])
        .unwrap()
        .remove(0)
    }

    fn record(meter: &BurnMeter, at: Instant, good: u64, bad: u64) {
        for _ in 0..good {
            meter.record_at(at, true);
        }
        for _ in 0..bad {
            meter.record_at(at, false);
        }
    }

    #[test]
    fn no_events_has_no_burn_rate() {
        let meter = BurnMeter::new(&slo(99.9), MINUTE, Duration::from_hours(1));
        assert_eq!(meter.burn_tenths_at(Instant::now(), MINUTE), None);
        assert!(!meter.firing(&BURN_WINDOWS[0]));
    }

    #[test]
    fn burn_rate_is_error_ratio_over_budget() {
        let meter = BurnMeter::new(&slo(99.0), MINUTE, Duration::from_hours(1));
        let now = Instant::now();
        // 1% budget, 1% errors: burning exactly at the objective.
        record(&meter, now, 99, 1);
        assert_eq!(meter.budget_ppm(), 10_000);
        assert_eq!(meter.counts_at(now, MINUTE), Counts { good: 99, bad: 1 });
        assert_eq!(meter.burn_tenths_at(now, MINUTE), Some(10));
    }

    #[test]
    fn old_buckets_leave_the_window() {
        let meter = BurnMeter::new(&slo(99.0), MINUTE, Duration::from_hours(1));
        let start = Instant::now();
        record(&meter, start, 0, 10);
        let later = start + 10 * MINUTE;
        record(&meter, later, 10, 0);
        assert_eq!(meter.burn_tenths_at(later, 5 * MINUTE), Some(0));
        assert_eq!(meter.burn_tenths_at(later, 15 * MINUTE), Some(500));
    }

    #[test]
    fn ring_slots_are_reused_after_retention() {
        let meter = BurnMeter::new(&slo(99.0), MINUTE, 5 * MINUTE);
        let start = Instant::now();
        record(&meter, start, 0, 10);
        // Five minutes on, the same slot comes round again and is reset.
        let later = start + 5 * MINUTE;
        record(&meter, later, 3, 0);
        assert_eq!(
            meter.counts_at(later, Duration::from_hours(24)),
            Counts { good: 3, bad: 0 }
        );
    }

    #[test]
    fn fast_burn_alert_needs_both_windows() {
        let meter = BurnMeter::new(&slo(99.9), MINUTE, Duration::from_hours(1));
        let start = Instant::now();
        let page = &BURN_WINDOWS[0]; // 14.4x over 1h and 5m
        // A burst of errors 30 minutes ago, healthy since.
        record(&meter, start, 0, 100);
        let later = start + 30 * MINUTE;
        record(&meter, later, 1_000, 0);
        assert!(!meter.firing_at(later, page), "short window has recovered");
        // Errors return: both windows now burn above 14.4x.
        record(&meter, later, 0, 50);
        assert!(meter.firing_at(later, page));
    }

    #[test]
    fn zero_budget_burns_at_max_on_any_error() {
        // `validate` refuses 100%, but `Slo`'s fields are public.
        let perfect = Slo {
            objective_ppm: PPM,
            ..slo(99.0)
        };
        let meter = BurnMeter::new(&perfect, MINUTE, MINUTE);
        let now = Instant::now();
        record(&meter, now, 10, 0);
        assert_eq!(meter.burn_tenths_at(now, MINUTE), Some(0));
        record(&meter, now, 0, 1);
        assert_eq!(meter.burn_tenths_at(now, MINUTE), Some(u32::MAX));
    }

    #[test]
    fn parses_burn_window_durations() {
        assert_eq!(parse_window("5m"), Some(5 * MINUTE));
        assert_eq!(parse_window("6h"), Some(Duration::from_hours(6)));
        assert_eq!(parse_window("3d"), Some(Duration::from_hours(72)));
        assert_eq!(parse_window("30s"), Some(Duration::from_secs(30)));
        for bad in ["", "m", "5", "5w", "-5m", "+5m", "1.5h"] {
            assert_eq!(parse_window(bad), None, "{bad}");
        }
        for w in BURN_WINDOWS {
            assert!(parse_window(w.long).is_some() && parse_window(w.short).is_some());
        }
    }

    #[test]
    fn alert_compares_the_exact_ratio_not_rounded_tenths() {
        let meter = BurnMeter::new(&slo(99.9), MINUTE, Duration::from_hours(1));
        let now = Instant::now();
        // 145 / 10_001 is a burn rate of 14.4986x: over 14.4x, but it
        // rounds down to 144 tenths.
        record(&meter, now, 10_001 - 145, 145);
        assert_eq!(meter.burn_tenths_at(now, MINUTE), Some(144));
        assert!(meter.firing_at(now, &BURN_WINDOWS[0]));
        // Exactly at the threshold does not fire: the rule uses `>`.
        let at = BurnMeter::new(&slo(99.9), MINUTE, Duration::from_hours(1));
        record(&at, now, 10_000 - 144, 144);
        assert!(!at.firing_at(now, &BURN_WINDOWS[0]));
    }

    #[test]
    fn burn_tenths_does_not_round_the_ratio_to_whole_ppm_first() {
        // 99.9999% leaves a 1 ppm budget. 1 bad in 1_000_001 is a burn of
        // 0.999999x, which is 9 tenths, but under 1 ppm of error.
        let meter = BurnMeter::new(&slo(99.9999), MINUTE, MINUTE);
        let now = Instant::now();
        record(&meter, now, 1_000_000, 1);
        assert_eq!(meter.burn_tenths_at(now, MINUTE), Some(9));
    }

    #[test]
    fn non_ascii_window_suffix_is_rejected() {
        assert_eq!(parse_window("5秒"), None);
        assert_eq!(parse_window("秒"), None);
    }

    #[test]
    fn late_record_does_not_reset_a_newer_slot() {
        let meter = BurnMeter::new(&slo(99.0), MINUTE, MINUTE);
        let start = Instant::now();
        record(&meter, start + MINUTE, 0, 1);
        // An event from the bucket before arrives late. It is outside
        // retention, so it is dropped rather than wiping the newer bucket.
        record(&meter, start, 1, 0);
        assert_eq!(
            meter.counts_at(start + MINUTE, MINUTE),
            Counts { good: 0, bad: 1 }
        );
    }

    #[test]
    fn records_from_many_threads() {
        let meter = std::sync::Arc::new(BurnMeter::new(&slo(99.0), MINUTE, MINUTE));
        let now = Instant::now();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let meter = meter.clone();
                std::thread::spawn(move || record(&meter, now, 25, 0))
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(meter.counts_at(now, MINUTE).total(), 100);
    }
}
