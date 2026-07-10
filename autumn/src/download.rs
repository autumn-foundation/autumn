//! Typed file downloads as an [`IntoResponse`].
//!
//! [`Download`] turns owned bytes, an async byte stream, an [`AsyncRead`](tokio::io::AsyncRead), or
//! a stored [`Blob`](crate::storage::Blob) into an HTTP response with the
//! right `Content-Disposition`, `Content-Type`, and (when known)
//! `Content-Length` headers — without hand-rolling header strings in every
//! handler.
//!
//! The blob-backed constructor streams bytes straight from the store without
//! buffering the whole object in memory, so it works for large files behind
//! authorization (no public presigned URL required). The byte stream is opened
//! lazily when the response is built, so a `Range` request fetches only the
//! requested slice rather than reading the whole object.
//!
//! # Serving a private stored file behind auth
//!
//! Because [`Download`] is a plain `IntoResponse`, a policy-protected handler
//! can serve a stored blob as a download in a single expression:
//!
//! ```ignore
//! use autumn_web::download::Download;
//! use autumn_web::storage::SharedBlobStore;
//! use autumn_web::{secured, AutumnError};
//!
//! #[secured(policy = "reports.read")]
//! async fn download_report(
//!     store: SharedBlobStore,
//!     report_key: String,
//! ) -> Result<Download, AutumnError> {
//!     Ok(Download::from_blob(&store, report_key).await?.filename("report.pdf"))
//! }
//! ```
//!
//! # Serving owned bytes
//!
//! ```no_run
//! use autumn_web::download::Download;
//!
//! async fn export_csv() -> Download {
//!     let csv = b"id,name\n1,ada\n".to_vec();
//!     Download::from_bytes(csv).filename("export.csv")
//! }
//! ```

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::Stream;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use http::{HeaderMap, HeaderValue};

use crate::etag::{ETag, IntoETag};
use crate::range::{self, Validator};

/// A boxed `'static` byte stream used as a download body.
type BoxByteStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>>;

/// How the browser should treat the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    /// Force a save dialog (`Content-Disposition: attachment`).
    Attachment,
    /// Render in place when possible (`Content-Disposition: inline`).
    Inline,
}

/// The payload backing a [`Download`].
enum DownloadBody {
    /// Fully-buffered bytes with a known length. **Range-capable**: a slice is
    /// served from memory.
    Bytes(Bytes),
    /// An opaque streaming body of unknown or externally-tracked length.
    /// **Not range-capable**: the source cannot be re-seeked, so it is always
    /// served in full and never advertises `Accept-Ranges`.
    Stream(BoxByteStream),
    /// A stored blob, retaining only the state needed to open the byte stream
    /// lazily (store handle + key + size) — never a pre-opened stream.
    /// **Range-capable**: the byte stream is opened only when the response is
    /// built, and a ranged request asks the store for only the requested slice
    /// via [`BlobStore::get_range`](crate::storage::BlobStore::get_range),
    /// never buffering the whole object for a seek. The non-ranged path opens
    /// the whole-object stream lazily via
    /// [`BlobStore::get_stream`](crate::storage::BlobStore::get_stream).
    #[cfg(feature = "storage")]
    Blob {
        /// Store handle used to open the object stream or a byte slice.
        store: crate::storage::SharedBlobStore,
        /// Object key within the store.
        key: String,
        /// Total object size in bytes.
        size: u64,
    },
}

impl DownloadBody {
    /// Whether this body can serve HTTP `Range` requests. Opaque streams
    /// cannot be re-seeked, so they are never range-capable.
    const fn is_range_capable(&self) -> bool {
        match self {
            Self::Bytes(_) => true,
            Self::Stream(_) => false,
            #[cfg(feature = "storage")]
            Self::Blob { .. } => true,
        }
    }
}

/// A typed file download.
///
/// Construct one from [`from_bytes`](Download::from_bytes),
/// [`from_stream`](Download::from_stream),
/// [`from_async_read`](Download::from_async_read), or
/// [`from_blob`](Download::from_blob), then chain the builder setters
/// ([`filename`](Download::filename), [`content_type`](Download::content_type),
/// [`inline`](Download::inline)) and return it from a handler.
///
/// See the [module docs](crate::download) for a worked example.
#[must_use = "a Download does nothing unless returned from a handler or converted with `into_response`"]
pub struct Download {
    body: DownloadBody,
    filename: Option<String>,
    /// Content-Type set explicitly via [`content_type`](Download::content_type).
    content_type: Option<String>,
    /// Fallback Content-Type (e.g. from blob metadata), used only when no
    /// explicit type is set and none can be inferred from the filename.
    default_content_type: Option<String>,
    disposition: Disposition,
    content_length: Option<u64>,
    /// Strong `ETag` used as an `If-Range` validator and emitted on the
    /// response, when set via [`etag`](Download::etag).
    etag: Option<ETag>,
    /// `Last-Modified` HTTP-date string used as an `If-Range` validator and
    /// emitted on the response, when set via
    /// [`last_modified`](Download::last_modified).
    last_modified: Option<String>,
}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let body = match &self.body {
            DownloadBody::Bytes(b) => format!("Bytes({} bytes)", b.len()),
            DownloadBody::Stream(_) => "Stream(..)".to_owned(),
            #[cfg(feature = "storage")]
            DownloadBody::Blob { key, size, .. } => format!("Blob(key={key:?}, {size} bytes)"),
        };
        f.debug_struct("Download")
            .field("body", &body)
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("default_content_type", &self.default_content_type)
            .field("disposition", &self.disposition)
            .field("content_length", &self.content_length)
            .field("etag", &self.etag)
            .field("last_modified", &self.last_modified)
            .finish()
    }
}

impl Download {
    /// Build a download from owned bytes.
    ///
    /// The `Content-Length` is set from the byte length.
    pub fn from_bytes(bytes: impl Into<Bytes>) -> Self {
        let bytes = bytes.into();
        let content_length = u64::try_from(bytes.len()).ok();
        Self {
            body: DownloadBody::Bytes(bytes),
            filename: None,
            content_type: None,
            default_content_type: None,
            disposition: Disposition::Attachment,
            content_length,
            etag: None,
            last_modified: None,
        }
    }

    /// Build a download from an async byte stream.
    ///
    /// The length is unknown, so no `Content-Length` header is emitted and the
    /// body is transferred with chunked encoding.
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    {
        Self {
            body: DownloadBody::Stream(Box::pin(stream)),
            filename: None,
            content_type: None,
            default_content_type: None,
            disposition: Disposition::Attachment,
            content_length: None,
            etag: None,
            last_modified: None,
        }
    }

    /// Build a download from any [`AsyncRead`](tokio::io::AsyncRead), streaming its bytes.
    ///
    /// The reader is wrapped with [`tokio_util::io::ReaderStream`], so the
    /// bytes are transferred incrementally rather than buffered in memory.
    pub fn from_async_read<R>(reader: R) -> Self
    where
        R: tokio::io::AsyncRead + Send + 'static,
    {
        Self::from_stream(tokio_util::io::ReaderStream::new(reader))
    }

    /// Build a download that streams a stored blob's bytes.
    ///
    /// Reads the blob's metadata ([`head`](crate::storage::BlobStore::head))
    /// for the `Content-Length` and a default `Content-Type`, then retains the
    /// store handle + key so the byte stream can be opened **lazily** when the
    /// response is built. It does **not** open (or buffer) the object here, so
    /// a later `Range` request fetches only the requested slice via
    /// [`get_range`](crate::storage::BlobStore::get_range) rather than reading
    /// the whole object. This keeps the bytes flowing through your own
    /// authorized handler — no public presigned URL is issued.
    ///
    /// The `Content-Length` is taken from the [`head`](crate::storage::BlobStore::head)
    /// metadata read here. There is a small time-of-check/time-of-use window:
    /// if the blob is overwritten with a different size between this `head` and
    /// the later stream open (a local-backend edge case; overwrites are
    /// otherwise atomic), the advertised length may disagree with the streamed
    /// bytes.
    ///
    /// # Errors
    ///
    /// Returns a [`BlobStoreError`](crate::storage::BlobStoreError) if the blob
    /// does not exist or its metadata cannot be read.
    #[cfg(feature = "storage")]
    pub async fn from_blob(
        store: &crate::storage::SharedBlobStore,
        key: impl Into<String>,
    ) -> Result<Self, crate::storage::BlobStoreError> {
        let key = key.into();
        let meta = store
            .head(&key)
            .await?
            .ok_or_else(|| crate::storage::BlobStoreError::NotFound(key.clone()))?;
        // Only metadata is read here; the byte stream is opened lazily when the
        // response is built. The retained store handle + key + size let the
        // non-ranged path open the whole-object stream and a ranged request
        // fetch only the requested slice — the full object is never buffered
        // for a seek.
        Ok(Self {
            body: DownloadBody::Blob {
                store: std::sync::Arc::clone(store),
                key,
                size: meta.byte_size,
            },
            filename: None,
            content_type: None,
            default_content_type: Some(meta.content_type),
            disposition: Disposition::Attachment,
            content_length: Some(meta.byte_size),
            etag: None,
            last_modified: None,
        })
    }

    /// Set the download filename, controlling the `Content-Disposition`
    /// `filename` (and, for non-ASCII names, `filename*`) parameter.
    ///
    /// The name is sanitized: control characters (including CR/LF) are
    /// stripped and the value is quoted, so a caller-supplied name cannot
    /// inject extra header directives.
    #[must_use = "builder setters return a new Download; use the returned value"]
    pub fn filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// Set the `Content-Type` explicitly, overriding any type inferred from the
    /// filename extension or blob metadata.
    #[must_use = "builder setters return a new Download; use the returned value"]
    pub fn content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Serve the file inline (`Content-Disposition: inline`) instead of forcing
    /// a save dialog.
    #[must_use = "builder setters return a new Download; use the returned value"]
    pub const fn inline(mut self) -> Self {
        self.disposition = Disposition::Inline;
        self
    }

    /// Attach a strong `ETag` for the download.
    ///
    /// The tag is emitted as an `ETag` header and used as the `If-Range`
    /// validator by [`into_response_ranged`](Download::into_response_ranged):
    /// when a client's `If-Range` tag no longer matches, the whole
    /// representation is served instead of a stale partial slice.
    #[must_use = "builder setters return a new Download; use the returned value"]
    pub fn etag(mut self, etag: impl IntoETag) -> Self {
        self.etag = Some(etag.into_etag());
        self
    }

    /// Attach a `Last-Modified` HTTP-date string for the download.
    ///
    /// Emitted as a `Last-Modified` header and used as the `If-Range` validator
    /// (date form) by [`into_response_ranged`](Download::into_response_ranged).
    /// The value must already be a formatted HTTP-date
    /// (e.g. `Wed, 21 Oct 2015 07:28:00 GMT`).
    #[must_use = "builder setters return a new Download; use the returned value"]
    pub fn last_modified(mut self, http_date: impl Into<String>) -> Self {
        self.last_modified = Some(http_date.into());
        self
    }

    /// Resolve the effective content type.
    ///
    /// Order: explicit `.content_type()` → inferred from the filename
    /// extension → blob-metadata default → `application/octet-stream`.
    fn resolve_content_type(&self) -> String {
        self.content_type
            .clone()
            .or_else(|| {
                self.filename
                    .as_deref()
                    .and_then(guess_mime_from_filename)
                    .map(str::to_owned)
            })
            .or_else(|| self.default_content_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_owned())
    }
}

impl Download {
    /// Apply the `Content-Type`, `Content-Disposition`, and (when set) `ETag` /
    /// `Last-Modified` headers this download carries onto `headers`.
    fn apply_metadata_headers(&self, headers: &mut HeaderMap) {
        let content_type = self.resolve_content_type();
        let disposition = build_content_disposition(self.disposition, self.filename.as_deref());
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        );
        // `disposition` is ASCII and CR/LF-free by construction, so this
        // never falls back in practice.
        headers.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_str(&disposition)
                .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
        );
        if let Some(etag) = &self.etag {
            headers.insert(ETAG, etag.header_value());
        }
        if let Some(lm) = &self.last_modified
            && let Ok(v) = HeaderValue::from_str(lm)
        {
            headers.insert(LAST_MODIFIED, v);
        }
    }

    /// Build the `If-Range` validator from any attached `ETag` / `Last-Modified`.
    fn range_validator(&self) -> Option<Validator<'_>> {
        if self.etag.is_none() && self.last_modified.is_none() {
            return None;
        }
        let mut v = Validator::new();
        if let Some(etag) = &self.etag {
            v = v.with_etag(etag);
        }
        if let Some(lm) = &self.last_modified {
            v = v.with_last_modified(lm);
        }
        Some(v)
    }

    /// Serve the download honouring the request's `Range` header.
    ///
    /// Unlike the plain [`IntoResponse`] conversion (which cannot see the
    /// request), this reads `Range` / `If-Range` from `req_headers` and can
    /// answer with `206 Partial Content`:
    ///
    /// - **Range-capable body** ([`from_bytes`](Download::from_bytes),
    ///   [`from_blob`](Download::from_blob)): a satisfiable range yields a `206`
    ///   with `Content-Range` and the sliced body — for a blob, only the
    ///   requested slice is fetched from the store
    ///   ([`BlobStore::get_range`](crate::storage::BlobStore::get_range)). An
    ///   unsatisfiable range yields `416`. No/invalid range yields the full
    ///   `200` with `Accept-Ranges: bytes`.
    /// - **Opaque stream** ([`from_stream`](Download::from_stream) /
    ///   [`from_async_read`](Download::from_async_read)): not seekable, so this
    ///   always serves the full `200` and does not advertise `Accept-Ranges`.
    ///
    /// This is the documented path to a seekable media response; see the
    /// [`range`] module docs for a `#[secured]` video example.
    // The method is `async` for the blob path (which awaits a ranged store
    // read); without the `storage` feature that arm is compiled out, leaving no
    // `.await`, but the signature stays stable across feature sets.
    #[cfg_attr(not(feature = "storage"), allow(clippy::unused_async))]
    pub async fn into_response_ranged(self, req_headers: &HeaderMap) -> Response {
        // Opaque streams cannot be re-seeked: serve the full body exactly as
        // the plain `IntoResponse` conversion would.
        if !self.body.is_range_capable() {
            return self.into_response();
        }

        let total = self.content_length.unwrap_or(0);
        let resolution = range::resolve(req_headers, total, self.range_validator());

        // Precompute the metadata headers while `self` is still whole, then
        // move the body out below.
        let mut meta = HeaderMap::new();
        self.apply_metadata_headers(&mut meta);

        match self.body {
            DownloadBody::Bytes(bytes) => {
                let mut response = range::partial_bytes_response(&resolution, bytes);
                // `partial_bytes_response` already set status, Accept-Ranges,
                // Content-Range, and Content-Length; layer the download's own
                // Content-Type / Content-Disposition / validators on top.
                merge_headers(response.headers_mut(), &meta);
                response
            }
            #[cfg(feature = "storage")]
            DownloadBody::Blob { store, key, size } => {
                blob_ranged_response(&resolution, &store, &key, size, &meta).await
            }
            // Not range-capable — handled by the early return above.
            DownloadBody::Stream(_) => unreachable!("opaque streams return early"),
        }
    }
}

/// Copy every (single-valued) header from `src` into `dst`, overwriting.
fn merge_headers(dst: &mut HeaderMap, src: &HeaderMap) {
    for (name, value) in src {
        dst.insert(name, value.clone());
    }
}

/// Build the ranged response for a stored blob, fetching only the requested
/// slice from the store for a `206`.
#[cfg(feature = "storage")]
async fn blob_ranged_response(
    resolution: &range::RangeResolution,
    store: &crate::storage::SharedBlobStore,
    key: &str,
    size: u64,
    meta: &HeaderMap,
) -> Response {
    use futures::StreamExt as _;

    match *resolution {
        range::RangeResolution::Full => {
            // Open the whole-object stream only now (never up-front), so a
            // non-ranged request streams without buffering the object.
            let stream = match store.get_stream(key).await {
                Ok(stream) => stream,
                Err(err) => return err.into_autumn_error().into_response(),
            };
            let stream = stream.map(|chunk| chunk.map_err(std::io::Error::other));
            let mut response = Body::from_stream(stream).into_response();
            let headers = response.headers_mut();
            range::set_accept_ranges(headers);
            headers.insert(CONTENT_LENGTH, HeaderValue::from(size));
            merge_headers(headers, meta);
            response
        }
        range::RangeResolution::Partial { start, end, total } => {
            let stream = match store.get_range(key, start, end).await {
                Ok(stream) => stream,
                Err(err) => return err.into_autumn_error().into_response(),
            };
            let stream = stream.map(|chunk| chunk.map_err(std::io::Error::other));
            let mut response = Body::from_stream(stream).into_response();
            *response.status_mut() = http::StatusCode::PARTIAL_CONTENT;
            let headers = response.headers_mut();
            range::set_accept_ranges(headers);
            if let Ok(v) = HeaderValue::from_str(&range::content_range_value(start, end, total)) {
                headers.insert(http::header::CONTENT_RANGE, v);
            }
            let len = end.saturating_sub(start).saturating_add(1);
            headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
            merge_headers(headers, meta);
            response
        }
        range::RangeResolution::Unsatisfiable { .. } => {
            let mut response = range::partial_bytes_response(resolution, Bytes::new());
            merge_headers(response.headers_mut(), meta);
            response
        }
    }
}

/// Build a whole-object stream for the plain [`IntoResponse`] path, which
/// cannot `.await` the store up front. The store's byte stream is opened
/// lazily on first poll (never buffering the object here); an open error
/// surfaces as a stream error mid-body.
#[cfg(feature = "storage")]
fn lazy_blob_stream(store: crate::storage::SharedBlobStore, key: String) -> BoxByteStream {
    use futures::TryStreamExt as _;

    let opened = futures::stream::once(async move {
        store
            .get_stream(&key)
            .await
            .map(|stream| stream.map_err(std::io::Error::other))
            .map_err(std::io::Error::other)
    })
    .try_flatten();
    Box::pin(opened)
}

impl IntoResponse for Download {
    fn into_response(self) -> Response {
        let content_length = self.content_length;

        // Compute metadata before moving the body out of `self`.
        let mut header_scratch = HeaderMap::new();
        self.apply_metadata_headers(&mut header_scratch);

        let body = match self.body {
            DownloadBody::Bytes(bytes) => Body::from(bytes),
            DownloadBody::Stream(stream) => Body::from_stream(stream),
            #[cfg(feature = "storage")]
            DownloadBody::Blob { store, key, .. } => {
                Body::from_stream(lazy_blob_stream(store, key))
            }
        };

        let mut response = body.into_response();
        let headers = response.headers_mut();
        for (name, value) in &header_scratch {
            headers.insert(name, value.clone());
        }
        if let Some(len) = content_length {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
        }
        // This plain conversion cannot inspect the request's `Range` header and
        // always returns the full body, so it deliberately does **not**
        // advertise `Accept-Ranges` — that promise is honored only by
        // `into_response_ranged`, which can actually answer with `206`/`416`.
        response
    }
}

/// RFC 2231 `attr-char`: alphanumerics plus these ASCII punctuation marks may
/// appear unescaped in an extended parameter value; everything else (including
/// all non-ASCII and control bytes) is percent-encoded.
const RFC2231_ATTR_CHAR: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

/// Strip CR/LF and other control characters so a caller-supplied filename can
/// never inject an extra header line or directive.
fn strip_header_controls(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

/// Wrap `value` in a quoted-string, escaping backslashes and double quotes.
fn quote_header_value(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Build the `Content-Disposition` header value.
///
/// Always ASCII and CR/LF-free by construction: control characters are
/// stripped, the value is reduced to its basename (so a path-like name cannot
/// leak directory components), the ASCII form is quoted, and non-ASCII
/// filenames are RFC 5987 percent-encoded (`filename*=UTF-8''…`) alongside an
/// ASCII fallback.
fn build_content_disposition(disposition: Disposition, filename: Option<&str>) -> String {
    let kind = match disposition {
        Disposition::Attachment => "attachment",
        Disposition::Inline => "inline",
    };

    let Some(raw) = filename else {
        return kind.to_owned();
    };

    let clean = strip_header_controls(raw);
    let clean = clean.trim();
    // Defense-in-depth: reduce a caller-supplied path to its basename so
    // `.filename("../../etc/passwd")` emits `filename="passwd"` rather than
    // leaking directory components into the header.
    let clean = clean.rsplit(['/', '\\']).next().unwrap_or(clean).trim();
    if clean.is_empty() {
        return kind.to_owned();
    }

    if clean.is_ascii() {
        format!("{kind}; filename={}", quote_header_value(clean))
    } else {
        let fallback: String = clean
            .chars()
            .map(|ch| if ch.is_ascii() { ch } else { '_' })
            .collect();
        let encoded = percent_encoding::utf8_percent_encode(clean, RFC2231_ATTR_CHAR);
        format!(
            "{kind}; filename={}; filename*=UTF-8''{encoded}",
            quote_header_value(&fallback)
        )
    }
}

/// Guess a MIME type from a filename's extension.
///
/// Self-contained (no external `mime_guess` dependency) covering the common
/// download types; returns `None` for unknown/absent extensions so the caller
/// can fall back to `application/octet-stream`.
fn guess_mime_from_filename(filename: &str) -> Option<&'static str> {
    let (_, ext) = filename.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "wasm" => "application/wasm",
        _ => return None,
    };
    Some(mime)
}

#[cfg(test)]
mod tests {
    use super::{Disposition, build_content_disposition, guess_mime_from_filename};

    #[test]
    fn ascii_filename_is_quoted_plain_form() {
        let disp = build_content_disposition(Disposition::Attachment, Some("report.pdf"));
        assert_eq!(disp, "attachment; filename=\"report.pdf\"");
    }

    #[test]
    fn non_ascii_filename_gets_extended_param() {
        let disp = build_content_disposition(Disposition::Attachment, Some("naïve.txt"));
        // Exact encoding: ASCII fallback with non-ASCII replaced by `_`, plus
        // the RFC 5987 extended value (ï → %C3%AF).
        assert_eq!(
            disp,
            "attachment; filename=\"na_ve.txt\"; filename*=UTF-8''na%C3%AFve.txt"
        );
        assert!(disp.is_ascii());
    }

    #[test]
    fn path_like_filename_is_reduced_to_basename() {
        let disp = build_content_disposition(Disposition::Attachment, Some("../../etc/passwd"));
        assert_eq!(disp, "attachment; filename=\"passwd\"");

        // Windows-style separators too.
        let disp = build_content_disposition(Disposition::Attachment, Some("a\\b\\report.pdf"));
        assert_eq!(disp, "attachment; filename=\"report.pdf\"");
    }

    #[test]
    fn crlf_is_stripped() {
        let disp = build_content_disposition(Disposition::Attachment, Some("a\r\nSet-Cookie: x=1"));
        assert!(!disp.contains('\r') && !disp.contains('\n'));
        assert_eq!(disp, "attachment; filename=\"aSet-Cookie: x=1\"");
    }

    #[test]
    fn quotes_are_escaped() {
        let disp = build_content_disposition(Disposition::Attachment, Some("a\"b.txt"));
        assert_eq!(disp, "attachment; filename=\"a\\\"b.txt\"");
    }

    #[test]
    fn inline_and_no_filename() {
        assert_eq!(
            build_content_disposition(Disposition::Inline, None),
            "inline"
        );
        assert_eq!(
            build_content_disposition(Disposition::Attachment, None),
            "attachment"
        );
    }

    #[test]
    fn mime_inference() {
        assert_eq!(guess_mime_from_filename("a.pdf"), Some("application/pdf"));
        assert_eq!(guess_mime_from_filename("a.PNG"), Some("image/png"));
        assert_eq!(guess_mime_from_filename("a.jpeg"), Some("image/jpeg"));
        assert_eq!(guess_mime_from_filename("noext"), None);
        assert_eq!(guess_mime_from_filename("a.unknownext"), None);
    }
}
