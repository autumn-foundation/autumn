//! The Diesel column representation of a `#[classified]` column (issue #1654).
//!
//! A classified column is a plain `Text` column in the database -- classification
//! is a statement about where the value may *go*, not about how it is stored (use
//! `#[encrypted]` for at-rest protection). What this wrapper provides is the
//! *only* conversion out of [`Classified`](super::Classified) that is not a
//! declassification.
//!
//! It has to be opaque for that to be safe: `ClassifiedText` has no accessor, no
//! `Serialize`, no `Display` and a redacted `Debug`, so the round trip
//! `Classified<String, _> -> ClassifiedText -> the database` cannot be spliced
//! into a `String` on the way past. Writing a row is not a gated sink; handing
//! the value to a serializer is.

use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql};
use diesel::serialize::{self, Output, ToSql};
use diesel::sql_types::Text;
use diesel::{AsExpression, FromSqlRow};

use super::{Classified, ClassifiedField};

/// The `serialize_as` / `deserialize_as` target `#[model]` gives Diesel for a
/// `#[classified]` column.
#[derive(AsExpression, FromSqlRow, Clone)]
#[diesel(sql_type = Text)]
pub struct ClassifiedText(String);

impl<F: ClassifiedField> From<Classified<String, F>> for ClassifiedText {
    fn from(value: Classified<String, F>) -> Self {
        Self(value.into_column_value())
    }
}

impl<F: ClassifiedField> From<ClassifiedText> for Classified<String, F> {
    fn from(column: ClassifiedText) -> Self {
        Self::new(column.0)
    }
}

/// Redacted: a classified value has no unaudited exit, and `Debug` output
/// reaches panic messages, error pages and logs.
impl std::fmt::Debug for ClassifiedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClassifiedText(<classified>)")
    }
}

impl<DB> ToSql<Text, DB> for ClassifiedText
where
    DB: Backend,
    String: ToSql<Text, DB>,
{
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, DB>) -> serialize::Result {
        <String as ToSql<Text, DB>>::to_sql(&self.0, out)
    }
}

impl<DB> FromSql<Text, DB> for ClassifiedText
where
    DB: Backend,
    String: FromSql<Text, DB>,
{
    fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
        String::from_sql(bytes).map(Self)
    }
}
