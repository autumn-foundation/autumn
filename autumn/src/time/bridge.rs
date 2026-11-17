//! Conversions between `jiff` and `chrono` values while the framework moves
//! from one to the other.
//!
//! Not part of the public API: this module exists only until every framework
//! module speaks `jiff` natively, and is removed with the `chrono` dependency.
//!
//! [`ChronoBridge`] is implemented on the `jiff` side (`ts.to_chrono()`,
//! `Timestamp::from_chrono(dt)`), [`JiffBridge`] on the `chrono` side
//! (`dt.to_jiff()`). Every conversion is total: a `chrono` value outside
//! `jiff`'s range saturates to the nearest end of it, and a leap-second
//! fraction is clamped to the last nanosecond of its second.

use jiff::{SignedDuration, Timestamp, civil};

/// The ambient wall-clock time as a UTC `chrono::NaiveDateTime`, for code the
/// repository macros generate.
#[must_use]
pub fn ambient_naive_utc() -> chrono::NaiveDateTime {
    crate::time::ambient_civil_utc().to_chrono()
}

/// `ts` as a UTC `chrono::NaiveDateTime`, for code the repository macros
/// generate.
#[must_use]
pub fn naive_utc(ts: Timestamp) -> chrono::NaiveDateTime {
    ts.to_chrono().naive_utc()
}

/// The largest valid sub-second nanosecond count.
const MAX_NANOS: u32 = 999_999_999;

/// A `jiff` value with a `chrono` counterpart.
pub trait ChronoBridge: Sized {
    /// The `chrono` type this value converts to and from.
    type Chrono;

    /// This value as its `chrono` counterpart.
    fn to_chrono(self) -> Self::Chrono;

    /// The `jiff` value for a `chrono` one, saturating at `jiff`'s range.
    fn from_chrono(value: Self::Chrono) -> Self;
}

/// A `chrono` value with a `jiff` counterpart.
pub trait JiffBridge {
    /// The `jiff` type this value converts to.
    type Jiff;

    /// This value as its `jiff` counterpart, saturating at `jiff`'s range.
    fn to_jiff(self) -> Self::Jiff;
}

/// Split `(secs, nanos)` with `nanos` of either sign into whole seconds and a
/// fraction in `0..1_000_000_000`.
const fn floor_split(secs: i64, nanos: i32) -> (i64, u32) {
    if nanos < 0 {
        (
            secs.saturating_sub(1),
            (nanos + 1_000_000_000).unsigned_abs(),
        )
    } else {
        (secs, nanos.unsigned_abs())
    }
}

impl ChronoBridge for Timestamp {
    type Chrono = chrono::DateTime<chrono::Utc>;

    fn to_chrono(self) -> Self::Chrono {
        let (secs, nanos) = floor_split(self.as_second(), self.subsec_nanosecond());
        // chrono's range contains jiff's, so the fallback never runs.
        chrono::DateTime::from_timestamp(secs, nanos).unwrap_or(if secs < 0 {
            chrono::DateTime::<chrono::Utc>::MIN_UTC
        } else {
            chrono::DateTime::<chrono::Utc>::MAX_UTC
        })
    }

    fn from_chrono(value: Self::Chrono) -> Self {
        let secs = value.timestamp();
        let nanos = i32::try_from(value.timestamp_subsec_nanos().min(MAX_NANOS)).unwrap_or(0);
        Self::new(secs, nanos).unwrap_or(if secs < 0 { Self::MIN } else { Self::MAX })
    }
}

impl JiffBridge for chrono::DateTime<chrono::Utc> {
    type Jiff = Timestamp;

    fn to_jiff(self) -> Timestamp {
        Timestamp::from_chrono(self)
    }
}

impl ChronoBridge for SignedDuration {
    type Chrono = chrono::TimeDelta;

    fn to_chrono(self) -> Self::Chrono {
        let (secs, nanos) = floor_split(self.as_secs(), self.subsec_nanos());
        chrono::TimeDelta::new(secs, nanos).unwrap_or(if secs < 0 {
            chrono::TimeDelta::MIN
        } else {
            chrono::TimeDelta::MAX
        })
    }

    fn from_chrono(value: Self::Chrono) -> Self {
        // `TimeDelta` spans less than `SignedDuration`, and its seconds and
        // fraction share a sign, so this never overflows.
        Self::new(value.num_seconds(), value.subsec_nanos())
    }
}

impl JiffBridge for chrono::TimeDelta {
    type Jiff = SignedDuration;

    fn to_jiff(self) -> SignedDuration {
        SignedDuration::from_chrono(self)
    }
}

impl ChronoBridge for civil::Date {
    type Chrono = chrono::NaiveDate;

    fn to_chrono(self) -> Self::Chrono {
        // Every civil date is a valid chrono date.
        chrono::NaiveDate::from_ymd_opt(
            i32::from(self.year()),
            u32::from(self.month().unsigned_abs()),
            u32::from(self.day().unsigned_abs()),
        )
        .unwrap_or(chrono::NaiveDate::MIN)
    }

    fn from_chrono(value: Self::Chrono) -> Self {
        use chrono::Datelike as _;
        let Ok(year) = i16::try_from(value.year()) else {
            return if value.year() < 0 {
                Self::MIN
            } else {
                Self::MAX
            };
        };
        let month = i8::try_from(value.month()).unwrap_or(1);
        let day = i8::try_from(value.day()).unwrap_or(1);
        Self::new(year, month, day).unwrap_or(if year < 0 { Self::MIN } else { Self::MAX })
    }
}

impl JiffBridge for chrono::NaiveDate {
    type Jiff = civil::Date;

    fn to_jiff(self) -> civil::Date {
        civil::Date::from_chrono(self)
    }
}

impl ChronoBridge for civil::Time {
    type Chrono = chrono::NaiveTime;

    fn to_chrono(self) -> Self::Chrono {
        chrono::NaiveTime::from_hms_nano_opt(
            u32::from(self.hour().unsigned_abs()),
            u32::from(self.minute().unsigned_abs()),
            u32::from(self.second().unsigned_abs()),
            self.subsec_nanosecond().unsigned_abs(),
        )
        .unwrap_or(chrono::NaiveTime::MIN)
    }

    fn from_chrono(value: Self::Chrono) -> Self {
        use chrono::Timelike as _;
        let hour = i8::try_from(value.hour()).unwrap_or(0);
        let minute = i8::try_from(value.minute()).unwrap_or(0);
        let second = i8::try_from(value.second()).unwrap_or(0);
        let nanos = i32::try_from(value.nanosecond().min(MAX_NANOS)).unwrap_or(0);
        Self::new(hour, minute, second, nanos).unwrap_or(Self::MIN)
    }
}

impl JiffBridge for chrono::NaiveTime {
    type Jiff = civil::Time;

    fn to_jiff(self) -> civil::Time {
        civil::Time::from_chrono(self)
    }
}

impl ChronoBridge for civil::DateTime {
    type Chrono = chrono::NaiveDateTime;

    fn to_chrono(self) -> Self::Chrono {
        self.date().to_chrono().and_time(self.time().to_chrono())
    }

    fn from_chrono(value: Self::Chrono) -> Self {
        use chrono::Datelike as _;
        let in_range =
            i16::try_from(value.date().year()).is_ok_and(|y| (-9999..=9999).contains(&y));
        if !in_range {
            return if value.date().year() < 0 {
                Self::MIN
            } else {
                Self::MAX
            };
        }
        civil::Date::from_chrono(value.date()).to_datetime(civil::Time::from_chrono(value.time()))
    }
}

impl JiffBridge for chrono::NaiveDateTime {
    type Jiff = civil::DateTime;

    fn to_jiff(self) -> civil::DateTime {
        civil::DateTime::from_chrono(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_including_pre_epoch_fractions() {
        for (secs, nanos) in [
            (0, 0),
            (1_700_000_000, 123_456_789),
            (-1, 999_000_000),
            (-86_400, 1),
        ] {
            let ts = Timestamp::new(secs, nanos).unwrap();
            let dt = ts.to_chrono();
            assert_eq!(
                dt.timestamp_nanos_opt(),
                i64::try_from(ts.as_nanosecond()).ok()
            );
            assert_eq!(dt.to_jiff(), ts);
        }
        assert_eq!(Timestamp::MAX.to_chrono().to_jiff(), Timestamp::MAX);
        assert_eq!(Timestamp::MIN.to_chrono().to_jiff(), Timestamp::MIN);
    }

    #[test]
    fn chrono_values_past_jiffs_range_saturate() {
        assert_eq!(
            chrono::DateTime::<chrono::Utc>::MAX_UTC.to_jiff(),
            Timestamp::MAX
        );
        assert_eq!(
            chrono::DateTime::<chrono::Utc>::MIN_UTC.to_jiff(),
            Timestamp::MIN
        );
        assert_eq!(chrono::NaiveDateTime::MAX.to_jiff(), civil::DateTime::MAX);
        assert_eq!(chrono::NaiveDate::MIN.to_jiff(), civil::Date::MIN);
        assert_eq!(
            chrono::TimeDelta::MAX.to_jiff().to_chrono(),
            chrono::TimeDelta::MAX
        );
        assert_eq!(SignedDuration::MAX.to_chrono(), chrono::TimeDelta::MAX);
        assert_eq!(SignedDuration::MIN.to_chrono(), chrono::TimeDelta::MIN);
    }

    #[test]
    fn durations_and_civil_values_round_trip() {
        for d in [
            SignedDuration::ZERO,
            SignedDuration::new(90, 500),
            SignedDuration::new(-90, -500),
        ] {
            assert_eq!(d.to_chrono().to_jiff(), d);
        }
        assert_eq!(
            SignedDuration::new(-1, -500_000_000).to_chrono(),
            chrono::TimeDelta::milliseconds(-1_500)
        );
        let dt = civil::date(2024, 2, 29).at(23, 59, 58, 7);
        assert_eq!(dt.to_chrono().to_string(), "2024-02-29 23:59:58.000000007");
        assert_eq!(dt.to_chrono().to_jiff(), dt);
        assert_eq!(dt.date().to_chrono().to_jiff(), dt.date());
        assert_eq!(dt.time().to_chrono().to_jiff(), dt.time());
    }
}
