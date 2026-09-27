//! Business calendars: working hours, weekends and holidays.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use chrono::{
    DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeDelta, TimeZone, Timelike,
    Utc, Weekday,
};
use chrono_tz::Tz;

use super::SlaError;

/// Seconds in one day.
const DAY_SECS: u32 = 86_400;

/// The scan horizon: one hundred years of days.
const SCAN_DAYS: usize = 36_525;

/// Stop a scan after this many days in a row with no working time.
const MAX_IDLE_DAYS: usize = 400;

/// Monday to Friday.
const WEEKDAYS: [Weekday; 5] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
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
    pub fn new(open: NaiveTime, close: NaiveTime) -> Result<Self, SlaError> {
        let close_secs = match close.num_seconds_from_midnight() {
            0 => DAY_SECS,
            secs => secs,
        };
        Self::from_secs(open.num_seconds_from_midnight(), close_secs)
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
/// use chrono::NaiveDate;
///
/// # fn main() -> Result<(), autumn_web::sla::SlaError> {
/// let calendar = BusinessCalendar::weekdays("09:00-17:00".parse()?)
///     .holiday(NaiveDate::from_ymd_opt(2026, 11, 26).unwrap())
///     .annual_holiday(12, 25);
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BusinessCalendar {
    windows: [Vec<WorkingHours>; 7],
    holidays: BTreeSet<NaiveDate>,
    annual_holidays: BTreeSet<(u32, u32)>,
    zone: Option<Tz>,
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
    pub fn holiday(mut self, date: NaiveDate) -> Self {
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
    pub const fn zone(mut self, zone: Tz) -> Self {
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
    pub const fn home_zone(&self) -> Option<Tz> {
        self.zone
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
    pub fn is_holiday(&self, date: NaiveDate) -> bool {
        self.holidays.contains(&date) || self.annual_holidays.contains(&(date.month(), date.day()))
    }

    /// Whether `at` is in working time in `zone`.
    #[must_use]
    pub fn is_working(&self, at: DateTime<Utc>, zone: Tz) -> bool {
        self.intervals(local_date(at, zone), zone)
            .iter()
            .any(|(start, end)| *start <= at && at < *end)
    }

    /// The first working instant at or after `at`.
    ///
    /// Returns `None` when no working time exists in the scan horizon.
    #[must_use]
    pub fn next_working_instant(&self, at: DateTime<Utc>, zone: Tz) -> Option<DateTime<Utc>> {
        self.scan(at, zone)
            .find(|(_, end)| *end > at)
            .map(|(start, _)| start.max(at))
    }

    /// The working time between `from` and `to` in `zone`.
    ///
    /// It counts at most the scan horizon (one hundred years) after `from`.
    #[must_use]
    pub fn working_time(&self, from: DateTime<Utc>, to: DateTime<Utc>, zone: Tz) -> Duration {
        if to <= from || !self.has_working_time() {
            return Duration::ZERO;
        }
        let last = local_date(to, zone);
        let mut total = TimeDelta::zero();
        for date in local_date(from, zone)
            .iter_days()
            .take(SCAN_DAYS)
            .take_while(|d| *d <= last)
        {
            for (start, end) in self.intervals(date, zone) {
                let (start, end) = (start.max(from), end.min(to));
                if end > start {
                    total = total
                        .checked_add(&end.signed_duration_since(start))
                        .unwrap_or(TimeDelta::MAX);
                }
            }
        }
        total.to_std().unwrap_or(Duration::ZERO)
    }

    /// The instant when `budget` of working time after `from` is used.
    ///
    /// A zero budget is due at the next working instant. Returns `None` when
    /// the calendar has no working time in the scan horizon (one hundred
    /// years).
    #[must_use]
    pub fn deadline(
        &self,
        from: DateTime<Utc>,
        budget: Duration,
        zone: Tz,
    ) -> Option<DateTime<Utc>> {
        let mut left = TimeDelta::from_std(budget).ok()?;
        if left.is_zero() {
            return self.next_working_instant(from, zone);
        }
        for (start, end) in self.scan(from, zone) {
            let start = start.max(from);
            if end <= start {
                continue;
            }
            let available = end.signed_duration_since(start);
            if available >= left {
                return start.checked_add_signed(left);
            }
            left = left.checked_sub(&available)?;
        }
        None
    }

    fn has_working_time(&self) -> bool {
        self.windows.iter().any(|day| !day.is_empty())
    }

    /// The working intervals from the local date of `from`, day by day. It
    /// stops at the scan horizon, or after a long run of days with no
    /// working time.
    fn scan(
        &self,
        from: DateTime<Utc>,
        zone: Tz,
    ) -> impl Iterator<Item = (DateTime<Utc>, DateTime<Utc>)> + '_ {
        let days = if self.has_working_time() {
            SCAN_DAYS
        } else {
            0
        };
        let mut idle = 0_usize;
        local_date(from, zone)
            .iter_days()
            .take(days)
            .map(move |date| self.intervals(date, zone))
            .take_while(move |intervals| {
                idle = if intervals.is_empty() {
                    idle.saturating_add(1)
                } else {
                    0
                };
                idle <= MAX_IDLE_DAYS
            })
            .flatten()
    }

    /// The working intervals of one local date, as UTC instants.
    fn intervals(&self, date: NaiveDate, zone: Tz) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        if self.is_holiday(date) {
            return Vec::new();
        }
        self.windows
            .get(day_index(date.weekday()))
            .map(|windows| {
                windows
                    .iter()
                    .filter_map(|w| {
                        let start = local_instant(date, w.open, zone)?;
                        let end = local_instant(date, w.close, zone)?;
                        (end > start).then_some((start, end))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

const fn day_index(day: Weekday) -> usize {
    day.num_days_from_monday() as usize
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

/// The local date of `at` in `zone`. At the far ends of the time range it
/// falls back to the UTC date and does not panic.
fn local_date(at: DateTime<Utc>, zone: Tz) -> NaiveDate {
    let offset = zone.offset_from_utc_datetime(&at.naive_utc()).fix();
    at.naive_utc()
        .checked_add_offset(offset)
        .map_or_else(|| at.date_naive(), |local| local.date())
}

/// The UTC instant of `secs` after local midnight on `date`.
///
/// A time that occurs two times takes the first one. A time in a gap (a
/// daylight-saving change or a skipped day) uses the offset before the gap,
/// so it moves forward by the length of the gap.
fn local_instant(date: NaiveDate, secs: u32, zone: Tz) -> Option<DateTime<Utc>> {
    let naive: NaiveDateTime = date
        .and_time(NaiveTime::MIN)
        .checked_add_signed(TimeDelta::try_seconds(i64::from(secs))?)?;
    if let Some(local) = zone.from_local_datetime(&naive).earliest() {
        return Some(local.with_timezone(&Utc));
    }
    let before = naive.checked_sub_signed(TimeDelta::try_days(2)?)?;
    let offset = zone.from_local_datetime(&before).earliest()?.offset().fix();
    Some(naive.checked_sub_offset(offset)?.and_utc())
}
