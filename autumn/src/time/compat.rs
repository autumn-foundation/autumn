//! Text formats that stay byte-identical across the move to `jiff`.
//!
//! Framework code writes timestamps into places where every byte matters:
//! JSON on the wire, `SQLite` `TEXT` columns compared as strings, hash
//! preimages, HTTP headers and mail headers. `jiff`'s own printers differ from
//! the bytes those places already hold in a few spots: it trims every trailing
//! zero of a fraction (`.1`, not `.100`), it prints `Z` where an RFC 3339
//! offset was `+00:00`, and its RFC 2822 printer writes `-0000` for UTC. The
//! helpers here reproduce the established formats exactly:
//!
//! - [`rfc3339`] / [`rfc3339_opts`]: `2024-01-02T03:04:05.100+00:00`, with the
//!   fraction in groups of 0, 3, 6 or 9 digits ([`Digits::AutoSi`]) or fixed.
//! - [`rfc2822`]: `Tue, 2 Jan 2024 03:04:05 +0000` (day not zero-padded).
//! - [`http_date`]: `Tue, 02 Jan 2024 03:04:05 GMT` (RFC 9110 IMF-fixdate).
//! - [`sqlite_text`] / [`sqlite_naive_text`]: `2024-01-02 03:04:05.100+00:00`
//!   and `2024-01-02 03:04:05.100`, which sort correctly as text.
//! - [`serde_autosi`] / [`serde_autosi_option`]: serde adapters that write
//!   `2024-01-02T03:04:05.100Z` and read anything `jiff` parses.
//! - [`ChronoJson`]: a `Serialize` wrapper that writes a `Timestamp`,
//!   `civil::DateTime`, `civil::Date` or `civil::Time` (or an `Option` of one)
//!   in the text `chrono`'s serde wrote for the type it replaced.
//!
//! Year handling follows the established RFC 3339 writer: years `0..=9999`
//! print as four digits, any other year with an explicit sign (`-0001`).
//!
//! These helpers never fail and never panic.

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

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::civil;
use jiff::tz::Offset;

/// How many fractional-second digits [`rfc3339_opts`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Digits {
    /// No fraction: `03:04:05`.
    Secs,
    /// Always three digits, truncated: `03:04:05.100`.
    Millis,
    /// Always six digits, truncated: `03:04:05.100000`.
    Micros,
    /// Always nine digits: `03:04:05.100000000`.
    Nanos,
    /// The shortest of 0, 3, 6 or 9 digits that holds the value exactly:
    /// `03:04:05`, `03:04:05.100`, `03:04:05.100200`, `03:04:05.100200300`.
    AutoSi,
}

/// `ts` as RFC 3339 with a `+00:00` offset and [`Digits::AutoSi`] fraction,
/// e.g. `2024-01-02T03:04:05.100+00:00`.
#[must_use]
pub fn rfc3339(ts: Timestamp) -> String {
    rfc3339_opts(ts, Digits::AutoSi, false)
}

/// `ts` as RFC 3339 with the chosen fraction width. `use_z` writes `Z` for
/// the UTC offset, otherwise `+00:00`.
#[must_use]
pub fn rfc3339_opts(ts: Timestamp, digits: Digits, use_z: bool) -> String {
    let mut out = String::with_capacity(35);
    write_civil(&mut out, Offset::UTC.to_datetime(ts), 'T', digits);
    out.push_str(if use_z { "Z" } else { "+00:00" });
    out
}

/// `ts` as RFC 2822 in UTC, e.g. `Tue, 2 Jan 2024 03:04:05 +0000`.
///
/// The day of the month is not zero-padded, and the offset is `+0000`. RFC
/// 2822 only covers years `0..=9999`; any other year is written with its sign
/// so the output stays readable instead of failing.
#[must_use]
pub fn rfc2822(ts: Timestamp) -> String {
    let dt = Offset::UTC.to_datetime(ts);
    let mut out = String::with_capacity(31);
    out.push_str(weekday_abbrev(dt.weekday()));
    out.push_str(", ");
    let _ = write!(out, "{}", dt.day());
    out.push(' ');
    out.push_str(month_abbrev(dt.month()));
    out.push(' ');
    write_year(&mut out, dt.year());
    let _ = write!(
        out,
        " {:02}:{:02}:{:02} +0000",
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    out
}

/// `ts` as an HTTP date (RFC 9110 IMF-fixdate), e.g.
/// `Tue, 02 Jan 2024 03:04:05 GMT`.
#[must_use]
pub fn http_date(ts: Timestamp) -> String {
    let dt = Offset::UTC.to_datetime(ts);
    let mut out = String::with_capacity(29);
    out.push_str(weekday_abbrev(dt.weekday()));
    let _ = write!(out, ", {:02} ", dt.day());
    out.push_str(month_abbrev(dt.month()));
    out.push(' ');
    write_year(&mut out, dt.year());
    let _ = write!(
        out,
        " {:02}:{:02}:{:02} GMT",
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    out
}

/// `ts` as `SQLite` `TEXT` for a timezone-aware column, e.g.
/// `2024-01-02 03:04:05.100+00:00`.
///
/// A space separates date and time, the fraction is [`Digits::AutoSi`], and
/// the offset is always `+00:00`. Values in this shape sort correctly as text,
/// so `SQLite` range comparisons on the column keep working.
#[must_use]
pub fn sqlite_text(ts: Timestamp) -> String {
    let mut out = String::with_capacity(35);
    write_civil(&mut out, Offset::UTC.to_datetime(ts), ' ', Digits::AutoSi);
    out.push_str("+00:00");
    out
}

/// `dt` as `SQLite` `TEXT` for a timezone-naive column, e.g.
/// `2024-01-02 03:04:05.100`: [`sqlite_text`] without the offset.
#[must_use]
pub fn sqlite_naive_text(dt: civil::DateTime) -> String {
    let mut out = String::with_capacity(29);
    write_civil(&mut out, dt, ' ', Digits::AutoSi);
    out
}

/// Serde adapter for a [`Timestamp`] that writes RFC 3339 with `Z` and a
/// [`Digits::AutoSi`] fraction (`2024-01-02T03:04:05.100Z`).
///
/// Use it with `#[serde(with = "autumn_web::time::compat::serde_autosi")]`.
/// Reading accepts anything [`Timestamp`]'s parser accepts: `Z` or a numeric
/// offset, a `T` or space separator, and any fraction up to nine digits.
pub mod serde_autosi {
    use jiff::Timestamp;
    use serde::{Deserialize as _, Deserializer, Serializer};

    /// Write `ts` as `2024-01-02T03:04:05.100Z`.
    ///
    /// # Errors
    ///
    /// Returns the serializer's own error.
    pub fn serialize<S: Serializer>(ts: &Timestamp, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::rfc3339_opts(*ts, super::Digits::AutoSi, true))
    }

    /// Read a timestamp from an RFC 3339 / RFC 9557 string.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is not a string `jiff` can parse as a
    /// timestamp.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Timestamp, D::Error> {
        Timestamp::deserialize(deserializer)
    }
}

/// [`serde_autosi`] for an `Option<Timestamp>`: `None` is `null`.
///
/// Combine with `#[serde(default)]` when the field may be absent.
pub mod serde_autosi_option {
    use jiff::Timestamp;
    use serde::{Deserialize as _, Deserializer, Serializer};

    /// Write `Some(ts)` as `2024-01-02T03:04:05.100Z` and `None` as `null`.
    ///
    /// # Errors
    ///
    /// Returns the serializer's own error.
    pub fn serialize<S: Serializer>(
        ts: &Option<Timestamp>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match ts {
            Some(ts) => {
                serializer.serialize_some(&super::rfc3339_opts(*ts, super::Digits::AutoSi, true))
            }
            None => serializer.serialize_none(),
        }
    }

    /// Read an optional timestamp; `null` is `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when a present value is not a string `jiff` can parse
    /// as a timestamp.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Timestamp>, D::Error> {
        Option::<Timestamp>::deserialize(deserializer)
    }
}

/// Serializes a `jiff` time value in the text `chrono`'s serde impls wrote for
/// the type it replaced.
///
/// `#[model]`'s durable codec writes each time column through this, so a
/// ledger snapshot or commit-hook payload stays byte-identical across the
/// move from `chrono` to `jiff`, and `ledger_verify` compares an unchanged row
/// equal to a head written before that move.
///
/// | Type                     | Written as                       | `chrono` type it matches |
/// | ------------------------ | -------------------------------- | ------------------------ |
/// | [`Timestamp`]            | `2024-01-02T03:04:05.120Z`       | `DateTime<Utc>`          |
/// | [`civil::DateTime`]      | `2024-01-02T03:04:05.120`        | `NaiveDateTime`          |
/// | [`civil::Time`]          | `03:04:05.120`                   | `NaiveTime`              |
/// | [`civil::Date`]          | `2024-01-02`                     | `NaiveDate`              |
///
/// The fraction is [`Digits::AutoSi`]. `jiff`'s own serde writes the shortest
/// fraction (`.12`). An `Option` of any of these writes `null` for `None`.
///
/// A year outside `0..=9999` is written in `jiff`'s own form (`-000001`), not
/// `chrono`'s (`-0001`): `jiff` cannot read the shorter form back, and the
/// codec has to round-trip.
#[derive(Debug, Clone, Copy)]
pub struct ChronoJson<'a, T>(pub &'a T);

impl serde::Serialize for ChronoJson<'_, Timestamp> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let ts = *self.0;
        if four_digit_year(Offset::UTC.to_datetime(ts).year()) {
            serializer.serialize_str(&rfc3339_opts(ts, Digits::AutoSi, true))
        } else {
            ts.serialize(serializer)
        }
    }
}

impl serde::Serialize for ChronoJson<'_, civil::DateTime> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let dt = *self.0;
        if four_digit_year(dt.year()) {
            let mut out = String::with_capacity(29);
            write_civil(&mut out, dt, 'T', Digits::AutoSi);
            serializer.serialize_str(&out)
        } else {
            dt.serialize(serializer)
        }
    }
}

impl serde::Serialize for ChronoJson<'_, civil::Time> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let t = *self.0;
        let mut out = String::with_capacity(18);
        let _ = write!(out, "{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second());
        // A civil time's fraction is always in `0..=999_999_999`.
        let nanos = u32::try_from(t.subsec_nanosecond()).unwrap_or(0);
        write_fraction(&mut out, nanos, Digits::AutoSi);
        serializer.serialize_str(&out)
    }
}

impl serde::Serialize for ChronoJson<'_, civil::Date> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // `jiff` and `chrono` agree on `YYYY-MM-DD` for every four-digit year.
        self.0.serialize(serializer)
    }
}

macro_rules! chrono_json_option {
    ($($ty:ty),* $(,)?) => {$(
        impl serde::Serialize for ChronoJson<'_, Option<$ty>> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self.0 {
                    Some(value) => serializer.serialize_some(&ChronoJson(value)),
                    None => serializer.serialize_none(),
                }
            }
        }
    )*};
}

chrono_json_option!(Timestamp, civil::DateTime, civil::Time, civil::Date);

/// Whether `year` prints as four digits in both `jiff` and `chrono`.
fn four_digit_year(year: i16) -> bool {
    (0..=9999).contains(&year)
}

/// Write `YYYY-MM-DD<sep>HH:MM:SS[.fraction]`.
fn write_civil(out: &mut String, dt: civil::DateTime, sep: char, digits: Digits) {
    write_year(out, dt.year());
    let _ = write!(out, "-{:02}-{:02}", dt.month(), dt.day());
    out.push(sep);
    let _ = write!(
        out,
        "{:02}:{:02}:{:02}",
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    // A civil time's fraction is always in `0..=999_999_999`.
    let nanos = u32::try_from(dt.subsec_nanosecond()).unwrap_or(0);
    write_fraction(out, nanos, digits);
}

/// Four digits for `0..=9999`, otherwise the year with an explicit sign.
fn write_year(out: &mut String, year: i16) {
    if (0..=9999).contains(&year) {
        let _ = write!(out, "{year:04}");
    } else {
        let _ = write!(out, "{year:+05}");
    }
}

/// Append the fraction of a second, with its leading dot, at `digits` width.
fn write_fraction(out: &mut String, nanos: u32, digits: Digits) {
    let _ = match digits {
        Digits::Secs => Ok(()),
        Digits::Millis => write!(out, ".{:03}", nanos / 1_000_000),
        Digits::Micros => write!(out, ".{:06}", nanos / 1_000),
        Digits::Nanos => write!(out, ".{nanos:09}"),
        Digits::AutoSi => {
            if nanos == 0 {
                Ok(())
            } else if nanos.is_multiple_of(1_000_000) {
                write!(out, ".{:03}", nanos / 1_000_000)
            } else if nanos.is_multiple_of(1_000) {
                write!(out, ".{:06}", nanos / 1_000)
            } else {
                write!(out, ".{nanos:09}")
            }
        }
    };
}

/// English three-letter weekday name.
const fn weekday_abbrev(weekday: civil::Weekday) -> &'static str {
    match weekday {
        civil::Weekday::Monday => "Mon",
        civil::Weekday::Tuesday => "Tue",
        civil::Weekday::Wednesday => "Wed",
        civil::Weekday::Thursday => "Thu",
        civil::Weekday::Friday => "Fri",
        civil::Weekday::Saturday => "Sat",
        civil::Weekday::Sunday => "Sun",
    }
}

/// English three-letter month name for a month in `1..=12`.
const fn month_abbrev(month: i8) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn rfc3339_uses_a_numeric_utc_offset_and_autosi_digits() {
        assert_eq!(
            rfc3339(ts("2024-01-02T03:04:05Z")),
            "2024-01-02T03:04:05+00:00"
        );
        assert_eq!(
            rfc3339(ts("2024-01-02T03:04:05.1Z")),
            "2024-01-02T03:04:05.100+00:00"
        );
        assert_eq!(
            rfc3339(ts("2024-01-02T03:04:05.1002Z")),
            "2024-01-02T03:04:05.100200+00:00"
        );
        assert_eq!(
            rfc3339(ts("2024-01-02T03:04:05.000000001Z")),
            "2024-01-02T03:04:05.000000001+00:00"
        );
    }

    #[test]
    fn rfc3339_opts_matches_each_fixed_width() {
        let t = ts("2024-01-02T03:04:05.123456789Z");
        assert_eq!(rfc3339_opts(t, Digits::Secs, true), "2024-01-02T03:04:05Z");
        assert_eq!(
            rfc3339_opts(t, Digits::Millis, true),
            "2024-01-02T03:04:05.123Z"
        );
        assert_eq!(
            rfc3339_opts(t, Digits::Micros, false),
            "2024-01-02T03:04:05.123456+00:00"
        );
        assert_eq!(
            rfc3339_opts(t, Digits::Nanos, true),
            "2024-01-02T03:04:05.123456789Z"
        );
        let whole = ts("2024-01-02T03:04:05Z");
        assert_eq!(
            rfc3339_opts(whole, Digits::Millis, true),
            "2024-01-02T03:04:05.000Z"
        );
        assert_eq!(
            rfc3339_opts(ts("2024-01-02T03:04:05.1Z"), Digits::AutoSi, true),
            "2024-01-02T03:04:05.100Z"
        );
    }

    #[test]
    fn rfc3339_signs_years_outside_four_digits() {
        assert_eq!(
            rfc3339(ts("-000001-06-01T00:00:00Z")),
            "-0001-06-01T00:00:00+00:00"
        );
        assert_eq!(
            rfc3339(ts("0000-01-01T00:00:00Z")),
            "0000-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn rfc3339_covers_the_ends_of_the_range() {
        assert_eq!(
            rfc3339(Timestamp::MAX),
            "9999-12-30T22:00:00.999999999+00:00"
        );
        assert_eq!(rfc3339(Timestamp::MIN), "-9999-01-02T01:59:59+00:00");
        assert_eq!(rfc3339(Timestamp::UNIX_EPOCH), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn pre_epoch_fractions_are_written_forward_from_the_second() {
        // One millisecond before the epoch is 23:59:59.999, not .-001.
        assert_eq!(
            rfc3339(Timestamp::new(-1, 999_000_000).unwrap()),
            "1969-12-31T23:59:59.999+00:00"
        );
        assert_eq!(
            rfc3339(Timestamp::new(0, -1_000_000).unwrap()),
            "1969-12-31T23:59:59.999+00:00"
        );
    }

    #[test]
    fn ledger_micros_preimage_matches_jiff_precision_display() {
        // The ledger hashes `format!("{:.6}", ts)`; it must stay the
        // six-digit `Z` form existing chains were hashed with.
        for s in [
            "2026-09-01T18:08:53Z",
            "2026-09-01T18:08:53.5Z",
            "2026-09-01T18:08:53.123456789Z",
        ] {
            let t = ts(s);
            assert_eq!(format!("{t:.6}"), rfc3339_opts(t, Digits::Micros, true));
        }
        assert_eq!(
            format!("{:.6}", ts("2026-09-01T18:08:53Z")),
            "2026-09-01T18:08:53.000000Z"
        );
    }

    #[test]
    fn rfc2822_has_an_unpadded_day_and_plus_zero_offset() {
        assert_eq!(
            rfc2822(ts("2024-01-02T03:04:05.9Z")),
            "Tue, 2 Jan 2024 03:04:05 +0000"
        );
        assert_eq!(
            rfc2822(ts("2003-07-15T10:52:37Z")),
            "Tue, 15 Jul 2003 10:52:37 +0000"
        );
    }

    #[test]
    fn http_date_is_imf_fixdate() {
        assert_eq!(
            http_date(ts("2024-01-02T03:04:05.9Z")),
            "Tue, 02 Jan 2024 03:04:05 GMT"
        );
        assert_eq!(
            http_date(ts("1994-11-06T08:49:37Z")),
            "Sun, 06 Nov 1994 08:49:37 GMT"
        );
    }

    #[test]
    fn sqlite_text_sorts_and_matches_the_stored_shape() {
        assert_eq!(
            sqlite_text(ts("2024-01-02T03:04:05Z")),
            "2024-01-02 03:04:05+00:00"
        );
        assert_eq!(
            sqlite_text(ts("2024-01-02T03:04:05.1Z")),
            "2024-01-02 03:04:05.100+00:00"
        );
        assert_eq!(
            sqlite_naive_text(civil::date(2024, 1, 2).at(3, 4, 5, 120_000_000)),
            "2024-01-02 03:04:05.120"
        );
        assert_eq!(
            sqlite_naive_text(civil::date(2024, 1, 2).at(3, 4, 5, 0)),
            "2024-01-02 03:04:05"
        );
        // `+` sorts below `.`, so a whole second sorts before its fractions.
        let whole = sqlite_text(ts("2024-01-02T03:04:05Z"));
        let frac = sqlite_text(ts("2024-01-02T03:04:05.001Z"));
        let next = sqlite_text(ts("2024-01-02T03:04:06Z"));
        assert!(whole < frac && frac < next);
    }

    #[test]
    fn serde_autosi_writes_grouped_digits_and_z() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Row {
            #[serde(with = "serde_autosi")]
            at: Timestamp,
            #[serde(with = "serde_autosi_option", default)]
            maybe: Option<Timestamp>,
        }
        let row = Row {
            at: ts("2024-01-02T03:04:05.1Z"),
            maybe: Some(ts("2024-01-02T03:04:05Z")),
        };
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(
            json,
            r#"{"at":"2024-01-02T03:04:05.100Z","maybe":"2024-01-02T03:04:05Z"}"#
        );
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap(), row);

        let none = Row {
            at: ts("2024-01-02T03:04:05.000001Z"),
            maybe: None,
        };
        let json = serde_json::to_string(&none).unwrap();
        assert_eq!(json, r#"{"at":"2024-01-02T03:04:05.000001Z","maybe":null}"#);
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap(), none);
    }

    #[test]
    fn serde_autosi_reads_the_offsets_older_writers_used() {
        #[derive(serde::Deserialize)]
        struct Row {
            #[serde(with = "serde_autosi")]
            at: Timestamp,
        }
        let want = ts("2024-01-02T03:04:05.1Z");
        for input in [
            "2024-01-02T03:04:05.100Z",
            "2024-01-02T03:04:05.100+00:00",
            "2024-01-02T03:04:05.1-00:00",
            "2024-01-02 03:04:05.100+00:00",
            "2024-01-02t03:04:05.100z",
            "2024-01-02T05:04:05.100+02:00",
        ] {
            let json = format!(r#"{{"at":"{input}"}}"#);
            let row: Row = serde_json::from_str(&json).unwrap();
            assert_eq!(row.at, want, "{input}");
        }
        let missing: Result<Row, _> = serde_json::from_str(r#"{"at":"not a time"}"#);
        assert!(missing.is_err());
    }

    fn chrono_json<T>(value: &T) -> String
    where
        for<'a> ChronoJson<'a, T>: serde::Serialize,
    {
        serde_json::to_string(&ChronoJson(value)).unwrap()
    }

    /// Each row is a fraction and the text `chrono` 0.4's serde wrote for
    /// `DateTime<Utc>`, `NaiveDateTime` and `NaiveTime` at `2026-09-01
    /// 18:08:53` plus that fraction.
    const CHRONO_FRACTIONS: [(i32, &str, &str, &str); 8] = [
        (0, "2026-09-01T18:08:53Z", "2026-09-01T18:08:53", "18:08:53"),
        (
            120_000_000,
            "2026-09-01T18:08:53.120Z",
            "2026-09-01T18:08:53.120",
            "18:08:53.120",
        ),
        (
            100_000_000,
            "2026-09-01T18:08:53.100Z",
            "2026-09-01T18:08:53.100",
            "18:08:53.100",
        ),
        (
            123_000,
            "2026-09-01T18:08:53.000123Z",
            "2026-09-01T18:08:53.000123",
            "18:08:53.000123",
        ),
        (
            123_400_000,
            "2026-09-01T18:08:53.123400Z",
            "2026-09-01T18:08:53.123400",
            "18:08:53.123400",
        ),
        (
            123_456_789,
            "2026-09-01T18:08:53.123456789Z",
            "2026-09-01T18:08:53.123456789",
            "18:08:53.123456789",
        ),
        (
            1,
            "2026-09-01T18:08:53.000000001Z",
            "2026-09-01T18:08:53.000000001",
            "18:08:53.000000001",
        ),
        (
            999_999_999,
            "2026-09-01T18:08:53.999999999Z",
            "2026-09-01T18:08:53.999999999",
            "18:08:53.999999999",
        ),
    ];

    #[test]
    fn chrono_json_matches_chrono_serde_byte_for_byte() {
        for (nanos, utc, naive, time) in CHRONO_FRACTIONS {
            let dt = civil::date(2026, 9, 1).at(18, 8, 53, nanos);
            let ts = Offset::UTC.to_timestamp(dt).unwrap();
            assert_eq!(chrono_json(&ts), format!("\"{utc}\""), "{nanos}");
            assert_eq!(chrono_json(&dt), format!("\"{naive}\""), "{nanos}");
            assert_eq!(chrono_json(&dt.time()), format!("\"{time}\""), "{nanos}");
            assert_eq!(chrono_json(&dt.date()), "\"2026-09-01\"");
            assert_eq!(chrono_json(&Some(ts)), format!("\"{utc}\""));
            assert_eq!(chrono_json(&Some(dt)), format!("\"{naive}\""));
            assert_eq!(chrono_json(&Some(dt.time())), format!("\"{time}\""));
            assert_eq!(chrono_json(&Some(dt.date())), "\"2026-09-01\"");
        }
        assert_eq!(chrono_json(&None::<Timestamp>), "null");
        assert_eq!(chrono_json(&None::<civil::DateTime>), "null");
        assert_eq!(chrono_json(&None::<civil::Time>), "null");
        assert_eq!(chrono_json(&None::<civil::Date>), "null");
    }

    #[test]
    fn chrono_json_pads_every_four_digit_year() {
        for (year, text) in [
            (0, "0000"),
            (1, "0001"),
            (999, "0999"),
            (2026, "2026"),
            (9999, "9999"),
        ] {
            let dt = civil::date(year, 1, 2).at(3, 4, 5, 120_000_000);
            assert_eq!(chrono_json(&dt), format!("\"{text}-01-02T03:04:05.120\""));
            assert_eq!(chrono_json(&dt.date()), format!("\"{text}-01-02\""));
            if let Ok(instant) = Offset::UTC.to_timestamp(dt) {
                assert_eq!(
                    chrono_json(&instant),
                    format!("\"{text}-01-02T03:04:05.120Z\"")
                );
            }
        }
    }

    #[test]
    fn chrono_json_reads_back_through_jiff() {
        fn round_trip<T>(value: &T)
        where
            T: serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
            for<'a> ChronoJson<'a, T>: serde::Serialize,
        {
            let json = serde_json::to_value(ChronoJson(value)).unwrap();
            assert_eq!(&serde_json::from_value::<T>(json).unwrap(), value);
        }
        for (nanos, ..) in CHRONO_FRACTIONS {
            let dt = civil::date(2026, 9, 1).at(18, 8, 53, nanos);
            round_trip(&dt);
            round_trip(&dt.time());
            round_trip(&dt.date());
            round_trip(&Offset::UTC.to_timestamp(dt).unwrap());
            round_trip(&Some(dt));
            round_trip(&None::<civil::Time>);
        }
        // A negative year keeps `jiff`'s six-digit form, which `jiff` reads.
        let ancient = ts("-000001-06-01T00:00:00.12Z");
        assert_eq!(chrono_json(&ancient), "\"-000001-06-01T00:00:00.12Z\"");
        round_trip(&ancient);
        round_trip(&civil::date(-1, 6, 1).at(0, 0, 0, 120_000_000));
        round_trip(&Timestamp::MIN);
        round_trip(&Timestamp::MAX);
    }
}
