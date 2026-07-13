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
#[cfg_attr(
    feature = "db",
    derive(diesel::AsExpression, diesel::FromSqlRow),
    diesel(sql_type = diesel::sql_types::Jsonb)
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

#[cfg(feature = "db")]
mod diesel_impls {
    //! `Blob` ↔ Postgres `JSONB` conversion.
    //!
    //! The `db` feature always pulls in Postgres support, so these impls
    //! target `Pg` directly. The wire format is the standard Postgres
    //! `jsonb` framing: a single 0x01 version byte followed by the
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
