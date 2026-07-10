//! Traced outbound HTTP client with retries and test mocks.
//!
//! Exposes [`Client`](crate::http_client::Client) as `autumn_web::http::Client` — a thin `reqwest`-backed
//! outbound HTTP client that propagates the active span's `traceparent` /
//! `tracestate` headers, retries transient failures, and is mockable in tests
//! via [`TestApp::http_mock`](crate::test::TestApp::http_mock).
//!
//! # Quick start
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::http::Client;
//!
//! #[post("/pay")]
//! async fn pay(client: Client) -> AutumnResult<Json<serde_json::Value>> {
//!     let resp = client
//!         .post("https://api.stripe.com/v1/charges")
//!         .header("authorization", "Bearer sk_test_xxx")
//!         .json(&serde_json::json!({"amount": 1000, "currency": "usd"}))
//!         .send()
//!         .await?;
//!     Ok(Json(resp.json()?))
//! }
//! ```
//!
//! # Test mocks
//!
//! ```rust,no_run
//! use autumn_web::test::TestApp;
//! use autumn_web::prelude::*;
//! use serde_json::json;
//!
//! // (handler shown above)
//!
//! #[tokio::test]
//! async fn pay_calls_stripe() {
//!     let mut app = TestApp::new().routes(routes![pay]);
//!     let mock = app.http_mock("stripe")
//!         .post("/v1/charges")
//!         .respond_with(200, json!({"id": "ch_123", "amount": 1000}));
//!
//!     let client = app.build();
//!     client.post("/pay").send().await.assert_status(200);
//!     mock.expect_called(1);
//! }
//! ```

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;

// ── Error ────────────────────────────────────────────────────────────────────

/// Errors produced by [`Client`] and [`RequestBuilder`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// An underlying `reqwest` transport error.
    #[error("outbound HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// JSON (de)serialisation failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// No mock entry matched the outgoing request.
    #[error("no mock registered for {0} {1}")]
    NoMock(String, String),
    /// The outbound circuit breaker is open.
    #[error("outbound circuit breaker is open")]
    CircuitBreakerOpen,
    /// A resolved connection target (or redirect target) is a blocked
    /// (private / link-local / loopback / reserved) IP address per the built-in
    /// SSRF policy. See [`is_blocked_ip`].
    #[error("SSRF policy blocked address: {0}")]
    SsrfBlocked(String),
    /// A redirect chain exceeded the configured maximum number of hops.
    #[error("too many redirects (max {0})")]
    TooManyRedirects(usize),
    /// A redirect's absolute `Location` target was rejected by the caller's
    /// validator (or by the built-in scheme-downgrade guard).
    #[error("redirect rejected: {0}")]
    RedirectRejected(String),
    /// A URL could not be parsed, was missing a host, or DNS resolution failed
    /// while composing a custom (redirect / pin / SSRF-safe) request.
    #[error("invalid or unresolvable URL: {0}")]
    InvalidUrl(String),
}

// ── Response ─────────────────────────────────────────────────────────────────

/// Completed outbound HTTP response with eagerly-collected body bytes.
///
/// Body is consumed once — call exactly one of [`json`](Self::json),
/// [`text`](Self::text), or [`bytes`](Self::bytes).
#[derive(Debug)]
pub struct Response {
    status: reqwest::StatusCode,
    headers: HeaderMap,
    body: Bytes,
    url: Option<reqwest::Url>,
}

impl Response {
    /// HTTP status code.
    pub const fn status(&self) -> reqwest::StatusCode {
        self.status
    }

    /// Response headers (sensitive values are **not** redacted here).
    pub const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// `true` when the status code is in the 2xx range.
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// URL that was ultimately requested (after redirects, if any).
    pub const fn url(&self) -> Option<&reqwest::Url> {
        self.url.as_ref()
    }

    /// Deserialise the body as JSON.
    ///
    /// # Errors
    /// Returns [`ClientError::Json`] if the body is not valid JSON for `T`.
    pub fn json<T: DeserializeOwned>(self) -> Result<T, ClientError> {
        serde_json::from_slice(&self.body).map_err(ClientError::Json)
    }

    /// Return the body as a UTF-8 string (lossy).
    pub fn text(self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Return the raw body bytes.
    pub fn bytes(self) -> Bytes {
        self.body
    }
}

// ── SSRF address policy ──────────────────────────────────────────────────────

/// Return `true` when `ip` must **not** be connected to because it belongs to a
/// private, loopback, link-local, CGNAT, benchmarking, documentation, multicast
/// or otherwise reserved range.
///
/// This is the built-in Server-Side Request Forgery (SSRF) deny-list used by
/// [`Client::get_ssrf_safe`]. Ranges are checked explicitly (rather than via the
/// unstable `IpAddr::is_global` family) so the code compiles on stable Rust.
///
/// IPv4-mapped (`::ffff:a.b.c.d`) and the deprecated IPv4-compatible
/// (`::a.b.c.d`) IPv6 forms are unwrapped and re-checked as IPv4, so encodings
/// such as `::ffff:169.254.169.254` are correctly blocked.
#[must_use]
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

/// Return `true` when `ip` is safe to connect to — the exact negation of
/// [`is_blocked_ip`].
#[must_use]
pub fn is_public_ip(ip: IpAddr) -> bool {
    !is_blocked_ip(ip)
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    // 0.0.0.0/8 (incl. unspecified)
    if a == 0 {
        return true;
    }
    // 10.0.0.0/8
    if a == 10 {
        return true;
    }
    // 100.64.0.0/10 (CGNAT)
    if a == 100 && (64..=127).contains(&b) {
        return true;
    }
    // 127.0.0.0/8 (loopback)
    if a == 127 {
        return true;
    }
    // 169.254.0.0/16 (link-local, incl. 169.254.169.254 cloud metadata)
    if a == 169 && b == 254 {
        return true;
    }
    // 172.16.0.0/12
    if a == 172 && (16..=31).contains(&b) {
        return true;
    }
    // 192.0.0.0/24 (IETF protocol assignments)
    if a == 192 && b == 0 && c == 0 {
        return true;
    }
    // 192.0.2.0/24 (TEST-NET-1)
    if a == 192 && b == 0 && c == 2 {
        return true;
    }
    // 192.168.0.0/16
    if a == 192 && b == 168 {
        return true;
    }
    // 198.18.0.0/15 (benchmarking)
    if a == 198 && (18..=19).contains(&b) {
        return true;
    }
    // 198.51.100.0/24 (TEST-NET-2)
    if a == 198 && b == 51 && c == 100 {
        return true;
    }
    // 203.0.113.0/24 (TEST-NET-3)
    if a == 203 && b == 0 && c == 113 {
        return true;
    }
    // 224.0.0.0/4 (multicast) and 240.0.0.0/4 (reserved, incl. 255.255.255.255)
    if a >= 224 {
        return true;
    }
    false
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    // Unwrap IPv4-mapped (::ffff:a.b.c.d) and deprecated IPv4-compatible
    // (::a.b.c.d) forms and re-check as IPv4. `to_ipv4` covers both, but we try
    // the mapped form first for clarity.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(v4);
    }
    if let Some(v4) = ip.to_ipv4() {
        return is_blocked_ipv4(v4);
    }

    let segs = ip.segments();
    // :: (unspecified)
    if ip == Ipv6Addr::UNSPECIFIED {
        return true;
    }
    // ::1 (loopback)
    if ip == Ipv6Addr::LOCALHOST {
        return true;
    }
    // fc00::/7 (unique local address)
    if segs[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    // fe80::/10 (link-local)
    if segs[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    // ff00::/8 (multicast)
    if segs[0] & 0xff00 == 0xff00 {
        return true;
    }
    // 2001:db8::/32 (documentation)
    if segs[0] == 0x2001 && segs[1] == 0x0db8 {
        return true;
    }
    false
}

// ── RetryPolicy ──────────────────────────────────────────────────────────────

/// Retry configuration for a [`RequestBuilder`].
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Maximum number of additional attempts after the first failure.  Zero
    /// means no retries (one attempt total).
    pub max_retries: u32,
    /// When `true` (the default), only GET / HEAD / PUT / DELETE / OPTIONS /
    /// TRACE are retried; POST and PATCH are not.
    pub retry_idempotent_only: bool,
    /// Maximum Retry-After sleep duration to accept before clamping.
    pub max_retry_after: Duration,
    /// Per-request timeout.
    pub request_timeout: Option<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_idempotent_only: true,
            max_retry_after: Duration::from_secs(10),
            request_timeout: Some(Duration::from_secs(30)),
        }
    }
}

// ── MockRegistry ─────────────────────────────────────────────────────────────

/// Internal mock entry stored by [`MockRegistry`].
pub(crate) struct MockEntry {
    pub(crate) method: Option<Method>,
    /// URL path to match against the path component of the outbound URL.
    pub(crate) path: String,
    /// Optional alias that must match the `Client`'s alias.
    pub(crate) alias: Option<String>,
    pub(crate) status: u16,
    pub(crate) body: Option<serde_json::Value>,
    pub(crate) call_count: Arc<AtomicUsize>,
}

/// Canned response returned by a [`MockRegistry`] match.
pub(crate) struct MockResponse {
    pub(crate) status: u16,
    pub(crate) body: Option<serde_json::Value>,
}

/// In-process mock registry used by [`TestApp::http_mock`](crate::test::TestApp::http_mock).
///
/// Stored in [`AppState`](crate::AppState) extensions during test builds so
/// that any [`Client`] extracted from state will intercept matching requests
/// and return canned responses without hitting the network.
pub struct MockRegistry {
    entries: Mutex<Vec<MockEntry>>,
}

impl MockRegistry {
    /// Create an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Register a new mock entry.
    pub(crate) fn register(&self, entry: MockEntry) {
        self.entries
            .lock()
            .expect("mock registry lock poisoned")
            .push(entry);
    }

    /// Find the first entry matching `(method, url, alias)` and increment its
    /// call counter.  Returns `None` when no entry matches.
    pub(crate) fn find_match(
        &self,
        method: &Method,
        url: &str,
        alias: Option<&str>,
    ) -> Option<MockResponse> {
        // Extract the URL path component for matching, stripping query and fragment.
        // For full URLs (https://…) reqwest::Url::parse gives us the clean path.
        // For relative paths we strip manually so "?query" doesn't break matching.
        // Extract the path without query/fragment. For full URLs reqwest parses
        // cleanly; for relative strings we strip manually.
        let url_path_owned: String = reqwest::Url::parse(url).map_or_else(
            |_| {
                let s = url.split_once('?').map_or(url, |(p, _)| p);
                s.split_once('#').map_or(s, |(p, _)| p).to_owned()
            },
            |parsed| parsed.path().to_owned(),
        );
        let url_path = url_path_owned.as_str();

        // Hold the lock only for the search; release before fetching metadata.
        let found = {
            let entries = self.entries.lock().expect("mock registry lock poisoned");
            entries.iter().find_map(|entry| {
                let method_ok = entry.method.as_ref().is_none_or(|m| m == method);
                // Path match: exact equality OR suffix at a segment boundary.
                // When the mock path starts with '/' the leading slash IS the
                // segment separator, so a non-empty prefix is also valid
                // (e.g. mock "/charges" matches URL path "/v1/charges").
                let path_ok = url_path == entry.path.as_str()
                    || url_path
                        .strip_suffix(entry.path.as_str())
                        .is_some_and(|prefix| {
                            prefix.is_empty()
                                || prefix.ends_with('/')
                                || entry.path.starts_with('/')
                        });
                let alias_ok = entry
                    .alias
                    .as_deref()
                    .is_none_or(|a| alias.is_some_and(|b| a == b));
                if method_ok && path_ok && alias_ok {
                    Some((entry.call_count.clone(), entry.status, entry.body.clone()))
                } else {
                    None
                }
            })
        };

        found.map(|(call_count, status, body)| {
            call_count.fetch_add(1, Ordering::SeqCst);
            MockResponse { status, body }
        })
    }
}

impl Default for MockRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Newtype stored in [`AppState`](crate::AppState) extensions so the
/// `MockRegistry` `Arc` survives a `build()` without double-wrapping.
pub struct HttpMockRegistryExt(pub Arc<MockRegistry>);

/// Shared, process-wide `reqwest::Client` registered in [`AppState`] at server
/// boot. Cloning is O(1) because `reqwest::Client` is internally `Arc`-backed,
/// and the connection pool is preserved across the clone.
///
/// `timeout_secs` records the per-request timeout the inner client was built
/// with.  [`Client::from_state`] compares this against the currently installed
/// config so that a `state_initializer` that replaces `AutumnConfig` with a
/// different timeout causes the stale inner to be discarded and a fresh client
/// to be built from the new config instead.
#[derive(Clone)]
pub(crate) struct SharedReqwestClient {
    pub(crate) client: reqwest::Client,
    pub(crate) timeout_secs: u64,
}

/// Handle returned by
/// [`MockSetupBuilder::respond_with`] that lets tests assert call counts.
pub struct MockHandle {
    alias: String,
    method: String,
    path: String,
    call_count: Arc<AtomicUsize>,
}

impl MockHandle {
    /// Assert that the mocked endpoint was called exactly `expected` times.
    ///
    /// # Panics
    ///
    /// Panics with a diagnostic message when the actual call count differs.
    pub fn expect_called(&self, expected: usize) {
        let actual = self.call_count.load(Ordering::SeqCst);
        assert_eq!(
            actual, expected,
            "http mock for {} {} {} expected {} call(s) but got {}",
            self.alias, self.method, self.path, expected, actual,
        );
    }

    /// Return the raw call count without asserting.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

/// Builder returned by [`TestApp::http_mock`](crate::test::TestApp::http_mock).
///
/// Chain a method call (`get`, `post`, …) and a path, then call
/// [`respond_with`](Self::respond_with) to register the entry and obtain a
/// [`MockHandle`] for later assertions.
pub struct MockSetupBuilder {
    pub(crate) registry: Arc<MockRegistry>,
    pub(crate) alias: String,
    pub(crate) method: Option<Method>,
    pub(crate) path: Option<String>,
}

impl MockSetupBuilder {
    /// Match `GET <path>`.
    #[must_use]
    pub fn get(mut self, path: &str) -> Self {
        self.method = Some(Method::GET);
        self.path = Some(path.to_owned());
        self
    }
    /// Match `POST <path>`.
    #[must_use]
    pub fn post(mut self, path: &str) -> Self {
        self.method = Some(Method::POST);
        self.path = Some(path.to_owned());
        self
    }
    /// Match `PUT <path>`.
    #[must_use]
    pub fn put(mut self, path: &str) -> Self {
        self.method = Some(Method::PUT);
        self.path = Some(path.to_owned());
        self
    }
    /// Match `PATCH <path>`.
    #[must_use]
    pub fn patch(mut self, path: &str) -> Self {
        self.method = Some(Method::PATCH);
        self.path = Some(path.to_owned());
        self
    }
    /// Match `DELETE <path>`.
    #[must_use]
    pub fn delete(mut self, path: &str) -> Self {
        self.method = Some(Method::DELETE);
        self.path = Some(path.to_owned());
        self
    }

    /// Match `HEAD <path>`.
    #[must_use]
    pub fn head(mut self, path: &str) -> Self {
        self.method = Some(Method::HEAD);
        self.path = Some(path.to_owned());
        self
    }

    /// Register the mock entry and return a [`MockHandle`] for assertions.
    ///
    /// `status` is the HTTP status code to return.
    /// `body` is serialised as JSON and returned as the response body.
    #[must_use]
    pub fn respond_with(self, status: u16, body: serde_json::Value) -> MockHandle {
        let path = self.path.clone().unwrap_or_default();
        let method_str = self
            .method
            .as_ref()
            .map_or_else(|| "*".to_owned(), ToString::to_string);
        let call_count = Arc::new(AtomicUsize::new(0));

        self.registry.register(MockEntry {
            method: self.method,
            path: path.clone(),
            alias: Some(self.alias.clone()),
            status,
            body: Some(body),
            call_count: call_count.clone(),
        });

        MockHandle {
            alias: self.alias,
            method: method_str,
            path,
            call_count,
        }
    }

    /// Convenience variant that returns the given status with an empty body.
    ///
    /// Unlike [`respond_with`](Self::respond_with), this stores `body: None` so
    /// the mock response truly has zero body bytes (not the JSON literal `null`).
    #[must_use]
    pub fn respond_with_status(self, status: u16) -> MockHandle {
        let path = self.path.clone().unwrap_or_default();
        let method_str = self
            .method
            .as_ref()
            .map_or_else(|| "*".to_owned(), ToString::to_string);
        let call_count = Arc::new(AtomicUsize::new(0));

        self.registry.register(MockEntry {
            method: self.method,
            path: path.clone(),
            alias: Some(self.alias.clone()),
            status,
            body: None,
            call_count: call_count.clone(),
        });

        MockHandle {
            alias: self.alias,
            method: method_str,
            path,
            call_count,
        }
    }
}

// ── Client ───────────────────────────────────────────────────────────────────

/// Traced outbound HTTP client with automatic retries and test-mock support.
///
/// Extracted from `AppState` via Axum's extractor machinery — declare it as a
/// handler parameter to get a pre-configured instance that respects
/// `[http.client]` config and, in test builds, intercepts requests against any
/// registered mocks.
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::http::Client;
///
/// #[get("/ping-upstream")]
/// async fn ping(client: Client) -> AutumnResult<&'static str> {
///     client.get("https://api.example.com/health").send().await?;
///     Ok("ok")
/// }
/// ```
///
/// You can also construct a standalone client outside of a handler:
///
/// ```rust
/// use autumn_web::http::Client;
///
/// let client = Client::new();
/// ```
#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
    /// Named alias — used to look up base URLs from config and to match mocks.
    alias: Option<String>,
    /// Base URL prepended to relative paths.
    base_url: Option<String>,
    /// Alias → base URL map loaded from `[http.client.base_urls]` config.
    base_urls: HashMap<String, String>,
    retry_policy: RetryPolicy,
    /// When present (test builds), matching requests bypass the network.
    mock: Option<Arc<MockRegistry>>,
    /// Resilience configuration for circuit breakers.
    resilience_config: Option<Arc<crate::config::ResilienceConfig>>,
}

impl Client {
    /// Create a new client with default settings (30 s timeout, 3 retries on
    /// idempotent methods).
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(Duration::from_secs(30))
    }

    /// Create a client with a custom per-request timeout.
    ///
    /// # Panics
    ///
    /// Panics if the underlying TLS backend cannot be initialised (should not
    /// happen with the default `rustls-tls` feature).
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        let inner = reqwest::ClientBuilder::new()
            .timeout(timeout)
            .build()
            .expect("failed to build reqwest client");
        Self {
            inner,
            alias: None,
            base_url: None,
            base_urls: HashMap::new(),
            retry_policy: RetryPolicy {
                max_retries: 3,
                retry_idempotent_only: true,
                max_retry_after: Duration::from_secs(10),
                request_timeout: Some(timeout),
            },
            mock: None,
            resilience_config: None,
        }
    }

    /// Build a bare `reqwest::Client` from `[http.client]` config.
    ///
    /// Used by `build_state` to create the single shared instance registered
    /// in `AppState` at server boot, and as the fallback when no shared client
    /// is available.
    ///
    /// # Panics
    ///
    /// Panics if the underlying TLS backend cannot be initialised.
    pub(crate) fn build_inner(config: &crate::config::HttpClientConfig) -> reqwest::Client {
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build reqwest client")
    }

    /// Assemble a `Client` around an already-built `reqwest::Client` using the
    /// policy fields from `config`.  The caller supplies the inner client so
    /// the connection pool can be shared across requests.
    fn from_config_with_inner(
        inner: reqwest::Client,
        config: &crate::config::HttpClientConfig,
    ) -> Self {
        let timeout = Duration::from_secs(config.timeout_secs);
        Self {
            inner,
            alias: None,
            base_url: None,
            base_urls: config.base_urls.clone(),
            retry_policy: RetryPolicy {
                max_retries: config.max_retries,
                retry_idempotent_only: true,
                max_retry_after: Duration::from_secs(config.max_retry_after_secs),
                request_timeout: Some(timeout),
            },
            mock: None,
            resilience_config: None,
        }
    }

    /// Assemble a `Client` with default policy around an already-built
    /// `reqwest::Client`.  Used when a shared inner client is available but
    /// no explicit `[http.client]` config is registered.
    fn with_inner(inner: reqwest::Client) -> Self {
        Self {
            inner,
            alias: None,
            base_url: None,
            base_urls: HashMap::new(),
            retry_policy: RetryPolicy::default(),
            mock: None,
            resilience_config: None,
        }
    }

    /// Create a client from `[http.client]` framework configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying TLS backend cannot be initialised (should not
    /// happen with the default `rustls-tls` feature).
    #[must_use]
    pub fn from_config(config: &crate::config::HttpClientConfig) -> Self {
        Self::from_config_with_inner(Self::build_inner(config), config)
    }

    /// Attach a mock registry (used by the test harness).
    pub(crate) fn with_mock(mut self, registry: Arc<MockRegistry>) -> Self {
        self.mock = Some(registry);
        self
    }

    /// Build a client from runtime application state.
    ///
    /// When the server was started via `AppBuilder`, a single `reqwest::Client`
    /// is registered in `AppState` at boot as a `SharedReqwestClient`.  This
    /// method clones that shared instance (O(1), preserves the connection pool)
    /// instead of constructing a new one, eliminating per-request TCP/TLS
    /// handshakes and DNS-resolver-spawn overhead.
    ///
    /// Falls back to `Self::new()` for detached or test state that does not
    /// carry a shared client.
    #[must_use]
    pub fn from_state(state: &crate::AppState) -> Self {
        let autumn_config = state.extension::<crate::config::AutumnConfig>();
        let config = state
            .extension::<crate::config::HttpConfig>()
            .or_else(|| autumn_config.as_ref().map(|c| Arc::new(c.http.clone())));

        // Only reuse the shared inner client when its baked-in timeout still
        // matches the effective config timeout.  A state_initializer that
        // replaces AutumnConfig/HttpConfig with a different timeout_secs runs
        // after build_state, so without this check the stale inner would
        // silently override the new config's per-request timeout.
        let effective_timeout_secs = config.as_ref().map_or_else(
            || crate::config::HttpClientConfig::default().timeout_secs,
            |c| c.client.timeout_secs,
        );
        let shared = state.extension::<SharedReqwestClient>().and_then(|s| {
            if s.timeout_secs == effective_timeout_secs {
                Some(s.client.clone())
            } else {
                None
            }
        });

        let mut client = match (config, shared) {
            (Some(cfg), Some(inner)) => Self::from_config_with_inner(inner, &cfg.client),
            (Some(cfg), None) => Self::from_config(&cfg.client),
            (None, Some(inner)) => Self::with_inner(inner),
            (None, None) => Self::new(),
        };

        client.resilience_config = autumn_config.map(|c| Arc::new(c.resilience.clone()));

        if let Some(ext) = state.extension::<HttpMockRegistryExt>() {
            client = client.with_mock(ext.0.clone());
        }

        client
    }

    /// Return a clone of this client scoped to the named alias.
    ///
    /// When a `[http.client.base_urls]` entry exists for the alias the client
    /// will prepend that URL to all relative paths. Mocks registered for the
    /// alias via [`TestApp::http_mock`](crate::test::TestApp::http_mock) will
    /// match requests made through this named client.
    #[must_use]
    pub fn named(&self, alias: &str) -> Self {
        let base_url = self
            .base_urls
            .get(alias)
            .cloned()
            .or_else(|| self.base_url.clone());
        Self {
            inner: self.inner.clone(),
            alias: Some(alias.to_owned()),
            base_url,
            base_urls: self.base_urls.clone(),
            retry_policy: self.retry_policy.clone(),
            mock: self.mock.clone(),
            resilience_config: self.resilience_config.clone(),
        }
    }

    /// Set (or override) the base URL prepended to relative request paths.
    #[must_use]
    pub fn with_base_url(&self, base_url: impl Into<String>) -> Self {
        Self {
            inner: self.inner.clone(),
            alias: self.alias.clone(),
            base_url: Some(base_url.into()),
            base_urls: self.base_urls.clone(),
            retry_policy: self.retry_policy.clone(),
            mock: self.mock.clone(),
            resilience_config: self.resilience_config.clone(),
        }
    }

    fn build_request(&self, method: Method, url: impl AsRef<str>) -> RequestBuilder {
        let url_str = url.as_ref();
        let full_url = if url_str.starts_with("http://") || url_str.starts_with("https://") {
            url_str.to_owned()
        } else if let Some(base) = &self.base_url {
            format!(
                "{}/{}",
                base.trim_end_matches('/'),
                url_str.trim_start_matches('/')
            )
        } else {
            url_str.to_owned()
        };

        RequestBuilder {
            client: self.inner.clone(),
            method,
            url: full_url,
            extra_headers: HeaderMap::new(),
            body: None,
            retry_policy: self.retry_policy.clone(),
            mock: self.mock.clone(),
            alias: self.alias.clone(),
            pending_error: None,
            resilience_config: self.resilience_config.clone(),
            redirect_mode: RedirectMode::Default,
            pin_addr: None,
            ssrf_safe: false,
        }
    }

    /// Build a `GET` request.
    #[must_use]
    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.build_request(Method::GET, url)
    }
    /// Build a `POST` request.
    #[must_use]
    pub fn post(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.build_request(Method::POST, url)
    }
    /// Build a `PUT` request.
    #[must_use]
    pub fn put(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.build_request(Method::PUT, url)
    }
    /// Build a `PATCH` request.
    #[must_use]
    pub fn patch(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.build_request(Method::PATCH, url)
    }
    /// Build a `DELETE` request.
    #[must_use]
    pub fn delete(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.build_request(Method::DELETE, url)
    }

    /// Build a `HEAD` request.
    #[must_use]
    pub fn head(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.build_request(Method::HEAD, url)
    }

    /// Build an SSRF-safe `GET` request.
    ///
    /// This is the composed safe path for fetching **untrusted** URLs. Before
    /// connecting it resolves the host **once**, validates every resolved IP
    /// against the built-in SSRF deny-list ([`is_blocked_ip`]) and rejects the
    /// request if *any* address is blocked. The connection is then pinned to a
    /// validated address so reqwest cannot re-resolve the host (closing the
    /// DNS-rebinding / TOCTOU window). Redirects are followed manually up to
    /// [`SSRF_SAFE_MAX_REDIRECTS`] hops, re-running resolve→validate→pin on each
    /// hop; a hop that downgrades the scheme from `https` to `http` is rejected
    /// as defence-in-depth.
    ///
    /// Like the test-mock path, this custom send path bypasses the process-wide
    /// circuit-breaker registry to avoid entangling per-URL SSRF fetches with
    /// the shared per-host breaker state.
    #[must_use]
    pub fn get_ssrf_safe(&self, url: impl Into<String>) -> RequestBuilder {
        let mut builder = self.build_request(Method::GET, url.into());
        builder.ssrf_safe = true;
        builder
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl axum::extract::FromRequestParts<crate::AppState> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut http::request::Parts,
        state: &crate::AppState,
    ) -> Result<Self, std::convert::Infallible> {
        Ok(Self::from_state(state))
    }
}

// ── RequestBuilder ───────────────────────────────────────────────────────────

/// Type alias for a redirect-`Location` validator.
type RedirectValidator = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Per-request redirect handling.
///
/// `Default` preserves the historical behaviour exactly (the shared-client fast
/// path with reqwest's built-in auto-follow). `None` and `Follow` route the
/// request through the custom one-shot-client send path.
enum RedirectMode {
    /// Historical behaviour: shared client, reqwest auto-follows up to 10 hops.
    Default,
    /// Never follow: a 3xx is returned to the caller verbatim.
    None,
    /// Follow up to `max` hops, calling `validator` on each absolute target
    /// before following it.
    Follow {
        max: usize,
        validator: RedirectValidator,
    },
}

/// Default hop cap for the composed [`Client::get_ssrf_safe`] safe path.
const SSRF_SAFE_MAX_REDIRECTS: usize = 5;

/// Fluent outbound request builder produced by [`Client`] methods.
pub struct RequestBuilder {
    client: reqwest::Client,
    method: Method,
    url: String,
    extra_headers: HeaderMap,
    /// Request body. `Bytes` gives O(1) clones across retry attempts.
    body: Option<Bytes>,
    retry_policy: RetryPolicy,
    mock: Option<Arc<MockRegistry>>,
    alias: Option<String>,
    /// Captures errors from `json()` or invalid headers to surface in `send()`.
    pending_error: Option<ClientError>,
    /// Resilience configuration for circuit breakers.
    resilience_config: Option<Arc<crate::config::ResilienceConfig>>,
    /// Per-request redirect handling (see [`RedirectMode`]).
    redirect_mode: RedirectMode,
    /// When set, connect directly to this socket, skipping DNS resolution while
    /// preserving the original `Host` header + SNI. See [`RequestBuilder::pin_to`].
    pin_addr: Option<SocketAddr>,
    /// When `true`, use the composed SSRF-safe send path (resolve→validate→pin
    /// with per-hop redirect validation). Set by [`Client::get_ssrf_safe`].
    ssrf_safe: bool,
}

impl RequestBuilder {
    /// Append a request header.
    ///
    /// Headers named `authorization`, `cookie`, or `set-cookie` are accepted
    /// normally but are **redacted** in tracing events and log output.
    /// Invalid header names or values emit a `tracing::warn!` and are skipped.
    #[must_use]
    pub fn header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        let name_str = name.as_ref();
        let value_str = value.as_ref();
        match (
            HeaderName::from_bytes(name_str.as_bytes()),
            HeaderValue::from_str(value_str),
        ) {
            (Ok(n), Ok(v)) => {
                self.extra_headers.insert(n, v);
            }
            (Err(e), _) => {
                tracing::warn!(header.name = name_str, error = %e, "invalid header name — header skipped");
            }
            (_, Err(e)) => {
                tracing::warn!(header.name = name_str, error = %e, "invalid header value — header skipped");
            }
        }
        self
    }

    /// Serialise `body` as JSON and set `Content-Type: application/json`.
    ///
    /// Serialisation errors are captured and returned when [`send`](Self::send)
    /// is called rather than being silently discarded.
    #[must_use]
    pub fn json<T: Serialize>(mut self, body: &T) -> Self {
        match serde_json::to_vec(body) {
            Ok(bytes) => {
                self.body = Some(Bytes::from(bytes));
                self = self.header("content-type", "application/json");
            }
            Err(e) => {
                self.pending_error = Some(ClientError::Json(e));
            }
        }
        self
    }

    /// Set a plain-text body.
    #[must_use]
    pub fn text_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(Bytes::from(body.into().into_bytes()));
        self
    }

    /// Override the maximum retry count for this request.
    ///
    /// Also clears the idempotent-only flag so non-idempotent methods such as
    /// `POST` and `PATCH` are retried when the caller explicitly requests it.
    #[must_use]
    pub const fn retries(mut self, max: u32) -> Self {
        self.retry_policy.max_retries = max;
        self.retry_policy.retry_idempotent_only = false;
        self
    }

    /// Override the maximum `Retry-After` sleep duration for this request.
    #[must_use]
    pub const fn max_retry_after(mut self, max: Duration) -> Self {
        self.retry_policy.max_retry_after = max;
        self
    }

    /// Disable retries for this request.
    #[must_use]
    pub const fn no_retry(mut self) -> Self {
        self.retry_policy.max_retries = 0;
        self
    }

    /// Disable redirect following for this request.
    ///
    /// A `3xx` response is returned to the caller verbatim (status, headers and
    /// body) rather than being followed. Routes the request through the custom
    /// one-shot-client send path, which bypasses the process-wide circuit
    /// breaker.
    #[must_use]
    pub fn no_redirect(mut self) -> Self {
        self.redirect_mode = RedirectMode::None;
        self
    }

    /// Follow up to `max` redirects, validating each hop before following it.
    ///
    /// Before following a `3xx`, the `Location` header is resolved to an
    /// absolute URL (relative locations are joined against the current URL) and
    /// `validator(&absolute_location)` is called. If it returns `false` the
    /// request fails with [`ClientError::RedirectRejected`]. If the chain would
    /// exceed `max` hops the request fails with
    /// [`ClientError::TooManyRedirects`] (so `max == 0` turns the first `3xx`
    /// into an error). Routes the request through the custom one-shot-client
    /// send path, which bypasses the process-wide circuit breaker.
    #[must_use]
    pub fn follow_redirects<F>(mut self, max: usize, validator: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.redirect_mode = RedirectMode::Follow {
            max,
            validator: Arc::new(validator),
        };
        self
    }

    /// Pin the connection to `addr`, skipping DNS resolution.
    ///
    /// The original `Host` header and TLS SNI are preserved; only the
    /// address the socket connects to is overridden. This protects against
    /// DNS-rebinding / TOCTOU attacks where a hostname re-resolves to a
    /// different (private) address between validation and connection.
    ///
    /// Implemented via a one-shot `reqwest::ClientBuilder::resolve(host, addr)`
    /// scoped to this request. Note that reqwest ignores the **port** in the
    /// resolve override and connects to the port from the request URL, so
    /// `addr.port()` is only honoured when it matches the URL's port (which it
    /// does for addresses obtained by resolving that same URL). Routes the
    /// request through the custom one-shot-client send path, which bypasses the
    /// process-wide circuit breaker.
    #[must_use]
    pub const fn pin_to(mut self, addr: SocketAddr) -> Self {
        self.pin_addr = Some(addr);
        self
    }

    /// Send the request, applying retries and returning a [`Response`].
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Json`] if a prior `.json()` call failed to
    /// serialise the body.  Returns [`ClientError::Request`] for transport
    /// errors that exhaust all retry attempts.  Returns [`ClientError::NoMock`]
    /// if the request is made in a test context without a matching mock entry.
    ///
    /// # Panics
    ///
    /// Contains an internal `unreachable!()` that guards against a logic error
    /// in the retry loop; it cannot be reached in practice.
    pub async fn send(self) -> Result<Response, ClientError> {
        // Surface any error captured during builder construction.
        if let Some(err) = self.pending_error {
            return Err(err);
        }

        // Bypassing circuit breaker if a mock registry is present.
        if self.mock.is_some() {
            return self.send_inner(false).await;
        }

        // Custom send path: any of no_redirect / follow_redirects / pin_to /
        // get_ssrf_safe builds one-shot reqwest client(s) with Policy::none()
        // (+ optional .resolve()) and does manual redirect handling. Like the
        // mock path it deliberately BYPASSES the process-global circuit breaker
        // to avoid entangling these one-off, per-URL requests with the shared
        // per-host breaker registry.
        if self.needs_custom_path() {
            return self.send_custom().await;
        }

        // ── Resilience / Circuit Breaker ──────────────────────────────────
        let host = url::Url::parse(&self.url).ok().map_or_else(
            || "unknown".to_owned(),
            |u| {
                let h = u.host_str().unwrap_or("unknown");
                u.port()
                    .map_or_else(|| h.to_owned(), |port| format!("{h}:{port}"))
            },
        );

        let breaker = self.resilience_config.as_ref().map_or_else(
            || {
                crate::circuit_breaker::global_registry().get_or_create(
                    &host,
                    crate::circuit_breaker::CircuitBreakerPolicy::default(),
                )
            },
            |rc| {
                let policy = crate::circuit_breaker::CircuitBreakerPolicy::from_config(rc, &host);
                crate::circuit_breaker::global_registry().get_or_create_with_config(&host, policy)
            },
        );

        // Check if circuit breaker is open
        if breaker.before_call().is_err() {
            return Err(ClientError::CircuitBreakerOpen);
        }
        let guard = crate::circuit_breaker::CircuitBreakerGuard::new(breaker.clone());

        let is_half_open = breaker.state() == crate::circuit_breaker::CircuitState::HalfOpen;
        let res = self.send_inner(is_half_open).await;
        match &res {
            Ok(resp) => {
                let success = resp.status().as_u16() < 500;
                if success {
                    guard.success();
                } else {
                    guard.failure();
                }
            }
            Err(_) => {
                guard.failure();
            }
        }
        res
    }

    async fn send_inner(self, suppress_retries: bool) -> Result<Response, ClientError> {
        // ── Mock short-circuit ──────────────────────────────────────────────
        if let Some(ref mock) = self.mock {
            match mock.find_match(&self.method, &self.url, self.alias.as_deref()) {
                Some(mock_resp) => {
                    let status = reqwest::StatusCode::from_u16(mock_resp.status)
                        .unwrap_or(reqwest::StatusCode::OK);
                    let body_bytes = mock_resp
                        .body
                        .as_ref()
                        .map(|v| serde_json::to_vec(v).unwrap_or_default())
                        .unwrap_or_default();

                    tracing::info!(
                        http.method = %self.method,
                        http.url = %self.url,
                        http.status = mock_resp.status,
                        "[mock] outbound request intercepted"
                    );

                    return Ok(Response {
                        status,
                        headers: HeaderMap::new(),
                        body: Bytes::from(body_bytes),
                        url: None,
                    });
                }
                None => {
                    // A mock registry is present but nothing matched — treat as
                    // a test failure rather than falling through to the network.
                    return Err(ClientError::NoMock(
                        self.method.to_string(),
                        self.url.clone(),
                    ));
                }
            }
        }

        // ── Real network request with retries ───────────────────────────────
        let start = Instant::now();
        let max_attempts = if suppress_retries {
            1
        } else if is_idempotent_method(&self.method) || !self.retry_policy.retry_idempotent_only {
            self.retry_policy.max_retries.saturating_add(1)
        } else {
            1
        };

        for attempt in 0..max_attempts {
            if attempt > 0 {
                // Cap the exponent to prevent u64 overflow when max_retries is large.
                let exp = (attempt - 1).min(10);
                let delay = Duration::from_millis(100 * (1_u64 << exp));
                tokio::time::sleep(delay).await;
            }

            let mut req = self.client.request(self.method.clone(), &self.url);

            // Inject W3C trace context headers from the active span.
            req = inject_trace_context(req);

            // Apply caller-supplied headers (may override or extend trace headers).
            for (name, value) in &self.extra_headers {
                req = req.header(name.clone(), value.clone());
            }

            if let Some(body) = &self.body {
                req = req.body(body.clone());
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let headers = resp.headers().clone();
                    let url_used = resp.url().clone();

                    // 429 → honour Retry-After and retry if attempts remain.
                    if status.as_u16() == 429 && attempt + 1 < max_attempts {
                        let mut sleep_delay =
                            parse_retry_after(&headers).unwrap_or(Duration::from_secs(1));
                        sleep_delay = sleep_delay.min(self.retry_policy.max_retry_after);
                        if let Some(req_timeout) = self.retry_policy.request_timeout {
                            sleep_delay = sleep_delay.min(req_timeout);
                        }
                        tokio::time::sleep(sleep_delay).await;
                        continue;
                    }

                    // 5xx transient gateway errors → retry if attempts remain.
                    if is_retryable_status(status.as_u16()) && attempt + 1 < max_attempts {
                        continue;
                    }

                    let body = resp
                        .bytes()
                        .await
                        .map_err(|e| ClientError::Request(e.without_url()))?;
                    let elapsed = start.elapsed();
                    log_request(
                        self.method.as_str(),
                        &url_used,
                        status.as_u16(),
                        elapsed,
                        &self.extra_headers,
                    );

                    return Ok(Response {
                        status,
                        headers,
                        body,
                        url: Some(url_used),
                    });
                }
                // Only retry transient connect/timeout errors; non-transient errors
                // (e.g. malformed URL) fail immediately.
                Err(e) if (e.is_connect() || e.is_timeout()) && attempt + 1 < max_attempts => {}
                Err(e) => return Err(ClientError::Request(e.without_url())),
            }
        }

        // The retry loop always returns inside the last attempt; this is unreachable.
        unreachable!("retry loop exited without returning a result — this is a bug")
    }

    /// `true` when any security-hardening option requires the custom send path.
    const fn needs_custom_path(&self) -> bool {
        self.ssrf_safe
            || self.pin_addr.is_some()
            || !matches!(self.redirect_mode, RedirectMode::Default)
    }

    /// Dispatch to the appropriate custom send path. Consumes `self`.
    async fn send_custom(self) -> Result<Response, ClientError> {
        let timeout = self
            .retry_policy
            .request_timeout
            .unwrap_or_else(|| Duration::from_secs(30));

        if self.ssrf_safe {
            return self.send_ssrf_safe(timeout).await;
        }

        // Extract the follow parameters (ending the borrow) before moving `self`.
        let follow = match &self.redirect_mode {
            RedirectMode::Follow { max, validator } => Some((*max, validator.clone())),
            RedirectMode::None | RedirectMode::Default => None,
        };
        if let Some((max, validator)) = follow {
            return self.follow_loop(max, validator, timeout).await;
        }

        // `None`: single one-shot request with Policy::none() so a 3xx is
        // returned verbatim. `Default` (pin_to only): keep reqwest's built-in
        // auto-follow but pin the initial host's resolution.
        let policy = match self.redirect_mode {
            RedirectMode::None => reqwest::redirect::Policy::none(),
            RedirectMode::Default | RedirectMode::Follow { .. } => {
                reqwest::redirect::Policy::default()
            }
        };
        let resolve = self.pin_resolve()?;
        let client = build_oneshot_client(resolve, policy, timeout)?;
        send_one(
            &client,
            &self.method,
            &self.url,
            &self.extra_headers,
            self.body.as_ref(),
            &self.retry_policy,
        )
        .await
    }

    /// Compute the `(host, addr)` resolve override for a pinned request, if any.
    fn pin_resolve(&self) -> Result<Option<(String, SocketAddr)>, ClientError> {
        match self.pin_addr {
            Some(addr) => Ok(Some((host_of(&self.url)?, addr))),
            None => Ok(None),
        }
    }

    /// Manual redirect-following loop with per-hop validation (Feature #1238).
    async fn follow_loop(
        self,
        max: usize,
        validator: RedirectValidator,
        timeout: Duration,
    ) -> Result<Response, ClientError> {
        let mut current = self.url.clone();
        for hop in 0.. {
            // Pin only applies to the first hop's original target.
            let resolve = if hop == 0 {
                match self.pin_addr {
                    Some(addr) => Some((host_of(&current)?, addr)),
                    None => None,
                }
            } else {
                None
            };
            let client = build_oneshot_client(resolve, reqwest::redirect::Policy::none(), timeout)?;
            // Only send the body on the first hop; a redirect drops it.
            let body = if hop == 0 { self.body.as_ref() } else { None };
            let resp = send_one(
                &client,
                &self.method,
                &current,
                &self.extra_headers,
                body,
                &self.retry_policy,
            )
            .await?;

            let Some(next) = redirect_target(&resp, &current)? else {
                return Ok(resp);
            };
            if hop >= max {
                return Err(ClientError::TooManyRedirects(max));
            }
            if !validator(&next) {
                return Err(ClientError::RedirectRejected(next));
            }
            current = next;
        }
        unreachable!("redirect loop is bounded by `max` and always returns")
    }

    /// Composed SSRF-safe send path (Features #1238 + #1239). Resolves and
    /// validates every hop, pins the connection, and rejects scheme downgrades.
    async fn send_ssrf_safe(self, timeout: Duration) -> Result<Response, ClientError> {
        let mut current = self.url.clone();
        for hop in 0.. {
            // Resolve host → validated address (rejects if ANY resolved IP is
            // blocked), then pin so reqwest cannot re-resolve.
            let addr = resolve_and_validate(&current).await?;
            let host = host_of(&current)?;
            let client = build_oneshot_client(
                Some((host, addr)),
                reqwest::redirect::Policy::none(),
                timeout,
            )?;
            let body = if hop == 0 { self.body.as_ref() } else { None };
            let resp = send_one(
                &client,
                &self.method,
                &current,
                &self.extra_headers,
                body,
                &self.retry_policy,
            )
            .await?;

            let Some(next) = redirect_target(&resp, &current)? else {
                return Ok(resp);
            };
            if hop >= SSRF_SAFE_MAX_REDIRECTS {
                return Err(ClientError::TooManyRedirects(SSRF_SAFE_MAX_REDIRECTS));
            }
            // Defence-in-depth: reject an https→http downgrade on redirect.
            if scheme_is_https(&current)? && !scheme_is_https(&next)? {
                return Err(ClientError::RedirectRejected(format!(
                    "https→http scheme downgrade on redirect to {next}"
                )));
            }
            current = next;
            // The next loop iteration re-resolves + re-validates `current`
            // before connecting, so a redirect to a blocked address is rejected
            // there with `SsrfBlocked`.
        }
        unreachable!("redirect loop is bounded by SSRF_SAFE_MAX_REDIRECTS")
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

const fn is_idempotent_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS | Method::TRACE
    )
}

const fn is_retryable_status(status: u16) -> bool {
    matches!(status, 502..=504)
}

// ── Custom send-path helpers (redirect / pin / SSRF-safe) ─────────────────────

/// Build a one-shot `reqwest::Client` for the custom send path, with the given
/// redirect policy, per-request timeout, and optional DNS `resolve` override.
fn build_oneshot_client(
    resolve: Option<(String, SocketAddr)>,
    policy: reqwest::redirect::Policy,
    timeout: Duration,
) -> Result<reqwest::Client, ClientError> {
    let mut builder = reqwest::ClientBuilder::new()
        .timeout(timeout)
        .redirect(policy);
    if let Some((host, addr)) = resolve {
        builder = builder.resolve(&host, addr);
    }
    builder.build().map_err(ClientError::Request)
}

/// Send a single request through `client` (no manual redirect following — the
/// client's redirect policy governs that) with the same transient-error and
/// 429/5xx retry behaviour as the shared path, and collect the [`Response`].
async fn send_one(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    extra_headers: &HeaderMap,
    body: Option<&Bytes>,
    retry_policy: &RetryPolicy,
) -> Result<Response, ClientError> {
    let start = Instant::now();
    let max_attempts = if is_idempotent_method(method) || !retry_policy.retry_idempotent_only {
        retry_policy.max_retries.saturating_add(1)
    } else {
        1
    };

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let exp = (attempt - 1).min(10);
            let delay = Duration::from_millis(100 * (1_u64 << exp));
            tokio::time::sleep(delay).await;
        }

        let mut req = client.request(method.clone(), url);
        req = inject_trace_context(req);
        for (name, value) in extra_headers {
            req = req.header(name.clone(), value.clone());
        }
        if let Some(body) = body {
            req = req.body(body.clone());
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let headers = resp.headers().clone();
                let url_used = resp.url().clone();

                if status.as_u16() == 429 && attempt + 1 < max_attempts {
                    let mut sleep_delay =
                        parse_retry_after(&headers).unwrap_or(Duration::from_secs(1));
                    sleep_delay = sleep_delay.min(retry_policy.max_retry_after);
                    if let Some(req_timeout) = retry_policy.request_timeout {
                        sleep_delay = sleep_delay.min(req_timeout);
                    }
                    tokio::time::sleep(sleep_delay).await;
                    continue;
                }
                if is_retryable_status(status.as_u16()) && attempt + 1 < max_attempts {
                    continue;
                }

                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| ClientError::Request(e.without_url()))?;
                log_request(
                    method.as_str(),
                    &url_used,
                    status.as_u16(),
                    start.elapsed(),
                    extra_headers,
                );
                return Ok(Response {
                    status,
                    headers,
                    body,
                    url: Some(url_used),
                });
            }
            Err(e) if (e.is_connect() || e.is_timeout()) && attempt + 1 < max_attempts => {}
            Err(e) => return Err(ClientError::Request(e.without_url())),
        }
    }

    unreachable!("retry loop exited without returning a result — this is a bug")
}

/// Extract the host portion of a URL as an owned `String`.
fn host_of(url: &str) -> Result<String, ClientError> {
    let parsed = url::Url::parse(url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    parsed
        .host_str()
        .map(str::to_owned)
        .ok_or_else(|| ClientError::InvalidUrl(format!("URL has no host: {url}")))
}

/// `true` when the URL's scheme is `https` (case-insensitive).
fn scheme_is_https(url: &str) -> Result<bool, ClientError> {
    let parsed = url::Url::parse(url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    Ok(parsed.scheme().eq_ignore_ascii_case("https"))
}

/// If `resp` is a `3xx` carrying a `Location` header, resolve it to an absolute
/// URL (joining relative locations against `base`). Returns `Ok(None)` when the
/// response is not a followable redirect (non-3xx, or 3xx without `Location`).
fn redirect_target(resp: &Response, base: &str) -> Result<Option<String>, ClientError> {
    if !resp.status().is_redirection() {
        return Ok(None);
    }
    let Some(location) = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    let base_url = url::Url::parse(base).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    let joined = base_url
        .join(location)
        .map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    Ok(Some(joined.to_string()))
}

/// Resolve `url`'s host to a single validated [`SocketAddr`], rejecting with
/// [`ClientError::SsrfBlocked`] if the host is (or resolves to) any blocked IP.
///
/// IP-literal hosts (including decimal/octal/hex encodings, which the `url`
/// crate normalises to an `Ipv4Addr` at parse time) are validated directly with
/// no DNS lookup. Domain hosts are resolved once via `tokio::net::lookup_host`
/// and rejected if **any** resolved address is blocked.
async fn resolve_and_validate(url: &str) -> Result<SocketAddr, ClientError> {
    let parsed = url::Url::parse(url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    let port = parsed.port_or_known_default().ok_or_else(|| {
        ClientError::InvalidUrl(format!("URL has no port and unknown scheme: {url}"))
    })?;
    let host = parsed
        .host()
        .ok_or_else(|| ClientError::InvalidUrl(format!("URL has no host: {url}")))?;

    match host {
        url::Host::Ipv4(v4) => {
            let ip = IpAddr::V4(v4);
            if is_blocked_ip(ip) {
                return Err(ClientError::SsrfBlocked(ip.to_string()));
            }
            Ok(SocketAddr::new(ip, port))
        }
        url::Host::Ipv6(v6) => {
            let ip = IpAddr::V6(v6);
            if is_blocked_ip(ip) {
                return Err(ClientError::SsrfBlocked(ip.to_string()));
            }
            Ok(SocketAddr::new(ip, port))
        }
        url::Host::Domain(name) => {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name, port))
                .await
                .map_err(|e| ClientError::InvalidUrl(format!("DNS lookup failed for {name}: {e}")))?
                .collect();
            if addrs.is_empty() {
                return Err(ClientError::InvalidUrl(format!(
                    "DNS lookup for {name} returned no addresses"
                )));
            }
            // Reject if ANY resolved address is blocked (fail closed).
            for addr in &addrs {
                if is_blocked_ip(addr.ip()) {
                    return Err(ClientError::SsrfBlocked(addr.ip().to_string()));
                }
            }
            Ok(addrs[0])
        }
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    // Integer seconds (most common form).
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP-date format per RFC 9110 (e.g. "Tue, 01 Jan 2030 00:00:00 GMT").
    let dt = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let now = chrono::Utc::now();
    let future = dt.with_timezone(&chrono::Utc);
    let secs = u64::try_from((future - now).num_seconds().max(0)).unwrap_or(0);
    Some(Duration::from_secs(secs))
}

const REDACTED_HEADERS: &[&str] = &["authorization", "cookie", "set-cookie"];

fn is_sensitive_header(name: &str) -> bool {
    REDACTED_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

fn log_request(
    method: &str,
    url: &reqwest::Url,
    status: u16,
    elapsed: Duration,
    headers: &HeaderMap,
) {
    let host = url.host_str().unwrap_or("unknown");
    let path = url.path();

    // Collect non-sensitive header names for the span (values are omitted).
    let sent_headers: Vec<&str> = headers
        .keys()
        .map(HeaderName::as_str)
        .filter(|k| !is_sensitive_header(k))
        .collect();

    tracing::info!(
        http.method = method,
        http.host = host,
        http.path = path,
        http.status = status,
        http.elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        http.sent_headers = ?sent_headers,
        "outbound request"
    );
}

/// Inject the active span's W3C `traceparent` / `tracestate` headers into the
/// request builder.  No-ops when the `telemetry-otlp` feature is disabled or
/// when there is no active span with a valid context.
#[allow(clippy::missing_const_for_fn)]
fn inject_trace_context(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    #[cfg(not(feature = "telemetry-otlp"))]
    {
        builder
    }
    #[cfg(feature = "telemetry-otlp")]
    {
        use std::collections::HashMap;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let cx = tracing::Span::current().context();
        let mut map = HashMap::<String, String>::new();
        opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&cx, &mut TraceHeaderInjector(&mut map));
        });
        let mut builder = builder;
        for (k, v) in map {
            if let Ok(value) = HeaderValue::from_str(&v) {
                builder = builder.header(k, value);
            }
        }
        builder
    }
}

#[cfg(feature = "telemetry-otlp")]
struct TraceHeaderInjector<'a>(&'a mut std::collections::HashMap<String, String>);

#[cfg(feature = "telemetry-otlp")]
impl opentelemetry::propagation::Injector for TraceHeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HttpClientConfig;

    // RED-PHASE TEST 1: Client can be constructed with defaults.
    #[test]
    fn client_constructs_with_defaults() {
        let client = Client::new();
        assert!(client.alias.is_none());
        assert!(client.base_url.is_none());
        assert_eq!(client.retry_policy.max_retries, 3);
    }

    // RED-PHASE TEST 2: Fluent RequestBuilder API compiles.
    #[test]
    fn request_builder_fluent_api_compiles() {
        let client = Client::new();
        let _builder = client
            .post("https://example.com/api")
            .header("x-api-key", "secret")
            .json(&serde_json::json!({"key": "value"}))
            .retries(2);
    }

    // RED-PHASE TEST 3: Response accessors work.
    #[test]
    fn response_accessors_work() {
        let payload = serde_json::json!({"id": 42, "name": "Alice"});
        let body = serde_json::to_vec(&payload).unwrap();
        let resp = Response {
            status: reqwest::StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(body),
            url: None,
        };
        assert_eq!(resp.status().as_u16(), 200);
        assert!(resp.is_success());
    }

    // RED-PHASE TEST 4: Response::json() deserialises correctly.
    #[test]
    fn response_json_deserialises() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct User {
            id: i32,
            name: String,
        }
        let payload = serde_json::json!({"id": 1, "name": "Bob"});
        let resp = Response {
            status: reqwest::StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(serde_json::to_vec(&payload).unwrap()),
            url: None,
        };
        let user: User = resp.json().unwrap();
        assert_eq!(user.id, 1);
        assert_eq!(user.name, "Bob");
    }

    // RED-PHASE TEST 5: Response::text() returns UTF-8 string.
    #[test]
    fn response_text_returns_string() {
        let resp = Response {
            status: reqwest::StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"hello world"),
            url: None,
        };
        assert_eq!(resp.text(), "hello world");
    }

    // RED-PHASE TEST 6: Response::bytes() returns raw bytes.
    #[test]
    fn response_bytes_returns_raw() {
        let resp = Response {
            status: reqwest::StatusCode::CREATED,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"\x00\x01\x02"),
            url: None,
        };
        assert_eq!(resp.bytes(), Bytes::from_static(b"\x00\x01\x02"));
    }

    // RED-PHASE TEST 7: HttpClientConfig deserialises from [http.client] TOML.
    #[test]
    fn config_deserialises_from_toml() {
        // Simulate the [http.client] section as it appears in autumn.toml.
        let toml = r#"
            [client]
            timeout_secs = 60
            max_retries = 5
            [client.base_urls]
            stripe = "https://api.stripe.com"
            sendgrid = "https://api.sendgrid.com"
        "#;
        let http_cfg: crate::config::HttpConfig = toml::from_str(toml).unwrap();
        let config = &http_cfg.client;
        assert_eq!(config.timeout_secs, 60);
        assert_eq!(config.max_retries, 5);
        assert_eq!(
            config.base_urls.get("stripe").map(String::as_str),
            Some("https://api.stripe.com")
        );
        assert_eq!(
            config.base_urls.get("sendgrid").map(String::as_str),
            Some("https://api.sendgrid.com")
        );
    }

    // RED-PHASE TEST 8: HttpClientConfig has correct defaults.
    #[test]
    fn config_has_correct_defaults() {
        let config = HttpClientConfig::default();
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.max_retries, 3);
        assert!(config.base_urls.is_empty());
    }

    // RED-PHASE TEST 9: is_idempotent_method returns correct values.
    #[test]
    fn idempotent_method_classification() {
        assert!(is_idempotent_method(&Method::GET));
        assert!(is_idempotent_method(&Method::HEAD));
        assert!(is_idempotent_method(&Method::PUT));
        assert!(is_idempotent_method(&Method::DELETE));
        assert!(is_idempotent_method(&Method::OPTIONS));
        assert!(is_idempotent_method(&Method::TRACE));
        assert!(!is_idempotent_method(&Method::POST));
        assert!(!is_idempotent_method(&Method::PATCH));
    }

    // RED-PHASE TEST 10: is_retryable_status returns correct values.
    #[test]
    fn retryable_status_classification() {
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(500));
        assert!(!is_retryable_status(429));
    }

    // RED-PHASE TEST 11: parse_retry_after parses seconds correctly.
    #[test]
    fn retry_after_header_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("retry-after"),
            HeaderValue::from_static("5"),
        );
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(5)));

        let empty = HeaderMap::new();
        assert_eq!(parse_retry_after(&empty), None);
    }

    // RED-PHASE TEST 12: Sensitive header detection.
    #[test]
    fn sensitive_header_detection() {
        assert!(is_sensitive_header("authorization"));
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("AUTHORIZATION"));
        assert!(is_sensitive_header("cookie"));
        assert!(is_sensitive_header("set-cookie"));
        assert!(!is_sensitive_header("content-type"));
        assert!(!is_sensitive_header("x-api-key"));
    }

    // RED-PHASE TEST 13: MockRegistry captures and matches calls.
    #[tokio::test]
    async fn mock_registry_captures_calls() {
        let registry = Arc::new(MockRegistry::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        registry.register(MockEntry {
            method: Some(Method::POST),
            path: "/charges".to_owned(),
            alias: Some("stripe".to_owned()),
            status: 200,
            body: Some(serde_json::json!({"id": "ch_123"})),
            call_count: call_count.clone(),
        });

        let client = Client::new().with_mock(registry).named("stripe");

        let resp = client
            .post("https://api.stripe.com/charges")
            .json(&serde_json::json!({"amount": 1000}))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["id"], "ch_123");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    // RED-PHASE TEST 14: MockHandle::expect_called passes when count matches.
    #[tokio::test]
    async fn mock_handle_expect_called_passes() {
        let registry = Arc::new(MockRegistry::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        registry.register(MockEntry {
            method: Some(Method::GET),
            path: "/users/1".to_owned(),
            alias: None,
            status: 200,
            body: Some(serde_json::json!({"name": "Alice"})),
            call_count: call_count.clone(),
        });

        let handle = MockHandle {
            alias: "test".to_owned(),
            method: "GET".to_owned(),
            path: "/users/1".to_owned(),
            call_count: call_count.clone(),
        };

        let client = Client::new().with_mock(registry);
        client
            .get("https://api.example.com/users/1")
            .send()
            .await
            .unwrap();

        handle.expect_called(1);
        assert_eq!(handle.call_count(), 1);
    }

    // RED-PHASE TEST 15: MockRegistry matches by URL path suffix.
    #[tokio::test]
    async fn mock_matches_by_path_suffix() {
        let registry = Arc::new(MockRegistry::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        registry.register(MockEntry {
            method: Some(Method::POST),
            path: "/v1/charges".to_owned(),
            alias: None,
            status: 201,
            body: Some(serde_json::json!({"created": true})),
            call_count: call_count.clone(),
        });

        let client = Client::new().with_mock(registry);
        let resp = client
            .post("https://api.stripe.com/v1/charges")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 201);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    // RED-PHASE TEST 16: NoMock error when mock registry has no match.
    #[tokio::test]
    async fn no_mock_error_when_unmatched() {
        let registry = Arc::new(MockRegistry::new());
        let client = Client::new().with_mock(registry);
        let result = client.post("https://api.example.com/unknown").send().await;
        assert!(matches!(result, Err(ClientError::NoMock(_, _))));
    }

    // RED-PHASE TEST 17: MockSetupBuilder registers and returns MockHandle.
    #[tokio::test]
    async fn mock_setup_builder_registers_entry() {
        let registry = Arc::new(MockRegistry::new());
        let builder = MockSetupBuilder {
            registry: registry.clone(),
            alias: "myservice".to_owned(),
            method: None,
            path: None,
        };

        let handle = builder
            .post("/api/resource")
            .respond_with(201, serde_json::json!({"ok": true}));

        let client = Client::new().with_mock(registry).named("myservice");
        client
            .post("https://myservice.example.com/api/resource")
            .send()
            .await
            .unwrap();

        handle.expect_called(1);
    }

    // RED-PHASE TEST 18: Client::from_config respects timeout and retries.
    #[test]
    fn client_from_config() {
        let config = HttpClientConfig {
            timeout_secs: 10,
            max_retries: 1,
            max_retry_after_secs: 10,
            base_urls: std::collections::HashMap::new(),
        };
        let client = Client::from_config(&config);
        assert_eq!(client.retry_policy.max_retries, 1);
    }

    // RED-PHASE TEST 19: Client.named() preserves mock registry.
    #[test]
    fn named_client_preserves_mock_registry() {
        let registry = Arc::new(MockRegistry::new());
        let client = Client::new().with_mock(registry);
        let named = client.named("stripe");
        assert!(named.mock.is_some());
        assert_eq!(named.alias.as_deref(), Some("stripe"));
    }

    // RED-PHASE TEST 20: base_url is prepended to relative paths.
    #[test]
    fn base_url_prepended_to_relative_path() {
        let client = Client::new();
        let client = client.with_base_url("https://api.stripe.com");
        let builder = client.post("/v1/charges");
        assert_eq!(builder.url, "https://api.stripe.com/v1/charges");
    }

    // RED-PHASE TEST 21: Absolute URLs bypass base_url.
    #[test]
    fn absolute_url_bypasses_base_url() {
        let client = Client::new().with_base_url("https://ignored.example.com");
        let builder = client.get("https://actual.example.com/path");
        assert_eq!(builder.url, "https://actual.example.com/path");
    }

    // RED-PHASE TEST 22: RetryPolicy can be overridden per-request.
    #[test]
    fn retry_override_per_request() {
        let client = Client::new(); // default: 3 retries
        let builder = client.get("https://example.com").retries(0);
        assert_eq!(builder.retry_policy.max_retries, 0);

        let no_retry = client.get("https://example.com").no_retry();
        assert_eq!(no_retry.retry_policy.max_retries, 0);
    }

    // RED-PHASE TEST 23: Client extracts from AppState.
    #[tokio::test]
    async fn client_extracts_from_state() {
        use axum::extract::FromRequestParts;
        let state = crate::AppState::for_test();
        let mut parts = axum::http::Request::new(axum::body::Body::empty())
            .into_parts()
            .0;
        let client = Client::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        // Default client: no mock, no alias
        assert!(client.mock.is_none());
        assert!(client.alias.is_none());
    }

    // RED-PHASE TEST 24: MockRegistryExt round-trips through AppState extensions.
    #[test]
    fn mock_registry_ext_round_trips_through_state() {
        let registry = Arc::new(MockRegistry::new());
        let ext = HttpMockRegistryExt(registry);
        let state = crate::AppState::for_test();
        state.insert_extension(ext);
        let retrieved = state.extension::<HttpMockRegistryExt>();
        assert!(retrieved.is_some());
    }

    // TEST 25: named() resolves base URL from base_urls map in config.
    #[test]
    fn named_client_resolves_base_url_from_config() {
        let mut base_urls = std::collections::HashMap::new();
        base_urls.insert("stripe".to_owned(), "https://api.stripe.com".to_owned());
        let config = HttpClientConfig {
            timeout_secs: 30,
            max_retries: 3,
            max_retry_after_secs: 10,
            base_urls,
        };
        let client = Client::from_config(&config);
        let stripe = client.named("stripe");
        assert_eq!(stripe.base_url.as_deref(), Some("https://api.stripe.com"));
        assert_eq!(stripe.alias.as_deref(), Some("stripe"));

        // Unknown alias falls back to client-level base_url (None in this case).
        let other = client.named("sendgrid");
        assert!(other.base_url.is_none());
    }

    // TEST 26: from_request_parts uses AutumnConfig.http when no HttpConfig extension.
    #[tokio::test]
    async fn client_extracts_from_autumn_config_in_state() {
        use axum::extract::FromRequestParts;
        let mut cfg = crate::config::AutumnConfig::default();
        cfg.http.client.max_retries = 7;
        let state = crate::AppState::for_test();
        state.insert_extension(cfg);

        let mut parts = axum::http::Request::new(axum::body::Body::empty())
            .into_parts()
            .0;
        let client = Client::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert_eq!(client.retry_policy.max_retries, 7);
    }

    // TEST 27: respond_with_status produces a truly empty body (not JSON null).
    #[tokio::test]
    async fn respond_with_status_produces_empty_body() {
        let registry = Arc::new(MockRegistry::new());
        let builder = MockSetupBuilder {
            registry: registry.clone(),
            alias: "svc".to_owned(),
            method: None,
            path: None,
        };
        let _handle = builder.delete("/items/1").respond_with_status(204);

        let client = Client::new().with_mock(registry).named("svc");
        let resp = client
            .delete("https://svc.example.com/items/1")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 204);
        assert_eq!(
            resp.bytes(),
            bytes::Bytes::new(),
            "body must be empty, not \"null\""
        );
    }

    // TEST 28: parse_retry_after handles HTTP-date format.
    #[test]
    fn retry_after_http_date_parsing() {
        let mut headers = HeaderMap::new();
        // A date far in the future to ensure the computed seconds > 0.
        headers.insert(
            reqwest::header::HeaderName::from_static("retry-after"),
            HeaderValue::from_static("Tue, 01 Jan 2030 00:00:00 GMT"),
        );
        let duration = parse_retry_after(&headers);
        assert!(duration.is_some(), "should parse HTTP-date Retry-After");
        assert!(
            duration.unwrap().as_secs() > 0,
            "future date should yield positive delay"
        );
    }

    // TEST 29: non-idempotent POST with retries disabled makes only one attempt.
    #[tokio::test]
    async fn non_idempotent_post_no_retry() {
        let registry = Arc::new(MockRegistry::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        registry.register(MockEntry {
            method: Some(Method::POST),
            path: "/endpoint".to_owned(),
            alias: None,
            status: 503,
            body: None,
            call_count: call_count.clone(),
        });

        // With retry_idempotent_only=true (default), POST should NOT retry.
        let client = Client::new().with_mock(registry);
        let resp = client
            .post("https://example.com/endpoint")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 503);
        // Mock was called exactly once — no retry for non-idempotent method.
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    // TEST 30: find_match strips query string from relative URLs before comparing.
    #[tokio::test]
    async fn mock_strips_query_from_url_before_matching() {
        let registry = Arc::new(MockRegistry::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        registry.register(MockEntry {
            method: Some(Method::GET),
            path: "/v1/charges".to_owned(),
            alias: None,
            status: 200,
            body: Some(serde_json::json!({"ok": true})),
            call_count: call_count.clone(),
        });

        // The URL has a query string; the mock is registered without one.
        let client = Client::new().with_mock(registry);
        let resp = client
            .get("https://api.stripe.com/v1/charges?expand[]=balance_transaction")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    // TEST 31: suffix match works when mock path starts with '/' and URL has a prefix.
    #[tokio::test]
    async fn mock_suffix_match_with_leading_slash_path() {
        let registry = Arc::new(MockRegistry::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        // Register only the leaf segment (with leading slash).
        registry.register(MockEntry {
            method: Some(Method::POST),
            path: "/charges".to_owned(),
            alias: None,
            status: 201,
            body: Some(serde_json::json!({"matched": true})),
            call_count: call_count.clone(),
        });

        let client = Client::new().with_mock(registry);
        // Full URL path is /v1/charges; mock path is /charges.
        let resp = client
            .post("https://api.stripe.com/v1/charges")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 201);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    // TEST 32: retries() clears retry_idempotent_only so POST actually retries.
    #[test]
    fn retries_clears_idempotent_only_flag() {
        let client = Client::new();
        let builder = client.post("https://example.com").retries(2);
        assert_eq!(builder.retry_policy.max_retries, 2);
        assert!(
            !builder.retry_policy.retry_idempotent_only,
            "explicit retries() call must allow non-idempotent methods to retry"
        );
    }

    // TEST 33: log_request covers the sensitive-header redaction path.
    #[test]
    fn log_request_completes_with_sensitive_headers() {
        let url = reqwest::Url::parse("https://api.example.com/v1/resource?q=1").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer sk_test_xxx"),
        );
        // Should complete without panicking; authorization is redacted from span.
        log_request("POST", &url, 201, Duration::from_millis(12), &headers);
    }

    // TEST 34: inject_trace_context passthrough (without telemetry-otlp feature).
    #[test]
    fn inject_trace_context_passthrough_without_telemetry() {
        let inner = reqwest::Client::new();
        let builder = inner.get("https://example.com");
        // Without telemetry-otlp the function is a no-op; verify it doesn't panic.
        let _b = inject_trace_context(builder);
    }

    // TEST 35: Real GET request exercises inject_trace_context, log_request, and
    // the success branch of the retry loop.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn real_get_request_covers_network_path() {
        use axum::{Router, routing::get};

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let app = Router::new().route("/ping", get(|| async { "pong" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/ping", addr.port()))
            .header("x-request-id", "test-35")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert!(resp.url().is_some());
        assert_eq!(resp.text(), "pong");

        crate::circuit_breaker::global_registry().clear();
    }

    // TEST 36: Real POST with JSON body covers the body-sending code path.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn real_post_with_json_body_covers_body_path() {
        use axum::{Json, Router, routing::post};
        use serde_json::Value;

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let app = Router::new().route(
            "/echo",
            post(|Json(body): Json<Value>| async move { Json(body) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{}/echo", addr.port()))
            .json(&serde_json::json!({"hello": "world"}))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        let body: Value = resp.json().unwrap();
        assert_eq!(body["hello"], "world");

        crate::circuit_breaker::global_registry().clear();
    }

    // TEST 37: GET with one 503 then 200 covers the retry-sleep and 5xx-retry paths.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn real_get_retries_on_503_then_succeeds() {
        use axum::{Router, routing::get};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering as SeqOrdering};

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let hit = Arc::new(AtomicU32::new(0));
        let hit2 = hit.clone();
        let app = Router::new().route(
            "/flaky",
            get(move || {
                let c = hit2.clone();
                async move {
                    if c.fetch_add(1, SeqOrdering::SeqCst) == 0 {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // retries(1): 2 total attempts, 100 ms sleep between them.
        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/flaky", addr.port()))
            .retries(1)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(hit.load(SeqOrdering::SeqCst), 2);

        crate::circuit_breaker::global_registry().clear();
    }

    // TEST 38: text_body sets a plain-text body.
    #[test]
    fn text_body_sets_body() {
        let client = Client::new();
        let builder = client.post("https://example.com").text_body("hello");
        assert_eq!(builder.body, Some(bytes::Bytes::from_static(b"hello")));
    }

    // TEST 39: ClientError::NoMock displays correctly.
    #[test]
    fn client_error_display() {
        let err = ClientError::NoMock("GET".to_owned(), "/path".to_owned());
        assert!(err.to_string().contains("GET"));
        assert!(err.to_string().contains("/path"));
    }

    // TEST 40: Outbound circuit breaker integration trips and fails fast.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_http_client_circuit_breaker_integration() {
        use axum::{Router, routing::get};
        use std::sync::atomic::{AtomicU32, Ordering as SeqOrdering};

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let hit = Arc::new(AtomicU32::new(0));
        let hit2 = hit.clone();
        let app = Router::new().route(
            "/flaky",
            get(move || {
                let c = hit2.clone();
                async move {
                    c.fetch_add(1, SeqOrdering::SeqCst);
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Build a resilience config with custom thresholds
        let mut rc = crate::config::ResilienceConfig::default();
        rc.circuit_breaker.defaults.failure_ratio_threshold = Some(0.5);
        rc.circuit_breaker.defaults.minimum_sample_count = Some(3);
        rc.circuit_breaker.defaults.open_duration_secs = Some(10);

        let client = Client::new();
        // Attach the resilience config
        let client = Client {
            resilience_config: Some(Arc::new(rc)),
            ..client
        };

        let url = format!("http://127.0.0.1:{}/flaky", addr.port());

        // Send 3 requests (all fail with 500)
        for _ in 0..3 {
            let res = client.get(&url).send().await;
            let res = res.unwrap();
            assert_eq!(res.status().as_u16(), 500);
        }

        // Now the breaker for 127.0.0.1 should be OPEN, and next request should fail fast
        let res = client.get(&url).send().await;
        assert!(matches!(res, Err(ClientError::CircuitBreakerOpen)));

        // Assert that the server was only hit 3 times
        assert_eq!(hit.load(SeqOrdering::SeqCst), 3);
        crate::circuit_breaker::global_registry().clear();
    }

    // RED-PHASE TEST 41: SharedReqwestClient round-trips through AppState extensions.
    #[test]
    fn shared_reqwest_client_ext_round_trips() {
        let ext = SharedReqwestClient {
            client: reqwest::Client::new(),
            timeout_secs: 30,
        };
        let state = crate::AppState::for_test();
        state.insert_extension(ext);
        let retrieved = state.extension::<SharedReqwestClient>();
        assert!(retrieved.is_some());
    }

    // RED-PHASE TEST 42: Client::head() compiles and builds a HEAD RequestBuilder.
    #[test]
    fn client_head_method_builds_request_builder() {
        let client = Client::new();
        let _builder = client.head("https://example.com/resource");
    }

    // RED-PHASE TEST 43: from_state reuses the SharedReqwestClient when registered.
    // Spins up a local echo server that returns the User-Agent header as the body,
    // then asserts the extracted Client carries the distinctive user-agent we set
    // on the shared inner client — proving from_state cloned it rather than
    // building a fresh default.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn from_state_reuses_shared_client() {
        use axum::{Router, routing::get};

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let app = Router::new().route(
            "/ua",
            get(|req: axum::http::Request<axum::body::Body>| async move {
                req.headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let distinctive_inner = reqwest::ClientBuilder::new()
            .user_agent("autumn-shared-pool-test")
            .build()
            .expect("failed to build inner client");
        let state = crate::AppState::for_test();
        state.insert_extension(SharedReqwestClient {
            client: distinctive_inner,
            timeout_secs: 30,
        });

        let client = Client::from_state(&state);
        let resp = client
            .get(format!("http://127.0.0.1:{}/ua", addr.port()))
            .send()
            .await
            .expect("request should succeed");

        assert_eq!(resp.text(), "autumn-shared-pool-test");
        crate::circuit_breaker::global_registry().clear();
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn from_state_falls_back_when_timeout_mismatches_shared_client() {
        // SharedReqwestClient was built with 5s; config says 10s → mismatch
        // → from_state must not reuse the shared inner (falls through to
        //   from_config which builds a fresh reqwest::Client).
        use crate::config::{AutumnConfig, HttpClientConfig};
        use std::sync::Arc;

        let mut config = AutumnConfig::default();
        config.http.client = HttpClientConfig {
            timeout_secs: 10,
            ..Default::default()
        };

        let state = crate::AppState::for_test();
        state.insert_extension(SharedReqwestClient {
            client: reqwest::Client::new(),
            timeout_secs: 5, // deliberately different from config
        });
        state.insert_extension(Arc::new(config));

        // Should not panic — falls back to building a fresh client.
        let _client = Client::from_state(&state);
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn from_state_reuses_shared_client_when_no_config() {
        // No HttpConfig/AutumnConfig in state, but SharedReqwestClient is
        // present → hits the (None, Some(inner)) arm → with_inner.
        let state = crate::AppState::for_test();
        // Default timeout_secs from HttpClientConfig matches the default used
        // in effective_timeout_secs, so the shared client is reused.
        let default_timeout = crate::config::HttpClientConfig::default().timeout_secs;
        state.insert_extension(SharedReqwestClient {
            client: reqwest::Client::new(),
            timeout_secs: default_timeout,
        });

        let _client = Client::from_state(&state);
    }

    // ── Security-hardening tests (#1238 redirects, #1239 SSRF/pinning) ────────

    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    // TEST 44: SSRF address policy — blocked ranges.
    #[test]
    fn ssrf_policy_blocks_private_and_reserved_ipv4() {
        let blocked = [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",      // CGNAT
            "127.0.0.1",       // loopback
            "169.254.169.254", // cloud metadata
            "172.16.5.4",      // private
            "192.0.0.1",       // IETF
            "192.0.2.5",       // TEST-NET-1
            "192.168.1.1",     // private
            "198.18.0.1",      // benchmarking
            "198.51.100.7",    // TEST-NET-2
            "203.0.113.9",     // TEST-NET-3
            "224.0.0.1",       // multicast
            "240.0.0.1",       // reserved
            "255.255.255.255", // broadcast
        ];
        for s in blocked {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_blocked_ip(ip), "{s} should be blocked");
            assert!(!is_public_ip(ip), "{s} should not be public");
        }
    }

    // TEST 45: SSRF address policy — public IPv4 is allowed.
    #[test]
    fn ssrf_policy_allows_public_ipv4() {
        for s in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_public_ip(ip), "{s} should be public");
            assert!(!is_blocked_ip(ip), "{s} should not be blocked");
        }
    }

    // TEST 46: SSRF address policy — IPv6 blocked ranges, mapped/compatible forms.
    #[test]
    fn ssrf_policy_ipv6_and_mapped_forms() {
        let blocked = [
            "::",                     // unspecified
            "::1",                    // loopback
            "fe80::1",                // link-local
            "fc00::1",                // ULA
            "ff02::1",                // multicast
            "2001:db8::1",            // documentation
            "::ffff:169.254.169.254", // IPv4-mapped metadata
            "::ffff:127.0.0.1",       // IPv4-mapped loopback
        ];
        for s in blocked {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_blocked_ip(ip), "{s} should be blocked");
        }
        // IPv4-compatible ::7f00:1 == 127.0.0.1 (deprecated form) is blocked.
        let compat = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0x7f00, 0x0001));
        assert!(
            is_blocked_ip(compat),
            "::7f00:1 (127.0.0.1) should be blocked"
        );

        // A real public IPv6 (Cloudflare DNS) is allowed.
        let public: IpAddr = "2606:4700:4700::1111".parse().unwrap();
        assert!(
            is_public_ip(public),
            "2606:4700:4700::1111 should be public"
        );
    }

    // TEST 47: the `url` crate normalises decimal/hex IP-literal hosts to Ipv4,
    // so `http://2130706433/` (== 127.0.0.1) is recognised as a blocked IP.
    #[test]
    fn ssrf_policy_decimal_encoded_host_is_blocked() {
        for raw in [
            "http://2130706433/",
            "http://0x7f000001/",
            "http://127.0.0.1/",
        ] {
            let parsed = url::Url::parse(raw).unwrap();
            match parsed.host() {
                Some(url::Host::Ipv4(v4)) => {
                    assert_eq!(
                        v4,
                        Ipv4Addr::LOCALHOST,
                        "{raw} should normalise to 127.0.0.1"
                    );
                    assert!(
                        is_blocked_ip(IpAddr::V4(v4)),
                        "{raw} host should be blocked"
                    );
                }
                other => panic!("{raw} did not parse to an Ipv4 host: {other:?}"),
            }
        }
    }

    // Small axum helper: spawn `app` on an ephemeral 127.0.0.1 port, return it.
    async fn spawn(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    fn redirect_302(location: String) -> axum::response::Response {
        axum::response::Response::builder()
            .status(302)
            .header("location", location)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    // TEST 48: no_redirect returns the 3xx verbatim without following it.
    #[tokio::test]
    async fn no_redirect_returns_3xx_unfollowed() {
        use axum::{Router, routing::get};
        let addr = spawn(Router::new().route(
            "/start",
            get(|| async { redirect_302("http://127.0.0.1:1/never".to_owned()) }),
        ))
        .await;

        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/start", addr.port()))
            .no_redirect()
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 302);
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("http://127.0.0.1:1/never")
        );
    }

    // TEST 49: follow_redirects follows a valid chain A→B and calls the validator.
    #[tokio::test]
    async fn follow_redirects_valid_chain_calls_validator() {
        use axum::{Router, routing::get};

        let b_addr = spawn(Router::new().route("/final", get(|| async { "final-body" }))).await;
        let b_port = b_addr.port();
        let a_addr = spawn(Router::new().route(
            "/start",
            get(move || async move { redirect_302(format!("http://127.0.0.1:{b_port}/final")) }),
        ))
        .await;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/start", a_addr.port()))
            .follow_redirects(5, move |_loc| {
                calls2.fetch_add(1, Ordering::SeqCst);
                true
            })
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text(), "final-body");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "validator called once per hop"
        );
    }

    // TEST 50: follow_redirects rejects a redirect to a private/blocked target,
    // and NEVER connects to that private address. Uses the SSRF IP policy as the
    // validator — structurally the same guard get_ssrf_safe applies per hop.
    #[tokio::test]
    async fn follow_redirects_rejects_private_target() {
        use axum::{Router, routing::get};
        use std::sync::atomic::AtomicBool;

        // A "private" server that must never be reached.
        let touched = Arc::new(AtomicBool::new(false));
        let touched2 = touched.clone();
        let priv_addr = spawn(Router::new().route(
            "/secret",
            get(move || {
                let t = touched2.clone();
                async move {
                    t.store(true, Ordering::SeqCst);
                    "SECRET"
                }
            }),
        ))
        .await;
        let priv_port = priv_addr.port();

        // Public-ish entrypoint that 302s to the private loopback target.
        let a_addr = spawn(Router::new().route(
            "/start",
            get(
                move || async move { redirect_302(format!("http://127.0.0.1:{priv_port}/secret")) },
            ),
        ))
        .await;

        let validator = |u: &str| -> bool {
            let Ok(p) = url::Url::parse(u) else {
                return false;
            };
            match p.host() {
                Some(url::Host::Ipv4(v4)) => is_public_ip(IpAddr::V4(v4)),
                Some(url::Host::Ipv6(v6)) => is_public_ip(IpAddr::V6(v6)),
                _ => true,
            }
        };

        let result = Client::new()
            .get(format!("http://127.0.0.1:{}/start", a_addr.port()))
            .follow_redirects(5, validator)
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::RedirectRejected(_))),
            "expected RedirectRejected, got {result:?}"
        );
        assert!(
            !touched.load(Ordering::SeqCst),
            "the private target must never be connected to"
        );
    }

    // TEST 51: follow_redirects with a chain longer than `max` → TooManyRedirects.
    #[tokio::test]
    async fn follow_redirects_cap_exceeded() {
        use axum::{Router, routing::get};

        // /loop always redirects back to itself → infinite chain.
        let addr =
            spawn(Router::new().route("/loop", get(|| async { redirect_302("/loop".to_owned()) })))
                .await;

        let result = Client::new()
            .get(format!("http://127.0.0.1:{}/loop", addr.port()))
            .follow_redirects(2, |_| true)
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::TooManyRedirects(2))),
            "expected TooManyRedirects(2), got {result:?}"
        );
    }

    // TEST 52: follow_redirects(0, ..) turns the first 3xx into TooManyRedirects.
    #[tokio::test]
    async fn follow_redirects_zero_max_errors_on_first_3xx() {
        use axum::{Router, routing::get};
        let addr = spawn(Router::new().route(
            "/start",
            get(|| async { redirect_302("http://127.0.0.1:1/x".to_owned()) }),
        ))
        .await;

        let result = Client::new()
            .get(format!("http://127.0.0.1:{}/start", addr.port()))
            .follow_redirects(0, |_| true)
            .send()
            .await;

        assert!(matches!(result, Err(ClientError::TooManyRedirects(0))));
    }

    // TEST 53: pin_to bypasses DNS and connects to the URL's port.
    //
    // The host `pinned.invalid` is guaranteed non-resolvable (.invalid TLD), yet
    // pinning to 127.0.0.1 reaches the listener — proving DNS was bypassed. The
    // pinned SocketAddr uses port 1 (which nothing listens on) while the URL uses
    // the real listener port; reaching the listener proves reqwest IGNORES the
    // resolve SocketAddr's port and connects to the URL's port instead.
    #[tokio::test]
    async fn pin_to_bypasses_dns_and_uses_url_port() {
        use axum::{Router, routing::get};
        let addr = spawn(Router::new().route("/ping", get(|| async { "pong" }))).await;
        let listener_port = addr.port();

        let resp = Client::new()
            .get(format!("http://pinned.invalid:{listener_port}/ping"))
            // Deliberately-wrong port (1) to probe reqwest's port handling.
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1))
            .send()
            .await
            .expect("pinned request should reach the loopback listener");

        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text(), "pong");
    }

    // TEST 54: get_ssrf_safe rejects a host that resolves to a blocked IP BEFORE
    // connecting. `localhost` resolves to 127.0.0.1 (and/or ::1), both blocked.
    // The listener's handler must never fire.
    #[tokio::test]
    async fn get_ssrf_safe_rejects_loopback_host_before_connecting() {
        use axum::{Router, routing::get};
        use std::sync::atomic::AtomicBool;

        let touched = Arc::new(AtomicBool::new(false));
        let touched2 = touched.clone();
        let addr = spawn(Router::new().route(
            "/x",
            get(move || {
                let t = touched2.clone();
                async move {
                    t.store(true, Ordering::SeqCst);
                    "reached"
                }
            }),
        ))
        .await;

        let result = Client::new()
            .get_ssrf_safe(format!("http://localhost:{}/x", addr.port()))
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::SsrfBlocked(_))),
            "expected SsrfBlocked, got {result:?}"
        );
        assert!(
            !touched.load(Ordering::SeqCst),
            "SSRF guard must reject before any connection"
        );
    }

    // TEST 55: get_ssrf_safe rejects a decimal-encoded loopback IP literal
    // (http://2130706433/ == 127.0.0.1) with no DNS lookup.
    #[tokio::test]
    async fn get_ssrf_safe_rejects_decimal_encoded_loopback() {
        let result = Client::new()
            .get_ssrf_safe("http://2130706433/")
            .send()
            .await;
        assert!(
            matches!(result, Err(ClientError::SsrfBlocked(_))),
            "expected SsrfBlocked, got {result:?}"
        );
    }

    // TEST 56: resolve_and_validate — the resolve→validate core of get_ssrf_safe.
    // A public IP literal validates (returning the URL's port); blocked literals
    // and decimal-encoded loopback are rejected with SsrfBlocked. Exercising the
    // actual network connect of the safe path against a public host is NOT
    // reproducible in-sandbox (no reachable public server); it is covered
    // structurally by the pin_to and follow_redirects tests. See the report.
    #[tokio::test]
    async fn resolve_and_validate_accepts_public_rejects_blocked() {
        // Public literal with an explicit port → Ok, port preserved.
        let ok = resolve_and_validate("http://8.8.8.8:8080/path")
            .await
            .unwrap();
        assert_eq!(
            ok,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 8080)
        );

        // https default port is inferred.
        let ok_https = resolve_and_validate("https://1.1.1.1/").await.unwrap();
        assert_eq!(ok_https.port(), 443);

        // Blocked literals and decimal-encoded loopback → SsrfBlocked.
        for raw in [
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://2130706433/",
        ] {
            let err = resolve_and_validate(raw).await;
            assert!(
                matches!(err, Err(ClientError::SsrfBlocked(_))),
                "{raw} should be SsrfBlocked, got {err:?}"
            );
        }
    }
}
