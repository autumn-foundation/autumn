//! ETag-based conditional GET helpers for HTML and JSON handlers.
//!
//! Autumn makes conditional `GET` a one-liner via [`fresh_when`] and the
//! [`EtagLayer`] middleware. Together they give a framework app three layers
//! of "don't do work twice":
//!
//! | Layer | Mechanism | Scope |
//! |-------|-----------|-------|
//! | Server render skip | `#[cached]` | avoid running the handler |
//! | Network skip | `fresh_when` / [`EtagLayer`] | avoid retransmitting the body |
//! | Static skip | ISR / `#[static_get]` | serve pre-rendered bytes |
//!
//! # Quick start
//!
//! ## HTML handler (Maud)
//!
//! ```rust,no_run
//! use autumn_web::etag::fresh_when;
//! use autumn_web::prelude::*;
//!
//! #[get("/posts/{id}")]
//! async fn show(id: Path<i64>, headers: http::HeaderMap, mut db: Db) -> AutumnResult<impl IntoResponse> {
//!     // post.etag() is derived from #[lock_version] — no manual hashing.
//!     // let post = Post::find(*id, &mut db).await?;
//!     // Ok(fresh_when(&headers, post.etag()).or(html! { h1 { (post.title) } }))
//!     Ok(StatusCode::OK)
//! }
//! ```
//!
//! ## JSON handler
//!
//! ```rust,no_run
//! use autumn_web::etag::fresh_when;
//! use autumn_web::prelude::*;
//!
//! #[get("/api/posts/{id}")]
//! async fn show_json(id: Path<i64>, headers: http::HeaderMap, mut db: Db) -> AutumnResult<impl IntoResponse> {
//!     // let post = Post::find(*id, &mut db).await?;
//!     // Ok(fresh_when(&headers, post.etag()).or(Json(post)))
//!     Ok(StatusCode::OK)
//! }
//! ```
//!
//! # htmx polling pattern
//!
//! ```html
//! <div hx-get="/posts/1" hx-trigger="every 5s" hx-swap="outerHTML">
//!   <!-- content -->
//! </div>
//! ```
//!
//! With `fresh_when` protecting the handler, unchanged polls return `304`
//! with zero body bytes and cost < 2 ms end-to-end.

use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::response::{IntoResponse, IntoResponseParts, Response as AxumResponse, ResponseParts};
use futures::stream::StreamExt as _;
use http::header::{
    CACHE_CONTROL, CONTENT_LOCATION, DATE, ETAG, EXPIRES, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    LAST_MODIFIED, SET_COOKIE, VARY,
};
use http::{HeaderMap, HeaderValue, Response, StatusCode};
use http_body_util::BodyExt;
use sha2::Digest as _;
use tower::{Layer, Service};

// ── ETag type ─────────────────────────────────────────────────────────────────

/// A validated HTTP `ETag` value.
///
/// The inner string is the *opaque tag* — without surrounding quotes or the
/// `W/` weak prefix. [`ETag::header_value`] produces the correctly formatted
/// `ETag` header value (e.g. `W/"abc123"` for a weak `ETag`).
///
/// # Creating an `ETag`
///
/// Most callers should use the [`IntoETag`] blanket conversions rather than
/// constructing an `ETag` directly:
///
/// ```rust
/// use autumn_web::etag::{ETag, IntoETag};
///
/// let e1: ETag = "abc".into_etag();       // strong, from str
/// let e2: ETag = 42_i64.into_etag();      // strong, from lock_version
/// let e3: ETag = String::from("v2").into_etag();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ETag {
    tag: String,
    weak: bool,
}

impl ETag {
    /// Construct a **strong** `ETag` from any tag string.
    ///
    /// Strong `ETag`s assert byte-for-byte equivalence.
    #[must_use]
    pub fn strong(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            weak: false,
        }
    }

    /// Construct a **weak** `ETag` from any tag string.
    ///
    /// Weak `ETag`s (prefixed `W/`) assert semantic equivalence: same content,
    /// possibly different encoding. Use them when the response can vary by
    /// `Accept-Encoding` or when comparing bodies with minor byte differences.
    #[must_use]
    pub fn weak(tag: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            weak: true,
        }
    }

    /// Returns the opaque tag value (no quotes, no `W/` prefix).
    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Returns `true` if this is a weak `ETag`.
    #[must_use]
    pub const fn is_weak(&self) -> bool {
        self.weak
    }

    /// Formats this `ETag` as an HTTP header value.
    ///
    /// - Strong: `"<tag>"`
    /// - Weak: `W/"<tag>"`
    #[must_use]
    pub fn header_value(&self) -> HeaderValue {
        let formatted = if self.weak {
            format!("W/\"{}\"", self.tag)
        } else {
            format!("\"{}\"", self.tag)
        };
        HeaderValue::from_str(&formatted).unwrap_or_else(|_| HeaderValue::from_static(""))
    }

    /// Returns `true` if the given raw `If-None-Match` header value matches
    /// this `ETag` according to RFC 7232 §3.2 weak comparison.
    ///
    /// `*` matches any `ETag`. Both strong and weak `ETag`s are compared by
    /// their opaque tag string.
    fn matches_if_none_match(&self, if_none_match: &str) -> bool {
        let if_none_match = if_none_match.trim();
        if if_none_match == "*" {
            return true;
        }
        for candidate in if_none_match.split(',') {
            let candidate = candidate.trim();
            // Strip W/ prefix then quotes for weak comparison.
            let tag = candidate
                .strip_prefix("W/")
                .unwrap_or(candidate)
                .trim_matches('"');
            if tag == self.tag {
                return true;
            }
        }
        false
    }
}

// ── IntoETag trait ────────────────────────────────────────────────────────────

/// Conversion trait for types that can serve as an `ETag` source.
///
/// Blanket implementations are provided for:
///
/// | Input type | `ETag` derivation |
/// |------------|-----------------|
/// | `String` / `&str` | SHA-256 of the string bytes (strong) |
/// | `i64` | SHA-256 of the integer (strong) — suitable for `#[lock_version]` |
/// | `(NaiveDateTime, i64)` | SHA-256 of `updated_at` + `lock_version` (strong) |
/// | Any `impl Hash` via [`hash_etag`] | `SipHash` → hex (weak) |
/// | `ETag` | identity |
pub trait IntoETag {
    /// Convert `self` into an [`ETag`].
    fn into_etag(self) -> ETag;
}

impl IntoETag for ETag {
    fn into_etag(self) -> ETag {
        self
    }
}

impl IntoETag for String {
    fn into_etag(self) -> ETag {
        ETag::strong(sha256_hex(self.as_bytes()))
    }
}

impl IntoETag for &str {
    fn into_etag(self) -> ETag {
        ETag::strong(sha256_hex(self.as_bytes()))
    }
}

impl IntoETag for i64 {
    fn into_etag(self) -> ETag {
        ETag::strong(sha256_hex(&self.to_be_bytes()))
    }
}

impl IntoETag for i32 {
    fn into_etag(self) -> ETag {
        i64::from(self).into_etag()
    }
}

/// `(updated_at, lock_version)` tuple — the idiomatic combo when a model has
/// both a timestamp and an optimistic-lock version.
impl IntoETag for (chrono::NaiveDateTime, i64) {
    fn into_etag(self) -> ETag {
        let mut hasher = sha2::Sha256::new();
        hasher.update(self.0.and_utc().timestamp().to_be_bytes());
        hasher.update(self.0.and_utc().timestamp_subsec_nanos().to_be_bytes());
        hasher.update(self.1.to_be_bytes());
        ETag::strong(hex_lower(hasher.finalize()))
    }
}

impl IntoETag for (chrono::NaiveDateTime, i32) {
    fn into_etag(self) -> ETag {
        (self.0, i64::from(self.1)).into_etag()
    }
}

/// Derive a **weak** `ETag` from any [`Hash`] value.
///
/// Uses `SipHash` so this is cheap but NOT cryptographically strong.
/// Suitable for response-body hashing in [`EtagLayer`].
///
/// # Example
///
/// ```rust
/// use autumn_web::etag::{hash_etag, IntoETag};
///
/// let etag = hash_etag(&vec![1u8, 2, 3]);
/// assert!(etag.is_weak());
/// ```
#[must_use]
pub fn hash_etag<T: Hash>(value: &T) -> ETag {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    ETag::weak(format!("{:016x}", hasher.finish()))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(sha2::Sha256::digest(bytes))
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    out.extend(bytes.iter().flat_map(|b| {
        [
            HEX[(b >> 4) as usize] as char,
            HEX[(b & 0xf) as usize] as char,
        ]
    }));
    out
}

// ── FreshWhen result ──────────────────────────────────────────────────────────

/// The outcome of a conditional-GET check.
///
/// Produced by [`fresh_when`]. Call `.or(response)` to either return a `304`
/// (if the resource is fresh) or the full response with `ETag` and
/// `Last-Modified` headers applied (if the resource is stale).
///
/// ```rust
/// # use autumn_web::etag::fresh_when;
/// # use http::{HeaderMap, StatusCode};
/// # use axum::response::IntoResponse;
/// let headers = HeaderMap::new();
/// let response = fresh_when(&headers, "v1").or(StatusCode::OK);
/// ```
#[must_use = "call `.or(response)` to resolve the conditional-GET result"]
pub struct FreshWhen {
    etag: ETag,
    last_modified: Option<chrono::DateTime<chrono::Utc>>,
    is_fresh: bool,
    /// Whether the request carried an `If-None-Match` header.
    /// Per RFC 7232 §3.3, `If-Modified-Since` MUST be ignored when this is true.
    if_none_match_present: bool,
    /// Raw `If-Modified-Since` header value from the request, stored for
    /// deferred evaluation in [`FreshWhen::last_modified`].
    raw_if_modified_since: Option<String>,
}

impl FreshWhen {
    /// `true` if the conditional check matched — the resource has not changed
    /// since the client's cached copy.
    #[must_use]
    pub const fn is_fresh(&self) -> bool {
        self.is_fresh
    }

    /// Attach a `Last-Modified` timestamp to the result.
    ///
    /// When `is_fresh`, the timestamp is included in the `304` response
    /// headers. When stale, it is included in the `200` response headers.
    ///
    /// If the request contained an `If-Modified-Since` header but **no**
    /// `If-None-Match` header (RFC 7232 §3.3), this method re-evaluates
    /// freshness: the resource is considered fresh when `dt ≤ If-Modified-Since`.
    pub fn last_modified(mut self, dt: impl Into<Option<chrono::DateTime<chrono::Utc>>>) -> Self {
        self.last_modified = dt.into();
        // RFC 7232 §3.3: evaluate IMS only when INM is absent.
        if !self.if_none_match_present
            && let Some(ref ims_str) = self.raw_if_modified_since
            && let Some(lm) = self.last_modified
        {
            // Truncate to whole seconds: http_date() emits second-level precision,
            // so a sub-second lm would compare as earlier than the IMS round-trip
            // value and yield a spurious stale result.
            let lm_secs = chrono::DateTime::from_timestamp(lm.timestamp(), 0).unwrap_or(lm);
            self.is_fresh = parse_http_date(ims_str)
                .is_some_and(|parsed| std::time::SystemTime::from(lm_secs) <= parsed);
        }
        self
    }

    /// Resolve to an HTTP response (eager).
    ///
    /// `response` is **always evaluated** before this method is called — Rust
    /// evaluates arguments eagerly. Use [`FreshWhen::or_else`] with a closure
    /// when building the response is expensive and should be skipped on fresh hits.
    ///
    /// - **Fresh**: returns `304 Not Modified` with `ETag` / `Last-Modified`
    ///   preserved, `Set-Cookie` stripped, and `Cache-Control` / `Vary` /
    ///   `Content-Location` / `Date` / `Expires` copied from `response`.
    /// - **Stale**: returns `response` with `ETag` and `Last-Modified` injected.
    pub fn or(self, response: impl IntoResponse) -> impl IntoResponse {
        let r = response.into_response();
        if self.is_fresh {
            let mut not_modified = not_modified_response(&self.etag, self.last_modified);
            // Copy RFC 7232 §4.1 preservation headers from the built response.
            for name in [CACHE_CONTROL, VARY, CONTENT_LOCATION, DATE, EXPIRES] {
                for v in r.headers().get_all(&name) {
                    not_modified.headers_mut().append(name.clone(), v.clone());
                }
            }
            // Copy Last-Modified from the response only when not already set via
            // the .last_modified() builder (to avoid duplicate values).
            if self.last_modified.is_none() {
                for v in r.headers().get_all(LAST_MODIFIED) {
                    not_modified.headers_mut().append(LAST_MODIFIED, v.clone());
                }
            }
            not_modified
        } else {
            let mut r = r;
            r.headers_mut().insert(ETAG, self.etag.header_value());
            if let Some(lm) = self.last_modified
                && let Ok(v) = HeaderValue::from_str(&http_date(lm))
            {
                r.headers_mut().insert(LAST_MODIFIED, v);
            }
            r
        }
    }

    /// Resolve to an HTTP response (lazy).
    ///
    /// `f` is called **only when the resource is stale**, so expensive rendering
    /// or serialization is skipped entirely on fresh hits. The `304` response
    /// is returned directly without evaluating the closure.
    ///
    /// Cache headers (`Cache-Control`, `Vary`, etc.) are **not** copied to the
    /// `304` because the response is never built for fresh hits. If you need
    /// those headers preserved on revalidations, either use [`FreshWhen::or`]
    /// or set them via a middleware layer.
    pub fn or_else<R: IntoResponse, F: FnOnce() -> R>(self, f: F) -> impl IntoResponse {
        if self.is_fresh {
            not_modified_response(&self.etag, self.last_modified)
        } else {
            let mut r = f().into_response();
            r.headers_mut().insert(ETAG, self.etag.header_value());
            if let Some(lm) = self.last_modified
                && let Ok(v) = HeaderValue::from_str(&http_date(lm))
            {
                r.headers_mut().insert(LAST_MODIFIED, v);
            }
            r
        }
    }
}

// ── fresh_when ────────────────────────────────────────────────────────────────

/// Check whether the client's cached copy of a resource is still fresh.
///
/// Returns a [`FreshWhen`] that you resolve with `.or(response)`.
///
/// # Conditional check logic
///
/// The resource is considered **fresh** (→ `304`) when:
///
/// 1. The request carries an `If-None-Match` header **and** the `ETag` matches
///    (weak comparison per RFC 7232 §3.2), **or**
/// 2. The request carries an `If-Modified-Since` header and
///    `last_modified` ≤ that timestamp (when no `If-None-Match` is present).
///
/// If neither conditional header is present, the resource is always stale.
///
/// # `ETag` determinism
///
/// `ETag` derivation never touches process-local state, RNGs, or the wall
/// clock — same inputs always produce the same `ETag` on every replica.
///
/// # Example
///
/// ```rust
/// use autumn_web::etag::fresh_when;
/// use http::{HeaderMap, HeaderValue, StatusCode};
/// use axum::response::IntoResponse;
///
/// // Simulate a client that already has the "v1" ETag cached.
/// let mut headers = HeaderMap::new();
/// headers.insert("if-none-match", HeaderValue::from_static("\"v1\""));
///
/// // "v1".into_etag() produces a SHA-256 of "v1".
/// // The fresh_when call hashes "v1" to match the header exactly — so we
/// // need the pre-computed hash for the test below. In real code you'd
/// // store and re-derive the ETag from stable model state.
/// let _r = fresh_when(&headers, 42_i64);
/// ```
pub fn fresh_when<E: IntoETag>(request_headers: &HeaderMap, etag: E) -> FreshWhen {
    let etag = etag.into_etag();
    let if_none_match_present = request_headers.contains_key(IF_NONE_MATCH);
    // RFC 7232 §3.3: evaluate INM first; IMS is deferred to last_modified().
    let is_fresh = if_none_match_present && check_if_none_match(request_headers, &etag);
    // Store the raw IMS header only when INM is absent; per RFC 7232 §3.3
    // IMS MUST be ignored when INM is present.
    let raw_if_modified_since = if if_none_match_present {
        None
    } else {
        request_headers
            .get(IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    FreshWhen {
        etag,
        last_modified: None,
        is_fresh,
        if_none_match_present,
        raw_if_modified_since,
    }
}

fn check_if_none_match(headers: &HeaderMap, etag: &ETag) -> bool {
    // Iterate all If-None-Match header fields (there may be more than one).
    headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .any(|v| v.to_str().is_ok_and(|s| etag.matches_if_none_match(s)))
}

fn not_modified_response(
    etag: &ETag,
    last_modified: Option<chrono::DateTime<chrono::Utc>>,
) -> Response<Body> {
    let mut builder = Response::builder().status(StatusCode::NOT_MODIFIED);
    let headers = builder.headers_mut().expect("builder not consumed");
    headers.insert(ETAG, etag.header_value());
    if let Some(lm) = last_modified
        && let Ok(v) = HeaderValue::from_str(&http_date(lm))
    {
        headers.insert(LAST_MODIFIED, v);
    }
    builder
        .body(Body::empty())
        .expect("304 body is always valid")
}

fn http_date(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn parse_http_date(s: &str) -> Option<std::time::SystemTime> {
    // Try RFC 7231 / RFC 1123 format: "Tue, 15 Nov 1994 08:12:31 GMT"
    chrono::DateTime::parse_from_rfc2822(s)
        .map(|dt| std::time::SystemTime::from(dt.with_timezone(&chrono::Utc)))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(
                s.trim_end_matches(" GMT"),
                "%a, %d %b %Y %H:%M:%S",
            )
            .map(|ndt| {
                std::time::SystemTime::from(
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ndt, chrono::Utc),
                )
            })
        })
        .ok()
}

// ── Cache-Control freshness ─────────────────────────────────────────────────

/// Cache visibility scope for a [`CacheControl`] directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    /// `public` — any cache (browser **and** shared/CDN) may store the response.
    Public,
    /// `private` — only the end-client's browser cache may store it.
    Private,
}

/// A declarative builder for the `Cache-Control` response header.
///
/// `CacheControl` is the freshness companion to [`fresh_when`]: where
/// `fresh_when` decides whether a cached copy is still *valid* (revalidation),
/// `Cache-Control` tells caches how long a copy stays *fresh* before they need
/// to revalidate at all. Build one with [`cache_for`] and attach it to any
/// response.
///
/// # Two ways to attach it
///
/// It implements [`IntoResponseParts`], so it composes as a tuple with any
/// [`IntoResponse`] body — the idiomatic axum form:
///
/// ```rust
/// use autumn_web::etag::cache_for;
/// use axum::response::IntoResponse;
/// use std::time::Duration;
///
/// # fn markup() -> &'static str { "<h1>hi</h1>" }
/// # fn handler() -> impl IntoResponse {
/// (cache_for(Duration::from_secs(60)).public(), markup())
/// # }
/// ```
///
/// Or call [`CacheControl::wrap`] for a single expression:
///
/// ```rust
/// use autumn_web::etag::cache_for;
/// use std::time::Duration;
///
/// # fn markup() -> &'static str { "<h1>hi</h1>" }
/// let response = cache_for(Duration::from_secs(60)).public().wrap(markup());
/// ```
///
/// Either way the `Cache-Control` header is **inserted** (replacing any prior
/// value), so a response never ends up with two conflicting `Cache-Control`
/// values.
///
/// # `max-age` vs `s-maxage`
///
/// - [`max_age`](CacheControl::max_age) (`max-age=N`) is the freshness lifetime
///   for **every** cache, including the end-user's browser.
/// - [`s_maxage`](CacheControl::s_maxage) (`s-maxage=N`) overrides `max-age`
///   **only for shared caches** (CDNs, reverse proxies). Use it to let a CDN
///   hold a page longer than the browser does — e.g. `max-age=60, s-maxage=600`
///   keeps the browser copy fresh for a minute but lets the CDN serve it for ten.
///
/// # `Vary` and personalized responses
///
/// A `public` response may be served by a **shared** cache to *any* client, so
/// never mark a page that embeds per-user state (a signed-in username, a
/// cart, CSRF-bound markup) `public` without also emitting a matching `Vary`
/// header (e.g. `Vary: Cookie`) so the shared cache keys separate variants per
/// user. When in doubt keep the response [`private`](CacheControl::private) or
/// [`no_store`](CacheControl::no_store) — a shared cache must never hand one
/// user's personalized page to another.
///
/// # Default-private safety
///
/// [`cache_for`] defaults to **`private`**. `public` is an explicit opt-in via
/// [`CacheControl::public`]. This means dropping `cache_for(..)` onto a
/// secured / authenticated handler can never *silently* publish that page to a
/// shared cache — you have to ask for `public` on purpose.
#[derive(Debug, Clone)]
#[must_use = "attach the CacheControl to a response via a tuple or `.wrap(..)`"]
#[allow(clippy::struct_excessive_bools)] // independent, orthogonal Cache-Control directive toggles
pub struct CacheControl {
    visibility: Option<Visibility>,
    max_age: Option<Duration>,
    s_maxage: Option<Duration>,
    stale_while_revalidate: Option<Duration>,
    no_store: bool,
    no_cache: bool,
    must_revalidate: bool,
    immutable: bool,
}

/// Start a `Cache-Control` freshness directive with a `max-age` lifetime.
///
/// The returned [`CacheControl`] defaults to **`private`** (browser-only) — see
/// [`CacheControl`] for the default-private safety rule, the `max-age` vs
/// `s-maxage` distinction, and the `Vary` interaction. Call
/// [`public`](CacheControl::public) to opt in to shared/CDN caching.
///
/// ```rust
/// use autumn_web::etag::cache_for;
/// use std::time::Duration;
///
/// // "private, max-age=60"
/// let cc = cache_for(Duration::from_secs(60));
/// assert_eq!(cc.header_value(), "private, max-age=60");
/// ```
pub const fn cache_for(ttl: Duration) -> CacheControl {
    CacheControl {
        visibility: Some(Visibility::Private),
        max_age: Some(ttl),
        s_maxage: None,
        stale_while_revalidate: None,
        no_store: false,
        no_cache: false,
        must_revalidate: false,
        immutable: false,
    }
}

impl CacheControl {
    /// Mark the response cacheable by **shared** caches (CDNs, proxies) as well
    /// as the browser (`public`). Explicit opt-in — see the default-private
    /// safety rule on [`CacheControl`]. Only mark a personalized page `public`
    /// alongside a matching `Vary` header.
    pub const fn public(mut self) -> Self {
        self.visibility = Some(Visibility::Public);
        self
    }

    /// Restrict the response to the end-client's browser cache (`private`).
    /// This is the default for [`cache_for`].
    pub const fn private(mut self) -> Self {
        self.visibility = Some(Visibility::Private);
        self
    }

    /// Set the `max-age=N` freshness lifetime (applies to every cache,
    /// including the browser). Rendered as whole seconds.
    pub const fn max_age(mut self, d: Duration) -> Self {
        self.max_age = Some(d);
        self
    }

    /// Set the `s-maxage=N` freshness lifetime for **shared** caches only
    /// (CDNs, reverse proxies); overrides `max-age` there. Rendered as whole
    /// seconds.
    pub const fn s_maxage(mut self, d: Duration) -> Self {
        self.s_maxage = Some(d);
        self
    }

    /// Set `stale-while-revalidate=N` — a shared cache may serve the stale
    /// copy for up to `N` seconds while it revalidates in the background.
    /// Rendered as whole seconds.
    pub const fn stale_while_revalidate(mut self, d: Duration) -> Self {
        self.stale_while_revalidate = Some(d);
        self
    }

    /// Emit exactly `no-store` — forbid any cache from storing the response.
    /// Overrides every freshness directive (`max-age`, `s-maxage`,
    /// `stale-while-revalidate`, visibility) in [`header_value`](Self::header_value).
    pub const fn no_store(mut self) -> Self {
        self.no_store = true;
        self
    }

    /// Add `no-cache` — a cache may store the response but must revalidate with
    /// the origin before reusing it.
    pub const fn no_cache(mut self) -> Self {
        self.no_cache = true;
        self
    }

    /// Add `must-revalidate` — once stale, a cache MUST revalidate and MUST NOT
    /// serve the stale copy on origin failure.
    pub const fn must_revalidate(mut self) -> Self {
        self.must_revalidate = true;
        self
    }

    /// Add `immutable` — the response body will not change over its freshness
    /// lifetime, so clients skip revalidation even on reload.
    pub const fn immutable(mut self) -> Self {
        self.immutable = true;
        self
    }

    /// Render the byte-for-byte `Cache-Control` header value.
    ///
    /// If [`no_store`](Self::no_store) is set the value is exactly `"no-store"`
    /// and every freshness directive is ignored. Otherwise directives are
    /// joined, comma+space separated, in this fixed deterministic order:
    /// visibility (`public`/`private`), `no-cache`, `max-age=N`, `s-maxage=N`,
    /// `stale-while-revalidate=N`, `must-revalidate`, `immutable`. Durations
    /// render as whole seconds.
    ///
    /// ```rust
    /// use autumn_web::etag::cache_for;
    /// use std::time::Duration;
    ///
    /// let v = cache_for(Duration::from_secs(60))
    ///     .public()
    ///     .s_maxage(Duration::from_secs(120))
    ///     .stale_while_revalidate(Duration::from_secs(30))
    ///     .header_value();
    /// assert_eq!(v, "public, max-age=60, s-maxage=120, stale-while-revalidate=30");
    /// ```
    #[must_use]
    pub fn header_value(&self) -> String {
        if self.no_store {
            return "no-store".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        match self.visibility {
            Some(Visibility::Public) => parts.push("public".to_string()),
            Some(Visibility::Private) => parts.push("private".to_string()),
            None => {}
        }
        if self.no_cache {
            parts.push("no-cache".to_string());
        }
        if let Some(d) = self.max_age {
            parts.push(format!("max-age={}", d.as_secs()));
        }
        if let Some(d) = self.s_maxage {
            parts.push(format!("s-maxage={}", d.as_secs()));
        }
        if let Some(d) = self.stale_while_revalidate {
            parts.push(format!("stale-while-revalidate={}", d.as_secs()));
        }
        if self.must_revalidate {
            parts.push("must-revalidate".to_string());
        }
        if self.immutable {
            parts.push("immutable".to_string());
        }
        parts.join(", ")
    }

    /// Insert the rendered `Cache-Control` header into `headers`, replacing any
    /// pre-existing value so exactly one `Cache-Control` remains.
    fn apply_to(&self, headers: &mut HeaderMap) {
        if let Ok(v) = HeaderValue::from_str(&self.header_value()) {
            headers.insert(CACHE_CONTROL, v);
        }
    }

    /// Attach the `Cache-Control` header to `response` and return it.
    ///
    /// The header is **inserted** (any pre-existing `Cache-Control` is
    /// replaced), so the response carries exactly one `Cache-Control` value.
    /// Prefer the tuple form `(cache_for(..).public(), body)` when you can; use
    /// `wrap` when a single expression is more convenient.
    #[must_use]
    pub fn wrap(self, response: impl IntoResponse) -> AxumResponse {
        let mut r = response.into_response();
        self.apply_to(r.headers_mut());
        r
    }
}

impl IntoResponseParts for CacheControl {
    type Error = std::convert::Infallible;

    fn into_response_parts(self, mut res: ResponseParts) -> Result<ResponseParts, Self::Error> {
        self.apply_to(res.headers_mut());
        Ok(res)
    }
}

// ── EtagLayer ─────────────────────────────────────────────────────────────────

/// Tower middleware that auto-derives a **weak** `ETag` from the response body.
///
/// Off by default. Add it to a route or router when you want transparent
/// `ETag` support without modifying the handler:
///
/// ```rust,no_run
/// use autumn_web::etag::EtagLayer;
/// use axum::Router;
/// use tower::ServiceBuilder;
///
/// let app: Router = Router::new()
///     // ... routes ...
///     .layer(EtagLayer::new());
/// ```
///
/// # Behaviour
///
/// For every `GET` request that produces a `200 OK` response:
///
/// 1. The response body is buffered (up to [`EtagLayer::MAX_BODY_BYTES`]).
/// 2. A weak `ETag` is derived from the body bytes via `SipHash`.
/// 3. If the request carried a matching `If-None-Match`, a `304` is returned.
/// 4. Otherwise, the `ETag` header is injected and the original response is
///    returned unchanged.
///
/// Responses that already carry an `ETag` header are passed through without
/// modification (handler-set `ETag`s take priority).
#[derive(Clone, Debug, Default)]
pub struct EtagLayer;

impl EtagLayer {
    /// Maximum body size (bytes) that will be buffered for `ETag` computation.
    /// Responses larger than this are passed through unchanged (no `ETag`).
    pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

    /// Create a new `EtagLayer`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for EtagLayer {
    type Service = EtagService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        EtagService { inner }
    }
}

/// The [`Service`] produced by [`EtagLayer`].
#[derive(Clone)]
pub struct EtagService<S> {
    inner: S,
}

impl<S, ReqBody> Service<http::Request<ReqBody>> for EtagService<S>
where
    S: Service<http::Request<ReqBody>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        // Collect all If-None-Match header fields and join them so that a match
        // in any field is honoured (multiple fields are equivalent to one
        // comma-separated list per RFC 7230 §3.2.2).
        let if_none_match: Option<String> = {
            let vals: Vec<&str> = req
                .headers()
                .get_all(IF_NONE_MATCH)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect();
            if vals.is_empty() {
                None
            } else {
                Some(vals.join(", "))
            }
        };

        let is_get = req.method() == http::Method::GET;
        let fut = self.inner.call(req);

        Box::pin(async move {
            let response = fut.await?;
            if !is_get || response.status() != StatusCode::OK {
                return Ok(response);
            }
            Ok(apply_etag(response, if_none_match.as_deref()).await)
        })
    }
}

/// Copy RFC 7232 §4.1 preservation headers from `src` to a 304 `dst`.
fn copy_304_headers(src: &http::HeaderMap, dst: &mut Response<Body>) {
    for name in [
        CACHE_CONTROL,
        VARY,
        CONTENT_LOCATION,
        DATE,
        EXPIRES,
        LAST_MODIFIED,
    ] {
        for v in src.get_all(&name) {
            dst.headers_mut().append(name.clone(), v.clone());
        }
    }
}

/// Core `ETag` logic applied to a confirmed `GET 200` response.
async fn apply_etag(response: Response<Body>, if_none_match: Option<&str>) -> Response<Body> {
    // If the handler already set an ETag, check If-None-Match against it
    // before buffering the body.
    if let Some(existing_etag) = response.headers().get(ETAG).cloned() {
        if let Some(inm) = if_none_match {
            let existing_tag = existing_etag.to_str().unwrap_or("");
            let tag = existing_tag
                .strip_prefix("W/")
                .unwrap_or(existing_tag)
                .trim_matches('"');
            let candidate_etag = ETag::strong(tag.to_owned());
            if candidate_etag.matches_if_none_match(inm) {
                let (parts, _body) = response.into_parts();
                let mut not_modified = not_modified_response(&candidate_etag, None);
                copy_304_headers(&parts.headers, &mut not_modified);
                not_modified.headers_mut().remove(SET_COOKIE);
                not_modified.headers_mut().insert(ETAG, existing_etag);
                return not_modified;
            }
        }
        return response;
    }

    let (mut parts, mut body) = response.into_parts();

    // Content-Length upfront guard: skip ETag on known-large bodies.
    if parts
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .is_some_and(|len| len > EtagLayer::MAX_BODY_BYTES)
    {
        return Response::from_parts(parts, body);
    }

    // Manual bounded collection with overflow-safe reconstruction.
    let mut buf = bytes::BytesMut::new();
    let mut overflow_frame: Option<http_body::Frame<bytes::Bytes>> = None;
    let mut stream_errored = false;

    loop {
        match BodyExt::frame(&mut body).await {
            None => break,
            Some(Err(_)) => {
                stream_errored = true;
                break;
            }
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => {
                    if buf.len() + data.len() > EtagLayer::MAX_BODY_BYTES {
                        overflow_frame = Some(http_body::Frame::data(data));
                        break;
                    }
                    buf.extend_from_slice(&data);
                }
                Err(non_data) => {
                    // Trailer or unknown frame — stop buffering and reconstruct
                    // so the non-data frame is forwarded rather than dropped.
                    overflow_frame = Some(non_data);
                    break;
                }
            },
        }
    }

    if stream_errored {
        // Surface the error so clients/caches see a truncated response rather
        // than silently-complete partial data.  Deliver any bytes already
        // buffered, then inject an error frame to close the body abnormally.
        let frozen = buf.freeze();
        let err = axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "upstream body error during ETag buffering",
        ));
        let frames: Vec<Result<bytes::Bytes, axum::Error>> = if frozen.is_empty() {
            vec![Err(err)]
        } else {
            vec![Ok(frozen), Err(err)]
        };
        return Response::from_parts(parts, Body::from_stream(futures::stream::iter(frames)));
    }
    if let Some(overflow) = overflow_frame {
        return Response::from_parts(parts, rebuild_oversized_body(buf.freeze(), overflow, body));
    }

    let bytes = buf.freeze();
    let etag = {
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        ETag::weak(format!("{:016x}", hasher.finish()))
    };

    if if_none_match.is_some_and(|inm| etag.matches_if_none_match(inm)) {
        let mut not_modified = not_modified_response(&etag, None);
        copy_304_headers(&parts.headers, &mut not_modified);
        not_modified.headers_mut().remove(SET_COOKIE);
        return not_modified;
    }

    parts.headers.insert(ETAG, etag.header_value());
    Response::from_parts(parts, Body::from(bytes))
}

// ── body helpers ──────────────────────────────────────────────────────────────

/// Reconstruct a streaming `Body` from bytes already consumed from it plus its
/// remaining frames, used by `EtagLayer` when the body exceeds `MAX_BODY_BYTES`.
///
/// Avoids silently replacing large streamed responses with an empty body by
/// chaining: `prefix` (frames collected before the limit was hit) +
/// `overflow` (the frame that tripped the limit) + `remaining` (the rest of
/// the original stream).
///
/// Uses [`http_body_util::StreamBody`] so that trailer frames are forwarded
/// rather than discarded (unlike [`Body::from_stream`]), and stream errors are
/// propagated rather than silently terminating the stream.
fn rebuild_oversized_body(
    prefix: bytes::Bytes,
    overflow: http_body::Frame<bytes::Bytes>,
    remaining: Body,
) -> Body {
    use http_body_util::StreamBody;

    let preamble = futures::stream::iter([
        Ok::<http_body::Frame<bytes::Bytes>, axum::Error>(http_body::Frame::data(prefix)),
        Ok(overflow),
    ]);
    let tail = futures::stream::unfold(remaining, |mut b| async move {
        BodyExt::frame(&mut b).await.map(|result| (result, b))
    });
    Body::new(StreamBody::new(preamble.chain(tail)))
}

// ── not_modified helpers ───────────────────────────────────────────────────────

/// Build a minimal `304 Not Modified` response.
///
/// Preserves the headers RFC 7232 §4.1 requires intermediaries to pass through:
/// `Cache-Control`, `Content-Location`, `Date`, `ETag`, `Expires`,
/// `Last-Modified`, `Vary`.
///
/// `Set-Cookie` is **stripped by default** to prevent stale auth tokens from
/// being replayed by shared caches.
#[must_use]
pub fn build_not_modified(
    original_headers: &HeaderMap,
    etag: &ETag,
    last_modified: Option<chrono::DateTime<chrono::Utc>>,
) -> Response<Body> {
    let mut response = not_modified_response(etag, last_modified);
    for name in [CACHE_CONTROL, VARY, CONTENT_LOCATION, DATE, EXPIRES] {
        for v in original_headers.get_all(&name) {
            response.headers_mut().append(name.clone(), v.clone());
        }
    }
    // Copy Last-Modified from original_headers only when not already set
    // by the explicit `last_modified` argument (avoids conflicting duplicates).
    if last_modified.is_none() {
        for v in original_headers.get_all(LAST_MODIFIED) {
            response.headers_mut().append(LAST_MODIFIED, v.clone());
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
    use tower::ServiceExt;

    // ── RED: ETag type ────────────────────────────────────────────────────────

    #[test]
    fn strong_etag_header_value_has_quotes() {
        let etag = ETag::strong("abc123");
        assert_eq!(etag.header_value().to_str().unwrap(), r#""abc123""#);
    }

    #[test]
    fn weak_etag_header_value_has_w_prefix() {
        let etag = ETag::weak("abc123");
        assert_eq!(etag.header_value().to_str().unwrap(), r#"W/"abc123""#);
    }

    #[test]
    fn etag_is_not_weak_by_default_strong_constructor() {
        let etag = ETag::strong("x");
        assert!(!etag.is_weak());
    }

    #[test]
    fn weak_etag_is_weak() {
        let etag = ETag::weak("x");
        assert!(etag.is_weak());
    }

    // ── RED: IntoETag conversions ─────────────────────────────────────────────

    #[test]
    fn str_into_etag_produces_deterministic_strong_etag() {
        let e1: ETag = "hello".into_etag();
        let e2: ETag = "hello".into_etag();
        assert_eq!(e1, e2);
        assert!(!e1.is_weak());
    }

    #[test]
    fn different_strings_produce_different_etags() {
        let e1: ETag = "hello".into_etag();
        let e2: ETag = "world".into_etag();
        assert_ne!(e1, e2);
    }

    #[test]
    fn string_into_etag_same_as_str() {
        let e1: ETag = "hello".into_etag();
        let e2: ETag = String::from("hello").into_etag();
        assert_eq!(e1, e2);
    }

    #[test]
    fn i64_into_etag_is_deterministic() {
        let e1: ETag = 42_i64.into_etag();
        let e2: ETag = 42_i64.into_etag();
        assert_eq!(e1, e2);
        assert!(!e1.is_weak());
    }

    #[test]
    fn different_i64_values_produce_different_etags() {
        let e1: ETag = 1_i64.into_etag();
        let e2: ETag = 2_i64.into_etag();
        assert_ne!(e1, e2);
    }

    #[test]
    fn i32_into_etag_matches_equivalent_i64() {
        let e1: ETag = 7_i32.into_etag();
        let e2: ETag = 7_i64.into_etag();
        assert_eq!(e1, e2);
    }

    #[test]
    fn tuple_into_etag_is_deterministic() {
        let dt = chrono::DateTime::from_timestamp(1_000_000, 0)
            .unwrap()
            .naive_utc();
        let e1: ETag = (dt, 3_i64).into_etag();
        let e2: ETag = (dt, 3_i64).into_etag();
        assert_eq!(e1, e2);
        assert!(!e1.is_weak());
    }

    #[test]
    fn tuple_etag_differs_when_lock_version_differs() {
        let dt = chrono::DateTime::from_timestamp(1_000_000, 0)
            .unwrap()
            .naive_utc();
        let e1: ETag = (dt, 1_i64).into_etag();
        let e2: ETag = (dt, 2_i64).into_etag();
        assert_ne!(e1, e2);
    }

    #[test]
    fn tuple_etag_differs_when_timestamp_differs() {
        let dt1 = chrono::DateTime::from_timestamp(1_000_000, 0)
            .unwrap()
            .naive_utc();
        let dt2 = chrono::DateTime::from_timestamp(1_000_001, 0)
            .unwrap()
            .naive_utc();
        let e1: ETag = (dt1, 1_i64).into_etag();
        let e2: ETag = (dt2, 1_i64).into_etag();
        assert_ne!(e1, e2);
    }

    #[test]
    fn hash_etag_is_weak() {
        let etag = hash_etag(&vec![1u8, 2, 3]);
        assert!(etag.is_weak());
    }

    #[test]
    fn hash_etag_is_deterministic_for_same_input() {
        let etag1 = hash_etag(&"stable_value");
        let etag2 = hash_etag(&"stable_value");
        assert_eq!(etag1, etag2);
    }

    // ── RED: ETag matching ────────────────────────────────────────────────────

    #[test]
    fn etag_matches_exact_quoted_value() {
        let etag = ETag::strong("abc");
        assert!(etag.matches_if_none_match(r#""abc""#));
    }

    #[test]
    fn etag_matches_weak_variant_by_tag() {
        let etag = ETag::strong("abc");
        assert!(etag.matches_if_none_match(r#"W/"abc""#));
    }

    #[test]
    fn etag_matches_star_wildcard() {
        let etag = ETag::strong("anything");
        assert!(etag.matches_if_none_match("*"));
    }

    #[test]
    fn etag_does_not_match_different_value() {
        let etag = ETag::strong("abc");
        assert!(!etag.matches_if_none_match(r#""xyz""#));
    }

    #[test]
    fn etag_matches_one_of_many_in_list() {
        let etag = ETag::strong("abc");
        assert!(etag.matches_if_none_match(r#""xyz", "abc", "foo""#));
    }

    // ── RED: fresh_when core behaviour ───────────────────────────────────────

    #[test]
    fn fresh_when_returns_stale_with_no_headers() {
        let headers = HeaderMap::new();
        let result = fresh_when(&headers, 1_i64);
        assert!(!result.is_fresh());
    }

    #[test]
    fn fresh_when_returns_fresh_on_matching_if_none_match() {
        let etag: ETag = 42_i64.into_etag();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.header_value());

        let result = fresh_when(&headers, 42_i64);
        assert!(result.is_fresh());
    }

    #[test]
    fn fresh_when_returns_stale_on_different_etag() {
        let etag: ETag = 1_i64.into_etag();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.header_value());

        // Resource changed — lock_version is now 2.
        let result = fresh_when(&headers, 2_i64);
        assert!(!result.is_fresh());
    }

    #[test]
    fn fresh_when_or_returns_304_when_fresh() {
        let etag: ETag = 7_i64.into_etag();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.header_value());

        let response = fresh_when(&headers, 7_i64)
            .or(StatusCode::OK)
            .into_response();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn fresh_when_or_returns_200_and_sets_etag_when_stale() {
        let headers = HeaderMap::new(); // no If-None-Match
        let response = fresh_when(&headers, 1_i64)
            .or(StatusCode::OK)
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let etag_header = response.headers().get(ETAG);
        assert!(
            etag_header.is_some(),
            "ETag header must be set on stale response"
        );
    }

    #[test]
    fn fresh_when_304_has_empty_body() {
        use http_body_util::BodyExt;

        let etag: ETag = 5_i64.into_etag();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.header_value());

        let response = fresh_when(&headers, 5_i64)
            .or(StatusCode::OK)
            .into_response();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

        // Body must be empty.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bytes = rt.block_on(async { response.into_body().collect().await.unwrap().to_bytes() });
        assert!(bytes.is_empty(), "304 body must be empty, got {bytes:?}");
    }

    #[test]
    fn fresh_when_or_includes_etag_in_304_headers() {
        let etag: ETag = 3_i64.into_etag();
        let etag_val = etag.header_value();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag_val.clone());

        let response = fresh_when(&headers, 3_i64)
            .or(StatusCode::OK)
            .into_response();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers().get(ETAG), Some(&etag_val));
    }

    #[test]
    fn fresh_when_wildcard_if_none_match_returns_fresh() {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));

        let result = fresh_when(&headers, "anything");
        assert!(result.is_fresh());
    }

    // ── RED: last_modified support ────────────────────────────────────────────

    #[test]
    fn fresh_when_last_modified_sets_header_on_stale_response() {
        use chrono::TimeZone;

        let headers = HeaderMap::new();
        let last_modified = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();

        let response = fresh_when(&headers, 1_i64)
            .last_modified(last_modified)
            .or(StatusCode::OK)
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(LAST_MODIFIED));
    }

    #[test]
    fn fresh_when_last_modified_sets_header_on_304() {
        use chrono::TimeZone;

        let etag: ETag = 9_i64.into_etag();
        let last_modified = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.header_value());

        let response = fresh_when(&headers, 9_i64)
            .last_modified(last_modified)
            .or(StatusCode::OK)
            .into_response();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(response.headers().contains_key(LAST_MODIFIED));
    }

    // ── RED: If-Modified-Since fallback ───────────────────────────────────────

    #[test]
    fn fresh_when_if_modified_since_fresh_when_last_modified_not_newer() {
        use chrono::TimeZone;

        let last_modified = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        // IMS is one second after last_modified → resource not modified since → fresh.
        let ims_time = chrono::Utc.timestamp_opt(1_700_000_001, 0).unwrap();
        let ims_str = http_date(ims_time);

        let mut headers = HeaderMap::new();
        headers.insert(IF_MODIFIED_SINCE, HeaderValue::from_str(&ims_str).unwrap());

        // No INM → freshness is re-evaluated in last_modified() per RFC 7232 §3.3.
        let result = fresh_when(&headers, 1_i64).last_modified(last_modified);
        assert!(result.is_fresh(), "IMS >= last_modified → fresh (304)");
    }

    #[test]
    fn fresh_when_if_modified_since_stale_when_resource_newer_than_ims() {
        use chrono::TimeZone;

        let last_modified = chrono::Utc.timestamp_opt(1_700_000_002, 0).unwrap();
        // IMS is before last_modified → resource was modified after IMS → stale.
        let ims_time = chrono::Utc.timestamp_opt(1_700_000_001, 0).unwrap();
        let ims_str = http_date(ims_time);

        let mut headers = HeaderMap::new();
        headers.insert(IF_MODIFIED_SINCE, HeaderValue::from_str(&ims_str).unwrap());

        let result = fresh_when(&headers, 1_i64).last_modified(last_modified);
        assert!(!result.is_fresh(), "last_modified > IMS → stale (200)");
    }

    #[test]
    fn fresh_when_ignores_ims_when_inm_present_rfc7232_s3_3() {
        use chrono::TimeZone;

        let last_modified = chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let ims_time = chrono::Utc.timestamp_opt(1_700_000_001, 0).unwrap();
        let ims_str = http_date(ims_time);

        // INM is present but does NOT match (different etag).
        let wrong_etag: ETag = 99_i64.into_etag();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, wrong_etag.header_value());
        headers.insert(IF_MODIFIED_SINCE, HeaderValue::from_str(&ims_str).unwrap());

        // INM present → IMS MUST be ignored (RFC 7232 §3.3). INM doesn't match → stale.
        let result = fresh_when(&headers, 1_i64).last_modified(last_modified);
        assert!(
            !result.is_fresh(),
            "IMS must be ignored when INM is present per RFC 7232 §3.3"
        );
    }

    // ── RED: EtagLayer ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn etag_layer_adds_etag_to_get_200() {
        use tower::ServiceExt;

        let svc = EtagLayer::new().layer(tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("hello world"))
                    .unwrap(),
            )
        }));

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let response = svc.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().contains_key(ETAG),
            "EtagLayer must inject ETag header"
        );
    }

    #[tokio::test]
    async fn etag_layer_returns_304_on_matching_if_none_match() {
        let svc = EtagLayer::new().layer(tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("hello world"))
                    .unwrap(),
            )
        }));

        // First call to discover the ETag.
        let first_req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let first_response = svc.clone().oneshot(first_req).await.unwrap();
        let etag = first_response.headers().get(ETAG).unwrap().clone();

        // Second call with If-None-Match.
        let second_req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap();
        let second_response = svc.oneshot(second_req).await.unwrap();
        assert_eq!(second_response.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn etag_layer_does_not_add_etag_to_post() {
        let svc = EtagLayer::new().layer(tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("ok"))
                    .unwrap(),
            )
        }));

        let req = Request::builder()
            .method(Method::POST)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let response = svc.oneshot(req).await.unwrap();
        assert!(!response.headers().contains_key(ETAG));
    }

    #[tokio::test]
    async fn etag_layer_does_not_override_existing_etag() {
        let svc = EtagLayer::new().layer(tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(ETAG, r#""handler-set""#)
                    .body(Body::from("body"))
                    .unwrap(),
            )
        }));

        let req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let response = svc.oneshot(req).await.unwrap();
        assert_eq!(
            response.headers().get(ETAG).unwrap().to_str().unwrap(),
            r#""handler-set""#
        );
    }

    #[tokio::test]
    async fn etag_layer_preserves_cache_control_on_304() {
        let svc = EtagLayer::new().layer(tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(CACHE_CONTROL, "max-age=60")
                    .body(Body::from("stable content"))
                    .unwrap(),
            )
        }));

        let first_req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let first = svc.clone().oneshot(first_req).await.unwrap();
        let etag = first.headers().get(ETAG).unwrap().clone();

        let second_req = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap();
        let second = svc.oneshot(second_req).await.unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            second
                .headers()
                .get(CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap(),
            "max-age=60"
        );
    }

    #[tokio::test]
    async fn etag_layer_strips_set_cookie_from_304() {
        let svc = EtagLayer::new().layer(tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(SET_COOKIE, "session=abc; HttpOnly")
                    .body(Body::from("content"))
                    .unwrap(),
            )
        }));

        let first = svc
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = first.headers().get(ETAG).unwrap().clone();

        let second = svc
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/")
                    .header(IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert!(
            !second.headers().contains_key(SET_COOKIE),
            "Set-Cookie must be stripped from 304"
        );
    }

    // ── RED: determinism across replicas ──────────────────────────────────────

    #[test]
    fn etag_derivation_is_deterministic_no_rng_or_clock() {
        // Same inputs → same ETag. Calling multiple times must yield identical results.
        let e1: ETag = (42_i64).into_etag();
        let e2: ETag = (42_i64).into_etag();
        let e3: ETag = (42_i64).into_etag();
        assert_eq!(e1, e2);
        assert_eq!(e2, e3);
    }

    // ── RED: build_not_modified helper ────────────────────────────────────────

    #[test]
    fn build_not_modified_preserves_cache_control_and_vary() {
        let mut orig = HeaderMap::new();
        orig.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        orig.insert(VARY, HeaderValue::from_static("Accept"));
        orig.insert(SET_COOKIE, HeaderValue::from_static("tok=x"));

        let etag = ETag::strong("tag");
        let response = build_not_modified(&orig, &etag, None);

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap(),
            "no-cache"
        );
        assert_eq!(
            response.headers().get(VARY).unwrap().to_str().unwrap(),
            "Accept"
        );
        // Set-Cookie is NOT preserved — build_not_modified strips it.
        assert!(!response.headers().contains_key(SET_COOKIE));
    }

    // ── Integration: first GET → 200 + ETag, second → 304 ────────────────────

    #[tokio::test]
    async fn integration_first_get_200_second_get_304() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI64, Ordering};

        let lock_version = Arc::new(AtomicI64::new(1));
        let lv = Arc::clone(&lock_version);

        let svc = EtagLayer::new().layer(tower::service_fn(move |_req: Request<Body>| {
            let v = lv.load(Ordering::SeqCst);
            async move {
                let etag: ETag = v.into_etag();
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(ETAG, etag.header_value())
                        .body(Body::from(format!("version={v}")))
                        .unwrap(),
                )
            }
        }));

        // First GET → 200 + ETag.
        let first = svc
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first.headers().get(ETAG).cloned().unwrap();

        // Second GET with matching If-None-Match → 304 empty body.
        let second = svc
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/resource")
                    .header(IF_NONE_MATCH, etag.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        let body_bytes = second.into_body().collect().await.unwrap().to_bytes();
        assert!(body_bytes.is_empty(), "304 body must be empty");

        // Simulate mutation: lock_version bumped.
        lock_version.store(2, Ordering::SeqCst);

        // Third GET with old ETag → 200 with new ETag.
        let third = svc
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/resource")
                    .header(IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(third.status(), StatusCode::OK);
        let new_etag = third.headers().get(ETAG).unwrap();
        let old_etag: ETag = 1_i64.into_etag();
        // New ETag must differ from old one.
        assert_ne!(new_etag, &old_etag.header_value());
    }

    // ── RED: CacheControl freshness (issue #1344) ─────────────────────────────

    // AC#2: byte-for-byte header_value() rendering.

    #[test]
    fn cache_for_default_is_private_max_age() {
        assert_eq!(
            cache_for(Duration::from_secs(60)).header_value(),
            "private, max-age=60"
        );
    }

    #[test]
    fn cache_for_public_with_s_maxage_and_swr_orders_deterministically() {
        let v = cache_for(Duration::from_secs(60))
            .public()
            .s_maxage(Duration::from_secs(120))
            .stale_while_revalidate(Duration::from_secs(30))
            .header_value();
        assert_eq!(
            v,
            "public, max-age=60, s-maxage=120, stale-while-revalidate=30"
        );
    }

    #[test]
    fn cache_for_no_store_emits_only_no_store() {
        // no-store overrides every freshness directive.
        let v = cache_for(Duration::from_secs(60))
            .public()
            .s_maxage(Duration::from_secs(120))
            .stale_while_revalidate(Duration::from_secs(30))
            .no_store()
            .header_value();
        assert_eq!(v, "no-store");
    }

    #[test]
    fn cache_control_full_ordering() {
        // Every directive present exercises the fixed ordering.
        let v = cache_for(Duration::from_secs(1))
            .public()
            .no_cache()
            .max_age(Duration::from_secs(10))
            .s_maxage(Duration::from_secs(20))
            .stale_while_revalidate(Duration::from_secs(30))
            .must_revalidate()
            .immutable()
            .header_value();
        assert_eq!(
            v,
            "public, no-cache, max-age=10, s-maxage=20, stale-while-revalidate=30, must-revalidate, immutable"
        );
    }

    #[test]
    fn cache_control_durations_render_as_whole_seconds() {
        // Sub-second remainder is truncated (as_secs()).
        let v = cache_for(Duration::from_millis(1500)).header_value();
        assert_eq!(v, "private, max-age=1");
    }

    // AC#4: default-private safety — never public unless asked.

    #[test]
    fn cache_for_never_public_by_default() {
        let v = cache_for(Duration::from_secs(300)).header_value();
        assert!(!v.contains("public"), "default must not be public: {v}");
        assert!(v.contains("private"), "default must be private: {v}");
    }

    #[test]
    fn cache_for_private_stays_private_after_explicit_call() {
        let v = cache_for(Duration::from_secs(300)).private().header_value();
        assert_eq!(v, "private, max-age=300");
    }

    #[test]
    fn cache_for_public_is_explicit_opt_in() {
        let v = cache_for(Duration::from_secs(300)).public().header_value();
        assert!(v.starts_with("public"), "{v}");
    }

    // AC#1: wrap() inserts the header on a response.

    #[tokio::test]
    async fn cache_control_wrap_sets_single_cache_control_header() {
        let response = cache_for(Duration::from_secs(60))
            .public()
            .wrap("hello".into_response());
        let values: Vec<_> = response.headers().get_all(CACHE_CONTROL).iter().collect();
        assert_eq!(values.len(), 1, "exactly one Cache-Control header");
        assert_eq!(values[0].to_str().unwrap(), "public, max-age=60");
    }

    #[tokio::test]
    async fn cache_control_wrap_replaces_existing_cache_control() {
        let base = Response::builder()
            .status(StatusCode::OK)
            .header(CACHE_CONTROL, "max-age=1")
            .body(Body::from("hi"))
            .unwrap();
        let response = cache_for(Duration::from_secs(60)).wrap(base);
        let values: Vec<_> = response.headers().get_all(CACHE_CONTROL).iter().collect();
        assert_eq!(values.len(), 1, "insert replaces, does not append");
        assert_eq!(values[0].to_str().unwrap(), "private, max-age=60");
    }

    // AC#1: IntoResponseParts tuple composition.

    #[tokio::test]
    async fn cache_control_tuple_sets_header() {
        let response = (cache_for(Duration::from_secs(90)).public(), "body").into_response();
        let values: Vec<_> = response.headers().get_all(CACHE_CONTROL).iter().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].to_str().unwrap(), "public, max-age=90");
    }

    // AC#3: composing ETag + freshness emits exactly ONE Cache-Control header.

    #[tokio::test]
    async fn fresh_when_or_with_cache_control_emits_single_header_on_200() {
        // Stale request (no If-None-Match): 200 with both ETag and Cache-Control.
        let headers = HeaderMap::new();
        let markup = "<h1>hi</h1>";
        let response = fresh_when(&headers, "v1")
            .or(cache_for(Duration::from_secs(60)).public().wrap(markup))
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(ETAG));
        let values: Vec<_> = response.headers().get_all(CACHE_CONTROL).iter().collect();
        assert_eq!(values.len(), 1, "exactly one Cache-Control on 200");
        assert_eq!(values[0].to_str().unwrap(), "public, max-age=60");
    }

    // AC#5: 304 revalidation preserves the freshness directives (single header).

    #[tokio::test]
    async fn fresh_when_or_preserves_cache_control_on_304() {
        let etag: ETag = "v1".into_etag();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.header_value());

        let markup = "<h1>hi</h1>";
        let response = fresh_when(&headers, "v1")
            .or(cache_for(Duration::from_secs(60)).public().wrap(markup))
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        let values: Vec<_> = response.headers().get_all(CACHE_CONTROL).iter().collect();
        assert_eq!(values.len(), 1, "exactly one Cache-Control on 304");
        assert_eq!(values[0].to_str().unwrap(), "public, max-age=60");
    }
}
