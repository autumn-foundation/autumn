//! Diesel column mapping for [`Sealed`](super::Sealed) and
//! [`BlindIndex`](super::BlindIndex) (issue #1771).
//!
//! Both are `Text` columns. Neither wrapper decodes on the way past: the server
//! writes and reads the same opaque strings it received, which is what keeps the
//! persistence layer out of the trust boundary.
//!
//! A value read back is validated, so a column holding something that is not an
//! envelope (a plaintext row predating the seal, or a hand-written `UPDATE`)
//! fails the read loudly instead of being handed on as if it were sealed.

use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql};
use diesel::serialize::{self, Output, ToSql};
use diesel::sql_types::Text;

use super::{BlindIndex, Sealed};

impl<DB> ToSql<Text, DB> for Sealed
where
    DB: Backend,
    String: ToSql<Text, DB>,
{
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, DB>) -> serialize::Result {
        <String as ToSql<Text, DB>>::to_sql(&self.0, out)
    }
}

impl<DB> FromSql<Text, DB> for Sealed
where
    DB: Backend,
    String: FromSql<Text, DB>,
{
    fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
        let envelope = <String as FromSql<Text, DB>>::from_sql(bytes)?;
        Self::from_envelope(envelope).map_err(|e| Box::new(e) as Box<_>)
    }
}

impl<DB> ToSql<Text, DB> for BlindIndex
where
    DB: Backend,
    String: ToSql<Text, DB>,
{
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, DB>) -> serialize::Result {
        <String as ToSql<Text, DB>>::to_sql(&self.0, out)
    }
}

impl<DB> FromSql<Text, DB> for BlindIndex
where
    DB: Backend,
    String: FromSql<Text, DB>,
{
    fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
        let token = <String as FromSql<Text, DB>>::from_sql(bytes)?;
        Self::from_token(token).map_err(|e| Box::new(e) as Box<_>)
    }
}
