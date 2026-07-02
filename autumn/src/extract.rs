//! Re-exports of Axum extractors for use in Autumn handlers.
//!
//! These are provided so users don't need `axum` as a direct dependency
//! for the most common extractor types.
//!
//! | Extractor | Purpose |
//! |-----------|---------|
//! | [`Form`] | Deserialize `application/x-www-form-urlencoded` request bodies |
//! | [`Json`] | Deserialize/serialize JSON request and response bodies |
//! | [`Path`] | Extract path parameters (e.g., `/users/{id}`) |
//! | [`Query`] | Deserialize URL query strings (e.g., `?page=2&limit=10`) |
//!
//! [`Json`] serves double duty -- it is both an extractor (parses JSON
//! request bodies) and a response type (serializes to JSON with
//! `Content-Type: application/json`).
//!
//! For the full set of Axum extractors, use
//! `autumn_web::reexports::axum::extract`.

use axum::extract::{FromRequest, FromRequestParts};
use axum::response::{IntoResponse, Response};

macro_rules! impl_extractor_deref {
    ($extractor:ident) => {
        impl<T> std::ops::Deref for $extractor<T> {
            type Target = T;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl<T> std::ops::DerefMut for $extractor<T> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }
    };
}

/// Deserialize `application/x-www-form-urlencoded` request bodies.
///
/// Wraps [`axum::extract::Form`] so parser failures use Autumn's
/// Problem Details error contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct Form<T>(pub T);

impl_extractor_deref!(Form);

impl<S, T> FromRequest<S> for Form<T>
where
    S: Send + Sync,
    axum::extract::Form<T>: FromRequest<S, Rejection = axum::extract::rejection::FormRejection>,
{
    type Rejection = crate::AutumnError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Form::from_request(req, state)
            .await
            .map(|axum::extract::Form(value)| Self(value))
            .map_err(|err| rejection_to_error(err.status(), err.body_text()))
    }
}

/// Deserialize and serialize JSON request/response bodies.
///
/// Wraps [`axum::extract::Json`] so JSON parse failures use Autumn's
/// Problem Details error contract while successful responses still serialize
/// exactly like Axum's `Json<T>`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl_extractor_deref!(Json);

impl<S, T> FromRequest<S> for Json<T>
where
    S: Send + Sync,
    axum::extract::Json<T>: FromRequest<S, Rejection = axum::extract::rejection::JsonRejection>,
{
    type Rejection = crate::AutumnError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Json::from_request(req, state)
            .await
            .map(|axum::extract::Json(value)| Self(value))
            .map_err(|err| rejection_to_error(err.status(), err.body_text()))
    }
}

impl<T> IntoResponse for Json<T>
where
    axum::Json<T>: IntoResponse,
{
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Extract typed path parameters from the URL.
///
/// Wraps [`axum::extract::Path`] so path parse failures use Autumn's
/// Problem Details error contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct Path<T>(pub T);

impl_extractor_deref!(Path);

impl<S, T> FromRequestParts<S> for Path<T>
where
    S: Send + Sync,
    axum::extract::Path<T>:
        FromRequestParts<S, Rejection = axum::extract::rejection::PathRejection>,
{
    type Rejection = crate::AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        axum::extract::Path::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Self(value))
            .map_err(|err| rejection_to_error(err.status(), err.body_text()))
    }
}

/// Deserialize URL query string parameters.
///
/// Wraps [`axum::extract::Query`] so query parse failures use Autumn's
/// Problem Details error contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl_extractor_deref!(Query);

impl<S, T> FromRequestParts<S> for Query<T>
where
    S: Send + Sync,
    axum::extract::Query<T>:
        FromRequestParts<S, Rejection = axum::extract::rejection::QueryRejection>,
{
    type Rejection = crate::AutumnError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        axum::extract::Query::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Self(value))
            .map_err(|err| rejection_to_error(err.status(), err.body_text()))
    }
}

fn rejection_to_error(status: http::StatusCode, body_text: String) -> crate::AutumnError {
    crate::AutumnError::bad_request_msg(body_text).with_status(status)
}

/// Multipart form-data extractor with Autumn upload policy integration.
///
/// This wraps Axum's multipart extractor and applies framework-level
/// validation from `security.upload`:
///
/// - MIME allow-list checks (`allowed_mime_types`)
/// - Per-file size caps when consuming field bytes or streaming to disk
///
/// Request size limits are enforced per request in this extractor via
/// `security.upload.max_request_size_bytes`.
#[cfg(feature = "multipart")]
pub struct Multipart {
    inner: axum::extract::Multipart,
    config: crate::security::config::UploadConfig,
}

#[cfg(feature = "multipart")]
impl Multipart {
    /// Read the next multipart field, validating MIME type when configured.
    ///
    /// # Errors
    ///
    /// Returns [`crate::AutumnError`] when multipart parsing fails or the
    /// field MIME type is not allowed by config.
    pub async fn next_field(&mut self) -> crate::AutumnResult<Option<MultipartField<'_>>> {
        let Some(field) = self
            .inner
            .next_field()
            .await
            .map_err(|err| multipart_error_to_error(&err))?
        else {
            return Ok(None);
        };

        // Only enforce MIME allow-lists for file parts. Regular form
        // fields often omit `Content-Type`.
        if field.file_name().is_some() && !self.config.allowed_mime_types.is_empty() {
            let Some(content_type) = field.content_type().map(str::to_owned) else {
                return Err(crate::AutumnError::bad_request_msg(
                    "missing content type on uploaded file",
                ));
            };
            if !self
                .config
                .allowed_mime_types
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&content_type))
            {
                return Err(crate::AutumnError::bad_request_msg(format!(
                    "unsupported upload content type: {content_type}"
                )));
            }
        }

        Ok(Some(MultipartField {
            inner: field,
            max_file_size_bytes: self.config.max_file_size_bytes,
        }))
    }
}

#[cfg(feature = "multipart")]
impl<S> axum::extract::FromRequest<S> for Multipart
where
    S: Send + Sync,
    axum::extract::Multipart:
        axum::extract::FromRequest<S, Rejection = axum::extract::multipart::MultipartRejection>,
{
    type Rejection = crate::AutumnError;

    async fn from_request(
        mut req: axum::extract::Request,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let config = req
            .extensions()
            .get::<crate::security::config::UploadConfig>()
            .cloned()
            .unwrap_or_default();
        axum::extract::DefaultBodyLimit::max(config.max_request_size_bytes).apply(&mut req);
        let inner = axum::extract::Multipart::from_request(req, state)
            .await
            .map_err(|err| multipart_rejection_to_error(&err))?;
        Ok(Self { inner, config })
    }
}

/// A multipart field wrapper that provides safe streaming helpers.
#[cfg(feature = "multipart")]
pub struct MultipartField<'a> {
    inner: axum::extract::multipart::Field<'a>,
    max_file_size_bytes: usize,
}

#[cfg(all(feature = "multipart", feature = "storage"))]
struct MultipartFieldStreamState<'a> {
    inner: axum::extract::multipart::Field<'a>,
    total: usize,
    max: usize,
    errored: bool,
}

#[cfg(feature = "multipart")]
#[allow(clippy::elidable_lifetime_names)]
impl<'a> MultipartField<'a> {
    /// Field name from the multipart form.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.inner.name()
    }

    /// Uploaded file name (if this field represents a file).
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.inner.file_name()
    }

    /// Declared MIME type for this field.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.inner.content_type()
    }

    /// Tighten the per-field upload cap below the global
    /// `security.upload.max_file_size_bytes`.
    ///
    /// Routes can use this to enforce stricter caps than the global
    /// policy. The effective cap is `min(current, max)` — calling this
    /// with a value larger than the global is a no-op, so a route
    /// can't accidentally relax the framework-level limit.
    ///
    /// Returns `413 Payload Too Large` if subsequent reads exceed the
    /// tightened cap, just like the unchained form.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// // Cap avatar uploads at 2 MiB even if the global cap is higher.
    /// let blob = field
    ///     .with_max_bytes(2 * 1024 * 1024)
    ///     .save_to_blob_store(&*store, &key)
    ///     .await?;
    /// ```
    #[must_use]
    pub fn with_max_bytes(mut self, max: usize) -> Self {
        self.max_file_size_bytes = self.max_file_size_bytes.min(max);
        self
    }

    /// Read this field fully into memory while enforcing file-size limits.
    ///
    /// # Errors
    ///
    /// Returns `413 Payload Too Large` if the field exceeds
    /// `security.upload.max_file_size_bytes`.
    pub async fn bytes_limited(mut self) -> crate::AutumnResult<Vec<u8>> {
        let mut out = Vec::new();
        let mut read = 0usize;
        while let Some(chunk) = self
            .inner
            .chunk()
            .await
            .map_err(|err| multipart_error_to_error(&err))?
        {
            read += chunk.len();
            if read > self.max_file_size_bytes {
                return Err(file_too_large_error(self.max_file_size_bytes));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Stream this field into a [`BlobStore`](crate::storage::BlobStore)
    /// while enforcing file-size limits.
    ///
    /// This is the production-ready replacement for [`save_to`](Self::save_to):
    /// the bytes flow through the configured blob backend (Local for dev,
    /// S3 for prod) so they survive container restarts and are visible to
    /// every replica.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use autumn_web::prelude::*;
    /// use autumn_web::extract::Multipart;
    /// use autumn_web::storage::BlobStoreState;
    ///
    /// #[post("/avatar")]
    /// async fn upload(state: State<AppState>, mut form: Multipart) -> AutumnResult<String> {
    ///     let store = state.extension::<BlobStoreState>().expect("storage configured");
    ///     while let Some(field) = form.next_field().await? {
    ///         if field.name() == Some("avatar") {
    ///             let blob = field
    ///                 .save_to_blob_store(store.store().as_ref(), "avatars/me.png")
    ///                 .await?;
    ///             return Ok(blob.key);
    ///         }
    ///     }
    ///     Err(autumn_web::AutumnError::bad_request_msg("missing avatar field"))
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the field exceeds
    /// `security.upload.max_file_size_bytes`, when the multipart body is
    /// malformed, or when the underlying [`BlobStore`](crate::storage::BlobStore)
    /// rejects the write.
    #[cfg(feature = "storage")]
    pub async fn save_to_blob_store<'b>(
        self,
        store: &'b (dyn crate::storage::BlobStore + '_),
        key: impl Into<String>,
    ) -> crate::AutumnResult<crate::storage::Blob>
    where
        'a: 'b,
    {
        let key = key.into();
        let content_type = self
            .inner
            .content_type()
            .map_or_else(|| "application/octet-stream".to_owned(), str::to_owned);

        // Adapt the multipart chunk iterator into the trait's
        // `ByteStream`, enforcing the per-file size cap as we go so we
        // never buffer the whole upload in memory and large files flow
        // straight through to the store's streaming path.
        let state = MultipartFieldStreamState {
            inner: self.inner,
            total: 0,
            max: self.max_file_size_bytes,
            errored: false,
        };

        let stream = futures::stream::unfold(state, |mut state| async move {
            if state.errored {
                return None;
            }
            match state.inner.chunk().await {
                Ok(Some(chunk)) => {
                    state.total = state.total.saturating_add(chunk.len());
                    if state.total > state.max {
                        let err = crate::storage::BlobStoreError::PayloadTooLarge(format!(
                            "uploaded file exceeds limit of {} bytes",
                            state.max,
                        ));
                        state.errored = true;
                        Some((Err(err), state))
                    } else {
                        Some((Ok(chunk), state))
                    }
                }
                Ok(None) => None,
                Err(err) => {
                    // multer/axum already classifies multipart parser
                    // failures: 400 for malformed bodies, 413 for body
                    // limit violations, etc. Preserve that — wrapping
                    // every parser error as `Io` would silently turn
                    // client errors into 500s on the way out.
                    state.errored = true;
                    let mapped = blob_error_from_multipart(&err);
                    Some((Err(mapped), state))
                }
            }
        });
        let stream: crate::storage::ByteStream<'b> = Box::pin(stream);

        store
            .put_stream(&key, &content_type, stream)
            .await
            .map_err(crate::storage::BlobStoreError::into_autumn_error)
    }

    /// Stream this field to disk while enforcing file-size limits.
    ///
    /// # Errors
    ///
    /// Returns an error if writing fails or the file exceeds configured
    /// limits. Partial files are removed on limit violations.
    pub async fn save_to<P: AsRef<std::path::Path>>(
        mut self,
        path: P,
    ) -> crate::AutumnResult<usize> {
        use tokio::io::AsyncWriteExt as _;

        let path = path.as_ref();
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(crate::AutumnError::internal_server_error)?;

        let mut written = 0usize;
        while let Some(chunk) = self
            .inner
            .chunk()
            .await
            .map_err(|err| multipart_error_to_error(&err))?
        {
            written += chunk.len();
            if written > self.max_file_size_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                return Err(file_too_large_error(self.max_file_size_bytes));
            }
            file.write_all(&chunk)
                .await
                .map_err(crate::AutumnError::internal_server_error)?;
        }
        file.flush()
            .await
            .map_err(crate::AutumnError::internal_server_error)?;
        Ok(written)
    }
}

#[cfg(feature = "multipart")]
fn multipart_rejection_to_error(
    err: &axum::extract::multipart::MultipartRejection,
) -> crate::AutumnError {
    crate::AutumnError::bad_request_msg(err.body_text()).with_status(err.status())
}

#[cfg(feature = "multipart")]
/// Map a multipart parser error to a `BlobStoreError` variant whose
/// `status()` matches what the parser would have reported as an HTTP
/// response — so a malformed-body parser failure becomes 400, a body-
/// limit violation becomes 413, and only true server-side problems
/// stay as 500. Without this, every parser error would wrap as
/// `BlobStoreError::Io` and `into_autumn_error` would surface them all
/// as 500s.
#[cfg(all(feature = "multipart", feature = "storage"))]
fn blob_error_from_multipart(
    err: &axum::extract::multipart::MultipartError,
) -> crate::storage::BlobStoreError {
    let status = err.status();
    let body = err.body_text();
    if status == http::StatusCode::PAYLOAD_TOO_LARGE {
        crate::storage::BlobStoreError::PayloadTooLarge(body)
    } else if status.is_client_error() {
        crate::storage::BlobStoreError::InvalidInput(body)
    } else {
        crate::storage::BlobStoreError::Io(body)
    }
}

#[cfg(feature = "multipart")]
fn multipart_error_to_error(err: &axum::extract::multipart::MultipartError) -> crate::AutumnError {
    crate::AutumnError::bad_request_msg(err.body_text()).with_status(err.status())
}

#[cfg(feature = "multipart")]
fn file_too_large_error(max_file_size_bytes: usize) -> crate::AutumnError {
    crate::AutumnError::bad_request_msg(format!(
        "uploaded file exceeds limit of {max_file_size_bytes} bytes",
    ))
    .with_status(http::StatusCode::PAYLOAD_TOO_LARGE)
}

pub use axum::extract::State;

#[cfg(all(test, feature = "multipart"))]
mod tests {
    use super::*;
    use axum::extract::FromRequest;
    use axum::http::Request;

    #[tokio::test]
    async fn test_multipart_field_bytes_limited_success() {
        let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\nhello\r\n--boundary--\r\n";
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let mut multipart = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();

        let wrapper = MultipartField {
            inner: field,
            max_file_size_bytes: 100,
        };

        let bytes = wrapper.bytes_limited().await.unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn test_multipart_field_bytes_limited_too_large() {
        let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\nhello world\r\n--boundary--\r\n";
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let mut multipart = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();

        let wrapper = MultipartField {
            inner: field,
            max_file_size_bytes: 5,
        };

        let err = wrapper.bytes_limited().await.unwrap_err();
        assert_eq!(err.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_multipart_field_save_to_success() {
        let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\nfile content\r\n--boundary--\r\n";
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let mut multipart = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();

        let wrapper = MultipartField {
            inner: field,
            max_file_size_bytes: 100,
        };

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("out.txt");

        let written = wrapper.save_to(&file_path).await.unwrap();
        assert_eq!(written, 12);

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "file content");
    }

    #[tokio::test]
    async fn test_multipart_field_save_to_too_large() {
        let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\nfile content\r\n--boundary--\r\n";
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let mut multipart = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();

        let wrapper = MultipartField {
            inner: field,
            max_file_size_bytes: 4,
        };

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("out_large.txt");

        let err = wrapper.save_to(&file_path).await.unwrap_err();
        assert_eq!(err.status(), http::StatusCode::PAYLOAD_TOO_LARGE);

        assert!(!file_path.exists());
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn test_multipart_field_save_to_blob_store_success() {
        use crate::storage::{BlobStore, LocalBlobStore, local::SigningKey};
        use std::time::Duration;

        let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\nContent-Type: text/plain\r\n\r\nblob content\r\n--boundary--\r\n";
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let mut multipart = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();

        let wrapper = MultipartField {
            inner: field,
            max_file_size_bytes: 100,
        };

        let root = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(
            "local",
            root.path(),
            "/blobs",
            Duration::from_secs(3600),
            SigningKey::random(),
            vec![],
        )
        .unwrap();

        let blob = wrapper.save_to_blob_store(&store, "myblob").await.unwrap();
        assert_eq!(blob.key, "myblob");
        assert_eq!(blob.content_type, "text/plain");

        let bytes = store.get("myblob").await.unwrap();
        assert_eq!(&bytes[..], b"blob content");
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn test_multipart_field_save_to_blob_store_too_large() {
        use crate::storage::{BlobStore, LocalBlobStore, local::SigningKey};
        use std::time::Duration;

        let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\nContent-Type: text/plain\r\n\r\nblob content\r\n--boundary--\r\n";
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let mut multipart = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();

        let wrapper = MultipartField {
            inner: field,
            max_file_size_bytes: 4, // "blob content" is 12 bytes
        };

        let root = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(
            "local",
            root.path(),
            "/blobs",
            Duration::from_secs(3600),
            SigningKey::random(),
            vec![],
        )
        .unwrap();

        let err = wrapper
            .save_to_blob_store(&store, "myblob")
            .await
            .unwrap_err();
        assert_eq!(err.status(), http::StatusCode::PAYLOAD_TOO_LARGE);

        // Verify that the blob was not created/persisted
        let get_err = store.get("myblob").await.unwrap_err();
        assert_eq!(get_err.status(), http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_multipart_field_metadata() {
        let body = "--boundary\r\nContent-Disposition: form-data; name=\"custom_name\"; filename=\"custom_file.png\"\r\nContent-Type: image/png\r\n\r\npng\r\n--boundary--\r\n";
        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let mut multipart = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();

        let wrapper = MultipartField {
            inner: field,
            max_file_size_bytes: 100,
        };

        assert_eq!(wrapper.name(), Some("custom_name"));
        assert_eq!(wrapper.file_name(), Some("custom_file.png"));
        assert_eq!(wrapper.content_type(), Some("image/png"));

        let tighter = wrapper.with_max_bytes(50);
        assert_eq!(tighter.max_file_size_bytes, 50);

        let not_tighter = tighter.with_max_bytes(200);
        assert_eq!(not_tighter.max_file_size_bytes, 50); // should not relax
    }
}

// ── Current request path ─────────────────────────────────────────────────────

/// The current request's URI path (e.g. `"/admin/posts/3/edit"`), without the
/// query string.
///
/// Infallible — always succeeds, even on requests with no matched route.
/// Intended for path-aware view helpers such as `nav_link`
/// (`autumn_web::widgets::nav_link`, behind the `maud` feature), which need
/// to know the current path to decide whether a navigation link is active.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
///
/// #[get("/posts")]
/// async fn index(CurrentPath(path): CurrentPath) -> Markup {
///     nav_link(&path, "/posts", "Posts")
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentPath(pub String);

impl CurrentPath {
    /// The current request path as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S> FromRequestParts<S> for CurrentPath
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        // Use OriginalUri rather than parts.uri directly: axum's Router::nest
        // strips the mount prefix from parts.uri inside the nested service,
        // but nav_link needs the full browser-visible path to compare against
        // normal absolute hrefs. OriginalUri falls back to parts.uri itself
        // when the request wasn't dispatched through a nested router.
        let axum::extract::OriginalUri(uri) =
            axum::extract::OriginalUri::from_request_parts(parts, state)
                .await
                .unwrap();
        Ok(Self(uri.path().to_owned()))
    }
}

// ── Trusted-proxy client-identity extractors ─────────────────────────────────

use crate::security::trusted_proxies::ResolvedClientIdentity;

/// The resolved client IP address after trusted-proxy evaluation.
///
/// Populated by the framework's proxy-resolver middleware from the operator's
/// `[security.trusted_proxies]` configuration.
///
/// # Plugin authors
///
/// > **Never read `X-Forwarded-*` headers directly.  Use this extractor.**
///
/// This is the only blessed way to obtain the real client IP in handlers and
/// middleware.  Direct reads of `X-Forwarded-For` or `X-Real-IP` will be
/// rejected by the `grep` CI guard introduced in #812.
///
/// # Failure
///
/// Returns `500 Internal Server Error` when the proxy-resolver middleware is
/// not installed.  Use `Option<ClientAddr>` for routes where the middleware may
/// be absent.
pub struct ClientAddr(pub std::net::IpAddr);

impl ClientAddr {
    /// The resolved client IP.
    #[must_use]
    pub const fn ip(&self) -> std::net::IpAddr {
        self.0
    }
}

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ResolvedClientIdentity>()
            .and_then(|id| id.addr)
            .map(ClientAddr)
            .ok_or((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "ClientAddr not resolved. Is the TrustedProxiesLayer installed?",
            ))
    }
}

impl<S> axum::extract::OptionalFromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ResolvedClientIdentity>()
            .and_then(|id| id.addr)
            .map(ClientAddr))
    }
}

/// The resolved external host as seen by the client after trusted-proxy evaluation.
///
/// Returns the value of `X-Forwarded-Host` when the proxy is trusted, otherwise
/// falls back to the `Host` header.
///
/// # Plugin authors
///
/// > **Never read `X-Forwarded-Host` directly.  Use this extractor.**
pub struct ClientHost(pub String);

impl ClientHost {
    /// The resolved host string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S> FromRequestParts<S> for ClientHost
where
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ResolvedClientIdentity>()
            .and_then(|id| id.host.clone())
            .map(ClientHost)
            .ok_or((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "ClientHost not resolved. Is the TrustedProxiesLayer installed?",
            ))
    }
}

impl<S> axum::extract::OptionalFromRequestParts<S> for ClientHost
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ResolvedClientIdentity>()
            .and_then(|id| id.host.clone())
            .map(ClientHost))
    }
}

/// The resolved external scheme (`"http"` or `"https"`) after trusted-proxy evaluation.
///
/// Returns the leftmost value of `X-Forwarded-Proto` when the proxy is trusted,
/// otherwise falls back to the request URI scheme or `"http"`.
///
/// # Plugin authors
///
/// > **Never read `X-Forwarded-Proto` directly.  Use this extractor.**
pub struct ClientScheme(pub String);

impl ClientScheme {
    /// The resolved scheme string (`"http"` or `"https"`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns `true` when the resolved scheme is `"https"`.
    #[must_use]
    pub fn is_https(&self) -> bool {
        self.0.eq_ignore_ascii_case("https")
    }
}

impl<S> FromRequestParts<S> for ClientScheme
where
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ResolvedClientIdentity>()
            .map(|id| Self(id.scheme.clone().unwrap_or_else(|| "http".to_owned())))
            .ok_or((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "ClientScheme not resolved. Is the TrustedProxiesLayer installed?",
            ))
    }
}

impl<S> axum::extract::OptionalFromRequestParts<S> for ClientScheme
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ResolvedClientIdentity>()
            .map(|id| Self(id.scheme.clone().unwrap_or_else(|| "http".to_owned()))))
    }
}

#[cfg(test)]
mod trusted_proxy_extractor_tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    fn make_identity(addr: &str, host: &str, scheme: &str) -> ResolvedClientIdentity {
        ResolvedClientIdentity {
            addr: Some(addr.parse().unwrap()),
            host: Some(host.to_owned()),
            scheme: Some(scheme.to_owned()),
        }
    }

    #[tokio::test]
    async fn client_addr_extractor_reads_from_extension() {
        async fn handler(ClientAddr(ip): ClientAddr) -> String {
            ip.to_string()
        }

        let app = Router::new().route("/", get(handler));

        let mut req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(make_identity("192.0.2.1", "app.example", "https"));

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"192.0.2.1");
    }

    #[tokio::test]
    async fn client_host_extractor_reads_from_extension() {
        async fn handler(ClientHost(host): ClientHost) -> String {
            host
        }

        let app = Router::new().route("/", get(handler));

        let mut req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(make_identity("192.0.2.1", "app.example", "https"));

        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"app.example");
    }

    #[tokio::test]
    async fn client_scheme_extractor_reads_from_extension() {
        async fn handler(ClientScheme(scheme): ClientScheme) -> String {
            scheme
        }

        let app = Router::new().route("/", get(handler));

        let mut req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(make_identity("192.0.2.1", "app.example", "https"));

        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"https");
    }

    #[tokio::test]
    async fn client_addr_missing_returns_500() {
        async fn handler(_: ClientAddr) -> &'static str {
            "ok"
        }

        let app = Router::new().route("/", get(handler));
        let req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn optional_client_addr_returns_none_when_missing() {
        async fn handler(addr: Option<ClientAddr>) -> String {
            if addr.is_some() {
                "some".to_owned()
            } else {
                "none".to_owned()
            }
        }

        let app = Router::new().route("/", get(handler));
        let req = axum::http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"none");
    }

    #[tokio::test]
    async fn current_path_extracts_uri_path() {
        async fn handler(CurrentPath(path): CurrentPath) -> String {
            path
        }

        let app = Router::new().route("/admin/posts", get(handler));
        let req = axum::http::Request::builder()
            .uri("/admin/posts")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"/admin/posts");
    }

    #[tokio::test]
    async fn current_path_strips_query_string() {
        async fn handler(CurrentPath(path): CurrentPath) -> String {
            path
        }

        let app = Router::new().route("/admin/posts", get(handler));
        let req = axum::http::Request::builder()
            .uri("/admin/posts?page=2")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"/admin/posts");
    }

    #[tokio::test]
    async fn current_path_preserves_prefix_under_nested_router() {
        // axum's Router::nest strips the mount prefix from `parts.uri` inside
        // the nested service, so a request to "/admin/posts" is seen as
        // "/posts" there. nav_link's documented use compares CurrentPath
        // against normal absolute hrefs like "/admin/posts", so CurrentPath
        // must return the browser-visible path (via OriginalUri), not the
        // nest-relative one.
        async fn handler(CurrentPath(path): CurrentPath) -> String {
            path
        }

        let inner = Router::new().route("/posts", get(handler));
        let app = Router::new().nest("/admin", inner);
        let req = axum::http::Request::builder()
            .uri("/admin/posts")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"/admin/posts");
    }
}
