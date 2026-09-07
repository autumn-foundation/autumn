//! `TEXT`-backed newtypes that give foreign model-field types a working
//! `SQLite` conversion (issue #1924).
//!
//! # Why a newtype
//!
//! `uuid::Uuid` and `rust_decimal::Decimal` are foreign to `autumn-web`, and so
//! is every diesel item their conversion would name, so `autumn-web` can
//! implement nothing for them: `ToSql<Text, Sqlite> for Uuid` breaks the orphan
//! rule, and even a *local* sql-type does not rescue `AsExpression`, which
//! diesel already blanket-implements for every `Expression`. A local newtype has
//! neither problem. This is the wrapper diesel prescribes for third-party types,
//! and the reason [`crate::storage::Blob`] needs none — `Blob` is already local.
//!
//! # Storage
//!
//! Both wrappers store `TEXT`, the same declared type the `SQLite` DDL emitter
//! already uses for these kinds. Both write a **canonical** form, because
//! `SQLite` compares `TEXT` byte for byte: two spellings of one value would
//! break `=` and `UNIQUE`.
//!
//! - [`SqliteUuid`] — the hyphenated lowercase form.
//! - [`SqliteDecimal`] — `Decimal::normalize`, which drops trailing fractional
//!   zeros, so `19.990` and `19.99` are one row, not two. The value stays
//!   numerically exact; `REAL` would round-trip through a binary float and lose
//!   digits.
//!
//! # Using them
//!
//! Both are `Copy`, deref to the wrapped type, convert with `From`/`Into`, and
//! are `#[serde(transparent)]`, so JSON and `Display` output match the wrapped
//! type exactly. `autumn generate` renders `Uuid` and `decimal{p,s}` fields as
//! these types on a `SQLite` app; Postgres apps keep `uuid::Uuid` and
//! `rust_decimal::Decimal`, which diesel converts natively there.
//!
//! # Limits
//!
//! **Ordering.** A `TEXT` column sorts lexicographically, so `ORDER BY` / `<` /
//! `>` on a [`SqliteDecimal`] column compares strings, not numbers: `"9"` sorts
//! after `"10"`, and `"-1.4"` before `"-1.5"`. Sort or range-filter in Rust, or
//! store minor units in an `i64`, when SQL ordering matters. [`SqliteUuid`] is
//! unaffected — hyphenated lowercase text sorts in UUID byte order.
//!
//! **Equality on rows this crate did not write.** `=` and `UNIQUE` match the
//! stored bytes. Every value written through these types is canonical, so they
//! agree with Rust equality. A row inserted by hand or migrated from elsewhere
//! may not be: `SqliteUuid` reads any form `uuid::Uuid::parse_str` accepts
//! (braced, URN, unhyphenated, uppercase), and such a row loads correctly but
//! will not match a `find_by_…` lookup for the same UUID. Write canonical text.
//!
//! **Scale.** Postgres `NUMERIC(p, s)` coerces a value to exactly `s` decimal
//! places; `SQLite` stores the normalized value, so a column holding `19.9`
//! reads back as `19.9`, not `19.90`. The two are numerically equal — format
//! for display rather than relying on the stored scale. Declared precision and
//! scale are enforced by a `CHECK` constraint the migration emits.

use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use diesel::deserialize::{self, FromSql};
use diesel::serialize::{self, IsNull, Output, ToSql};
use diesel::sql_types::Text;
use diesel::sqlite::Sqlite;
use serde::{Deserialize, Serialize};

/// Declare a `TEXT`-backed `SQLite` newtype over a foreign `$inner` type.
///
/// `$encode` renders the wrapped value; `FromStr` on `$inner` parses it back.
macro_rules! text_backed_newtype {
    (
        $(#[$meta:meta])*
        $name:ident($inner:ty), encode = |$value:ident| $encode:expr $(,)?
    ) => {
        $(#[$meta])*
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
        #[diesel(sql_type = Text)]
        #[serde(transparent)]
        pub struct $name(pub $inner);

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

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = <$inner as FromStr>::Err;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                <$inner as FromStr>::from_str(s).map(Self)
            }
        }

        impl ToSql<Text, Sqlite> for $name {
            fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
                let $value = &self.0;
                out.set_value($encode);
                Ok(IsNull::No)
            }
        }

        impl FromSql<Text, Sqlite> for $name {
            fn from_sql(
                bytes: <Sqlite as diesel::backend::Backend>::RawValue<'_>,
            ) -> deserialize::Result<Self> {
                let text = <String as FromSql<Text, Sqlite>>::from_sql(bytes)?;
                text.parse().map_err(Into::into)
            }
        }
    };
}

text_backed_newtype!(
    /// A `uuid::Uuid` model field on the `SQLite` backend, stored as `TEXT`
    /// (issue #1924).
    ///
    /// ```rust,ignore
    /// # use autumn_web::db::sqlite_types::SqliteUuid;
    /// # fn demo(id: uuid::Uuid) {
    /// let wrapped: SqliteUuid = id.into();
    /// assert_eq!(wrapped.to_string(), id.to_string());
    /// assert_eq!(*wrapped, id);
    /// # }
    /// ```
    SqliteUuid(uuid::Uuid),
    encode = |value| value.to_string(),
);

text_backed_newtype!(
    /// A `rust_decimal::Decimal` model field on the `SQLite` backend, stored as
    /// `TEXT` (issue #1924).
    ///
    /// Written normalized — trailing fractional zeros dropped — so one value has
    /// one spelling and `SQLite`'s byte-wise `=` and `UNIQUE` agree with Rust
    /// equality. The value stays numerically exact. See the module docs for the
    /// lexicographic-ordering limit, which normalizing does not fix.
    SqliteDecimal(rust_decimal::Decimal),
    encode = |value| value.normalize().to_string(),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_newtype_round_trips_through_text() {
        let id = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let wrapped = SqliteUuid::from(id);
        assert_eq!(wrapped.to_string(), id.to_string());
        assert_eq!(wrapped.to_string().parse::<SqliteUuid>().unwrap(), wrapped);
        assert_eq!(uuid::Uuid::from(wrapped), id);
        assert_eq!(*wrapped, id);
    }

    #[test]
    fn decimal_newtype_round_trips_through_text() {
        let value = SqliteDecimal::from(rust_decimal::Decimal::from_str("-1234.5678").unwrap());
        assert_eq!(value.to_string().parse::<SqliteDecimal>().unwrap(), value);
    }

    /// The property `=` and `UNIQUE` depend on: numerically equal values must
    /// produce identical stored text. `Decimal`'s own `to_string` does not —
    /// it keeps the written scale — which is why the encoder normalizes.
    #[test]
    fn decimal_encoding_is_canonical_per_value() {
        let written = rust_decimal::Decimal::from_str("19.990").unwrap();
        let typed = rust_decimal::Decimal::from_str("19.99").unwrap();
        assert_eq!(written, typed, "the two spellings are one value");
        assert_ne!(
            written.to_string(),
            typed.to_string(),
            "…but Decimal's own text differs, which SQLite would treat as two rows"
        );
        assert_eq!(
            written.normalize().to_string(),
            typed.normalize().to_string()
        );
        assert_eq!(
            rust_decimal::Decimal::from_str("0.10")
                .unwrap()
                .normalize()
                .to_string(),
            "0.1",
        );
    }

    #[test]
    fn newtypes_serialize_transparently() {
        let id = uuid::Uuid::from_u128(9);
        assert_eq!(
            serde_json::to_string(&SqliteUuid::from(id)).unwrap(),
            serde_json::to_string(&id).unwrap(),
        );
        let dec = rust_decimal::Decimal::from_str("-3.25").unwrap();
        assert_eq!(
            serde_json::to_string(&SqliteDecimal::from(dec)).unwrap(),
            serde_json::to_string(&dec).unwrap(),
        );
    }
}
