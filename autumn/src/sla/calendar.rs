//! Business calendars: working hours, weekends and holidays.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use jiff::civil::{self, Date, Weekday};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use super::SlaError;

/// Seconds in one day.
const DAY_SECS: u32 = 86_400;

/// The scan horizon: one hundred years of days.
const SCAN_DAYS: usize = 36_525;

/// Monday to Friday.
const WEEKDAYS: [Weekday; 5] = [
    Weekday::Monday,
    Weekday::Tuesday,
    Weekday::Wednesday,
    Weekday::Thursday,
    Weekday::Friday,
];

/// One working window in a day, in local wall time.
///
/// The window starts at `open` and stops at `close`. `close` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkingHours {
    open: u32,
    close: u32,
}

impl WorkingHours {
    /// The full day, from `00:00` to `24:00`.
    pub const ALL_DAY: Self = Self {
        open: 0,
        close: DAY_SECS,
    };

    /// Make a window from `open` to `close`.
    ///
    /// A `close` of `00:00` means midnight at the end of the day.
    ///
    /// # Errors
    ///
    /// Returns [`SlaError::InvalidHours`] when `close` is not after `open`.
    pub fn new(open: civil::Time, close: civil::Time) -> Result<Self, SlaError> {
        let close_secs = match seconds_from_midnight(close) {
            0 => DAY_SECS,
            secs => secs,
        };
        Self::from_secs(seconds_from_midnight(open), close_secs)
            .ok_or_else(|| SlaError::InvalidHours(format!("{open}-{close}")))
    }

    fn from_secs(open: u32, close: u32) -> Option<Self> {
        (open < close && close <= DAY_SECS).then_some(Self { open, close })
    }

    /// The length of the window.
    #[must_use]
    pub fn length(&self) -> Duration {
        Duration::from_secs(u64::from(self.close.saturating_sub(self.open)))
    }
}

impl FromStr for WorkingHours {
    type Err = SlaError;

    /// Parse `"HH:MM-HH:MM"`. `"24:00"` is the end of the day.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || SlaError::InvalidHours(text.to_owned());
        let (open, close) = text.trim().split_once('-').ok_or_else(invalid)?;
        let open = parse_clock(open).ok_or_else(invalid)?;
        let close = parse_clock(close).ok_or_else(invalid)?;
        Self::from_secs(open, close).ok_or_else(invalid)
    }
}

/// The whole seconds of `time` after midnight. A fraction is dropped.
fn seconds_from_midnight(time: civil::Time) -> u32 {
    u32::try_from(time.duration_since(civil::Time::midnight()).as_secs()).unwrap_or(0)
}

/// Parse `"HH:MM"` (up to `"24:00"`) to seconds after midnight.
fn parse_clock(text: &str) -> Option<u32> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let digits = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    if hours.is_empty()
        || hours.len() > 2
        || minutes.len() != 2
        || !digits(hours)
        || !digits(minutes)
    {
        return None;
    }
    let hours: u32 = hours.parse().ok()?;
    let minutes: u32 = minutes.parse().ok()?;
    if minutes >= 60 || hours > 24 || (hours == 24 && minutes > 0) {
        return None;
    }
    hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)
}

/// A business calendar.
///
/// It sets the working windows for each weekday and the holidays. Hours are
/// local wall time. The time zone comes from the obligation, from
/// [`zone`](Self::zone), or from the app default, in that order.
///
/// ```rust
/// use autumn_web::sla::BusinessCalendar;
/// use autumn_web::time::civil::date;
///
/// # fn main() -> Result<(), autumn_web::sla::SlaError> {
/// let calendar = BusinessCalendar::weekdays("09:00-17:00".parse()?)
///     .holiday(date(2026, 11, 26))
///     .annual_holiday(12, 25);
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BusinessCalendar {
    windows: [Vec<WorkingHours>; 7],
    holidays: BTreeSet<Date>,
    annual_holidays: BTreeSet<(u32, u32)>,
    zone: Option<TimeZone>,
    day_length: Option<Duration>,
}

impl BusinessCalendar {
    /// Make an empty calendar. It has no working time.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make a calendar that works `hours` from Monday to Friday.
    #[must_use]
    pub fn weekdays(hours: WorkingHours) -> Self {
        WEEKDAYS
            .into_iter()
            .fold(Self::new(), |calendar, day| calendar.hours(day, hours))
    }

    /// Add a working window to `day`. Overlapping windows merge.
    #[must_use]
    pub fn hours(mut self, day: Weekday, hours: WorkingHours) -> Self {
        if let Some(windows) = self.windows.get_mut(day_index(day)) {
            windows.push(hours);
            *windows = merge(std::mem::take(windows));
        }
        self
    }

    /// Add a holiday on one date.
    #[must_use]
    pub fn holiday(mut self, date: Date) -> Self {
        self.holidays.insert(date);
        self
    }

    /// Add a holiday on the same month and day each year.
    #[must_use]
    pub fn annual_holiday(mut self, month: u32, day: u32) -> Self {
        self.annual_holidays.insert((month, day));
        self
    }

    /// Set the home time zone of the calendar.
    #[must_use]
    pub fn zone(mut self, zone: TimeZone) -> Self {
        self.zone = Some(zone);
        self
    }

    /// Set the length of one business day.
    ///
    /// The default is the working time of the longest working day.
    #[must_use]
    pub const fn business_day(mut self, length: Duration) -> Self {
        self.day_length = Some(length);
        self
    }

    /// The home time zone, if set.
    #[must_use]
    pub const fn home_zone(&self) -> Option<&TimeZone> {
        self.zone.as_ref()
    }

    /// The length of one business day.
    #[must_use]
    pub fn day_length(&self) -> Duration {
        self.day_length.unwrap_or_else(|| {
            self.windows
                .iter()
                .map(|day| {
                    day.iter()
                        .fold(Duration::ZERO, |sum, w| sum.saturating_add(w.length()))
                })
                .max()
                .unwrap_or(Duration::ZERO)
        })
    }

    /// Whether `date` is a holiday.
    #[must_use]
    pub fn is_holiday(&self, date: Date) -> bool {
        let month = u32::from(date.month().unsigned_abs());
        let day = u32::from(date.day().unsigned_abs());
        self.holidays.contains(&date) || self.annual_holidays.contains(&(month, day))
    }

    /// Whether `at` is in working time in `zone`.
    #[must_use]
    pub fn is_working(&self, at: Timestamp, zone: &TimeZone) -> bool {
        self.intervals(local_date(at, zone), zone)
            .iter()
            .any(|(start, end)| *start <= at && at < *end)
    }

    /// The first working instant at or after `at`.
    ///
    /// Returns `None` when no working time exists in the scan horizon.
    #[must_use]
    pub fn next_working_instant(&self, at: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        self.scan(at, zone)
            .find(|(_, end)| *end > at)
            .map(|(start, _)| start.max(at))
    }

    /// The working time between `from` and `to` in `zone`.
    ///
    /// It counts at most the scan horizon (one hundred years) after `from`.
    #[must_use]
    pub fn working_time(&self, from: Timestamp, to: Timestamp, zone: &TimeZone) -> Duration {
        if to <= from || !self.has_working_time() {
            return Duration::ZERO;
        }
        let last = local_date(to, zone);
        let mut total = SignedDuration::ZERO;
        for date in days_from(local_date(from, zone))
            .take(SCAN_DAYS)
            .take_while(|d| *d <= last)
        {
            for (start, end) in self.intervals(date, zone) {
                let (start, end) = (start.max(from), end.min(to));
                if end > start {
                    total = total
                        .checked_add(end.duration_since(start))
                        .unwrap_or(SignedDuration::MAX);
                }
            }
        }
        Duration::try_from(total).unwrap_or(Duration::ZERO)
    }

    /// The instant when `budget` of working time after `from` is used.
    ///
    /// A zero budget is due at the next working instant. Returns `None` when
    /// the calendar has no working time in the scan horizon (one hundred
    /// years).
    #[must_use]
    pub fn deadline(
        &self,
        from: Timestamp,
        budget: Duration,
        zone: &TimeZone,
    ) -> Option<Timestamp> {
        let mut left = SignedDuration::try_from(budget).ok()?;
        if left.is_zero() {
            return self.next_working_instant(from, zone);
        }
        for (start, end) in self.scan(from, zone) {
            let start = start.max(from);
            if end <= start {
                continue;
            }
            let available = end.duration_since(start);
            if available >= left {
                return start.checked_add(left).ok();
            }
            left = left.checked_sub(available)?;
        }
        None
    }

    fn has_working_time(&self) -> bool {
        self.windows.iter().any(|day| !day.is_empty())
    }

    /// The working intervals from the local date of `from`, day by day, up to
    /// the scan horizon. Weekdays and annual holidays repeat only every 400
    /// years, so no shorter cutoff is safe.
    fn scan<'a>(
        &'a self,
        from: Timestamp,
        zone: &'a TimeZone,
    ) -> impl Iterator<Item = (Timestamp, Timestamp)> + 'a {
        let days = if self.has_working_time() {
            SCAN_DAYS
        } else {
            0
        };
        days_from(local_date(from, zone))
            .take(days)
            .flat_map(move |date| self.intervals(date, zone))
    }

    /// The working intervals of one local date, as UTC instants.
    fn intervals(&self, date: Date, zone: &TimeZone) -> Vec<(Timestamp, Timestamp)> {
        if self.is_holiday(date) {
            return Vec::new();
        }
        self.windows
            .get(day_index(date.weekday()))
            .map(|windows| {
                // A window stays inside its own local date. On a skipped
                // date (a whole-day gap) every window moves past the end of
                // the date, so the date has no working time. A close past
                // the end of the time range stops at `Timestamp::MAX`.
                let day_end = local_instant(date, DAY_SECS, zone).unwrap_or(Timestamp::MAX);
                let resolved: Vec<_> = windows
                    .iter()
                    .filter_map(|w| {
                        let start = local_instant(date, w.open, zone)?;
                        let end = local_instant(date, w.close, zone)
                            .unwrap_or(Timestamp::MAX)
                            .min(day_end);
                        (end > start).then_some((start, end))
                    })
                    .collect();
                // A daylight-saving gap can move one window onto another.
                merge_instants(resolved)
            })
            .unwrap_or_default()
    }
}

/// Sort UTC intervals and merge the ones that overlap or touch.
fn merge_instants(mut intervals: Vec<(Timestamp, Timestamp)>) -> Vec<(Timestamp, Timestamp)> {
    intervals.sort_unstable();
    let mut merged: Vec<(Timestamp, Timestamp)> = Vec::with_capacity(intervals.len());
    for (start, end) in intervals {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn day_index(day: Weekday) -> usize {
    usize::from(day.to_monday_zero_offset().unsigned_abs())
}

/// `first` and each date after it, up to the end of the date range.
fn days_from(first: Date) -> impl Iterator<Item = Date> {
    std::iter::successors(Some(first), |date| date.tomorrow().ok())
}

/// Sort windows and merge the ones that overlap or touch.
fn merge(mut windows: Vec<WorkingHours>) -> Vec<WorkingHours> {
    windows.sort_unstable();
    let mut merged: Vec<WorkingHours> = Vec::with_capacity(windows.len());
    for window in windows {
        match merged.last_mut() {
            Some(last) if window.open <= last.close => last.close = last.close.max(window.close),
            _ => merged.push(window),
        }
    }
    merged
}

/// The local date of `at` in `zone`. Every timestamp has a local date in
/// every zone, so this never fails.
fn local_date(at: Timestamp, zone: &TimeZone) -> Date {
    zone.to_datetime(at).date()
}

/// The UTC instant of `secs` after local midnight on `date`.
///
/// A time that occurs two times takes the first one. A time in a gap (a
/// daylight-saving change or a skipped day) uses the offset before the gap,
/// so it moves forward by the length of the gap.
fn local_instant(date: Date, secs: u32, zone: &TimeZone) -> Option<Timestamp> {
    let local = date
        .to_datetime(civil::Time::midnight())
        .checked_add(SignedDuration::from_secs(i64::from(secs)))
        .ok()?;
    zone.to_ambiguous_timestamp(local).compatible().ok()
}
