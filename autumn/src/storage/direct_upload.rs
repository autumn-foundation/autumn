//! Direct browser-to-storage upload helpers.
//!
//! The direct upload flow:
//!
//! 1. App route (CSRF-protected) calls [`BlobStore::presign_put`] and returns
//!    a [`PresignPutResult`] to the browser.
//! 2. Browser PUTs the file bytes directly to [`PresignPutResult::url`] using
//!    [`PresignPutResult::method`] and [`PresignPutResult::headers`].
//!    For [`LocalBlobStore`](super::LocalBlobStore) this is an HMAC-signed
//!    route on the Autumn app itself (`/_blobs/<key>?...`). For S3 backends
//!    it is a real AWS `SigV4` presigned URL.
//! 3. App route (CSRF-protected) calls [`complete_direct_upload`] to confirm
//!    the upload landed and receive a [`Blob`] handle to store in
//!    the database.
//!
//! ## Orphan handling
//!
//! A blob becomes an *orphan* when step 2 succeeds (bytes land in storage) but
//! step 3 never fires — the user closed the tab, an error occurred, or a
//! malicious actor uploaded to the presigned URL without completing the bind.
//!
//! The chosen policy is **bind-step promotion**: a blob only becomes part of
//! your data model when `complete_direct_upload` returns a `Blob` that your
//! code saves to the database. Unbound blobs are invisible to the application.
//!
//! To clean up unbound blobs, add a lifecycle policy to your storage backend:
//! - **S3/R2/compatible**: configure an object-expiry rule for the upload
//!   prefix (e.g., delete objects under `uploads/` after 24 hours) and promote
//!   them to a permanent prefix only on completion.
//! - **Local backend (dev/single-replica)**: orphan cleanup can be a periodic
//!   `delete` sweep of objects older than the presign TTL, or simply ignored
//!   in dev.
//!
//! ## Security
//!
//! A presigned PUT URL grants the holder the right to upload bytes to a
//! **specific key**. It does **not** grant the right to bind that blob to any
//! model. The bind step is always your own CSRF- and session-protected route.
//! See [`complete_direct_upload`] for how to enforce this.

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

use std::time::Duration;

use super::{Blob, BlobMeta, BlobStore, BlobStoreError};

/// Everything a browser needs to PUT a file directly to the storage backend.
///
/// Returned by [`BlobStore::presign_put`]. Hand the whole struct to your
/// frontend (e.g., via a JSON endpoint) so it can start the upload without
/// routing bytes through your Autumn process.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PresignPutResult {
    /// URL the browser should send the PUT request to.
    pub url: String,
    /// HTTP method to use (always `"PUT"` in the current implementation;
    /// reserved for future extensions that require `"POST"`).
    pub method: String,
    /// HTTP headers the browser **must** include in the PUT request.
    ///
    /// For the local backend this map is empty. For S3 backends it typically
    /// includes `"Content-Type"` and any AWS-specific `x-amz-*` headers
    /// required by the presigning configuration.
    pub headers: std::collections::HashMap<String, String>,
    /// Duration after which the presigned envelope expires.
    pub expires_in: Duration,
}

/// Confirm a direct upload and return a [`Blob`] handle.
///
/// Call this **after** the browser signals that the direct PUT to storage
/// completed. Verifies that the key exists via
/// [`BlobStore::head`] and converts the metadata into a `Blob` handle
/// suitable for saving in your model.
///
/// # Security
///
/// Always call this from a CSRF-protected endpoint (consistent with other
/// mutating routes in Autumn). Possession of the presigned PUT URL alone does
/// **not** authorize the bind step — the completion route carries its own
/// session / CSRF context.
///
/// # Errors
///
/// Returns [`BlobStoreError::NotFound`] when the key is not present in the
/// store (upload did not complete or has already been deleted). Returns any
/// backend error from [`BlobStore::head`] unchanged.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::storage::{BlobStoreState, complete_direct_upload};
///
/// #[post("/uploads/complete")]
/// #[secured]  // enforces session authentication
/// async fn complete_upload(
///     State(state): State<AppState>,
///     CsrfFormField(_): CsrfFormField,  // enforces CSRF
///     Form(form): Form<CompleteForm>,
/// ) -> AutumnResult<Markup> {
///     let blobs = state.extension::<BlobStoreState>().unwrap();
///     let blob = complete_direct_upload(blobs.store().as_ref(), &form.key).await?;
///     // Now save `blob` to your model in the database …
///     Ok(/* … */)
/// }
/// ```
pub async fn complete_direct_upload(
    store: &dyn BlobStore,
    key: &str,
) -> Result<Blob, BlobStoreError> {
    store.head(key).await?.map_or_else(
        || {
            Err(BlobStoreError::NotFound(format!(
                "direct upload incomplete or upload has expired: \
                 key {key:?} not found in storage; \
                 ensure the browser PUT completed before calling complete_direct_upload"
            )))
        },
        |meta| Ok(blob_from_meta(store.provider_id(), key, meta)),
    )
}

fn blob_from_meta(provider_id: &str, key: &str, meta: BlobMeta) -> Blob {
    Blob {
        provider_id: provider_id.to_owned(),
        key: key.to_owned(),
        content_type: meta.content_type,
        byte_size: meta.byte_size,
        etag: meta.etag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{BlobFuture, ByteStream};
    use bytes::Bytes;
    use std::collections::HashMap;

    struct AlwaysFoundStore {
        meta: BlobMeta,
    }

    impl BlobStore for AlwaysFoundStore {
        fn provider_id(&self) -> &'static str {
            "test"
        }
        fn put<'a>(&'a self, _: &'a str, _: &'a str, _: Bytes) -> BlobFuture<'a, Blob> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn put_stream<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: ByteStream<'a>,
        ) -> BlobFuture<'a, Blob> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn get<'a>(&'a self, _: &'a str) -> BlobFuture<'a, Bytes> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn delete<'a>(&'a self, _: &'a str) -> BlobFuture<'a, ()> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn head<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
            let meta = BlobMeta {
                key: key.to_owned(),
                content_type: self.meta.content_type.clone(),
                byte_size: self.meta.byte_size,
                etag: self.meta.etag.clone(),
            };
            Box::pin(async move { Ok(Some(meta)) })
        }
        fn presigned_url<'a>(&'a self, _: &'a str, _: Duration) -> BlobFuture<'a, String> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
    }

    struct NotFoundStore;

    impl BlobStore for NotFoundStore {
        fn provider_id(&self) -> &'static str {
            "test"
        }
        fn put<'a>(&'a self, _: &'a str, _: &'a str, _: Bytes) -> BlobFuture<'a, Blob> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn put_stream<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: ByteStream<'a>,
        ) -> BlobFuture<'a, Blob> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn get<'a>(&'a self, _: &'a str) -> BlobFuture<'a, Bytes> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn delete<'a>(&'a self, _: &'a str) -> BlobFuture<'a, ()> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
        fn head<'a>(&'a self, _: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
            Box::pin(async { Ok(None) })
        }
        fn presigned_url<'a>(&'a self, _: &'a str, _: Duration) -> BlobFuture<'a, String> {
            Box::pin(async { Err(BlobStoreError::Unsupported("noop".into())) })
        }
    }

    #[tokio::test]
    async fn complete_direct_upload_returns_blob_from_head() {
        let store = AlwaysFoundStore {
            meta: BlobMeta {
                key: "ignored".into(),
                content_type: "image/png".into(),
                byte_size: 1234,
                etag: Some("abc123".into()),
            },
        };
        let blob = complete_direct_upload(&store, "avatars/me.png")
            .await
            .unwrap();
        assert_eq!(blob.provider_id, "test");
        assert_eq!(blob.key, "avatars/me.png");
        assert_eq!(blob.content_type, "image/png");
        assert_eq!(blob.byte_size, 1234);
        assert_eq!(blob.etag.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn complete_direct_upload_returns_not_found_when_missing() {
        let store = NotFoundStore;
        let err = complete_direct_upload(&store, "missing/file.png")
            .await
            .unwrap_err();
        assert!(
            matches!(err, BlobStoreError::NotFound(_)),
            "expected NotFound, got {err:?}"
        );
        assert!(err.to_string().contains("direct upload incomplete"));
    }

    #[test]
    fn presign_put_result_clone_and_debug() {
        let r = PresignPutResult {
            url: "https://s3.example.com/bucket/key?sig=abc".into(),
            method: "PUT".into(),
            headers: HashMap::from([("Content-Type".into(), "image/jpeg".into())]),
            expires_in: Duration::from_secs(900),
        };
        let cloned = r.clone();
        assert_eq!(cloned.url, r.url);
        assert!(!format!("{r:?}").is_empty());
    }
}
