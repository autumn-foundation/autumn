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
//! `Classified<String, F> -> ClassifiedText<F> -> the database` cannot be spliced
//! into a `String` on the way past. Writing a row is not a gated sink; handing
//! the value to a serializer is.
//!
//! # Why it carries the field marker
//!
//! The wrapper is generic over the same `F` as the value it wraps, and both
//! conversions are `F`-to-`F`. An `F`-erasing wrapper would have been a way to
//! *retype* a classified column: convert `Classified<String, EmailMarker>` into
//! the wrapper, convert the wrapper back out as `Classified<String, PhoneMarker>`,
//! and the email is now releasable through the phone column's boundary -- with
//! the audit record written against the wrong column. Keeping `F` on the wrapper
//! makes that round trip a type error.

use std::marker::PhantomData;

use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql};
use diesel::serialize::{self, Output, ToSql};
use diesel::sql_types::Text;
use diesel::{AsExpression, FromSqlRow};

use super::{Classified, ClassifiedField};

/// The `serialize_as` / `deserialize_as` target `#[model]` gives Diesel for a
/// `#[classified]` column, carrying the column's field marker so the value
/// cannot be retyped on the way through.
#[derive(AsExpression, FromSqlRow, Clone)]
#[diesel(sql_type = Text)]
pub struct ClassifiedText<F: ClassifiedField>(String, PhantomData<fn() -> F>);

impl<F: ClassifiedField> From<Classified<String, F>> for ClassifiedText<F> {
    fn from(value: Classified<String, F>) -> Self {
        Self(value.into_column_value(), PhantomData)
    }
}

impl<F: ClassifiedField> From<ClassifiedText<F>> for Classified<String, F> {
    fn from(column: ClassifiedText<F>) -> Self {
        Self::new(column.0)
    }
}

/// Redacted: a classified value has no unaudited exit, and `Debug` output
/// reaches panic messages, error pages and logs.
impl<F: ClassifiedField> std::fmt::Debug for ClassifiedText<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClassifiedText(<classified>)")
    }
}

impl<F, DB> ToSql<Text, DB> for ClassifiedText<F>
where
    F: ClassifiedField,
    DB: Backend,
    String: ToSql<Text, DB>,
{
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, DB>) -> serialize::Result {
        <String as ToSql<Text, DB>>::to_sql(&self.0, out)
    }
}

impl<F, DB> FromSql<Text, DB> for ClassifiedText<F>
where
    F: ClassifiedField,
    DB: Backend,
    String: FromSql<Text, DB>,
{
    fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
        String::from_sql(bytes).map(|value| Self(value, PhantomData))
    }
}
