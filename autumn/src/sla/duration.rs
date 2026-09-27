//! Business durations such as `"2 business days"`.

use std::str::FromStr;
use std::time::Duration;

use super::{BusinessCalendar, SlaError};

/// A budget of business time.
///
/// Days use the business-day length of the calendar. Hours and minutes are
/// working time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct BusinessDuration {
    days: u32,
    secs: u64,
}

impl BusinessDuration {
    /// No time.
    pub const ZERO: Self = Self { days: 0, secs: 0 };

    /// `n` business days.
    #[must_use]
    pub const fn days(n: u32) -> Self {
        Self { days: n, secs: 0 }
    }

    /// `n` business hours.
    #[must_use]
    pub const fn hours(n: u32) -> Self {
        Self {
            days: 0,
            secs: (n as u64).saturating_mul(3_600),
        }
    }

    /// `n` business minutes.
    #[must_use]
    pub const fn minutes(n: u32) -> Self {
        Self {
            days: 0,
            secs: (n as u64).saturating_mul(60),
        }
    }

    /// Make a duration from days and working seconds.
    #[must_use]
    pub const fn from_parts(days: u32, secs: u64) -> Self {
        Self { days, secs }
    }

    /// The business days in this duration.
    #[must_use]
    pub const fn business_days(&self) -> u32 {
        self.days
    }

    /// The working seconds in this duration, not counting days.
    #[must_use]
    pub const fn working_secs(&self) -> u64 {
        self.secs
    }

    /// The working time of this duration on `calendar`.
    #[must_use]
    pub fn resolve(&self, calendar: &BusinessCalendar) -> Duration {
        calendar
            .day_length()
            .saturating_mul(self.days)
            .saturating_add(Duration::from_secs(self.secs))
    }
}

impl FromStr for BusinessDuration {
    type Err = SlaError;

    /// Parse a form such as `"2 business days"` or `"1 day, 4 hours"`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        parse(text).ok_or_else(|| SlaError::InvalidDuration(text.to_owned()))
    }
}

impl std::fmt::Display for BusinessDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts = [
            (u64::from(self.days), "day"),
            (self.secs / 3_600, "hour"),
            (self.secs % 3_600 / 60, "minute"),
            (self.secs % 60, "second"),
        ];
        let mut first = true;
        for (count, unit) in parts {
            if count == 0 {
                continue;
            }
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            let plural = if count == 1 { "" } else { "s" };
            write!(f, "{count} business {unit}{plural}")?;
        }
        if first {
            f.write_str("0 business minutes")?;
        }
        Ok(())
    }
}

/// Parse a list of `<count> [business] <unit>` parts. Commas and `and` join
/// the parts.
fn parse(text: &str) -> Option<BusinessDuration> {
    let lower = text.to_ascii_lowercase().replace(',', " ");
    let mut words = lower.split_whitespace().filter(|w| *w != "and").peekable();
    let mut total = BusinessDuration::ZERO;
    let mut parts = 0_u32;
    while let Some(word) = words.next() {
        let count: u64 = word.parse().ok()?;
        if words.peek() == Some(&"business") {
            words.next();
        }
        match words.next()? {
            "day" | "days" => {
                total.days = total.days.checked_add(u32::try_from(count).ok()?)?;
            }
            unit => {
                let scale = match unit {
                    "hour" | "hours" => 3_600,
                    "minute" | "minutes" => 60,
                    "second" | "seconds" => 1,
                    _ => return None,
                };
                total.secs = total.secs.checked_add(count.checked_mul(scale)?)?;
            }
        }
        parts = parts.saturating_add(1);
    }
    (parts > 0).then_some(total)
}
