//! One-time submit tokens — at-most-once form submissions with no client JS.
//!
//! A user who double-clicks **Submit** (or hits Back→resubmit, or whose
//! browser silently retries a flaky POST) can otherwise create duplicate
//! records. The two adjacent primitives don't close this hole on their own:
//!
//! - [`CsrfLayer`](crate::security::CsrfLayer) mints a *stable per-session*
//!   token — a valid `_csrf` value submitted twice passes both times.
//! - [`IdempotencyLayer`](crate::idempotency::IdempotencyLayer) is
//!   *header-driven* on `Idempotency-Key`, which API clients send but browsers
//!   never attach to a plain form POST.
//!
//! [`SubmitTokenLayer`] fuses the per-request token plumbing with the shared
//! idempotency store to give every scaffolded form a **server-side, default-on**
//! at-most-once guarantee:
//!
//! 1. On every render a fresh random token is made available via the
//!    [`SubmitToken`] extractor, embeddable as a hidden `_submit_token` field.
//! 2. On a mutating request the guard extracts `_submit_token` from the form
//!    body and consumes it against the store. First use runs the handler and
//!    records the response; a replayed token short-circuits and replays the
//!    first response instead of re-running the handler.
//! 3. No client header is required — this is the explicit difference from the
//!    `Idempotency-Key` layer.
//!
//! # Examples
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::security::SubmitToken;
//!
//! #[get("/form")]
//! async fn form(submit_token: SubmitToken) -> Markup {
//!     html! {
//!         form method="POST" action="/submit" {
//!             input type="hidden" name="_submit_token" value=(submit_token.token());
//!             input type="text" name="title";
//!             button { "Submit" }
//!         }
//!     }
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::{HeaderMap, Method, Request, Response, StatusCode};
use futures::StreamExt as _;
use sha2::Digest as _;
use tower::{Layer, Service};
use uuid::Uuid;

use super::config::SubmitTokenConfig;
use crate::idempotency::{IdempotencyRecord, IdempotencyStore};

/// Response header set on a replayed submit-token response.
const SUBMIT_TOKEN_REPLAYED: &str = "x-submit-token-replayed";

/// Maximum response body size cached for replay. Scaffold create/update
/// handlers redirect (303) with tiny bodies; larger responses stream through
/// without caching.
const MAX_CACHEABLE_RESPONSE_BODY: usize = 10 * 1024 * 1024; // 10 MiB

/// Hard cap on the request body bytes buffered while scanning for the token.
const MAX_SCAN_BYTES_CAP: usize = 2 * 1024 * 1024; // 2 MiB

/// The configured submit-token form field name.
///
/// Placed in request extensions by [`SubmitTokenLayer`] so templates emit the
/// hidden input under the correct name even when
/// `security.submit_token.field_name` is customised.
#[derive(Clone, Debug)]
pub struct SubmitFormField(pub String);

/// A one-time submit token minted per render.
///
/// Use this as a handler parameter to embed the token in an HTML form's hidden
/// `_submit_token` field. A fresh token is generated for every request (both
/// the GET that renders the form and the 422 re-render on a rejected POST) and
/// stored in request extensions by [`SubmitTokenLayer`].
#[derive(Clone, Debug)]
pub struct SubmitToken(String);

impl SubmitToken {
    /// Returns the submit-token value for embedding in forms.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) const fn new(token: String) -> Self {
        Self(token)
    }
}

impl std::fmt::Display for SubmitToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<S> FromRequestParts<S> for SubmitToken
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Submit token not found in request extensions. Is SubmitTokenLayer enabled?",
        ))
    }
}

impl<S> OptionalFromRequestParts<S> for SubmitToken
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

impl<S> FromRequestParts<S> for SubmitFormField
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Submit form field not found in request extensions. Is SubmitTokenLayer enabled?",
        ))
    }
}

impl<S> OptionalFromRequestParts<S> for SubmitFormField
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

const fn is_mutating_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(
        String::with_capacity(bytes.as_ref().len() * 2),
        |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

/// Derive the store key for a submitted token. The token is a per-render random
/// UUID, so it is globally unique and needs no method/path/principal scoping —
/// the token itself identifies the single logical submission.
fn storage_key(token: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"autumn.submit_token:v1:");
    hasher.update(token.as_bytes());
    format!("submit:{}", hex_lower(hasher.finalize()))
}

// ── Multipart / body scanning helpers ─────────────────────────────────────────

/// Return the byte position of the first occurrence of `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scan a buffered `multipart/form-data` body for a named text field.
fn scan_multipart_field<'a>(bytes: &'a [u8], boundary: &str, field_name: &str) -> Option<&'a str> {
    let delimiter = format!("--{boundary}");
    let delim = delimiter.as_bytes();
    let end_marker = format!("\r\n{delimiter}");
    let end_bytes = end_marker.as_bytes();
    let mut pos = 0;

    loop {
        let rel = find_bytes(&bytes[pos..], delim)?;
        pos += rel + delim.len();

        match bytes.get(pos..pos + 2) {
            Some(b"\r\n") => pos += 2,
            _ => break,
        }

        let header_end = find_bytes(&bytes[pos..], b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&bytes[pos..pos + header_end]).ok()?;
        let value_start = pos + header_end + 4;

        let is_match = headers.lines().any(|line| {
            if !line
                .to_ascii_lowercase()
                .starts_with("content-disposition:")
            {
                return false;
            }
            line.split(';').skip(1).any(|attr| {
                attr.trim()
                    .strip_prefix("name=")
                    .map(|v| v.trim_matches('"'))
                    == Some(field_name)
            })
        });

        if is_match {
            let end = find_bytes(&bytes[value_start..], end_bytes)
                .map_or(bytes.len(), |i| value_start + i);
            return std::str::from_utf8(&bytes[value_start..end]).ok();
        }

        let next = find_bytes(&bytes[value_start..], end_bytes)?;
        pos = value_start + next + 2;
    }

    None
}

fn scan_for_token(
    bytes: &[u8],
    is_urlencoded: bool,
    boundary: Option<&str>,
    field: &str,
) -> Option<String> {
    if is_urlencoded {
        url::form_urlencoded::parse(bytes)
            .find(|(key, _)| key == field)
            .map(|(_, value)| value.into_owned())
    } else if let Some(boundary) = boundary {
        scan_multipart_field(bytes, boundary, field).map(str::to_owned)
    } else {
        None
    }
}

/// Body collected up to a scan/cache limit.
enum CollectedBody {
    /// The whole body fits within the limit and is fully buffered.
    Full(Bytes),
    /// The body exceeded the limit. `prefix` is the first `limit` bytes of the
    /// body — including the leading bytes of the chunk that crossed the cap, so
    /// a token near the front is scannable even when the very first chunk is
    /// already over-limit; `body` chains the full, unmodified body (every byte
    /// of every chunk) with the remaining stream for pass-through.
    Oversized { prefix: Bytes, body: Body },
    /// The body stream yielded an error before EOF (and before crossing the
    /// limit). The bytes read so far are discarded: buffering-then-rebuilding
    /// them would hand a downstream handler a silently TRUNCATED form (a valid
    /// leading `_submit_token` followed by missing tail fields), so the caller
    /// must fail the request instead of pretending the short read succeeded.
    Errored(axum::Error),
}

/// Buffer `body` up to `limit` bytes without corrupting oversized bodies.
async fn collect_body(body: Body, limit: usize) -> CollectedBody {
    let mut buf = Vec::<u8>::new();
    let mut stream = body.into_data_stream();
    loop {
        match stream.next().await {
            // Clean end of stream: return everything buffered so far.
            None => break,
            // A mid-stream read error must be propagated, never swallowed into a
            // truncated `Full`: the handler would otherwise receive a form whose
            // tail fields were lost while the leading token still scanned and
            // consumed, so the caller fails the request on this variant.
            Some(Err(err)) => return CollectedBody::Errored(err),
            Some(Ok(chunk)) => {
                let remaining = limit.saturating_sub(buf.len());
                if chunk.len() > remaining {
                    // Fill the scan prefix up to `limit` with the leading bytes
                    // of the over-limit chunk, so a token at the front of the
                    // form is still found even when the FIRST chunk already
                    // exceeds the cap (e.g. an upstream middleware rebuilt the
                    // buffered body as one `Body::from(bytes)`, leaving `buf`
                    // empty here).
                    let mut prefix_buf = buf.clone();
                    prefix_buf.extend_from_slice(&chunk[..remaining]);
                    let prefix = Bytes::from(prefix_buf);
                    // Replay the FULL body unchanged: the buffered prefix bytes
                    // followed by the *complete* over-limit chunk (not just its
                    // scanned head) and the rest of the stream. The scan prefix
                    // is only for locating the token; the handler must receive
                    // every byte.
                    let mut leading = Vec::with_capacity(2);
                    if !buf.is_empty() {
                        leading.push(Ok::<Bytes, axum::Error>(Bytes::from(buf)));
                    }
                    leading.push(Ok::<Bytes, axum::Error>(chunk));
                    let body = Body::from_stream(futures::stream::iter(leading).chain(stream));
                    return CollectedBody::Oversized { prefix, body };
                }
                buf.extend_from_slice(&chunk);
            }
        }
    }
    CollectedBody::Full(Bytes::from(buf))
}

/// Extract the submitted `_submit_token` from the request body, returning the
/// token (if present) and a request whose body is preserved for the handler.
///
/// Returns `Err` when the body stream errors mid-read while scanning: the caller
/// must reject the request rather than forward a truncated form (see
/// [`CollectedBody::Errored`]).
async fn extract_submitted_token(
    req: Request<Body>,
    field: &str,
    max_scan_bytes: usize,
) -> Result<(Option<String>, Request<Body>), axum::Error> {
    let (parts, body) = req.into_parts();

    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // Media types are case-insensitive (RFC 9110 8.3.1) and the header may carry
    // leading whitespace; normalize the type token for the urlencoded check.
    let is_urlencoded = content_type
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("application/x-www-form-urlencoded");
    // Parse the boundary with `multer::parse_boundary` — the exact parser
    // `axum::extract::Multipart` uses downstream (via `mime`) — so the guard and
    // the extractor can never disagree about the boundary. A hand-rolled
    // `split(';')` diverges on quoted values: `mime` permits a `;` inside a
    // quoted parameter value, so `boundary="x;y"` parses to the boundary `x;y`
    // in the real extractor while a split truncates it to `x`, leaving the form
    // to be handled while its `_submit_token` is never scanned/consumed (the
    // request stays REPLAYABLE). `parse_boundary` is case-insensitive on the
    // media type / `boundary` param name and preserves the boundary VALUE's
    // case; it returns `Err` for a non-multipart type, so `.ok()` yields `None`.
    let boundary = multer::parse_boundary(content_type).ok();
    // content_type borrow ends here.

    if !is_urlencoded && boundary.is_none() {
        return Ok((None, Request::from_parts(parts, body)));
    }

    match collect_body(body, max_scan_bytes).await {
        CollectedBody::Full(bytes) => {
            let token = scan_for_token(&bytes, is_urlencoded, boundary.as_deref(), field);
            Ok((token, Request::from_parts(parts, Body::from(bytes))))
        }
        CollectedBody::Oversized { prefix, body } => {
            let token = scan_for_token(&prefix, is_urlencoded, boundary.as_deref(), field);
            Ok((token, Request::from_parts(parts, body)))
        }
        CollectedBody::Errored(err) => Err(err),
    }
}

/// Headers to strip when caching a response for replay: hop-by-hop headers plus
/// `set-cookie` (never resurrect a stale cookie on a replay) and our own marker.
fn replay_headers(headers: &HeaderMap) -> Vec<(String, Vec<u8>)> {
    const SKIP: &[&str] = &[
        "connection",
        "transfer-encoding",
        "keep-alive",
        "upgrade",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "set-cookie",
        SUBMIT_TOKEN_REPLAYED,
    ];
    headers
        .iter()
        .filter(|(name, _)| !SKIP.contains(&name.as_str()))
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
        .collect()
}

fn replay_response(record: &IdempotencyRecord) -> Response<Body> {
    let mut builder = Response::builder().status(record.status);
    for (name, value) in &record.headers {
        builder = builder.header(name.as_str(), value.as_slice());
    }
    builder
        .header(SUBMIT_TOKEN_REPLAYED, "true")
        .body(Body::from(record.body.clone()))
        .unwrap_or_else(|_| {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() =
                StatusCode::from_u16(record.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            resp
        })
}

fn in_flight_conflict_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::CONFLICT)
        .header("retry-after", "1")
        .body(Body::from(
            "this form submission is already being processed; retry after 1 second",
        ))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// `400` returned when the request body stream errors mid-read while scanning
/// for the token. Failing here is safer than forwarding a truncated form to the
/// handler (missing tail fields) after a leading token has already been consumed.
fn body_read_error_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::from("could not read the request body"))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// `500` returned when the handler's response body stream errors while it is
/// being buffered for replay caching. The token is not recorded, and the
/// in-flight lock is intentionally kept held (it expires via `in_flight_ttl`)
/// so a retry is rejected in-flight rather than re-running the already-committed
/// mutation; surface an error rather than a truncated body.
fn response_read_error_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("could not read the response body"))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

// ── Layer / Service ───────────────────────────────────────────────────────────

struct SubmitTokenSettings {
    store: Arc<dyn IdempotencyStore>,
    field_name: String,
    ttl: Duration,
    in_flight_ttl: Duration,
    exempt_paths: Vec<String>,
    max_scan_bytes: usize,
}

// `Arc::make_mut` in the builder methods requires `Clone` on the inner value.
impl Clone for SubmitTokenSettings {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            field_name: self.field_name.clone(),
            ttl: self.ttl,
            in_flight_ttl: self.in_flight_ttl,
            exempt_paths: self.exempt_paths.clone(),
            max_scan_bytes: self.max_scan_bytes,
        }
    }
}

/// Tower [`Layer`] that enforces at-most-once form submissions via one-time
/// submit tokens.
///
/// Applied automatically when `security.submit_token.enabled = true` in config
/// (the default). Mints a fresh [`SubmitToken`] into request extensions on every
/// request, and guards mutating requests that carry a `_submit_token` form
/// field.
#[derive(Clone)]
pub struct SubmitTokenLayer {
    settings: Arc<SubmitTokenSettings>,
}

impl std::fmt::Debug for SubmitTokenLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubmitTokenLayer")
            .field("field_name", &self.settings.field_name)
            .field("ttl", &self.settings.ttl)
            .finish_non_exhaustive()
    }
}

impl SubmitTokenLayer {
    /// Create a new submit-token layer backed by `store` and configured from
    /// `config`.
    #[must_use]
    pub fn new(store: Arc<dyn IdempotencyStore>, config: &SubmitTokenConfig) -> Self {
        let ttl = Duration::from_secs(config.ttl_secs);
        // The in-flight lock TTL is a SEPARATE knob from the replay `ttl`: it
        // must outlast any active mutating request so a slow submission's retry
        // stays excluded until the first request records its consumed token.
        // Deriving it from `ttl_secs` would let lowering the replay window reopen
        // the double-execute re-entry gap (issue #1360).
        let in_flight_ttl = Duration::from_secs(config.in_flight_ttl_secs);
        Self {
            settings: Arc::new(SubmitTokenSettings {
                store,
                field_name: config.field_name.clone(),
                ttl,
                in_flight_ttl,
                exempt_paths: config.exempt_paths.clone(),
                max_scan_bytes: MAX_SCAN_BYTES_CAP,
            }),
        }
    }

    /// Limit the form-body bytes read when scanning for the token field. The
    /// effective limit is `min(n, 2 MiB)`.
    #[must_use]
    pub fn with_max_scan_bytes(mut self, n: usize) -> Self {
        Arc::make_mut(&mut self.settings).max_scan_bytes = n.min(MAX_SCAN_BYTES_CAP);
        self
    }

    /// Add a path prefix that is exempt from submit-token guarding.
    #[must_use]
    pub fn with_exempt_path(mut self, path: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.settings)
            .exempt_paths
            .push(path.into());
        self
    }
}

impl<S> Layer<S> for SubmitTokenLayer {
    type Service = SubmitTokenService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SubmitTokenService {
            inner,
            settings: Arc::clone(&self.settings),
        }
    }
}

/// Tower [`Service`] produced by [`SubmitTokenLayer`].
#[derive(Clone)]
pub struct SubmitTokenService<S> {
    inner: S,
    settings: Arc<SubmitTokenSettings>,
}

impl<S> Service<Request<Body>> for SubmitTokenService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        // Mint a fresh token for every request and expose it via extensions so
        // GET renders and 422 re-renders can embed it in the next form.
        let minted = Uuid::new_v4().to_string();
        req.extensions_mut().insert(SubmitToken(minted));
        req.extensions_mut()
            .insert(SubmitFormField(self.settings.field_name.clone()));

        let clean = crate::security::path::clean_path(req.uri().path());
        let path = clean.as_str();
        let is_exempt = self.settings.exempt_paths.iter().any(|prefix| {
            if path == prefix {
                true
            } else if let Some(stripped) = path.strip_prefix(prefix) {
                prefix.ends_with('/') || stripped.starts_with('/')
            } else {
                false
            }
        });
        let is_guarded = !is_exempt && is_mutating_method(req.method());

        let settings = Arc::clone(&self.settings);
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            if !is_guarded {
                return inner.call(req).await;
            }

            let (submitted, req) =
                match extract_submitted_token(req, &settings.field_name, settings.max_scan_bytes)
                    .await
                {
                    Ok(pair) => pair,
                    Err(error) => {
                        // A body-stream read error while scanning must reject the
                        // request: forwarding the bytes read so far would hand the
                        // handler a truncated form whose leading token we already
                        // scanned, silently dropping tail fields.
                        tracing::warn!(
                            error = %error,
                            "Submit-token body scan failed on a read error; rejecting the request"
                        );
                        return Ok(body_read_error_response());
                    }
                };

            let Some(token) = submitted.filter(|t| !t.is_empty()) else {
                // No submit token present — pass through unchanged.
                return inner.call(req).await;
            };

            let key = storage_key(&token);

            // Already consumed — replay the stored first response. Use the
            // fallible `try_get` so a backend read/deserialization failure fails
            // CLOSED (matching `IdempotencyService`'s lookup path) instead of
            // collapsing to a cache miss: a swallowed error would fall through,
            // acquire a fresh lock, and re-run the mutation for an
            // already-consumed token. No lock is held yet, so surface `503`.
            match settings.store.try_get(&key) {
                Ok(Some(entry)) => return Ok(replay_response(&entry.record)),
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        "Submit-token consumed-token lookup failed; failing closed"
                    );
                    return Ok(crate::idempotency::persistence_failed_response());
                }
            }

            // Acquire the in-flight lock. A concurrent duplicate that loses the
            // race gets a 409 so it can never re-run the handler.
            if !settings.store.try_lock(&key, settings.in_flight_ttl) {
                return Ok(in_flight_conflict_response());
            }

            // Double-check after locking: a racing request may have completed
            // and stored its response between our miss and the lock acquisition.
            // A read failure here must also fail closed. Because we now hold the
            // in-flight lock, keep it held (do NOT unlock, letting it expire via
            // `in_flight_ttl`) so a retry with the same token is rejected
            // in-flight rather than re-running the handler — exactly as
            // `IdempotencyService` does on a post-lock lookup error.
            match settings.store.try_get(&key) {
                Ok(Some(entry)) => {
                    settings.store.unlock(&key);
                    return Ok(replay_response(&entry.record));
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        "Submit-token consumed-token lookup failed after lock acquisition; failing closed"
                    );
                    return Ok(crate::idempotency::persistence_failed_response());
                }
            }

            let response = inner.call(req).await?;
            Ok(cache_consumed_token_response(response, &settings, &key).await)
        })
    }
}

/// Run the handler's response through the consumed-token replay cache: buffer it
/// (up to [`MAX_CACHEABLE_RESPONSE_BODY`]), record 2xx/3xx responses under `key`
/// so a replay returns them verbatim, and release the in-flight lock. Kept out
/// of [`SubmitTokenService::call`] so that hot method stays under the line cap.
async fn cache_consumed_token_response(
    response: Response<Body>,
    settings: &SubmitTokenSettings,
    key: &str,
) -> Response<Body> {
    let (parts, body) = response.into_parts();
    match collect_body(body, MAX_CACHEABLE_RESPONSE_BODY).await {
        CollectedBody::Full(bytes) => {
            let status = parts.status.as_u16();
            // Cache successful (2xx) and redirect (3xx) responses so the
            // replayed submit returns the first response verbatim.
            if (200..400).contains(&status) {
                let record = IdempotencyRecord {
                    status,
                    headers: replay_headers(&parts.headers),
                    body: bytes.to_vec(),
                    metadata: Vec::new(),
                };
                // Persist the consumed-token record. If the store write fails
                // after the handler already committed its mutation and returned
                // a 2xx/3xx, fail closed exactly as `IdempotencyLayer` does: keep
                // the in-flight lock held (by not unlocking, it expires via
                // `in_flight_ttl`) so a retry carrying the same token gets a
                // `409` in-flight conflict rather than silently re-running the
                // create/update, and surface `503` instead of an un-recorded
                // success.
                if let Err(error) = settings
                    .store
                    .try_set(key, record, Vec::new(), settings.ttl)
                {
                    tracing::error!(
                        error = %error,
                        "Submit-token persistence failed after handler success; failing closed"
                    );
                    return crate::idempotency::persistence_failed_response();
                }
            }
            settings.store.unlock(key);
            Response::from_parts(parts, Body::from(bytes))
        }
        CollectedBody::Oversized { body, .. } => {
            // Too large to cache — stream through. The lock is released; a later
            // retry re-runs (acceptable: form responses are tiny redirects, so
            // this path is not hit in practice).
            settings.store.unlock(key);
            Response::from_parts(parts, body)
        }
        CollectedBody::Errored(error) => {
            let status = parts.status.as_u16();
            // The response body errored while buffering for the replay cache.
            // Mirror the `Full` branch's commit policy, keyed on status:
            //
            // * 2xx/3xx (committed, cacheable): the handler already committed
            //   its mutation, but we can neither record the token nor replay a
            //   truncated body. Fail closed exactly like the `try_set`
            //   persistence-failure path above — keep the in-flight lock held
            //   (by not unlocking, it expires via `in_flight_ttl`) so a retry
            //   carrying the same token gets a `409` in-flight conflict rather
            //   than re-running the committed mutation.
            // * non-2xx/3xx (not committed, not cacheable): like the `Full`
            //   branch's clean non-success path, this stores no record and the
            //   request stays retryable, so release the lock. An immediate
            //   resubmit of a failed/validation request re-runs the handler
            //   instead of getting a spurious 24h `409` in-flight conflict.
            tracing::error!(
                error = %error,
                "Submit-token response buffering failed on a read error; failing closed"
            );
            if !(200..400).contains(&status) {
                settings.store.unlock(key);
            }
            response_read_error_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idempotency::{IdempotencyEntry, IdempotencyStoreError, MemoryIdempotencyStore};
    use axum::Router;
    use axum::routing::{get, post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn default_config() -> SubmitTokenConfig {
        SubmitTokenConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn layer_with_store(store: Arc<dyn IdempotencyStore>) -> SubmitTokenLayer {
        SubmitTokenLayer::new(store, &default_config())
    }

    fn urlencoded_post(token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/submit")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::from(format!("_submit_token={token}&title=hello")))
            .unwrap()
    }

    /// Build a `multipart/form-data` POST carrying `_submit_token`. The raw
    /// `content_type` header value and the `boundary` used for the body
    /// delimiters are supplied separately so tests can vary the media-type
    /// casing independently of the (case-sensitive) boundary value.
    fn multipart_post(content_type: &str, boundary: &str, token: &str) -> Request<Body> {
        let body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"_submit_token\"\r\n\
             \r\n\
             {token}\r\n\
             --{boundary}--\r\n"
        );
        Request::builder()
            .method("POST")
            .uri("/submit")
            .header("Content-Type", content_type)
            .body(Body::from(body))
            .unwrap()
    }

    #[test]
    fn submit_token_extractor_exposes_value() {
        let token = SubmitToken::new("abc-123".to_owned());
        assert_eq!(token.token(), "abc-123");
        assert_eq!(token.to_string(), "abc-123");
    }

    #[tokio::test]
    async fn mints_token_available_to_extractor() {
        async fn handler(submit_token: SubmitToken) -> String {
            submit_token.token().to_owned()
        }
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let app = Router::new()
            .route("/", get(handler))
            .layer(layer_with_store(store));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let token = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            Uuid::parse_str(&token).is_ok(),
            "minted token should be a uuid: {token}"
        );
    }

    #[tokio::test]
    async fn first_use_runs_handler_replay_short_circuits() {
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "tok-replay";
        // First submit runs the handler.
        let first = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get(SUBMIT_TOKEN_REPLAYED).is_none());
        let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&first_body[..], b"created");

        // Second submit with the same token replays without re-running.
        let second = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(SUBMIT_TOKEN_REPLAYED)
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );
        let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&second_body[..], b"created");

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must run exactly once"
        );
    }

    #[tokio::test]
    async fn lowercase_multipart_consumes_token_and_replay_short_circuits() {
        // Positive control: a conventionally-cased multipart body already works.
        // This proves the multipart body format the tests build is correct, so
        // any failure of the mixed-case test below is unambiguously about casing.
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "tok-mp-lower";
        let ct = "multipart/form-data; boundary=simpleboundary123";
        let boundary = "simpleboundary123";

        let first = app
            .clone()
            .oneshot(multipart_post(ct, boundary, token))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get(SUBMIT_TOKEN_REPLAYED).is_none());

        let second = app
            .clone()
            .oneshot(multipart_post(ct, boundary, token))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(SUBMIT_TOKEN_REPLAYED)
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must run exactly once"
        );
    }

    #[tokio::test]
    async fn mixed_case_multipart_consumes_token_and_replay_short_circuits() {
        // Media types are case-insensitive (RFC 9110); the Multipart extractor
        // accepts `Multipart/Form-Data` with a `Boundary=` parameter. The body
        // scanner must recognize it and consume the token so a replay is caught.
        // The weird-case boundary value is identical in the header and the body
        // delimiters, which also proves the boundary VALUE stays case-sensitive.
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "tok-mp-mixed";
        let ct = "Multipart/Form-Data; Boundary=BoUnDaRy-XyZ-123";
        let boundary = "BoUnDaRy-XyZ-123";

        let first = app
            .clone()
            .oneshot(multipart_post(ct, boundary, token))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get(SUBMIT_TOKEN_REPLAYED).is_none());

        let second = app
            .clone()
            .oneshot(multipart_post(ct, boundary, token))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(SUBMIT_TOKEN_REPLAYED)
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must run exactly once"
        );
    }

    #[tokio::test]
    async fn quoted_semicolon_boundary_consumes_token_and_replay_short_circuits() {
        // A `;` inside a QUOTED boundary parameter is a valid RFC 2046 /
        // `mime` restricted quoted char, so `boundary="x;y"` parses to the
        // boundary `x;y` in the `multer`/`mime` parser axum's Multipart
        // extractor uses downstream. A hand-rolled `split(';')` truncates it
        // to `x`, so the guard fails to find/consume `_submit_token` while the
        // handler still parses and acts on the form — leaving the request
        // REPLAYABLE. The boundary parser must match the extractor's so the
        // token is consumed and a replay is short-circuited. Fully lowercase,
        // RFC-shaped multipart — no casing trick.
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "tok-mp-quoted-semi";
        let ct = "multipart/form-data; boundary=\"x;y\"";
        let boundary = "x;y";

        let first = app
            .clone()
            .oneshot(multipart_post(ct, boundary, token))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get(SUBMIT_TOKEN_REPLAYED).is_none());

        let second = app
            .clone()
            .oneshot(multipart_post(ct, boundary, token))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(SUBMIT_TOKEN_REPLAYED)
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must run exactly once"
        );
    }

    #[tokio::test]
    async fn uppercase_urlencoded_consumes_token_and_replay_short_circuits() {
        // The urlencoded branch has the same casing hole and is fixed by the
        // same media-type normalization.
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "tok-ue-upper";
        let make_req = || {
            Request::builder()
                .method("POST")
                .uri("/submit")
                .header("Content-Type", "APPLICATION/X-WWW-FORM-URLENCODED")
                .body(Body::from(format!("_submit_token={token}&title=hello")))
                .unwrap()
        };

        let first = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get(SUBMIT_TOKEN_REPLAYED).is_none());

        let second = app.clone().oneshot(make_req()).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(SUBMIT_TOKEN_REPLAYED)
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must run exactly once"
        );
    }

    #[tokio::test]
    async fn missing_token_passes_through() {
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            )
            .layer(layer_with_store(store));

        // No `_submit_token` field — both requests run the handler.
        for _ in 0..2 {
            let req = Request::builder()
                .method("POST")
                .uri("/submit")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from("title=hello"))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn expired_token_re_runs_after_ttl() {
        // A very short TTL means the stored record expires and a later submit
        // re-runs rather than replaying.
        let store = MemoryIdempotencyStore::new(Duration::from_millis(10));
        let key = storage_key("ttl-token");
        store.set(
            &key,
            IdempotencyRecord {
                status: 200,
                headers: Vec::new(),
                body: b"first".to_vec(),
                metadata: Vec::new(),
            },
            Vec::new(),
            Duration::from_millis(10),
        );
        assert!(store.get(&key).is_some());
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            store.get(&key).is_none(),
            "record must expire after its TTL"
        );
    }

    #[tokio::test]
    async fn distinct_from_csrf_replayed_submit_token_short_circuits() {
        // Even a request that would pass CSRF is short-circuited when its
        // submit token has already been consumed: the guard consults the store,
        // not the CSRF cookie/token.
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "tok-csrf-distinct";
        // Each submit carries a valid _csrf field alongside the submit token.
        let build = || {
            Request::builder()
                .method("POST")
                .uri("/submit")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "_csrf=valid-csrf&_submit_token={token}"
                )))
                .unwrap()
        };
        let first = app.clone().oneshot(build()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        // A replay with the same submit token (and a still-valid _csrf) is
        // short-circuited by the guard.
        let second = app.clone().oneshot(build()).await.unwrap();
        assert_eq!(
            second
                .headers()
                .get(SUBMIT_TOKEN_REPLAYED)
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    /// Success metric (AC #7): 10 identical concurrent POSTs at a scaffolded
    /// create endpoint persist exactly ONE row; the replays return the first
    /// response without re-running the side effect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ten_concurrent_posts_persist_exactly_one_row() {
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        // Simulates the durable side effect (a DB insert).
        let rows = Arc::new(AtomicUsize::new(0));
        let rows_inner = rows.clone();
        let app = Router::new()
            .route(
                "/posts",
                post(move || {
                    let rows = rows_inner.clone();
                    async move {
                        // Simulate real work so requests genuinely overlap.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        rows.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::SEE_OTHER, [("location", "/posts")], "")
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "concurrent-token";
        let mut handles = Vec::new();
        for _ in 0..10 {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                let req = Request::builder()
                    .method("POST")
                    .uri("/posts")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("_submit_token={token}&title=hello")))
                    .unwrap();
                app.oneshot(req).await.unwrap().status()
            }));
        }

        let mut succeeded = 0;
        let mut conflicts = 0;
        for h in handles {
            let status = h.await.unwrap();
            if status == StatusCode::SEE_OTHER {
                succeeded += 1;
            } else if status == StatusCode::CONFLICT {
                conflicts += 1;
            } else {
                panic!("unexpected status: {status}");
            }
        }

        assert_eq!(
            rows.load(Ordering::SeqCst),
            1,
            "exactly one row must be persisted from 10 concurrent identical POSTs"
        );
        assert_eq!(
            succeeded + conflicts,
            10,
            "every request must either return the first response or be rejected in-flight"
        );
        assert!(
            succeeded >= 1,
            "at least the first submission must succeed and be replayable"
        );
    }

    /// Regression: when the FIRST body chunk already exceeds `max_scan_bytes`
    /// (e.g. an upstream middleware rebuilt the buffered form as a single
    /// `Body::from(bytes)`), the leading `_submit_token` must still be scanned
    /// from that over-limit chunk — the guard fires — while the handler still
    /// receives the complete, untruncated body.
    #[tokio::test]
    async fn over_limit_first_chunk_detects_leading_token_and_preserves_body() {
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let seen_len = Arc::new(AtomicUsize::new(0));
        let seen_len_inner = seen_len.clone();
        // The handler consumes the whole body so we can assert nothing was
        // truncated on its way through the scan.
        let app = Router::new()
            .route(
                "/submit",
                post(move |body: Bytes| {
                    let count = count_inner.clone();
                    let seen_len = seen_len_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        seen_len.store(body.len(), Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            // A tiny scan cap the very first chunk already blows past.
            .layer(layer_with_store(store).with_max_scan_bytes(64));

        let token = "over-limit-token";
        // `_submit_token` sits at the front (well within the 64-byte cap); a
        // large filler pushes the single chunk far over the cap.
        let filler = "x".repeat(4096);
        let full_body = format!("_submit_token={token}&title={filler}");
        let full_len = full_body.len();
        assert!(full_len > 64, "body must exceed the scan cap for this test");
        let make = || {
            Request::builder()
                .method("POST")
                .uri("/submit")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(full_body.clone()))
                .unwrap()
        };

        // First submit: the guard scans the leading bytes of the over-limit
        // chunk, runs the handler once, and the handler sees the full body.
        let first = app.clone().oneshot(make()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert!(first.headers().get(SUBMIT_TOKEN_REPLAYED).is_none());
        assert_eq!(
            seen_len.load(Ordering::SeqCst),
            full_len,
            "handler must receive the complete body, not just the scanned prefix"
        );

        // Second submit with the same token replays — proving the leading token
        // was detected despite the over-limit first chunk.
        let second = app.clone().oneshot(make()).await.unwrap();
        assert_eq!(
            second
                .headers()
                .get(SUBMIT_TOKEN_REPLAYED)
                .map(|v| v.to_str().unwrap()),
            Some("true"),
            "an over-limit first chunk with a leading token must still be guarded"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must run exactly once"
        );
    }

    /// Store stub whose consumed-token persistence always fails, while locking
    /// and reads behave like the in-memory store. Mirrors the idempotency
    /// layer's failing-backend tests so we can prove fail-closed semantics.
    struct FailingSetStore {
        inner: MemoryIdempotencyStore,
    }

    impl FailingSetStore {
        fn new() -> Self {
            Self {
                inner: MemoryIdempotencyStore::new(Duration::from_secs(600)),
            }
        }
    }

    impl IdempotencyStore for FailingSetStore {
        fn get(&self, key: &str) -> Option<IdempotencyEntry> {
            self.inner.get(key)
        }

        fn set(&self, _key: &str, _record: IdempotencyRecord, _body_hash: Vec<u8>, _ttl: Duration) {
            // No-op: the fallible `try_set` path is what the guard uses; this
            // simulated backend never persists so retries cannot see a record.
        }

        fn try_set(
            &self,
            _key: &str,
            _record: IdempotencyRecord,
            _body_hash: Vec<u8>,
            _ttl: Duration,
        ) -> Result<(), IdempotencyStoreError> {
            Err(IdempotencyStoreError::backend(
                "simulated consumed-token persistence failure",
            ))
        }

        fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool {
            self.inner.try_lock(key, lock_ttl)
        }

        fn unlock(&self, key: &str) {
            self.inner.unlock(key);
        }
    }

    /// Finding C (fail closed): when the store cannot persist the consumed-token
    /// record after the handler already committed its mutation, the guard must
    /// NOT return a bare success (which a retry would re-run). It fails closed
    /// exactly like `IdempotencyLayer`: the first request surfaces `503`, and
    /// the in-flight lock stays held so a retry with the same token is rejected
    /// with an in-flight `409` instead of re-running the handler.
    #[tokio::test]
    async fn persistence_failure_fails_closed_and_holds_lock() {
        let store: Arc<dyn IdempotencyStore> = Arc::new(FailingSetStore::new());
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        let token = "tok-persist-fail";

        // First submit: handler runs once, but the store write fails, so the
        // guard fails closed with 503 instead of returning the 200 "created".
        let first = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(
            first.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a persistence failure after handler success must fail closed, not return the success"
        );

        // Retry with the same token: the in-flight lock is still held (it was
        // deliberately not released), so the retry is rejected in-flight and the
        // handler does NOT run a second time.
        let second = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(
            second.status(),
            StatusCode::CONFLICT,
            "a retry after a persistence failure must be rejected in-flight, not re-run"
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "the handler must run at most once even when persistence fails"
        );
    }

    /// Finding (fail closed on response-stream error): when the handler has
    /// already committed its mutation and returned a 2xx, but its response body
    /// stream then errors mid-buffer, the guard can neither record the token nor
    /// replay the truncated body. It must fail closed exactly like the
    /// persistence-failure path: the first request surfaces `500`, no
    /// consumed-token record is stored, and the in-flight lock stays held so a
    /// retry with the same token is rejected in-flight with a `409` instead of
    /// re-running the committed mutation.
    #[tokio::test]
    async fn response_stream_error_fails_closed_and_holds_lock() {
        let store = Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let store_dyn: Arc<dyn IdempotencyStore> = store.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        // The handler commits its mutation and returns a 200
                        // whose body stream delivers a leading chunk and then
                        // errors before EOF — so the replay cache buffering hits
                        // `CollectedBody::Errored` after the commit.
                        count.fetch_add(1, Ordering::SeqCst);
                        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
                            Ok(Bytes::from("created")),
                            Err(std::io::Error::other("simulated response read failure")),
                        ];
                        Response::new(Body::from_stream(futures::stream::iter(chunks)))
                    }
                }),
            )
            .layer(layer_with_store(store_dyn));

        let token = "tok-resp-stream-fail";

        // First submit: handler runs once and commits, but its response body
        // stream errors while buffering, so the guard fails closed with 500
        // instead of returning a truncated success.
        let first = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(
            first.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a response-stream error after handler commit must fail closed, not return a truncated body"
        );

        // No consumed-token record was persisted (the body never fully buffered).
        let key = storage_key(token);
        assert!(
            store.get(&key).is_none(),
            "a response-stream error must not persist a consumed-token record"
        );

        // Retry with the same token: the in-flight lock is still held (it was
        // deliberately not released), so the retry is rejected in-flight and the
        // handler does NOT run a second time.
        let second = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(
            second.status(),
            StatusCode::CONFLICT,
            "a retry after a response-stream error must be rejected in-flight, not re-run"
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "the handler must run at most once even when the response stream errors"
        );
    }

    /// Companion to `response_stream_error_fails_closed_and_holds_lock`: when the
    /// erroring response carries a NON-success status (e.g. `422`), the handler
    /// did not commit a cacheable mutation, so — matching the `Full` branch's
    /// clean non-success path — no record is stored and the in-flight lock is
    /// released. An immediate resubmit with the same token must therefore be
    /// retryable: it re-acquires the lock and re-runs the handler rather than
    /// getting a spurious `409` in-flight conflict for the full `in_flight_ttl`.
    #[tokio::test]
    async fn response_stream_error_on_non_success_releases_lock() {
        let store = Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let store_dyn: Arc<dyn IdempotencyStore> = store.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        // The handler returns a 422 (validation failure — no
                        // committed, cacheable mutation) whose body stream
                        // delivers a leading chunk and then errors before EOF,
                        // so the replay cache buffering hits
                        // `CollectedBody::Errored` on a non-success status.
                        count.fetch_add(1, Ordering::SeqCst);
                        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
                            Ok(Bytes::from("invalid")),
                            Err(std::io::Error::other("simulated response read failure")),
                        ];
                        Response::builder()
                            .status(StatusCode::UNPROCESSABLE_ENTITY)
                            .body(Body::from_stream(futures::stream::iter(chunks)))
                            .unwrap()
                    }
                }),
            )
            .layer(layer_with_store(store_dyn));

        let token = "tok-resp-stream-fail-422";

        // First submit: handler runs once and returns a 422 whose body stream
        // errors while buffering, so the guard surfaces 500 but — because the
        // status is non-success — releases the in-flight lock.
        let first = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(
            first.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a response-stream error must fail closed with 500 rather than a truncated body"
        );

        // No consumed-token record was persisted (non-success, and the body
        // never fully buffered).
        let key = storage_key(token);
        assert!(
            store.get(&key).is_none(),
            "a non-success response-stream error must not persist a consumed-token record"
        );

        // Retry with the same token: the lock was released, so the retry is NOT
        // rejected in-flight — it re-acquires the lock and re-runs the handler.
        let second = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_ne!(
            second.status(),
            StatusCode::CONFLICT,
            "a retry after a non-success response-stream error must be retryable, not a 409 in-flight conflict"
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "the handler must re-run on retry once the non-success lock is released"
        );
    }

    /// Store stub whose consumed-token lookups can be flipped to fail. Locking
    /// and writes behave like the in-memory store so a token can first be
    /// consumed, then have its lookup fail on replay. Mirrors the write-side
    /// `FailingSetStore` to prove the READ path also fails closed.
    struct FailingGetStore {
        inner: MemoryIdempotencyStore,
        fail_reads: std::sync::atomic::AtomicBool,
    }

    impl FailingGetStore {
        fn new() -> Self {
            Self {
                inner: MemoryIdempotencyStore::new(Duration::from_secs(600)),
                fail_reads: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl IdempotencyStore for FailingGetStore {
        fn get(&self, key: &str) -> Option<IdempotencyEntry> {
            // The infallible path collapses a read failure to a cache miss —
            // exactly the fail-open behaviour the guard must NOT rely on. It
            // uses `try_get` instead, so this stays here only for the trait.
            if self.fail_reads.load(Ordering::SeqCst) {
                None
            } else {
                self.inner.get(key)
            }
        }

        fn try_get(&self, key: &str) -> Result<Option<IdempotencyEntry>, IdempotencyStoreError> {
            if self.fail_reads.load(Ordering::SeqCst) {
                Err(IdempotencyStoreError::backend(
                    "simulated consumed-token lookup failure",
                ))
            } else {
                self.inner.try_get(key)
            }
        }

        fn set(&self, key: &str, record: IdempotencyRecord, body_hash: Vec<u8>, ttl: Duration) {
            self.inner.set(key, record, body_hash, ttl);
        }

        fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool {
            self.inner.try_lock(key, lock_ttl)
        }

        fn unlock(&self, key: &str) {
            self.inner.unlock(key);
        }
    }

    /// Finding (fail closed on read): once a token has been consumed, a backend
    /// read failure on the consumed-token lookup must NOT collapse to a cache
    /// miss that acquires a fresh lock and re-runs the mutation. The guard uses
    /// the fallible `try_get`, so a lookup error fails closed with `503` and the
    /// handler is not re-run — matching `IdempotencyService`'s lookup path.
    #[tokio::test]
    async fn read_failure_fails_closed_and_does_not_rerun() {
        let store = Arc::new(FailingGetStore::new());
        let store_dyn: Arc<dyn IdempotencyStore> = store.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store_dyn));

        let token = "tok-read-fail";

        // First submit consumes the token and records the response.
        let first = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Now make consumed-token lookups fail. A replay must fail closed with
        // 503 rather than treating the errored lookup as a miss and re-running.
        store.fail_reads.store(true, Ordering::SeqCst);
        let second = app.clone().oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(
            second.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a consumed-token lookup failure must fail closed, not re-run the handler"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "the handler must not re-run when the consumed-token lookup fails"
        );
    }

    /// Finding J (preserve body read errors): when the request body stream
    /// yields an error mid-read while the guard is scanning for the token, the
    /// request must FAIL (400) rather than reach the handler with a silently
    /// truncated form. A leading `_submit_token` followed by a lost tail must
    /// never be forwarded as if the short read had succeeded.
    #[tokio::test]
    async fn body_read_error_rejects_request_and_does_not_reach_handler() {
        let store: Arc<dyn IdempotencyStore> =
            Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move |_body: Bytes| {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(layer_with_store(store));

        // A urlencoded body whose stream delivers a leading chunk (with the
        // token near the front) and then errors before EOF.
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from("_submit_token=tok-read-err&ti")),
            Err(std::io::Error::other("simulated body read failure")),
        ];
        let body = Body::from_stream(futures::stream::iter(chunks));
        let req = Request::builder()
            .method("POST")
            .uri("/submit")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "a mid-read body-stream error must reject the request, not forward a truncated form"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "the handler must never see a truncated body when the stream errors mid-read"
        );
    }

    /// Store that records the TTL handed to `try_lock` (the in-flight lock
    /// duration) versus `set` (the consumed-record replay window), so a test can
    /// assert the two are governed by SEPARATE config knobs. Locking and storage
    /// otherwise behave like the in-memory store.
    struct TtlRecordingStore {
        inner: MemoryIdempotencyStore,
        lock_ttl: std::sync::Mutex<Option<Duration>>,
        set_ttl: std::sync::Mutex<Option<Duration>>,
    }

    impl TtlRecordingStore {
        fn new() -> Self {
            Self {
                inner: MemoryIdempotencyStore::new(Duration::from_secs(600)),
                lock_ttl: std::sync::Mutex::new(None),
                set_ttl: std::sync::Mutex::new(None),
            }
        }
    }

    impl IdempotencyStore for TtlRecordingStore {
        fn get(&self, key: &str) -> Option<IdempotencyEntry> {
            self.inner.get(key)
        }

        fn set(&self, key: &str, record: IdempotencyRecord, body_hash: Vec<u8>, ttl: Duration) {
            *self.set_ttl.lock().unwrap() = Some(ttl);
            self.inner.set(key, record, body_hash, ttl);
        }

        fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool {
            *self.lock_ttl.lock().unwrap() = Some(lock_ttl);
            self.inner.try_lock(key, lock_ttl)
        }

        fn unlock(&self, key: &str) {
            self.inner.unlock(key);
        }
    }

    /// Finding L (decouple in-flight lock TTL from replay TTL): the in-flight
    /// submission lock must be governed by `in_flight_ttl_secs`, NOT by the
    /// `ttl_secs` replay window. An operator who lowers `ttl_secs` must not
    /// shorten how long an active submission is excluded from re-entry — else a
    /// create/update that outruns the shrunken TTL would let a retry with the
    /// same token acquire a FRESH lock before the consumed record lands and both
    /// requests execute the mutation.
    #[tokio::test]
    async fn in_flight_lock_ttl_is_decoupled_from_replay_ttl() {
        let store = Arc::new(TtlRecordingStore::new());
        let store_dyn: Arc<dyn IdempotencyStore> = store.clone();
        // A deliberately tiny replay window paired with a normal in-flight TTL.
        let config = SubmitTokenConfig {
            enabled: true,
            ttl_secs: 1,
            in_flight_ttl_secs: 86_400,
            ..Default::default()
        };
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(SubmitTokenLayer::new(store_dyn, &config));

        let resp = app.oneshot(urlencoded_post("tok-decoupled")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The lock excluding a concurrent retry lives for the full
        // in_flight_ttl_secs — lowering ttl_secs does NOT open the re-entry gap.
        let lock_ttl = store
            .lock_ttl
            .lock()
            .unwrap()
            .expect("the guard must acquire an in-flight lock");
        assert_eq!(
            lock_ttl,
            Duration::from_secs(86_400),
            "the in-flight lock TTL must come from in_flight_ttl_secs, not ttl_secs"
        );
        // ...while the consumed-record replay window still tracks ttl_secs.
        let set_ttl = store
            .set_ttl
            .lock()
            .unwrap()
            .expect("the guard must record the consumed token");
        assert_eq!(
            set_ttl,
            Duration::from_secs(1),
            "the consumed-record replay TTL must still come from ttl_secs"
        );
    }

    /// Finding L (behavioural): even with a tiny replay `ttl_secs`, a token whose
    /// submission is still in-flight (lock held, consumed record not yet stored)
    /// excludes a retry with a `409` rather than letting it re-run the mutation.
    #[tokio::test]
    async fn in_flight_token_excludes_retry_with_small_replay_ttl() {
        let store = Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(600)));
        let store_dyn: Arc<dyn IdempotencyStore> = store.clone();
        let config = SubmitTokenConfig {
            enabled: true,
            ttl_secs: 1,
            in_flight_ttl_secs: 86_400,
            ..Default::default()
        };
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = count.clone();
        let app = Router::new()
            .route(
                "/submit",
                post(move || {
                    let count = count_inner.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(SubmitTokenLayer::new(store_dyn, &config));

        // Simulate the first request being mid-flight: hold the in-flight lock
        // for the token key with the normal in-flight TTL, without recording a
        // consumed response yet.
        let token = "tok-inflight";
        let key = storage_key(token);
        assert!(store.try_lock(&key, Duration::from_secs(86_400)));

        // A concurrent retry with the same token is rejected in-flight and never
        // reaches the handler.
        let resp = app.oneshot(urlencoded_post(token)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "a retry against a still-in-flight token must be excluded, not re-run"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "the handler must not run for a retry held out by the in-flight lock"
        );
    }
}
