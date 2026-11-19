//! Cron searches over `jiff` zoned date-times.
//!
//! `croner` is generic over its [`CronDateTime`] trait. Its own `jiff`
//! adapter targets an older `jiff` major, so the framework implements the
//! trait itself on [`CronZoned`], a thin wrapper around a
//! [`ZonedDateTime`]. The wrapper exists only because neither the trait nor
//! the zoned type is local to this crate.
//!
//! Daylight saving time follows `croner`'s documented policy, which this
//! adapter feeds through [`CronDateTime::resolve_civil`]:
//!
//! - a wall clock time that happens twice (the clock goes back) resolves to
//!   both instants, earlier first;
//! - a wall clock time that never happens (the clock goes forward) reports a
//!   gap, and `croner` runs a fixed-time job at the first instant after it.

use core::cmp::Ordering;

use croner::errors::CronError;
use croner::{CivilDate, CivilDateTime, CivilTime, CronDateTime, Resolution, Weekday};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{SignedDuration, Timestamp, ZonedDateTime, civil};

/// A [`ZonedDateTime`] that `croner` can search over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronZoned(ZonedDateTime);

impl CronZoned {
    /// `ts` seen from `tz`.
    pub fn new(ts: Timestamp, tz: TimeZone) -> Self {
        Self(ts.to_zoned_datetime(tz))
    }

    /// The instant this value names.
    pub fn timestamp(&self) -> Timestamp {
        self.0.timestamp()
    }

    /// The wrapped zoned date-time.
    #[cfg(test)]
    pub const fn zoned(&self) -> &ZonedDateTime {
        &self.0
    }
}

impl core::fmt::Display for CronZoned {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// `jiff`'s civil date-time as `croner`'s. `jiff` keeps every part in range,
/// so the parts are taken as they are.
fn to_croner(dt: civil::DateTime) -> CivilDateTime {
    CivilDateTime::new(
        CivilDate::from_parts_unchecked(
            i32::from(dt.year()),
            u32::from(dt.month().unsigned_abs()),
            u32::from(dt.day().unsigned_abs()),
        ),
        CivilTime::from_parts_unchecked(
            u32::from(dt.hour().unsigned_abs()),
            u32::from(dt.minute().unsigned_abs()),
            u32::from(dt.second().unsigned_abs()),
        ),
    )
}

/// `croner`'s civil date-time as `jiff`'s. Fails outside `jiff`'s year range
/// (`-9999..=9999`).
fn to_jiff(dt: CivilDateTime) -> Result<civil::DateTime, CronError> {
    let year = i16::try_from(dt.year()).map_err(|_| CronError::InvalidDate)?;
    let part = |value: u32| i8::try_from(value).map_err(|_| CronError::InvalidDate);
    civil::DateTime::new(
        year,
        part(dt.month())?,
        part(dt.day())?,
        part(dt.hour())?,
        part(dt.minute())?,
        part(dt.second())?,
        0,
    )
    .map_err(|_| CronError::InvalidDate)
}

impl CronDateTime for CronZoned {
    fn to_civil(&self) -> CivilDateTime {
        to_croner(self.0.datetime())
    }

    fn civil_weekday(&self) -> Weekday {
        Weekday::from_days_from_sunday(u32::from(
            self.0.weekday().to_sunday_zero_offset().unsigned_abs(),
        ))
    }

    fn resolve_civil(&self, civil: CivilDateTime) -> Result<Resolution<Self>, CronError> {
        let dt = to_jiff(civil)?;
        let ambiguous = self.0.time_zone().to_ambiguous_zoned_datetime(dt);
        Ok(match ambiguous.offset() {
            AmbiguousOffset::Unambiguous { .. } => Resolution::Single(Self(
                ambiguous
                    .unambiguous()
                    .map_err(|_| CronError::InvalidTime)?,
            )),
            AmbiguousOffset::Fold { .. } => {
                let earlier = ambiguous
                    .clone()
                    .earlier()
                    .map_err(|_| CronError::InvalidTime)?;
                let later = ambiguous.later().map_err(|_| CronError::InvalidTime)?;
                Resolution::Ambiguous(Self(earlier), Self(later))
            }
            AmbiguousOffset::Gap { .. } => Resolution::Gap,
        })
    }

    fn checked_add_seconds(&self, seconds: i64) -> Option<Self> {
        self.0
            .checked_add(SignedDuration::from_secs(seconds))
            .ok()
            .map(Self)
    }

    fn cmp_instant(&self, other: &Self) -> Ordering {
        // Both halves of a repeated wall clock range name distinct instants,
        // so compare the timestamps, not the wall clock.
        self.0.timestamp().cmp(&other.0.timestamp())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_york() -> TimeZone {
        TimeZone::get("America/New_York").expect("bundled zone")
    }

    fn at(tz: &TimeZone, y: i16, mo: i8, d: i8, h: i8, mi: i8) -> CronZoned {
        CronZoned(
            civil::date(y, mo, d)
                .at(h, mi, 0, 0)
                .to_zoned_datetime(tz.clone())
                .expect("unambiguous wall clock time"),
        )
    }

    fn cron(expression: &str) -> croner::Cron {
        expression.parse().expect("valid cron expression")
    }

    #[test]
    fn civil_round_trip_and_weekday() {
        let tz = new_york();
        let value = at(&tz, 2026, 3, 6, 9, 30);
        let civil = value.to_civil();
        assert_eq!(
            (civil.year(), civil.month(), civil.day()),
            (2026, 3, 6),
            "date parts"
        );
        assert_eq!((civil.hour(), civil.minute(), civil.second()), (9, 30, 0));
        assert_eq!(value.civil_weekday(), Weekday::Friday);
        assert_eq!(
            to_jiff(civil).expect("in range"),
            civil::date(2026, 3, 6).at(9, 30, 0, 0)
        );
    }

    #[test]
    fn years_outside_jiffs_range_are_an_error_not_a_panic() {
        let far = CivilDateTime::new(
            CivilDate::from_parts_unchecked(10_000, 1, 1),
            CivilTime::from_parts_unchecked(0, 0, 0),
        );
        assert!(to_jiff(far).is_err());
        let value = at(&TimeZone::UTC, 2026, 1, 1, 0, 0);
        assert!(value.resolve_civil(far).is_err());
    }

    #[test]
    fn checked_add_seconds_moves_along_the_time_line_and_stops_at_the_end() {
        let tz = new_york();
        // 01:30 EST plus one hour of elapsed time is 03:30 EDT on the day the
        // clock goes forward.
        let before_gap = at(&tz, 2026, 3, 8, 1, 30);
        let after = before_gap.checked_add_seconds(3_600).expect("in range");
        assert_eq!(
            after.zoned().datetime(),
            civil::date(2026, 3, 8).at(3, 30, 0, 0)
        );

        let end = CronZoned::new(Timestamp::MAX, TimeZone::UTC);
        assert!(end.checked_add_seconds(1).is_none());
    }

    #[test]
    fn a_daily_job_in_the_gap_runs_right_after_it() {
        // 2026-03-08 02:00 to 03:00 does not exist in New York.
        let tz = new_york();
        let start = at(&tz, 2026, 3, 7, 12, 0);
        let next = cron("0 30 2 * * *")
            .find_next_occurrence(&start, false)
            .expect("next occurrence");
        assert_eq!(
            next.zoned().datetime(),
            civil::date(2026, 3, 8).at(3, 0, 0, 0)
        );
        assert_eq!(next.zoned().offset().seconds(), -4 * 3_600, "EDT");

        let following = cron("0 30 2 * * *")
            .find_next_occurrence(&next, false)
            .expect("following occurrence");
        assert_eq!(
            following.zoned().datetime(),
            civil::date(2026, 3, 9).at(2, 30, 0, 0)
        );
    }

    #[test]
    fn a_daily_job_in_the_fold_runs_once_on_the_earlier_instant() {
        // 2026-11-01 01:00 to 02:00 happens twice in New York.
        let tz = new_york();
        let start = at(&tz, 2026, 10, 31, 12, 0);
        let job = cron("0 30 1 * * *");
        let next = job
            .find_next_occurrence(&start, false)
            .expect("next occurrence");
        assert_eq!(
            next.zoned().datetime(),
            civil::date(2026, 11, 1).at(1, 30, 0, 0)
        );
        assert_eq!(
            next.zoned().offset().seconds(),
            -4 * 3_600,
            "EDT, the first pass"
        );

        let following = job
            .find_next_occurrence(&next, false)
            .expect("following occurrence");
        assert_eq!(
            following.zoned().datetime(),
            civil::date(2026, 11, 2).at(1, 30, 0, 0),
            "the repeated 01:30 EST does not fire a second time"
        );
    }

    #[test]
    fn an_interval_job_ticks_through_both_halves_of_the_fold() {
        let tz = new_york();
        let job = cron("0 0 * * * *");
        let start = at(&tz, 2026, 11, 1, 0, 30);
        let ticks: Vec<Timestamp> = job
            .iter_after(start)
            .take(3)
            .map(|tick| tick.timestamp())
            .collect();
        let hour = SignedDuration::from_hours(1);
        let first = ticks.first().copied().expect("first tick");
        assert_eq!(
            ticks,
            [first, first + hour, first + hour + hour],
            "01:00 EDT, 01:00 EST and 02:00 EST are an hour apart"
        );
    }
}
