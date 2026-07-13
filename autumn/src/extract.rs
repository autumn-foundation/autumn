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
        let Some(mut field) = self
            .inner
            .next_field()
            .await
            .map_err(|err| multipart_error_to_error(&err))?
        else {
            return Ok(None);
        };

        // Only file parts carry uploadable content worth validating. Regular
        // form fields omit a filename entirely and are never sniffed. A file
        // part always carries a filename (`Some`), so we sniff whenever one is
        // present AND an allow-list is configured OR strict mismatch-rejection
        // is enabled — either check needs the actual (magic-byte) content type
        // rather than the spoofable client header.
        let needs_sniff = field.file_name().is_some()
            && (!self.config.allowed_mime_types.is_empty()
                || self.config.reject_on_content_type_mismatch);

        // An optional/empty file input submits a part with `filename=""` and a
        // 0-byte body. Only that genuinely-empty case is exempt from
        // enforcement (see below); a `filename=""` part with a NON-empty body
        // must still be sniffed and enforced, since callers can consume its
        // bytes by field name. Capture the empty-filename flag now, before the
        // body is consumed by the prefix-buffering loop, to avoid borrowing
        // `field` after it has been read. On the sniff path this flag also
        // drives the `file_name()` normalization: a genuinely-empty optional
        // input surfaces as `None`, a non-empty-body empty-filename part as a
        // file.
        let filename_is_empty = field.file_name().is_some_and(str::is_empty);

        if !needs_sniff {
            // Default path: no MIME policy, so no sniffing and no enforcement.
            // Hand the field through with ZERO buffering — exactly as before
            // #1873 — so a handler can reject or stream it without the whole
            // part being pulled into memory first. `file_name()` normalization
            // is scoped to the sniff path (below), where body emptiness is
            // already observed from the bounded sniff prefix; here there is no
            // classification happening, so the raw inner filename (`Some("")`
            // for an empty-filename part) is surfaced unchanged. Default config
            // was explicitly unaffected by #1873.
            return Ok(Some(MultipartField::new(
                field,
                self.config.max_file_size_bytes,
            )));
        }

        // Buffer a bounded leading prefix (a few chunks at most) for sniffing,
        // preserving streaming: the rest of the field is still pulled lazily by
        // the consuming methods, which replay this prefix first.
        let mut prefix: Vec<u8> = Vec::with_capacity(SNIFF_PREFIX_BYTES);
        while prefix.len() < SNIFF_PREFIX_BYTES {
            match field
                .chunk()
                .await
                .map_err(|err| multipart_error_to_error(&err))?
            {
                Some(chunk) => {
                    prefix.extend_from_slice(&chunk);
                    // Reject as soon as the buffered prefix exceeds the per-file
                    // cap, so an oversized leading chunk bails without reading on.
                    if prefix.len() > self.config.max_file_size_bytes {
                        return Err(file_too_large_error(self.config.max_file_size_bytes));
                    }
                }
                None => break,
            }
        }

        // Borrow the declared header rather than allocating: `declared_essence`
        // only needs a slice, and the borrow of `field` ends before it is moved
        // into the returned wrapper below.
        // Compare on the type ESSENCE (media type without parameters), so a
        // header like `image/png; charset=binary` matches sniffed `image/png`.
        let declared_essence = field.content_type().map(content_type_essence);
        let sniffed = sniff_content_type(&prefix);

        // A genuinely-empty optional file input (empty filename AND 0-byte
        // body) carries no upload: skip enforcement so it passes through as a
        // non-file/absent field instead of aborting the submit. Every other
        // part with a filename — including `filename=""` with a non-empty body
        // — is sniffed and enforced exactly as below, closing the bypass where
        // a crafted empty filename could smuggle disallowed content.
        let is_empty_file_input = filename_is_empty && prefix.is_empty();
        if !is_empty_file_input {
            // Enforce the allow-list (sniffed → markup-guard → declared
            // fallback) and, when enabled, strict declared-vs-sniffed matching.
            // See the helpers below for the exact rules.
            if !self.config.allowed_mime_types.is_empty() {
                enforce_upload_allow_list(
                    &self.config.allowed_mime_types,
                    &prefix,
                    declared_essence,
                    sniffed,
                )?;
            }
            if self.config.reject_on_content_type_mismatch {
                enforce_content_type_match(declared_essence, sniffed)?;
            }
        }

        Ok(Some(MultipartField {
            inner: field,
            max_file_size_bytes: self.config.max_file_size_bytes,
            prefix,
            sniffed_content_type: sniffed,
            is_empty_optional_input: is_empty_file_input,
        }))
    }
}

/// Sniff the content type of a file from its leading bytes via magic-byte
/// detection. Returns `None` when the format is unrecognized. `infer` yields a
/// `&'static str` media type, so no allocation is needed.
#[cfg(feature = "multipart")]
fn sniff_content_type(prefix: &[u8]) -> Option<&'static str> {
    infer::get(prefix).map(|kind| kind.mime_type())
}

/// The essence of a content-type header — the media type without any
/// parameters (e.g. `image/png` from `image/png; charset=binary`).
#[cfg(feature = "multipart")]
fn content_type_essence(raw: &str) -> &str {
    raw.split(';').next().unwrap_or("").trim()
}

/// Whether the leading bytes look like textual markup (`<…`), after skipping a
/// UTF-8 BOM and any leading ASCII whitespace. Catches HTML (`<!DOCTYPE`,
/// `<html`, `<script`), SVG (`<svg`), and XML (`<?xml`) that `infer` cannot
/// recognize by magic bytes, so they can never be admitted via the declared
/// content-type fallback.
#[cfg(feature = "multipart")]
fn prefix_looks_like_markup(prefix: &[u8]) -> bool {
    let bytes = prefix.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(prefix);
    bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'<')
}

/// Bound a content-type value before echoing it into an error message, so a
/// hostile client can't inflate responses with an arbitrarily long header.
#[cfg(feature = "multipart")]
fn truncate_for_error(value: &str) -> String {
    const MAX_CHARS: usize = 128;
    value.chars().take(MAX_CHARS).collect()
}

/// Types that legitimately have no magic-byte signature, so a content sniff of
/// `None` is expected and the declared type may be trusted for the allow-list.
/// Binary/sniffable types (image/*, application/pdf, …) are deliberately
/// excluded: a genuine one always sniffs positively, so `None` for such a type
/// means the declared header is spoofed.
#[cfg(feature = "multipart")]
fn is_signatureless_text_type(essence: &str) -> bool {
    // Media types are case-insensitive, so normalize before comparing.
    let lower = essence.to_ascii_lowercase();
    lower.starts_with("text/") || matches!(lower.as_str(), "application/json" | "application/csv")
}

/// Enforce a MIME allow-list against a file part, using content sniffing as the
/// primary signal. `infer` only recognizes binary formats by magic bytes; text
/// formats (text/plain, application/json, text/csv, …) have no signature and
/// always sniff to `None`. Precedence:
///
/// 1. **Sniffed recognized** → must be in the list, else reject. Header ignored.
/// 2. **Unrecognized + markup** (leading `<…`) → reject unconditionally, so
///    HTML/SVG/XML can't ride in under a spoofed declared type.
/// 3. **Unrecognized + not markup** → the declared header is trusted ONLY when
///    it names a signature-less TEXT type (see [`is_signatureless_text_type`])
///    that is in the list. A declared binary/sniffable type on unrecognizable
///    bytes is definitionally a spoof (a real one would have sniffed) → reject.
#[cfg(feature = "multipart")]
fn enforce_upload_allow_list(
    allowed: &[String],
    prefix: &[u8],
    declared_essence: Option<&str>,
    sniffed: Option<&str>,
) -> crate::AutumnResult<()> {
    let in_list = |value: &str| {
        allowed
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(value))
    };
    if let Some(sniffed) = sniffed {
        if !in_list(sniffed) {
            return Err(crate::AutumnError::bad_request_msg(format!(
                "upload content type not allowed: sniffed={sniffed}"
            )));
        }
    } else if prefix_looks_like_markup(prefix) {
        return Err(crate::AutumnError::bad_request_msg(
            "upload rejected: unrecognized file content looks like markup",
        ));
    } else if !matches!(
        declared_essence,
        Some(essence) if is_signatureless_text_type(essence) && in_list(essence)
    ) {
        return Err(crate::AutumnError::bad_request_msg(format!(
            "upload content type could not be verified from its content: declared={}",
            declared_essence.map_or_else(String::new, truncate_for_error)
        )));
    }
    Ok(())
}

/// Strict declared-vs-sniffed matching (`reject_on_content_type_mismatch`).
/// Comparison is on the declared essence. Omitting the header is itself a
/// failure — otherwise an attacker could bypass the check by sending no
/// `Content-Type`.
#[cfg(feature = "multipart")]
fn enforce_content_type_match(
    declared_essence: Option<&str>,
    sniffed: Option<&str>,
) -> crate::AutumnResult<()> {
    let Some(declared_essence) = declared_essence else {
        return Err(crate::AutumnError::bad_request_msg(
            "content-type mismatch check enabled but the upload declared no content type",
        ));
    };
    match sniffed {
        Some(sniffed) if declared_essence.eq_ignore_ascii_case(sniffed) => Ok(()),
        Some(sniffed) => Err(crate::AutumnError::bad_request_msg(format!(
            "declared content type {} does not match sniffed content type {sniffed}",
            truncate_for_error(declared_essence)
        ))),
        None => Err(crate::AutumnError::bad_request_msg(format!(
            "cannot verify declared content type {}: file content is unrecognized",
            truncate_for_error(declared_essence)
        ))),
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

/// Number of leading bytes buffered for magic-byte content sniffing.
///
/// `infer` only needs a small header prefix to recognize a format, so we read
/// at most this many bytes (a few chunks) and never buffer the whole upload.
#[cfg(feature = "multipart")]
const SNIFF_PREFIX_BYTES: usize = 512;

/// A multipart field wrapper that provides safe streaming helpers.
#[cfg(feature = "multipart")]
pub struct MultipartField<'a> {
    inner: axum::extract::multipart::Field<'a>,
    max_file_size_bytes: usize,
    /// Leading bytes already consumed from `inner` during content sniffing.
    /// Empty when no sniffing occurred. Consuming methods must emit these
    /// first so the already-read prefix is not lost.
    prefix: Vec<u8>,
    /// Sniffed (magic-byte) content type, if the leading bytes were recognized.
    /// `infer` returns a `&'static str`, so this stores the borrow directly.
    sniffed_content_type: Option<&'static str>,
    /// Whether this part is a genuinely-empty optional file input: an empty
    /// `filename=""` AND a 0-byte body. Set only on the sniff path (an
    /// allow-list or strict-mismatch policy is configured), where body
    /// emptiness is already observed from the bounded sniff prefix, and used by
    /// [`file_name`](Self::file_name) to normalize such a part to `None` while
    /// leaving an empty-filename part with a non-empty body classified as a
    /// file (`Some("")`). Always `false` on the default (no-policy) path, which
    /// streams through without buffering and performs no `file_name()`
    /// normalization.
    is_empty_optional_input: bool,
}

#[cfg(all(feature = "multipart", feature = "storage"))]
struct MultipartFieldStreamState<'a> {
    inner: axum::extract::multipart::Field<'a>,
    /// Sniffed prefix bytes to emit before draining `inner`. Taken once.
    prefix: Option<bytes::Bytes>,
    total: usize,
    max: usize,
    errored: bool,
}

#[cfg(feature = "multipart")]
#[allow(clippy::elidable_lifetime_names)]
impl<'a> MultipartField<'a> {
    /// Wrap a raw multipart field with no sniffed prefix (the common path for
    /// non-file parts or when no content validation is configured).
    const fn new(inner: axum::extract::multipart::Field<'a>, max_file_size_bytes: usize) -> Self {
        Self {
            inner,
            max_file_size_bytes,
            prefix: Vec::new(),
            sniffed_content_type: None,
            is_empty_optional_input: false,
        }
    }

    /// Field name from the multipart form.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.inner.name()
    }

    /// The sniffed (magic-byte) content type of this field, if it was
    /// validated by content and the format was recognized.
    ///
    /// Returns `None` when no sniffing occurred (non-file part, or no
    /// allow-list / mismatch policy configured) or the content was
    /// unrecognized. Prefer this over [`content_type`](Self::content_type),
    /// which returns the spoofable client-declared header.
    #[must_use]
    pub const fn sniffed_content_type(&self) -> Option<&str> {
        self.sniffed_content_type
    }

    /// Uploaded file name (if this field represents a file).
    ///
    /// Normalization applies on the SNIFF PATH ONLY — i.e. when an allow-list
    /// or strict-mismatch policy is configured — and to exactly ONE case there:
    /// an empty `filename=""` AND a 0-byte body is returned as `None`, because
    /// an optional file input left blank submits such a part to represent an
    /// absent file rather than a real upload (see issue #1873). Handlers that
    /// distinguish uploads via `field.file_name().is_some()` therefore treat it
    /// as "no file provided" instead of persisting a zero-byte file.
    ///
    /// On the sniff path every other part is returned unchanged, so this getter
    /// agrees with the file/non-file classification `next_field` uses for
    /// enforcement:
    ///
    /// - empty filename + NON-empty body → `Some("")` (it is a file: it is
    ///   sniffed and enforced under an allow-list, so it is surfaced as one);
    /// - non-empty filename → `Some("name")`.
    ///
    /// On the DEFAULT path (no MIME policy) no normalization occurs: the field
    /// streams through unbuffered and the raw inner value is returned verbatim
    /// (`Some("")` for an empty-filename part). Default config was explicitly
    /// unaffected by #1873, and no enforcement/classification happens there, so
    /// there is no inconsistency to resolve.
    ///
    /// The sniff-path decision is made in `next_field`, where body emptiness is
    /// observable from the bounded prefix, and recorded on this field — this
    /// getter cannot inspect the (lazily read) body itself.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        if self.is_empty_optional_input {
            None
        } else {
            self.inner.file_name()
        }
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
        // Reuse the sniff prefix as the output buffer: it already holds exactly
        // the leading bytes in order, so appending the inner stream after it
        // reproduces the original byte sequence with no extra allocation.
        let mut out = self.prefix;
        let mut read = out.len();
        if read > self.max_file_size_bytes {
            return Err(file_too_large_error(self.max_file_size_bytes));
        }
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
        // Persist the VERIFIED (sniffed) type when available, falling back to
        // the client-declared header only when no sniffing occurred.
        let content_type = self
            .sniffed_content_type
            .map(str::to_owned)
            .or_else(|| self.inner.content_type().map(str::to_owned))
            .unwrap_or_else(|| "application/octet-stream".to_owned());

        let prefix = if self.prefix.is_empty() {
            None
        } else {
            Some(bytes::Bytes::from(self.prefix))
        };

        // Adapt the multipart chunk iterator into the trait's
        // `ByteStream`, enforcing the per-file size cap as we go so we
        // never buffer the whole upload in memory and large files flow
        // straight through to the store's streaming path. Any sniffed prefix
        // is replayed as the first chunk so it is not lost.
        let state = MultipartFieldStreamState {
            inner: self.inner,
            prefix,
            total: 0,
            max: self.max_file_size_bytes,
            errored: false,
        };

        let stream = futures::stream::unfold(state, |mut state| async move {
            if state.errored {
                return None;
            }
            if let Some(prefix) = state.prefix.take() {
                state.total = state.total.saturating_add(prefix.len());
                if state.total > state.max {
                    let err = crate::storage::BlobStoreError::PayloadTooLarge(format!(
                        "uploaded file exceeds limit of {} bytes",
                        state.max,
                    ));
                    state.errored = true;
                    return Some((Err(err), state));
                }
                return Some((Ok(prefix), state));
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
        // Write any sniffed prefix bytes first so they are not lost, counting
        // them toward the per-file cap.
        if !self.prefix.is_empty() {
            written += self.prefix.len();
            if written > self.max_file_size_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                return Err(file_too_large_error(self.max_file_size_bytes));
            }
            file.write_all(&self.prefix)
                .await
                .map_err(crate::AutumnError::internal_server_error)?;
        }
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

        let wrapper = MultipartField::new(field, 100);

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

        let wrapper = MultipartField::new(field, 5);

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

        let wrapper = MultipartField::new(field, 100);

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

        let wrapper = MultipartField::new(field, 4);

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

        let wrapper = MultipartField::new(field, 100);

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

        let wrapper = MultipartField::new(field, 4); // "blob content" is 12 bytes

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

        let wrapper = MultipartField::new(field, 100);

        assert_eq!(wrapper.name(), Some("custom_name"));
        assert_eq!(wrapper.file_name(), Some("custom_file.png"));
        assert_eq!(wrapper.content_type(), Some("image/png"));

        let tighter = wrapper.with_max_bytes(50);
        assert_eq!(tighter.max_file_size_bytes, 50);

        let not_tighter = tighter.with_max_bytes(200);
        assert_eq!(not_tighter.max_file_size_bytes, 50); // should not relax
    }

    #[test]
    fn sniff_content_type_recognizes_png() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        assert_eq!(sniff_content_type(&png), Some("image/png"));
    }

    #[test]
    fn sniff_content_type_unknown_is_none() {
        assert_eq!(sniff_content_type(&[0x01, 0x02]), None);
        assert_eq!(sniff_content_type(&[]), None);
    }

    #[test]
    fn content_type_essence_strips_parameters() {
        assert_eq!(
            content_type_essence("image/png; charset=binary"),
            "image/png"
        );
        assert_eq!(content_type_essence("  text/csv  "), "text/csv");
        assert_eq!(content_type_essence("application/json"), "application/json");
        assert_eq!(content_type_essence(""), "");
    }

    #[test]
    fn is_signatureless_text_type_is_case_insensitive() {
        // Media types are case-insensitive, so mixed-case declarations must be
        // recognized as signature-less text.
        assert!(is_signatureless_text_type("text/csv"));
        assert!(is_signatureless_text_type("Text/Csv"));
        assert!(is_signatureless_text_type("TEXT/PLAIN"));
        assert!(is_signatureless_text_type("application/json"));
        assert!(is_signatureless_text_type("Application/JSON"));
        assert!(is_signatureless_text_type("application/csv"));
        // Binary/sniffable types are never treated as signature-less text —
        // the P1 fix must be preserved regardless of case.
        assert!(!is_signatureless_text_type("image/png"));
        assert!(!is_signatureless_text_type("Image/PNG"));
        assert!(!is_signatureless_text_type("application/octet-stream"));
        assert!(!is_signatureless_text_type("application/pdf"));
    }

    #[test]
    fn prefix_looks_like_markup_detects_leading_angle_bracket() {
        assert!(prefix_looks_like_markup(b"<!DOCTYPE html>"));
        assert!(prefix_looks_like_markup(b"<svg onload=alert(1)>"));
        assert!(prefix_looks_like_markup(b"<?xml version=\"1.0\"?>"));
        // Leading whitespace and a UTF-8 BOM are skipped.
        assert!(prefix_looks_like_markup(b"   \n\t<html>"));
        assert!(prefix_looks_like_markup(&[0xEF, 0xBB, 0xBF, b'<', b'a']));
        // Genuine non-markup text and binary are not flagged.
        assert!(!prefix_looks_like_markup(b"a,b\n1,2"));
        assert!(!prefix_looks_like_markup(br#"{"a":1}"#));
        assert!(!prefix_looks_like_markup(&[0x01, 0x02]));
        assert!(!prefix_looks_like_markup(b""));
    }

    #[tokio::test]
    async fn next_field_exposes_sniffed_content_type_for_genuine_png() {
        // Genuine PNG bytes declared as octet-stream: the extractor must
        // recognize the real type from magic bytes and expose it.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(
            b"--boundary\r\nContent-Disposition: form-data; name=\"file\"; \
              filename=\"real.png\"\r\nContent-Type: application/octet-stream\r\n\r\n",
        );
        body.extend_from_slice(&[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ]);
        body.extend_from_slice(b"\r\n--boundary--\r\n");

        let req = Request::builder()
            .header("content-type", "multipart/form-data; boundary=boundary")
            .body(axum::body::Body::from(body))
            .unwrap();

        let inner = axum::extract::Multipart::from_request(req, &())
            .await
            .unwrap();
        let mut multipart = Multipart {
            inner,
            config: crate::security::config::UploadConfig {
                allowed_mime_types: vec!["image/png".to_owned()],
                ..crate::security::config::UploadConfig::default()
            },
        };

        let field = multipart.next_field().await.unwrap().unwrap();
        assert_eq!(field.sniffed_content_type(), Some("image/png"));
        // The prefix bytes consumed during sniffing are not lost.
        let bytes = field.bytes_limited().await.unwrap();
        assert_eq!(bytes.len(), 16);
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
