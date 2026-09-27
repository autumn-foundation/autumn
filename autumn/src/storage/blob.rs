//! [`Blob`] value type stored on application models.
//!
//! A `Blob` references bytes owned by a [`BlobStore`](super::BlobStore).
//! Apps store `Blob` columns; the store owns the bytes; the database
//! owns lifecycle.
//!
//! With the `db` feature enabled, [`Blob`] derives Diesel's
//! `AsExpression` / `FromSqlRow` for `Jsonb` so it can sit on a
//! `#[model]` as a single Postgres `JSONB` column without any extra
//! glue.

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

use serde::{Deserialize, Serialize};

/// A reference to a stored blob.
///
/// `Blob` carries the minimum metadata an application needs to render,
/// validate, or delete the underlying bytes. The bytes themselves live
/// in a [`BlobStore`](super::BlobStore).
///
/// # Database storage
///
/// With the `db` feature enabled, `Blob` is shaped to round-trip through
/// a Postgres `JSONB` column on a `#[model]`:
///
/// ```rust,ignore
/// use autumn_web::model;
/// use autumn_web::storage::Blob;
///
/// #[model]
/// pub struct User {
///     pub id: i64,
///     pub name: String,
///     pub avatar: Option<Blob>,
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Postgres backend: `Blob` sits on a `JSONB` column.
#[cfg_attr(
    all(feature = "db", not(feature = "sqlite")),
    derive(diesel::AsExpression, diesel::FromSqlRow),
    diesel(sql_type = diesel::sql_types::Jsonb)
)]
// SQLite backend (issue #1924): SQLite has no `JSONB` type, so `Blob` sits on a
// `TEXT` column storing the same metadata JSON. `Blob` is a local type, so these
// conversions are orphan-rule-legal (unlike `uuid::Uuid` / `rust_decimal::Decimal`).
#[cfg_attr(
    feature = "sqlite",
    derive(diesel::AsExpression, diesel::FromSqlRow),
    diesel(sql_type = diesel::sql_types::Text)
)]
pub struct Blob {
    /// Identifier of the [`BlobStore`](super::BlobStore) this blob lives in.
    ///
    /// Recorded so applications can detect cross-store mismatches when
    /// the framework's configured backend changes.
    pub provider_id: String,

    /// Stable key of the blob inside the store. Use the same key with
    /// [`BlobStore::get`](super::BlobStore::get),
    /// [`BlobStore::delete`](super::BlobStore::delete), and
    /// [`BlobStore::presigned_url`](super::BlobStore::presigned_url).
    pub key: String,

    /// MIME type the blob was uploaded with.
    pub content_type: String,

    /// Size of the blob in bytes.
    pub byte_size: u64,

    /// Optional ETag-style integrity tag reported by the backend.
    ///
    /// On the [`LocalBlobStore`](super::LocalBlobStore) this is a hex
    /// SHA-256 of the bytes; on S3 it is the upstream `ETag` (typically
    /// the `MD5` hash, or a multipart-style composite for large objects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

impl Blob {
    /// Construct a new blob handle.
    ///
    /// Most applications get one of these from
    /// [`BlobStore::put`](super::BlobStore::put) or
    /// [`MultipartField::save_to_blob_store`](crate::extract::MultipartField::save_to_blob_store);
    /// constructing one by hand is intended for tests and migrations.
    #[must_use]
    pub fn new(
        provider_id: impl Into<String>,
        key: impl Into<String>,
        content_type: impl Into<String>,
        byte_size: u64,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            key: key.into(),
            content_type: content_type.into(),
            byte_size,
            etag: None,
        }
    }

    /// Builder helper attaching an integrity tag.
    #[must_use]
    pub fn with_etag(mut self, etag: impl Into<String>) -> Self {
        self.etag = Some(etag.into());
        self
    }
}

/// Lightweight metadata returned by [`BlobStore::head`](super::BlobStore::head).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    /// Stable key of the blob inside the store.
    pub key: String,
    /// MIME type the blob was stored with.
    pub content_type: String,
    /// Size of the blob in bytes.
    pub byte_size: u64,
    /// Optional integrity tag, if the backend produces one.
    pub etag: Option<String>,
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
mod diesel_impls {
    //! `Blob` ↔ Postgres `JSONB` conversion.
    //!
    //! The default (Postgres) `db` build stores `Blob` in a `JSONB` column, so
    //! these impls target `Pg` directly. The wire format is the standard
    //! Postgres `jsonb` framing: a single 0x01 version byte followed by the
    //! UTF-8 JSON body.
    use std::io::Write as _;

    use diesel::backend::Backend;
    use diesel::deserialize::{self, FromSql};
    use diesel::pg::Pg;
    use diesel::serialize::{self, IsNull, Output, ToSql};
    use diesel::sql_types::Jsonb;

    use super::Blob;

    impl ToSql<Jsonb, Pg> for Blob {
        fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Pg>) -> serialize::Result {
            out.write_all(&[1])?;
            serde_json::to_writer(out, self)
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(IsNull::No)
        }
    }

    impl FromSql<Jsonb, Pg> for Blob {
        fn from_sql(bytes: <Pg as Backend>::RawValue<'_>) -> deserialize::Result<Self> {
            let value = <serde_json::Value as FromSql<Jsonb, Pg>>::from_sql(bytes)?;
            serde_json::from_value(value).map_err(Into::into)
        }
    }
}

#[cfg(feature = "sqlite")]
mod diesel_impls_sqlite {
    //! `Blob` ↔ `SQLite` `TEXT` conversion (issue #1924).
    //!
    //! `SQLite` has no `JSONB` storage class, so under the `sqlite` runtime
    //! backend `Blob` sits on a `TEXT` column and stores the same metadata as
    //! a UTF-8 JSON string. Because `Blob` is a local type, these `FromSql` /
    //! `ToSql<Text, Sqlite>` impls are orphan-rule-legal (the reason
    //! `uuid::Uuid` / `rust_decimal::Decimal` need a wrapper and stay rejected
    //! at generate time). The JSON body is `serde_json`-serialized/deserialized,
    //! so a `Blob` round-trips losslessly on `SQLite` exactly as it does on the
    //! Postgres `JSONB` path.
    use diesel::deserialize::{self, FromSql};
    use diesel::serialize::{self, IsNull, Output, ToSql};
    use diesel::sql_types::Text;
    use diesel::sqlite::Sqlite;

    use super::Blob;

    impl ToSql<Text, Sqlite> for Blob {
        fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
            let json = serde_json::to_string(self)
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
            out.set_value(json);
            Ok(IsNull::No)
        }
    }

    impl FromSql<Text, Sqlite> for Blob {
        fn from_sql(
            bytes: <Sqlite as diesel::backend::Backend>::RawValue<'_>,
        ) -> deserialize::Result<Self> {
            let json = <String as FromSql<Text, Sqlite>>::from_sql(bytes)?;
            serde_json::from_str(&json).map_err(Into::into)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrips_via_serde_json() {
        let blob = Blob::new("local", "avatars/1.png", "image/png", 1234).with_etag("abc");
        let json = serde_json::to_string(&blob).unwrap();
        let parsed: Blob = serde_json::from_str(&json).unwrap();
        assert_eq!(blob, parsed);
    }

    #[test]
    fn blob_drops_etag_when_none() {
        let blob = Blob::new("local", "k", "image/png", 1);
        let json = serde_json::to_string(&blob).unwrap();
        assert!(!json.contains("etag"));
    }
}
