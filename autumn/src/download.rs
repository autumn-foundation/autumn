//! Typed file downloads as an [`IntoResponse`].
//!
//! [`Download`] turns owned bytes, an async byte stream, an [`AsyncRead`], or
//! a stored [`Blob`](crate::storage::Blob) into an HTTP response with the
//! right `Content-Disposition`, `Content-Type`, and (when known)
//! `Content-Length` headers — without hand-rolling header strings in every
//! handler.
//!
//! The blob-backed constructor streams bytes straight from the store without
//! buffering the whole object in memory, so it works for large files behind
//! authorization (no public presigned URL required).
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
use http::HeaderValue;
use http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};

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
    /// Fully-buffered bytes with a known length.
    Bytes(Bytes),
    /// A streaming body of unknown or externally-tracked length.
    Stream(BoxByteStream),
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
}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let body = match &self.body {
            DownloadBody::Bytes(b) => format!("Bytes({} bytes)", b.len()),
            DownloadBody::Stream(_) => "Stream(..)".to_owned(),
        };
        f.debug_struct("Download")
            .field("body", &body)
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("default_content_type", &self.default_content_type)
            .field("disposition", &self.disposition)
            .field("content_length", &self.content_length)
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
        }
    }

    /// Build a download from any [`AsyncRead`], streaming its bytes.
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
    /// for the `Content-Length` and a default `Content-Type`, and opens a
    /// streaming body ([`get_stream`](crate::storage::BlobStore::get_stream))
    /// that does **not** buffer the whole object in memory. This keeps the
    /// bytes flowing through your own authorized handler — no public presigned
    /// URL is issued.
    ///
    /// # Errors
    ///
    /// Returns a [`BlobStoreError`](crate::storage::BlobStoreError) if the blob
    /// does not exist or the store cannot be read.
    #[cfg(feature = "storage")]
    pub async fn from_blob(
        store: &crate::storage::SharedBlobStore,
        key: impl Into<String>,
    ) -> Result<Self, crate::storage::BlobStoreError> {
        use futures::StreamExt as _;

        let key = key.into();
        let meta = store
            .head(&key)
            .await?
            .ok_or_else(|| crate::storage::BlobStoreError::NotFound(key.clone()))?;
        // `get_stream` yields a `'static` stream, so the download detaches
        // cleanly from the borrow of `store`.
        let stream = store.get_stream(&key).await?;
        let stream = stream.map(|chunk| chunk.map_err(std::io::Error::other));
        Ok(Self {
            body: DownloadBody::Stream(Box::pin(stream)),
            filename: None,
            content_type: None,
            default_content_type: Some(meta.content_type),
            disposition: Disposition::Attachment,
            content_length: Some(meta.byte_size),
        })
    }

    /// Set the download filename, controlling the `Content-Disposition`
    /// `filename` (and, for non-ASCII names, `filename*`) parameter.
    ///
    /// The name is sanitized: control characters (including CR/LF) are
    /// stripped and the value is quoted, so a caller-supplied name cannot
    /// inject extra header directives.
    #[must_use]
    pub fn filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// Set the `Content-Type` explicitly, overriding any type inferred from the
    /// filename extension or blob metadata.
    #[must_use]
    pub fn content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Serve the file inline (`Content-Disposition: inline`) instead of forcing
    /// a save dialog.
    #[must_use]
    pub const fn inline(mut self) -> Self {
        self.disposition = Disposition::Inline;
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

impl IntoResponse for Download {
    fn into_response(self) -> Response {
        let content_type = self.resolve_content_type();
        let disposition = build_content_disposition(self.disposition, self.filename.as_deref());
        let content_length = self.content_length;

        let body = match self.body {
            DownloadBody::Bytes(bytes) => Body::from(bytes),
            DownloadBody::Stream(stream) => Body::from_stream(stream),
        };

        let mut response = body.into_response();
        let headers = response.headers_mut();
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
        if let Some(len) = content_length {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
        }
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
/// stripped, the ASCII form is quoted, and non-ASCII filenames are RFC 5987
/// percent-encoded (`filename*=UTF-8''…`) alongside an ASCII fallback.
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
        assert!(disp.contains("filename*=UTF-8''"));
        assert!(disp.is_ascii());
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
