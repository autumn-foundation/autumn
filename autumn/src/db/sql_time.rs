//! Diesel conversions for `jiff` date and time values.
//!
//! Model and application code holds plain `jiff` values
//! ([`Timestamp`], [`civil::DateTime`], [`civil::Date`], [`civil::Time`]).
//! Diesel can only bind and load a type that implements its traits, and both
//! `jiff` and `diesel` are foreign to this crate, so the conversions live on
//! local newtypes:
//!
//! | Rust value                   | Wrapper                      | Postgres SQL type | `SQLite` SQL type     |
//! | ---------------------------- | ---------------------------- | ----------------- | --------------------- |
//! | [`Timestamp`]                | [`SqlTimestamp`]             | `Timestamptz`     | `TimestamptzSqlite`   |
//! | [`civil::DateTime`]          | [`SqlDateTime`]              | `Timestamp` (and `Timestamptz` as UTC) | `Timestamp` (and `TimestamptzSqlite`) |
//! | [`civil::Date`]              | [`SqlDate`]                  | `Date`            | `Date`                |
//! | [`civil::Time`]              | [`SqlTime`]                  | `Time`            | `Time`                |
//! | `Option<…>` of the above     | [`SqlNullableTimestamp`] etc.| `Nullable<…>`     | `Nullable<…>`         |
//!
//! Each wrapper is `#[repr(transparent)]`, converts with `From`/`Into` in
//! both directions, derefs to the wrapped value and is `#[serde(transparent)]`.
//! The [`AsSqlTime`] extension trait wraps a value inline:
//!
//! ```rust,ignore
//! use autumn_web::db::AsSqlTime as _;
//!
//! posts::table.filter(posts::created_at.lt(cutoff.as_sql_time()));
//! ```
//!
//! `Option<SqlTimestamp>` already binds and loads as `Nullable<…>`. The
//! `SqlNullable*` wrappers exist for `#[diesel(deserialize_as = …)]` on an
//! `Option<Timestamp>` field: that attribute needs a type that converts into
//! `Option<Timestamp>`, which the orphan rule forbids for
//! `Option<SqlTimestamp>`.
//!
//! # Postgres
//!
//! Values travel in Postgres' binary encoding: microseconds since
//! `2000-01-01 00:00:00` for timestamps, days since `2000-01-01` for dates and
//! microseconds since midnight for times. Sub-microsecond digits are
//! truncated toward zero on write.
//!
//! `jiff` covers years `-9999..=9999`; Postgres covers more, and also stores
//! `infinity` and `-infinity`. A timestamp or date past the end of `jiff`'s
//! range reads back as the matching `MAX`/`MIN` constant instead of failing,
//! so a far-future sentinel row (for example a `run_at` in year 262142)
//! stays readable. Writing `Timestamp::MAX` and reading it back gives
//! `Timestamp::MAX` again. Postgres starts at 4713 BC, so it rejects a write
//! of `Timestamp::MIN` or anything else earlier than that.
//!
//! # `SQLite`
//!
//! `SQLite` has no date type; values are `TEXT`. The writer produces the
//! established layout:
//!
//! - timestamp: `2024-01-02 03:04:05.100+00:00`
//! - civil date-time: `2024-01-02 03:04:05.100`
//! - date: `2024-01-02`
//! - time: `03:04:05.100`
//!
//! The fraction has 0, 3, 6 or 9 digits, the fewest that hold the value
//! exactly. Values in this layout sort correctly as text, so `ORDER BY`,
//! `<`/`>` and `=` on the column keep matching the rows already stored.
//!
//! The reader accepts every layout an existing database is likely to hold: a
//! space or `T` separator, minutes without seconds, any number of fraction
//! digits (beyond nine they are ignored), an optional `Z` or `±HH:MM` offset
//! (absent means UTC, which is what `DEFAULT CURRENT_TIMESTAMP` writes) and a
//! julian day number such as `julianday('now')` stores. A civil date-time
//! read from text with an offset keeps the wall-clock digits and ignores the
//! offset.

use std::fmt;
use std::ops::Deref;

use diesel::sql_types::{Date, Nullable, Time, Timestamp as SqlTimestampType};
use jiff::Timestamp;
use jiff::civil;
use serde::{Deserialize, Serialize};

/// A [`Timestamp`] that Diesel binds as `Timestamptz` (Postgres) or
/// `TimestamptzSqlite` (`SQLite`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[cfg_attr(feature = "postgres", diesel(sql_type = diesel::sql_types::Timestamptz))]
#[diesel(sql_type = diesel::sql_types::TimestamptzSqlite)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlTimestamp(pub Timestamp);

/// A [`civil::DateTime`] that Diesel binds as `Timestamp`.
///
/// It also binds as `Timestamptz` (Postgres) and `TimestamptzSqlite`
/// (`SQLite`), reading and writing the wall-clock value as UTC.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[diesel(sql_type = SqlTimestampType)]
#[cfg_attr(feature = "postgres", diesel(sql_type = diesel::sql_types::Timestamptz))]
#[diesel(sql_type = diesel::sql_types::TimestamptzSqlite)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlDateTime(pub civil::DateTime);

/// A [`civil::Date`] that Diesel binds as `Date`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[diesel(sql_type = Date)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlDate(pub civil::Date);

/// A [`civil::Time`] that Diesel binds as `Time`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[diesel(sql_type = Time)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlTime(pub civil::Time);

/// An `Option<Timestamp>` that Diesel binds as `Nullable<Timestamptz>`
/// (Postgres) or `Nullable<TimestamptzSqlite>` (`SQLite`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[cfg_attr(
    feature = "postgres",
    diesel(sql_type = Nullable<diesel::sql_types::Timestamptz>)
)]
#[diesel(sql_type = Nullable<diesel::sql_types::TimestamptzSqlite>)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlNullableTimestamp(pub Option<Timestamp>);

/// An `Option<civil::DateTime>` that Diesel binds as `Nullable<Timestamp>`,
/// and also as `Nullable<Timestamptz>` (Postgres) and
/// `Nullable<TimestamptzSqlite>` (`SQLite`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[diesel(sql_type = Nullable<SqlTimestampType>)]
#[cfg_attr(
    feature = "postgres",
    diesel(sql_type = Nullable<diesel::sql_types::Timestamptz>)
)]
#[diesel(sql_type = Nullable<diesel::sql_types::TimestamptzSqlite>)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlNullableDateTime(pub Option<civil::DateTime>);

/// An `Option<civil::Date>` that Diesel binds as `Nullable<Date>`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[diesel(sql_type = Nullable<Date>)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlNullableDate(pub Option<civil::Date>);

/// An `Option<civil::Time>` that Diesel binds as `Nullable<Time>`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    diesel::AsExpression,
    diesel::FromSqlRow,
)]
#[diesel(sql_type = Nullable<Time>)]
#[serde(transparent)]
#[repr(transparent)]
pub struct SqlNullableTime(pub Option<civil::Time>);

/// The conversions every wrapper shares: `into_inner`, `From` in both
/// directions (and from a reference), `Deref`, `AsRef`.
macro_rules! wrapper_conversions {
    ($name:ident($inner:ty)) => {
        impl $name {
            /// The wrapped value.
            #[must_use]
            pub const fn into_inner(self) -> $inner {
                self.0
            }
        }

        impl From<$inner> for $name {
            fn from(value: $inner) -> Self {
                Self(value)
            }
        }

        impl From<&$inner> for $name {
            fn from(value: &$inner) -> Self {
                Self(*value)
            }
        }

        impl From<$name> for $inner {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl Deref for $name {
            type Target = $inner;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl AsRef<$inner> for $name {
            fn as_ref(&self) -> &$inner {
                &self.0
            }
        }
    };
}

wrapper_conversions!(SqlTimestamp(Timestamp));
wrapper_conversions!(SqlDateTime(civil::DateTime));
wrapper_conversions!(SqlDate(civil::Date));
wrapper_conversions!(SqlTime(civil::Time));
wrapper_conversions!(SqlNullableTimestamp(Option<Timestamp>));
wrapper_conversions!(SqlNullableDateTime(Option<civil::DateTime>));
wrapper_conversions!(SqlNullableDate(Option<civil::Date>));
wrapper_conversions!(SqlNullableTime(Option<civil::Time>));

/// `Display` for a non-null wrapper prints the wrapped value unchanged.
macro_rules! wrapper_display {
    ($($name:ident),*) => {
        $(
            impl fmt::Display for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    fmt::Display::fmt(&self.0, f)
                }
            }
        )*
    };
}

wrapper_display!(SqlTimestamp, SqlDateTime, SqlDate, SqlTime);

/// Conversions between a nullable wrapper and `Option` of its non-null
/// wrapper.
macro_rules! nullable_option_conversions {
    ($nullable:ident => $wrapper:ident) => {
        impl From<Option<$wrapper>> for $nullable {
            fn from(value: Option<$wrapper>) -> Self {
                Self(value.map(|v| v.0))
            }
        }

        impl From<$nullable> for Option<$wrapper> {
            fn from(value: $nullable) -> Self {
                value.0.map($wrapper)
            }
        }
    };
}

nullable_option_conversions!(SqlNullableTimestamp => SqlTimestamp);
nullable_option_conversions!(SqlNullableDateTime => SqlDateTime);
nullable_option_conversions!(SqlNullableDate => SqlDate);
nullable_option_conversions!(SqlNullableTime => SqlTime);

/// `ToSql`/`FromSql` for one SQL type on one backend, for a wrapper and its
/// nullable twin, from an `encode` and a `decode` function over the wrapped
/// value.
macro_rules! backend_impls {
    (
        $backend:ty, $st:ty,
        $wrapper:ident, $nullable:ident,
        encode = $encode:path, decode = $decode:path $(,)?
    ) => {
        impl ToSql<$st, $backend> for $wrapper {
            fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, $backend>) -> serialize::Result {
                $encode(self.0, out)
            }
        }

        impl FromSql<$st, $backend> for $wrapper {
            fn from_sql(
                bytes: <$backend as diesel::backend::Backend>::RawValue<'_>,
            ) -> deserialize::Result<Self> {
                $decode(bytes).map(Self)
            }
        }

        impl ToSql<Nullable<$st>, $backend> for $nullable {
            fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, $backend>) -> serialize::Result {
                match self.0 {
                    Some(value) => $encode(value, out),
                    None => Ok(IsNull::Yes),
                }
            }
        }

        impl FromSql<Nullable<$st>, $backend> for $nullable {
            fn from_sql(
                bytes: <$backend as diesel::backend::Backend>::RawValue<'_>,
            ) -> deserialize::Result<Self> {
                $decode(bytes).map(|value| Self(Some(value)))
            }

            fn from_nullable_sql(
                bytes: Option<<$backend as diesel::backend::Backend>::RawValue<'_>>,
            ) -> deserialize::Result<Self> {
                match bytes {
                    Some(bytes) => <Self as FromSql<Nullable<$st>, $backend>>::from_sql(bytes),
                    None => Ok(Self(None)),
                }
            }
        }
    };
}

/// Wrap a `jiff` value for a Diesel query: `cutoff.as_sql_time()`.
///
/// The method is not called `as_sql`: Diesel's prelude brings
/// `diesel::IntoSql::as_sql` into scope for every type, and the two names
/// would be ambiguous.
///
/// ```rust,ignore
/// use autumn_web::db::AsSqlTime as _;
///
/// diesel::update(jobs::table.filter(jobs::run_at.le(now.as_sql_time())))
///     .set(jobs::locked_until.eq(lease.as_sql_time()));
/// ```
pub trait AsSqlTime {
    /// The Diesel wrapper for this value.
    type Sql;

    /// This value, wrapped for Diesel.
    fn as_sql_time(&self) -> Self::Sql;
}

macro_rules! as_sql_time {
    ($($inner:ty => $wrapper:ident),* $(,)?) => {
        $(
            impl AsSqlTime for $inner {
                type Sql = $wrapper;

                fn as_sql_time(&self) -> $wrapper {
                    $wrapper(*self)
                }
            }
        )*
    };
}

as_sql_time!(
    Timestamp => SqlTimestamp,
    civil::DateTime => SqlDateTime,
    civil::Date => SqlDate,
    civil::Time => SqlTime,
    Option<Timestamp> => SqlNullableTimestamp,
    Option<civil::DateTime> => SqlNullableDateTime,
    Option<civil::Date> => SqlNullableDate,
    Option<civil::Time> => SqlNullableTime,
);

// ── Postgres ──────────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use diesel::deserialize::{self, FromSql};
    use diesel::pg::data_types::{PgDate, PgTime, PgTimestamp};
    use diesel::pg::{Pg, PgValue};
    use diesel::serialize::{self, IsNull, Output, ToSql};
    use diesel::sql_types::{Date, Nullable, Time, Timestamp as SqlTimestampType, Timestamptz};
    use jiff::civil;
    use jiff::{SignedDuration, Span, Timestamp};

    use super::{
        SqlDate, SqlDateTime, SqlNullableDate, SqlNullableDateTime, SqlNullableTime,
        SqlNullableTimestamp, SqlTime, SqlTimestamp,
    };

    /// Postgres' epoch for timestamps and dates: `2000-01-01 00:00:00`.
    const EPOCH_DATETIME: civil::DateTime = civil::date(2000, 1, 1).at(0, 0, 0, 0);
    const EPOCH_DATE: civil::Date = civil::date(2000, 1, 1);
    /// `2000-01-01T00:00:00Z` as Unix seconds.
    const EPOCH_UNIX_SECONDS: i64 = 946_684_800;

    fn epoch_timestamp() -> Timestamp {
        Timestamp::from_second(EPOCH_UNIX_SECONDS).unwrap_or(Timestamp::UNIX_EPOCH)
    }

    /// Microseconds from Postgres' epoch to `ts`, truncated toward zero.
    pub(super) fn timestamp_micros(
        ts: Timestamp,
    ) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        i64::try_from(ts.duration_since(epoch_timestamp()).as_micros())
            .map_err(|_| format!("timestamp {ts} is out of range for Postgres").into())
    }

    /// Microseconds from Postgres' epoch to `dt`, truncated toward zero.
    pub(super) fn datetime_micros(
        dt: civil::DateTime,
    ) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        i64::try_from(dt.duration_since(EPOCH_DATETIME).as_micros())
            .map_err(|_| format!("date-time {dt} is out of range for Postgres").into())
    }

    /// The timestamp `micros` after Postgres' epoch, saturating at
    /// [`Timestamp::MAX`] / [`Timestamp::MIN`].
    ///
    /// Any value at or past the microsecond `Timestamp::MAX` falls in
    /// (including `infinity`, stored as `i64::MAX`) reads as
    /// `Timestamp::MAX`, so a written `Timestamp::MAX` reads back unchanged.
    pub(super) fn timestamp_from_micros(micros: i64) -> Timestamp {
        let max = timestamp_micros(Timestamp::MAX).unwrap_or(i64::MAX);
        let min = timestamp_micros(Timestamp::MIN).unwrap_or(i64::MIN);
        if micros >= max {
            return Timestamp::MAX;
        }
        if micros <= min {
            return Timestamp::MIN;
        }
        epoch_timestamp()
            .checked_add(SignedDuration::from_micros(micros))
            .unwrap_or(if micros > 0 {
                Timestamp::MAX
            } else {
                Timestamp::MIN
            })
    }

    /// The civil date-time `micros` after Postgres' epoch, saturating at
    /// [`civil::DateTime::MAX`] / [`civil::DateTime::MIN`].
    pub(super) fn datetime_from_micros(micros: i64) -> civil::DateTime {
        let max = datetime_micros(civil::DateTime::MAX).unwrap_or(i64::MAX);
        let min = datetime_micros(civil::DateTime::MIN).unwrap_or(i64::MIN);
        if micros >= max {
            return civil::DateTime::MAX;
        }
        if micros <= min {
            return civil::DateTime::MIN;
        }
        EPOCH_DATETIME
            .checked_add(SignedDuration::from_micros(micros))
            .unwrap_or(if micros > 0 {
                civil::DateTime::MAX
            } else {
                civil::DateTime::MIN
            })
    }

    /// The date `days` after Postgres' epoch, saturating at
    /// [`civil::Date::MAX`] / [`civil::Date::MIN`].
    pub(super) fn date_from_days(days: i32) -> civil::Date {
        Span::new()
            .try_days(days)
            .ok()
            .and_then(|span| EPOCH_DATE.checked_add(span).ok())
            .unwrap_or(if days > 0 {
                civil::Date::MAX
            } else {
                civil::Date::MIN
            })
    }

    fn write_timestamp(ts: Timestamp, out: &mut Output<'_, '_, Pg>) -> serialize::Result {
        let value = PgTimestamp(timestamp_micros(ts)?);
        ToSql::<Timestamptz, Pg>::to_sql(&value, &mut out.reborrow())
    }

    fn read_timestamp(bytes: PgValue<'_>) -> deserialize::Result<Timestamp> {
        let PgTimestamp(micros) = FromSql::<Timestamptz, Pg>::from_sql(bytes)?;
        Ok(timestamp_from_micros(micros))
    }

    fn write_datetime(dt: civil::DateTime, out: &mut Output<'_, '_, Pg>) -> serialize::Result {
        let value = PgTimestamp(datetime_micros(dt)?);
        ToSql::<SqlTimestampType, Pg>::to_sql(&value, &mut out.reborrow())
    }

    fn read_datetime(bytes: PgValue<'_>) -> deserialize::Result<civil::DateTime> {
        let PgTimestamp(micros) = FromSql::<SqlTimestampType, Pg>::from_sql(bytes)?;
        Ok(datetime_from_micros(micros))
    }

    // `Timestamp` and `Timestamptz` share one binary encoding (microseconds
    // since the epoch, in UTC for `Timestamptz`), so a civil date-time binds to
    // a `Timestamptz` column as a UTC wall-clock value.
    fn write_datetime_tz(dt: civil::DateTime, out: &mut Output<'_, '_, Pg>) -> serialize::Result {
        let value = PgTimestamp(datetime_micros(dt)?);
        ToSql::<Timestamptz, Pg>::to_sql(&value, &mut out.reborrow())
    }

    fn read_datetime_tz(bytes: PgValue<'_>) -> deserialize::Result<civil::DateTime> {
        let PgTimestamp(micros) = FromSql::<Timestamptz, Pg>::from_sql(bytes)?;
        Ok(datetime_from_micros(micros))
    }

    fn write_date(date: civil::Date, out: &mut Output<'_, '_, Pg>) -> serialize::Result {
        let days = i32::try_from(date.duration_since(EPOCH_DATE).as_secs() / 86_400)
            .map_err(|_| format!("date {date} is out of range for Postgres"))?;
        ToSql::<Date, Pg>::to_sql(&PgDate(days), &mut out.reborrow())
    }

    fn read_date(bytes: PgValue<'_>) -> deserialize::Result<civil::Date> {
        let PgDate(days) = FromSql::<Date, Pg>::from_sql(bytes)?;
        Ok(date_from_days(days))
    }

    fn write_time(time: civil::Time, out: &mut Output<'_, '_, Pg>) -> serialize::Result {
        let micros = i64::try_from(time.duration_since(civil::Time::midnight()).as_micros())
            .map_err(|_| format!("time {time} is out of range for Postgres"))?;
        ToSql::<Time, Pg>::to_sql(&PgTime(micros), &mut out.reborrow())
    }

    fn read_time(bytes: PgValue<'_>) -> deserialize::Result<civil::Time> {
        let PgTime(micros) = FromSql::<Time, Pg>::from_sql(bytes)?;
        Ok(civil::Time::midnight().checked_add(SignedDuration::from_micros(micros))?)
    }

    backend_impls!(
        Pg,
        Timestamptz,
        SqlTimestamp,
        SqlNullableTimestamp,
        encode = write_timestamp,
        decode = read_timestamp,
    );
    backend_impls!(
        Pg,
        SqlTimestampType,
        SqlDateTime,
        SqlNullableDateTime,
        encode = write_datetime,
        decode = read_datetime,
    );
    backend_impls!(
        Pg,
        Timestamptz,
        SqlDateTime,
        SqlNullableDateTime,
        encode = write_datetime_tz,
        decode = read_datetime_tz,
    );
    backend_impls!(
        Pg,
        Date,
        SqlDate,
        SqlNullableDate,
        encode = write_date,
        decode = read_date,
    );
    backend_impls!(
        Pg,
        Time,
        SqlTime,
        SqlNullableTime,
        encode = write_time,
        decode = read_time,
    );
}

// ── SQLite ────────────────────────────────────────────────────────────────────

mod sqlite {
    use std::fmt::Write as _;

    use diesel::backend::Backend;
    use diesel::deserialize::{self, FromSql};
    use diesel::serialize::{self, IsNull, Output, ToSql};
    use diesel::sql_types::{
        Date, Nullable, Time, Timestamp as SqlTimestampType, TimestamptzSqlite,
    };
    use diesel::sqlite::Sqlite;
    use jiff::Timestamp;
    use jiff::civil;
    use jiff::tz::Offset;

    use super::{
        SqlDate, SqlDateTime, SqlNullableDate, SqlNullableDateTime, SqlNullableTime,
        SqlNullableTimestamp, SqlTime, SqlTimestamp,
    };
    use crate::time::compat;

    type RawValue<'a> = <Sqlite as Backend>::RawValue<'a>;
    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    /// `date` as `%Y-%m-%d`: four-digit years, and a sign outside `0..=9999`.
    pub(super) fn date_text(date: civil::Date) -> String {
        let mut out = String::with_capacity(11);
        write_date(&mut out, date);
        out
    }

    /// `time` as `%H:%M:%S` plus a 0-, 3-, 6- or 9-digit fraction.
    pub(super) fn time_text(time: civil::Time) -> String {
        let mut out = String::with_capacity(18);
        write_time(&mut out, time);
        out
    }

    fn write_date(out: &mut String, date: civil::Date) {
        let year = date.year();
        if (0..=9999).contains(&year) {
            let _ = write!(out, "{year:04}");
        } else {
            let _ = write!(out, "{year:+05}");
        }
        let _ = write!(out, "-{:02}-{:02}", date.month(), date.day());
    }

    fn write_time(out: &mut String, time: civil::Time) {
        let _ = write!(
            out,
            "{:02}:{:02}:{:02}",
            time.hour(),
            time.minute(),
            time.second()
        );
        let nanos = time.subsec_nanosecond();
        if nanos == 0 {
            return;
        }
        if nanos % 1_000_000 == 0 {
            let _ = write!(out, ".{:03}", nanos / 1_000_000);
        } else if nanos % 1_000 == 0 {
            let _ = write!(out, ".{:06}", nanos / 1_000);
        } else {
            let _ = write!(out, ".{nanos:09}");
        }
    }

    /// A date and time as read from `SQLite` text, before range checks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Parsed {
        year: i64,
        month: i8,
        day: i8,
        time: ParsedTime,
        /// Seconds east of UTC; text without an offset is UTC.
        offset: i32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(super) struct ParsedTime {
        hour: i8,
        minute: i8,
        second: i8,
        nanos: i32,
    }

    /// A byte cursor over the text being parsed.
    struct Cursor<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> Cursor<'a> {
        const fn new(text: &'a str) -> Self {
            Self {
                bytes: text.as_bytes(),
                pos: 0,
            }
        }

        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.pos).copied()
        }

        const fn at_end(&self) -> bool {
            self.pos >= self.bytes.len()
        }

        fn eat(&mut self, byte: u8) -> bool {
            if self.peek() == Some(byte) {
                self.pos = self.pos.saturating_add(1);
                true
            } else {
                false
            }
        }

        fn eat_any(&mut self, bytes: &[u8]) -> Option<u8> {
            let next = self.peek()?;
            if bytes.contains(&next) {
                self.pos = self.pos.saturating_add(1);
                Some(next)
            } else {
                None
            }
        }

        /// Between `min` and `max` ASCII digits as a number, with the count
        /// read.
        fn digits(&mut self, min: usize, max: usize) -> Option<(i64, usize)> {
            let mut value: i64 = 0;
            let mut count = 0;
            while count < max {
                let Some(byte) = self.peek().filter(u8::is_ascii_digit) else {
                    break;
                };
                value = value
                    .checked_mul(10)?
                    .checked_add(i64::from(byte.wrapping_sub(b'0')))?;
                count += 1;
                self.pos = self.pos.saturating_add(1);
            }
            (count >= min).then_some((value, count))
        }

        /// Exactly two digits.
        fn two(&mut self) -> Option<i8> {
            let (value, _) = self.digits(2, 2)?;
            i8::try_from(value).ok()
        }

        /// Skip any run of ASCII digits.
        fn skip_digits(&mut self) {
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.pos = self.pos.saturating_add(1);
            }
        }
    }

    /// `HH:MM[:SS[.fraction]]`. Fraction digits past nine are ignored, and a
    /// leap second (`60`) reads as `59`.
    fn parse_clock(cur: &mut Cursor<'_>) -> Option<ParsedTime> {
        let hour = cur.two()?;
        if !cur.eat(b':') {
            return None;
        }
        let minute = cur.two()?;
        let mut second = 0;
        let mut nanos = 0;
        if cur.eat(b':') {
            second = cur.two()?.min(59);
            if cur.eat_any(b".,").is_some() {
                let (value, count) = cur.digits(1, 9)?;
                cur.skip_digits();
                let scale =
                    10_i64.checked_pow(u32::try_from(9_usize.saturating_sub(count)).ok()?)?;
                nanos = i32::try_from(value.checked_mul(scale)?).ok()?;
            }
        }
        Some(ParsedTime {
            hour,
            minute,
            second,
            nanos,
        })
    }

    /// An optional `Z` or `±HH[:MM[:SS]]` offset, as seconds east of UTC.
    /// No offset at all is UTC, `0`. `None` means the text is not an offset.
    fn parse_offset(cur: &mut Cursor<'_>) -> Option<i32> {
        if cur.at_end() {
            return Some(0);
        }
        // A single space before the offset (`10:00:00 +02:00`) is tolerated.
        let _ = cur.eat(b' ');
        if cur.eat_any(b"Zz").is_some() {
            return Some(0);
        }
        let sign: i32 = match cur.eat_any(b"+-")? {
            b'-' => -1,
            _ => 1,
        };
        let hours = i32::from(cur.two()?);
        let mut minutes = 0;
        let mut seconds = 0;
        let colon = cur.eat(b':');
        if colon || cur.peek().is_some_and(|b| b.is_ascii_digit()) {
            minutes = i32::from(cur.two()?);
            if (!colon || cur.eat(b':')) && cur.peek().is_some_and(|b| b.is_ascii_digit()) {
                seconds = i32::from(cur.two()?);
            }
        }
        let total = hours
            .checked_mul(3600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(seconds)?;
        sign.checked_mul(total)
    }

    /// `[±]YYYY-MM-DD[( |T)HH:MM[:SS[.f]]][offset]`, surrounding whitespace
    /// ignored. A date with no time is midnight.
    pub(super) fn parse_datetime_text(text: &str) -> Option<Parsed> {
        let mut cur = Cursor::new(text.trim());
        let negative = match cur.eat_any(b"+-") {
            Some(b'-') => true,
            Some(_) | None => false,
        };
        let (year, _) = cur.digits(4, 9)?;
        let year = if negative { year.checked_neg()? } else { year };
        if !cur.eat(b'-') {
            return None;
        }
        let month = cur.two()?;
        if !cur.eat(b'-') {
            return None;
        }
        let day = cur.two()?;
        let (time, offset) = if cur.at_end() {
            (ParsedTime::default(), 0)
        } else {
            cur.eat_any(b" Tt")?;
            let time = parse_clock(&mut cur)?;
            (time, parse_offset(&mut cur)?)
        };
        cur.at_end().then_some(Parsed {
            year,
            month,
            day,
            time,
            offset,
        })
    }

    /// `HH:MM[:SS[.f]][offset]`; the offset is read and ignored.
    pub(super) fn parse_time_text(text: &str) -> Option<civil::Time> {
        let mut cur = Cursor::new(text.trim());
        let time = parse_clock(&mut cur)?;
        parse_offset(&mut cur)?;
        if !cur.at_end() {
            return None;
        }
        civil::Time::new(time.hour, time.minute, time.second, time.nanos).ok()
    }

    /// Years `jiff` cannot hold, as the end of the range they fall past.
    enum Range<T> {
        In(T),
        Past(T),
    }

    fn civil_from_parsed(parsed: Parsed) -> Result<Range<civil::DateTime>, BoxError> {
        if parsed.year > 9999 {
            return Ok(Range::Past(civil::DateTime::MAX));
        }
        if parsed.year < -9999 {
            return Ok(Range::Past(civil::DateTime::MIN));
        }
        let year = i16::try_from(parsed.year)?;
        let t = parsed.time;
        let dt = civil::DateTime::new(
            year,
            parsed.month,
            parsed.day,
            t.hour,
            t.minute,
            t.second,
            t.nanos,
        )?;
        Ok(Range::In(dt))
    }

    /// A timestamp from its `SQLite` text. No offset means UTC. Values past
    /// either end of `jiff`'s range saturate to [`Timestamp::MAX`] /
    /// [`Timestamp::MIN`], which keeps a stored `9999-12-31 23:59:59` sentinel
    /// readable.
    pub(super) fn timestamp_from_text(text: &str) -> Result<Timestamp, BoxError> {
        if let Some(parsed) = parse_datetime_text(text) {
            let dt = match civil_from_parsed(parsed)? {
                Range::In(dt) => dt,
                Range::Past(dt) => return Ok(saturate_timestamp(dt.year())),
            };
            let offset = Offset::from_seconds(parsed.offset)?;
            return Ok(offset
                .to_timestamp(dt)
                .unwrap_or_else(|_| saturate_timestamp(dt.year())));
        }
        if let Some(ts) = timestamp_from_julian_text(text) {
            return Ok(ts);
        }
        Err(format!("invalid timestamp {text:?}").into())
    }

    /// A civil date-time from its `SQLite` text. An offset in the text is
    /// ignored; the wall-clock digits are kept. Years past either end of
    /// `jiff`'s range saturate to [`civil::DateTime::MAX`] /
    /// [`civil::DateTime::MIN`]. A julian day number reads as its UTC
    /// date-time.
    pub(super) fn datetime_from_text(text: &str) -> Result<civil::DateTime, BoxError> {
        if let Some(parsed) = parse_datetime_text(text) {
            return match civil_from_parsed(parsed)? {
                Range::In(dt) | Range::Past(dt) => Ok(dt),
            };
        }
        if let Some(ts) = timestamp_from_julian_text(text) {
            return Ok(Offset::UTC.to_datetime(ts));
        }
        Err(format!("invalid date-time {text:?}").into())
    }

    /// A date from its `SQLite` text. A time part, if present, is ignored.
    pub(super) fn date_from_text(text: &str) -> Result<civil::Date, BoxError> {
        let parsed = parse_datetime_text(text).ok_or_else(|| format!("invalid date {text:?}"))?;
        match civil_from_parsed(parsed)? {
            Range::In(dt) | Range::Past(dt) => Ok(dt.date()),
        }
    }

    pub(super) fn time_from_text(text: &str) -> Result<civil::Time, BoxError> {
        parse_time_text(text).ok_or_else(|| format!("invalid time {text:?}").into())
    }

    const fn saturate_timestamp(year: i16) -> Timestamp {
        if year > 0 {
            Timestamp::MAX
        } else {
            Timestamp::MIN
        }
    }

    /// A julian day number (`julianday('now')`), as `SQLite` stores a `REAL`
    /// date. A negative fraction of a second before 1970 is dropped.
    fn timestamp_from_julian_text(text: &str) -> Option<Timestamp> {
        const EPOCH_IN_JULIAN_DAYS: f64 = 2_440_587.5;
        const SECONDS_IN_DAY: f64 = 86_400.0;
        let julian_days = text.trim().parse::<f64>().ok()?;
        if !julian_days.is_finite() {
            return None;
        }
        let seconds = (julian_days - EPOCH_IN_JULIAN_DAYS) * SECONDS_IN_DAY;
        // `seconds` is finite here; an `as` cast saturates out-of-range
        // values, which `Timestamp::new` then rejects.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "whole seconds and nanoseconds are truncated on purpose"
        )]
        let (whole, nanos) = (
            seconds.trunc() as i64,
            (seconds.fract() * 1e9).max(0.0) as i32,
        );
        Timestamp::new(whole, nanos).ok()
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "every encoder returns the `serialize::Result` that `ToSql` does"
    )]
    fn write_text(text: String, out: &mut Output<'_, '_, Sqlite>) -> serialize::Result {
        out.set_value(text);
        Ok(IsNull::No)
    }

    fn write_timestamp(ts: Timestamp, out: &mut Output<'_, '_, Sqlite>) -> serialize::Result {
        write_text(compat::sqlite_text(ts), out)
    }

    fn write_datetime(dt: civil::DateTime, out: &mut Output<'_, '_, Sqlite>) -> serialize::Result {
        write_text(compat::sqlite_naive_text(dt), out)
    }

    fn write_date_value(date: civil::Date, out: &mut Output<'_, '_, Sqlite>) -> serialize::Result {
        write_text(date_text(date), out)
    }

    fn write_time_value(time: civil::Time, out: &mut Output<'_, '_, Sqlite>) -> serialize::Result {
        write_text(time_text(time), out)
    }

    fn read_timestamp(mut value: RawValue<'_>) -> deserialize::Result<Timestamp> {
        timestamp_from_text(value.read_text())
    }

    fn read_datetime(mut value: RawValue<'_>) -> deserialize::Result<civil::DateTime> {
        datetime_from_text(value.read_text())
    }

    fn read_date(mut value: RawValue<'_>) -> deserialize::Result<civil::Date> {
        date_from_text(value.read_text())
    }

    fn read_time(mut value: RawValue<'_>) -> deserialize::Result<civil::Time> {
        time_from_text(value.read_text())
    }

    backend_impls!(
        Sqlite,
        TimestamptzSqlite,
        SqlTimestamp,
        SqlNullableTimestamp,
        encode = write_timestamp,
        decode = read_timestamp,
    );
    backend_impls!(
        Sqlite,
        SqlTimestampType,
        SqlDateTime,
        SqlNullableDateTime,
        encode = write_datetime,
        decode = read_datetime,
    );
    backend_impls!(
        Sqlite,
        TimestamptzSqlite,
        SqlDateTime,
        SqlNullableDateTime,
        encode = write_datetime,
        decode = read_datetime,
    );
    backend_impls!(
        Sqlite,
        Date,
        SqlDate,
        SqlNullableDate,
        encode = write_date_value,
        decode = read_date,
    );
    backend_impls!(
        Sqlite,
        Time,
        SqlTime,
        SqlNullableTime,
        encode = write_time_value,
        decode = read_time,
    );
}

#[cfg(test)]
mod tests {
    use diesel::prelude::*;
    use jiff::civil::{date, time};
    use jiff::{SignedDuration, Timestamp};

    use super::sqlite::{
        date_from_text, date_text, datetime_from_text, time_from_text, time_text,
        timestamp_from_text,
    };
    use super::*;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid timestamp literal")
    }

    // ── SQLite text ──────────────────────────────────────────────────────────

    #[test]
    fn sqlite_writer_uses_the_established_text_layout() {
        assert_eq!(
            crate::time::compat::sqlite_text(ts("2024-01-02T03:04:05Z")),
            "2024-01-02 03:04:05+00:00"
        );
        assert_eq!(
            crate::time::compat::sqlite_text(ts("2024-01-02T03:04:05.1Z")),
            "2024-01-02 03:04:05.100+00:00"
        );
        assert_eq!(
            crate::time::compat::sqlite_naive_text(date(2024, 1, 2).at(3, 4, 5, 120_000)),
            "2024-01-02 03:04:05.000120"
        );
        assert_eq!(date_text(date(2024, 1, 2)), "2024-01-02");
        assert_eq!(date_text(date(-1, 1, 2)), "-0001-01-02");
        assert_eq!(time_text(time(3, 4, 5, 0)), "03:04:05");
        assert_eq!(time_text(time(3, 4, 5, 500_000_000)), "03:04:05.500");
        assert_eq!(time_text(time(3, 4, 5, 123_456_000)), "03:04:05.123456");
        assert_eq!(time_text(time(3, 4, 5, 123_456_789)), "03:04:05.123456789");
    }

    #[test]
    fn sqlite_reader_accepts_every_legacy_timestamp_shape() {
        let expected = ts("2024-01-02T03:04:05Z");
        let with_millis = ts("2024-01-02T03:04:05.1Z");
        let cases = [
            // The established writer.
            ("2024-01-02 03:04:05+00:00", expected),
            ("2024-01-02 03:04:05.100+00:00", with_millis),
            // No offset, as `CURRENT_TIMESTAMP` and naive writers store it.
            ("2024-01-02 03:04:05", expected),
            ("2024-01-02 03:04:05.100", with_millis),
            // `strftime('%Y-%m-%d %H:%M:%f')`.
            ("2024-01-02 03:04:05.100", with_millis),
            // RFC 3339 spellings.
            ("2024-01-02T03:04:05Z", expected),
            ("2024-01-02t03:04:05z", expected),
            ("2024-01-02T03:04:05.100Z", with_millis),
            ("2024-01-02T05:04:05+02:00", expected),
            ("2024-01-02T05:04:05+0200", expected),
            ("2023-01-01T23:04:05-04:00", ts("2023-01-02T03:04:05Z")),
            ("2024-01-02 03:04:05 +00:00", expected),
            // Minutes without seconds.
            ("2024-01-02 03:04", ts("2024-01-02T03:04:00Z")),
            ("2024-01-02T03:04Z", ts("2024-01-02T03:04:00Z")),
            // Fraction digits past nine are ignored.
            (
                "2024-01-02 03:04:05.1234567891234",
                ts("2024-01-02T03:04:05.123456789Z"),
            ),
            // A date alone is midnight.
            ("2024-01-02", ts("2024-01-02T00:00:00Z")),
            // Surrounding whitespace.
            ("  2024-01-02 03:04:05  ", expected),
        ];
        for (text, want) in cases {
            assert_eq!(
                timestamp_from_text(text).expect(text),
                want,
                "reading {text:?}"
            );
        }
    }

    #[test]
    fn sqlite_reader_saturates_values_past_jiffs_range() {
        assert_eq!(
            timestamp_from_text("9999-12-31 23:59:59+00:00").expect("old sentinel"),
            Timestamp::MAX
        );
        assert_eq!(
            timestamp_from_text("+262142-12-31 23:59:59.999999999+00:00").expect("chrono max"),
            Timestamp::MAX
        );
        assert_eq!(
            timestamp_from_text("-262143-01-01 00:00:00+00:00").expect("chrono min"),
            Timestamp::MIN
        );
        assert_eq!(
            datetime_from_text("+10000-01-01 00:00:00").expect("past civil max"),
            civil::DateTime::MAX
        );
        // `Timestamp::MAX` itself round-trips through the writer.
        assert_eq!(
            timestamp_from_text(&crate::time::compat::sqlite_text(Timestamp::MAX))
                .expect("max round trip"),
            Timestamp::MAX
        );
    }

    #[test]
    fn sqlite_reader_rejects_garbage() {
        for text in [
            "",
            "yesterday",
            "2024-13-01 00:00:00",
            "2024-02-30 00:00:00",
            "2024-01-02 25:00:00",
            "2024-01-02 03:04:05+",
            "2024-01-02 03:04:05 UTC",
            "2024-01-02X03:04:05",
        ] {
            assert!(timestamp_from_text(text).is_err(), "{text:?} should fail");
        }
    }

    #[test]
    fn sqlite_reader_for_civil_values_keeps_the_wall_clock() {
        assert_eq!(
            datetime_from_text("2024-01-02 03:04:05.100+02:00").expect("offset ignored"),
            date(2024, 1, 2).at(3, 4, 5, 100_000_000)
        );
        assert_eq!(
            datetime_from_text("2024-01-02T03:04:05Z").expect("Z ignored"),
            date(2024, 1, 2).at(3, 4, 5, 0)
        );
        assert_eq!(
            date_from_text("2024-01-02").expect("date"),
            date(2024, 1, 2)
        );
        assert_eq!(time_from_text("09:30").expect("HH:MM"), time(9, 30, 0, 0));
        assert_eq!(
            time_from_text("09:30:15.250+00:00").expect("offset ignored"),
            time(9, 30, 15, 250_000_000)
        );
        assert!(time_from_text("9:30").is_err());
    }

    #[test]
    fn sqlite_reader_decodes_julian_day_numbers() {
        // 2024-01-02T12:00:00Z is julian day 2460312.0.
        assert_eq!(
            timestamp_from_text("2460312.0").expect("julian"),
            ts("2024-01-02T12:00:00Z")
        );
        assert_eq!(
            datetime_from_text("2460312.5").expect("julian"),
            date(2024, 1, 3).at(0, 0, 0, 0)
        );
        assert!(timestamp_from_text("NaN").is_err());
        assert!(timestamp_from_text("1e300").is_err());
    }

    // ── SQLite connection ────────────────────────────────────────────────────

    diesel::table! {
        sql_time_rows (id) {
            id -> Integer,
            at -> TimestamptzSqlite,
            wall -> Timestamp,
            day -> Date,
            clock -> Time,
            maybe_at -> Nullable<TimestamptzSqlite>,
            maybe_wall -> Nullable<Timestamp>,
        }
    }

    #[derive(Debug, PartialEq, Eq, Queryable, Insertable, Selectable)]
    #[diesel(table_name = sql_time_rows)]
    #[diesel(check_for_backend(diesel::sqlite::Sqlite))]
    struct Row {
        id: i32,
        at: SqlTimestamp,
        wall: SqlDateTime,
        day: SqlDate,
        clock: SqlTime,
        maybe_at: SqlNullableTimestamp,
        maybe_wall: Option<SqlDateTime>,
    }

    fn sqlite() -> SqliteConnection {
        let mut conn =
            SqliteConnection::establish(":memory:").expect("in-memory SQLite connection");
        diesel::sql_query(
            "CREATE TABLE sql_time_rows (
                id INTEGER PRIMARY KEY,
                at TEXT NOT NULL,
                wall TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                day TEXT NOT NULL DEFAULT CURRENT_DATE,
                clock TEXT NOT NULL DEFAULT CURRENT_TIME,
                maybe_at TEXT,
                maybe_wall TEXT
            )",
        )
        .execute(&mut conn)
        .expect("create table");
        conn
    }

    #[test]
    fn sqlite_round_trips_every_wrapper() {
        let mut conn = sqlite();
        let rows = vec![
            Row {
                id: 1,
                at: SqlTimestamp(ts("2024-01-02T03:04:05.123456789Z")),
                wall: SqlDateTime(date(2024, 1, 2).at(3, 4, 5, 100_000_000)),
                day: SqlDate(date(2024, 1, 2)),
                clock: SqlTime(time(9, 30, 0, 0)),
                maybe_at: SqlNullableTimestamp(Some(ts("2030-06-01T00:00:00Z"))),
                maybe_wall: Some(SqlDateTime(date(2030, 6, 1).at(0, 0, 0, 0))),
            },
            Row {
                id: 2,
                at: SqlTimestamp(Timestamp::MAX),
                wall: SqlDateTime(civil::DateTime::MIN),
                day: SqlDate(civil::Date::MAX),
                clock: SqlTime(civil::Time::MAX),
                maybe_at: SqlNullableTimestamp(None),
                maybe_wall: None,
            },
        ];
        diesel::insert_into(sql_time_rows::table)
            .values(&rows)
            .execute(&mut conn)
            .expect("insert");

        let loaded: Vec<Row> = sql_time_rows::table
            .order(sql_time_rows::id)
            .select(Row::as_select())
            .load(&mut conn)
            .expect("load");
        assert_eq!(loaded, rows);

        let stored: Vec<String> =
            diesel::sql_query("SELECT at AS text FROM sql_time_rows ORDER BY id")
                .load::<TextRow>(&mut conn)
                .expect("raw text")
                .into_iter()
                .map(|row| row.text)
                .collect();
        assert_eq!(
            stored,
            [
                "2024-01-02 03:04:05.123456789+00:00",
                "9999-12-30 22:00:00.999999999+00:00",
            ]
        );
    }

    #[derive(QueryableByName)]
    struct TextRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        text: String,
    }

    #[test]
    fn sqlite_text_order_and_equality_hold_across_old_and_new_rows() {
        let mut conn = sqlite();
        // Rows as an earlier writer stored them.
        for (id, text) in [
            (10, "2024-01-02 03:04:05.100+00:00"),
            (11, "2024-01-02 03:04:05+00:00"),
            (12, "2024-01-02 03:04:05.000001+00:00"),
            (13, "2024-01-02 03:04:06+00:00"),
        ] {
            diesel::sql_query("INSERT INTO sql_time_rows (id, at) VALUES (?, ?)")
                .bind::<diesel::sql_types::Integer, _>(id)
                .bind::<diesel::sql_types::Text, _>(text)
                .execute(&mut conn)
                .expect("insert legacy row");
        }
        // Rows through the wrappers.
        for (id, at) in [
            (20, ts("2024-01-02T03:04:05.05Z")),
            (21, ts("2024-01-02T03:04:05.999999999Z")),
            (22, ts("2024-01-02T03:04:04Z")),
        ] {
            diesel::insert_into(sql_time_rows::table)
                .values((
                    sql_time_rows::id.eq(id),
                    sql_time_rows::at.eq(at.as_sql_time()),
                ))
                .execute(&mut conn)
                .expect("insert new row");
        }

        let by_text: Vec<SqlTimestamp> = sql_time_rows::table
            .order(sql_time_rows::at)
            .select(sql_time_rows::at)
            .load(&mut conn)
            .expect("ordered load");
        let mut by_value = by_text.clone();
        by_value.sort();
        assert_eq!(by_text, by_value, "text order must match time order");

        // A new value equal to an old row matches it byte for byte.
        let found: Vec<i32> = sql_time_rows::table
            .filter(sql_time_rows::at.eq(ts("2024-01-02T03:04:05.1Z").as_sql_time()))
            .select(sql_time_rows::id)
            .load(&mut conn)
            .expect("equality filter");
        assert_eq!(found, [10]);

        let later: i64 = sql_time_rows::table
            .filter(sql_time_rows::at.gt(ts("2024-01-02T03:04:05Z").as_sql_time()))
            .count()
            .get_result(&mut conn)
            .expect("range filter");
        assert_eq!(later, 5);
    }

    #[test]
    fn sqlite_decodes_current_timestamp_and_sqlite_date_functions() {
        let mut conn = sqlite();
        diesel::sql_query("INSERT INTO sql_time_rows (id, at) VALUES (1, CURRENT_TIMESTAMP)")
            .execute(&mut conn)
            .expect("insert defaults");
        let row: Row = sql_time_rows::table
            .select(Row::as_select())
            .first(&mut conn)
            .expect("defaults decode");
        assert!(row.at.0 > ts("2020-01-01T00:00:00Z"), "{row:?}");
        assert_eq!(row.wall.date(), row.day.0);

        let at: SqlTimestamp =
            diesel::select(diesel::dsl::sql::<diesel::sql_types::TimestamptzSqlite>(
                "strftime('%Y-%m-%d %H:%M:%f', '2024-01-02 03:04:05.678')",
            ))
            .get_result(&mut conn)
            .expect("strftime decodes");
        assert_eq!(at.0, ts("2024-01-02T03:04:05.678Z"));

        let wall: SqlDateTime = diesel::select(diesel::dsl::sql::<diesel::sql_types::Timestamp>(
            "datetime('2024-01-02 03:04:05')",
        ))
        .get_result(&mut conn)
        .expect("datetime() decodes");
        assert_eq!(wall.0, date(2024, 1, 2).at(3, 4, 5, 0));
    }

    #[test]
    fn sqlite_decodes_julianday_reals() {
        let mut conn = sqlite();
        let at: SqlTimestamp = diesel::select(diesel::dsl::sql::<
            diesel::sql_types::TimestamptzSqlite,
        >("julianday('2024-01-02 03:04:05')"))
        .get_result(&mut conn)
        .expect("julianday decodes");
        let drift = at.0.duration_since(ts("2024-01-02T03:04:05Z")).abs();
        assert!(
            drift < SignedDuration::from_millis(1),
            "{at} drifted {drift:?}"
        );
    }

    #[test]
    fn as_sql_and_conversions_wrap_without_change() {
        let at = ts("2024-01-02T03:04:05Z");
        assert_eq!(at.as_sql_time(), SqlTimestamp(at));
        assert_eq!(*at.as_sql_time(), at);
        assert_eq!(Timestamp::from(SqlTimestamp::from(&at)), at);
        assert_eq!(Some(at).as_sql_time(), SqlNullableTimestamp(Some(at)));
        assert_eq!(
            Option::<Timestamp>::from(SqlNullableTimestamp(Some(at))),
            Some(at)
        );
        assert_eq!(
            Option::<SqlTimestamp>::from(SqlNullableTimestamp(None)),
            None
        );
        assert_eq!(SqlTimestamp(at).to_string(), at.to_string());
        assert_eq!(
            serde_json::to_string(&SqlDate(date(2024, 1, 2))).expect("json"),
            "\"2024-01-02\""
        );
    }

    // ── Postgres ─────────────────────────────────────────────────────────────

    #[cfg(feature = "postgres")]
    #[test]
    fn pg_encoding_saturates_out_of_range_values() {
        use super::pg::{
            date_from_days, datetime_from_micros, datetime_micros, timestamp_from_micros,
            timestamp_micros,
        };

        assert_eq!(timestamp_from_micros(0), ts("2000-01-01T00:00:00Z"));
        assert_eq!(
            timestamp_micros(ts("2000-01-01T00:00:01.000001999Z")).expect("in range"),
            1_000_001
        );
        assert_eq!(
            timestamp_micros(ts("1999-12-31T23:59:59.999999Z")).expect("in range"),
            -1
        );
        // Truncation is toward zero, before and after the epoch alike.
        assert_eq!(
            timestamp_micros(ts("1999-12-31T23:59:59.999999999Z")).expect("in range"),
            0
        );
        assert_eq!(timestamp_from_micros(i64::MAX), Timestamp::MAX);
        assert_eq!(timestamp_from_micros(i64::MIN), Timestamp::MIN);
        // chrono's `MAX_UTC`, +262142-12-31T23:59:59.999999Z, as Postgres
        // stores it.
        assert_eq!(
            timestamp_from_micros(8_210_266_876_799_999_999),
            Timestamp::MAX
        );
        let max = timestamp_micros(Timestamp::MAX).expect("max fits");
        assert_eq!(timestamp_from_micros(max), Timestamp::MAX);
        assert!(timestamp_from_micros(max - 1) < Timestamp::MAX);

        assert_eq!(datetime_from_micros(0), date(2000, 1, 1).at(0, 0, 0, 0));
        assert_eq!(datetime_from_micros(i64::MAX), civil::DateTime::MAX);
        let max = datetime_micros(civil::DateTime::MAX).expect("max fits");
        assert_eq!(datetime_from_micros(max), civil::DateTime::MAX);

        assert_eq!(date_from_days(0), date(2000, 1, 1));
        assert_eq!(date_from_days(-1), date(1999, 12, 31));
        assert_eq!(date_from_days(i32::MAX), civil::Date::MAX);
        assert_eq!(date_from_days(i32::MIN), civil::Date::MIN);
    }
}

/// The Postgres container test sits in its own module so that only the
/// async `RunQueryDsl` is in scope.
#[cfg(all(test, feature = "postgres", feature = "test-support"))]
mod pg_container_tests {
    use jiff::Timestamp;
    use jiff::civil::{self, date, time};

    use super::*;
    use crate::db::prelude::*;

    fn ts(text: &str) -> Timestamp {
        text.parse().expect("valid timestamp literal")
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    #[allow(
        clippy::too_many_lines,
        reason = "one container start covers every wrapper and sentinel"
    )]
    async fn pg_round_trips_wrappers_and_reads_the_far_future_sentinel() {
        use diesel::sql_types::{Nullable, Timestamptz};
        use diesel_async::AsyncPgConnection;
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::postgres::Postgres;

        diesel::table! {
            pg_sql_time_rows (id) {
                id -> Integer,
                at -> Timestamptz,
                wall -> Timestamp,
                day -> Date,
                clock -> Time,
                maybe_at -> Nullable<Timestamptz>,
                wall_tz -> Timestamptz,
            }
        }

        #[derive(Debug, PartialEq, Eq, Queryable, Insertable, Selectable)]
        #[diesel(table_name = pg_sql_time_rows)]
        #[diesel(check_for_backend(diesel::pg::Pg))]
        struct PgRow {
            id: i32,
            at: SqlTimestamp,
            wall: SqlDateTime,
            day: SqlDate,
            clock: SqlTime,
            maybe_at: SqlNullableTimestamp,
            wall_tz: SqlDateTime,
        }

        let container = Postgres::default()
            .start()
            .await
            .expect("failed to start Postgres testcontainer (is Docker running?)");
        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("container port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let mut conn = AsyncPgConnection::establish(&url).await.expect("connect");

        diesel::sql_query(
            "CREATE TABLE pg_sql_time_rows (
                id INTEGER PRIMARY KEY,
                at TIMESTAMPTZ NOT NULL,
                wall TIMESTAMP NOT NULL,
                day DATE NOT NULL,
                clock TIME NOT NULL,
                maybe_at TIMESTAMPTZ,
                wall_tz TIMESTAMPTZ NOT NULL
            )",
        )
        .execute(&mut conn)
        .await
        .expect("create table");

        let rows = vec![
            PgRow {
                id: 1,
                at: SqlTimestamp(ts("2024-01-02T03:04:05.123456Z")),
                wall: SqlDateTime(date(2024, 1, 2).at(3, 4, 5, 100_000_000)),
                day: SqlDate(date(1999, 12, 31)),
                clock: SqlTime(time(9, 30, 0, 250_000)),
                maybe_at: SqlNullableTimestamp(None),
                wall_tz: SqlDateTime(date(2024, 1, 2).at(3, 4, 5, 0)),
            },
            PgRow {
                id: 2,
                at: SqlTimestamp(Timestamp::MAX),
                // Postgres starts at 4713 BC, so `MIN` is not storable.
                wall: SqlDateTime(date(1, 1, 1).at(0, 0, 0, 0)),
                day: SqlDate(civil::Date::MAX),
                clock: SqlTime(time(23, 59, 59, 999_999_000)),
                maybe_at: SqlNullableTimestamp(Some(ts("0001-01-01T00:00:00Z"))),
                wall_tz: SqlDateTime(date(1970, 1, 1).at(0, 0, 0, 0)),
            },
        ];
        diesel::insert_into(pg_sql_time_rows::table)
            .values(&rows)
            .execute(&mut conn)
            .await
            .expect("insert");
        let loaded: Vec<PgRow> = pg_sql_time_rows::table
            .order(pg_sql_time_rows::id)
            .select(PgRow::as_select())
            .load(&mut conn)
            .await
            .expect("load");
        assert_eq!(loaded, rows);

        // A UTC wall-clock value lands on the matching instant.
        let instant: SqlTimestamp = pg_sql_time_rows::table
            .filter(pg_sql_time_rows::id.eq(1))
            .select(pg_sql_time_rows::wall_tz)
            .first::<SqlDateTime>(&mut conn)
            .await
            .map(|wall| {
                SqlTimestamp(
                    jiff::tz::Offset::UTC
                        .to_timestamp(wall.0)
                        .expect("in range"),
                )
            })
            .expect("wall_tz");
        assert_eq!(instant.0, ts("2024-01-02T03:04:05Z"));

        // Sub-microsecond digits are truncated on write.
        let truncated: SqlTimestamp = diesel::select(
            ts("2024-01-02T03:04:05.123456789Z")
                .as_sql_time()
                .into_sql::<Timestamptz>(),
        )
        .get_result(&mut conn)
        .await
        .expect("truncated");
        assert_eq!(truncated.0, ts("2024-01-02T03:04:05.123456Z"));

        // The far-future sentinel an earlier writer stored, plus infinity.
        for literal in [
            "'262142-12-31 23:59:59.999999+00'::timestamptz",
            "'infinity'::timestamptz",
        ] {
            let read: SqlNullableTimestamp =
                diesel::select(diesel::dsl::sql::<Nullable<Timestamptz>>(literal))
                    .get_result(&mut conn)
                    .await
                    .expect(literal);
            assert_eq!(read.0, Some(Timestamp::MAX), "{literal}");
        }
        let read: SqlTimestamp =
            diesel::select(diesel::dsl::sql::<Timestamptz>("'-infinity'::timestamptz"))
                .get_result(&mut conn)
                .await
                .expect("-infinity");
        assert_eq!(read.0, Timestamp::MIN);

        // An array bind of wrappers.
        let stamps = [
            ts("2024-01-02T03:04:05.123456Z"),
            ts("2025-01-02T03:04:05Z"),
        ];
        let count: i64 = pg_sql_time_rows::table
            .filter(
                pg_sql_time_rows::at
                    .eq_any(stamps.iter().map(SqlTimestamp::from).collect::<Vec<_>>()),
            )
            .count()
            .get_result(&mut conn)
            .await
            .expect("array bind");
        assert_eq!(count, 1);
    }
}
