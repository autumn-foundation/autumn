//! Traced outbound HTTP client with retries and test mocks.
//!
//! Exposes [`Client`](crate::http_client::Client) as `autumn_web::http::Client` — a thin `reqwest`-backed
//! outbound HTTP client that propagates the active span's `traceparent` /
//! `tracestate` headers and the current request's `x-request-id`, opens one
//! CLIENT span per attempt, retries transient failures, and is mockable in
//! tests via [`TestApp::http_mock`](crate::test::TestApp::http_mock).
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

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::RetryBudgetConfig;
use crate::deadline::{DEADLINE_HEADER, Deadline};
use crate::retry_budget::{RetryBudget, RetryBudgets, RetryKind};

// ── Error ────────────────────────────────────────────────────────────────────

/// Errors produced by [`Client`] and [`RequestBuilder`].
///
/// `#[non_exhaustive]`: match it with a `_` arm, so a new variant is not a
/// breaking change.
#[non_exhaustive]
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
    /// A staging fault (`[fault_injection]`, target `http`) failed the call
    /// before it was sent.
    #[error("{0}")]
    FaultInjected(String),
    /// The client-side adaptive throttle rejected the attempt locally,
    /// because the host rejected too many recent attempts (issue #3068).
    /// See `[http.client.adaptive_throttle]`.
    #[error("outbound request to {host} throttled locally: the host rejects too many requests")]
    ThrottledLocally {
        /// The host (`host` or `host:port`).
        host: String,
    },
    /// The capsule recorded a transport failure for this call, and replay
    /// reproduced it (#1634).
    ///
    /// A recorded transport error cannot be rebuilt as a `reqwest::Error` — it
    /// has no public constructor — but it must not be downgraded to a status
    /// either: a handler whose bug is "we do not handle a connection reset"
    /// has to meet the reset again.
    ///
    /// Its `Display` is the recorded text *exactly*, with no replay marker of
    /// its own. A handler that propagates this error puts the text into the
    /// capsule's outcome, and the replay verdict compares outcome messages
    /// verbatim — so a marker here would report an unchanged failure as a
    /// mismatch, which is the one thing the comparison exists to get right.
    #[error("{0}")]
    ReplayedRequestFailure(String),
    /// Outbound HTTP is blocked because this process is replaying a failure
    /// capsule (see the crate-internal `block_outbound_for_replay`).
    #[error(
        "outbound HTTP is not recorded in a failure capsule and is blocked during replay: {0} {1}"
    )]
    BlockedDuringReplay(String, String),
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
    /// [`pin_to`](RequestBuilder::pin_to) was combined with
    /// [`follow_redirects`](RequestBuilder::follow_redirects) — an incompatible
    /// pair. A single pinned `SocketAddr` only covers the first hop; later
    /// redirect hops re-resolve via normal DNS, so following a cross-host `3xx`
    /// would silently escape the pin and defeat its purpose. Use
    /// [`Client::get_ssrf_safe`] for pinned, per-hop-revalidated redirect
    /// following; `pin_to` alone (returns the `3xx` unfollowed); or
    /// `follow_redirects` without `pin_to`.
    #[error("{0}")]
    IncompatiblePinRedirect(&'static str),
    /// [`pin_to`](RequestBuilder::pin_to) was used on a request whose URL host is
    /// an IP literal. reqwest/hyper treat an IP-literal host as already-resolved
    /// and never consult the DNS resolver, so the `resolve_to_addrs` override
    /// that installs the pin is skipped and the socket connects to the literal in
    /// the URL — not the pinned address — silently bypassing the pin. `pin_to`
    /// therefore requires a domain (hostname) host; put the desired IP directly
    /// in the URL, or use a domain host. (`get_ssrf_safe` never sets a pin: it
    /// validates the literal and connects to that same IP, so it is unaffected.)
    #[error("{0}")]
    PinRequiresDomainHost(&'static str),
    /// [`pin_to`](RequestBuilder::pin_to) was combined with
    /// [`Client::get_ssrf_safe`] — an incompatible pair. The SSRF-safe path runs
    /// its own per-hop resolve→validate→pin and never reads the caller's
    /// `pin_to` address, so an explicit pin would be silently ignored. Use
    /// `pin_to` alone for a caller-chosen fixed address, or `get_ssrf_safe`
    /// alone for guarded automatic per-hop pinning — not both.
    #[error("{0}")]
    PinNotAllowedWithSsrfSafe(&'static str),
    /// The simulated network ([`crate::sim::SimNet`], issue #2967) failed the
    /// call: a drop, a partition, a timeout, a host it does not know, or a
    /// request or response it could not build.
    #[error("simulated network: {0}")]
    SimNetwork(String),

    /// The request deadline passed before an attempt could start (issue
    /// #3058). See [`crate::deadline`].
    #[error("request deadline exceeded before the outbound request could start")]
    DeadlineExceeded,
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
/// IPv6 addresses that embed an IPv4 via a transition mechanism — IPv4-mapped
/// (`::ffff:a.b.c.d`), the deprecated IPv4-compatible (`::a.b.c.d`), NAT64
/// (`64:ff9b::/96`), 6to4 (`2002::/16`), and the SIIT IPv4-translated prefix
/// (`::ffff:0:0:0/96`) — are unwrapped and re-checked as IPv4, so encodings
/// such as `::ffff:169.254.169.254`, `64:ff9b::a9fe:a9fe`, `2002:a9fe:a9fe::`,
/// and `::ffff:0:169.254.169.254` are all correctly blocked.
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
    // 192.88.99.0/24 (6to4 anycast relay, RFC 3068 / RFC 7526)
    if a == 192 && b == 88 && c == 99 {
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

/// Extract any IPv4 address embedded in an IPv6 address via a transition
/// mechanism, so the IPv4 SSRF policy can be re-applied to it:
///
/// - IPv4-mapped `::ffff:a.b.c.d`
/// - deprecated IPv4-compatible `::a.b.c.d`
/// - NAT64 well-known prefix `64:ff9b::/96` (RFC 6052) — e.g.
///   `64:ff9b::a9fe:a9fe` decodes to `169.254.169.254`
/// - 6to4 `2002::/16` (RFC 3056) — e.g. `2002:a9fe:a9fe::` embeds
///   `169.254.169.254`
/// - SIIT "IPv4-translated" prefix `::ffff:0:0:0/96` (RFC 6052) — e.g.
///   `::ffff:0:169.254.169.254` decodes to `169.254.169.254`. Note this is a
///   DIFFERENT segment layout from IPv4-mapped `::ffff:0:0/96` (here
///   `segments()[4] == 0xffff && segments()[5] == 0`, whereas IPv4-mapped has
///   `segments()[5] == 0xffff`), so it is NOT caught by `to_ipv4_mapped()`.
///
/// Returns `None` for a genuinely-native IPv6 address (no embedded v4). Using
/// `octets()` avoids any lossy `u16 -> u8` casts.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let segs = ip.segments();
    // SIIT IPv4-translated `::ffff:0:0:0/96` (RFC 6052): 64 zero bits, then
    // 0xffff, then 16 zero bits, then the IPv4 in the last 32 bits. Distinct
    // from IPv4-mapped `::ffff:0:0/96` (segment[5] == 0xffff) — here
    // segment[4] == 0xffff and segment[5] == 0 — so `to_ipv4_mapped()` above
    // does NOT catch it. Decode it so the embedded IPv4 is re-checked by the
    // IPv4 policy (e.g. `::ffff:0:169.254.169.254` must be blocked).
    if segs[0] == 0
        && segs[1] == 0
        && segs[2] == 0
        && segs[3] == 0
        && segs[4] == 0xffff
        && segs[5] == 0
    {
        let o = ip.octets();
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    let o = ip.octets();
    // NAT64 64:ff9b::/96 — embedded IPv4 in the last 32 bits (octets 12..16),
    // with octets 4..12 all zero.
    if o[0] == 0x00
        && o[1] == 0x64
        && o[2] == 0xff
        && o[3] == 0x9b
        && o[4..12].iter().all(|&b| b == 0)
    {
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 6to4 2002::/16 — embedded IPv4 in bits 16..48 (octets 2..6).
    if o[0] == 0x20 && o[1] == 0x02 {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    // Deprecated IPv4-compatible ::a.b.c.d (upper 96 bits zero).
    ip.to_ipv4()
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    // Block any private / loopback / link-local / metadata IPv4 that is
    // tunnelled inside this v6 address (IPv4-mapped, IPv4-compatible, NAT64,
    // 6to4, or SIIT IPv4-translated `::ffff:0:0:0/96`) by re-running the IPv4
    // policy on the embedded address. A public
    // embedded IPv4 (e.g. 6to4 `2002:0808:0808::` == 8.8.8.8) is NOT blocked
    // here — it falls through to the native v6 range checks below.
    if let Some(v4) = embedded_ipv4(ip)
        && is_blocked_ipv4(v4)
    {
        return true;
    }

    let segs = ip.segments();
    // RFC 8215 local-use NAT64 prefix `64:ff9b:1::/48`: deny the whole prefix
    // outright. Unlike the well-known `64:ff9b::/96` (where a public embedded
    // IPv4 like 8.8.8.8 is legitimately allowed), this is a private/site-local
    // NAT64 allocation with no legitimate public destination, and the RFC 6052
    // embedding position varies with prefix length — a blanket deny is both
    // simpler and strictly safer (e.g. `64:ff9b:1::a9fe:a9fe` == 169.254.169.254).
    if segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2] == 0x0001 {
        return true;
    }
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
    // fec0::/10 (deprecated site-local — defence-in-depth)
    if segs[0] & 0xffc0 == 0xfec0 {
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

    // Remaining IANA IPv6 special-purpose prefixes with Globally Reachable =
    // False. These are reserved / benchmarking / documentation / discard ranges
    // that must never be dialled, matching the deny-list's reserved policy.
    // Each check uses explicit masks so it cannot catch an adjacent *public*
    // address (e.g. benchmarking 2001:2::/48 must not swallow Teredo 2001:0::/32
    // where s[1] == 0, and documentation 2001:db8::/32 stays its own check).
    //
    //   Prefix               RFC        Purpose
    //   100::/64             RFC 6666   Discard-Only address block
    //   2001:2::/48          RFC 5180   Benchmarking
    //   2001:10::/28         RFC 4843   ORCHID (deprecated)
    //   2001:20::/28         RFC 7343   ORCHIDv2
    //   3fff::/20            RFC 9637   Documentation
    //   5f00::/16            RFC 9602   Segment Routing (SRv6) SIDs
    //   2620:4f:8000::/48    RFC 7534   Direct Delegation AS112 service
    let s = segs;
    // 100::/64 (Discard-Only, RFC 6666)
    if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0 {
        return true;
    }
    // 2001:2::/48 (Benchmarking, RFC 5180)
    if s[0] == 0x2001 && s[1] == 0x0002 && s[2] == 0x0000 {
        return true;
    }
    // 2001:10::/28 (ORCHID, deprecated RFC 4843)
    if s[0] == 0x2001 && (s[1] & 0xFFF0) == 0x0010 {
        return true;
    }
    // 2001:20::/28 (ORCHIDv2, RFC 7343)
    if s[0] == 0x2001 && (s[1] & 0xFFF0) == 0x0020 {
        return true;
    }
    // 3fff::/20 (Documentation, RFC 9637)
    if s[0] == 0x3fff && (s[1] & 0xF000) == 0x0000 {
        return true;
    }
    // 5f00::/16 (SRv6 SIDs, RFC 9602)
    if s[0] == 0x5f00 {
        return true;
    }
    // 2620:4f:8000::/48 (Direct Delegation AS112, RFC 7534)
    if s[0] == 0x2620 && s[1] == 0x004f && s[2] == 0x8000 {
        return true;
    }

    false
}

// ── RetryPolicy ──────────────────────────────────────────────────────────────

/// Retry configuration for a [`RequestBuilder`].
///
/// The wait before retry `n` (0 = first retry) is full jitter:
/// `random(0, min(max_backoff, 100 ms * 2^n))`. See [`crate::backoff`].
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Maximum number of additional attempts after the first failure.  Zero
    /// means no retries (one attempt total).
    pub max_retries: u32,
    /// When `true` (the default), only GET / HEAD / PUT / DELETE / OPTIONS /
    /// TRACE are retried; POST and PATCH are not. Clear it with
    /// [`RequestBuilder::retry_non_idempotent`].
    pub retry_idempotent_only: bool,
    /// Cap on a `Retry-After` hint. The wait is also at most the backoff
    /// plus 5 s, so a value above 5 s has no effect.
    pub max_retry_after: Duration,
    /// Per-request timeout.
    pub request_timeout: Option<Duration>,
    /// Cap on the jittered backoff between attempts. Default: 20 s.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_idempotent_only: true,
            max_retry_after: Duration::from_secs(10),
            request_timeout: Some(Duration::from_secs(30)),
            max_backoff: Duration::from_millis(crate::backoff::DEFAULT_HTTP_MAX_BACKOFF_MS),
        }
    }
}

/// Base of the exponential retry backoff.
const BASE_BACKOFF: Duration = Duration::from_millis(100);

/// The header that [`RequestBuilder::retry_non_idempotent`] adds.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

impl RetryPolicy {
    /// The wait before the retry that follows failed attempt `attempt`
    /// (0-indexed).
    ///
    /// Without a hint, this is the jittered backoff. With a `Retry-After`
    /// hint, the hint is capped by `max_retry_after` and `request_timeout`.
    /// The wait is then `backoff + min(hint, 5 s)`. See
    /// [`crate::backoff::retry_after_wait`].
    fn retry_delay(
        &self,
        entropy: &dyn crate::entropy::Entropy,
        attempt: u32,
        hint: Option<Duration>,
    ) -> Duration {
        let backoff = crate::backoff::full_jitter(entropy, BASE_BACKOFF, self.max_backoff, attempt);
        let Some(mut hint) = hint else {
            return backoff;
        };
        hint = hint.min(self.max_retry_after);
        if let Some(timeout) = self.request_timeout {
            hint = hint.min(timeout);
        }
        crate::backoff::retry_after_wait(hint, backoff)
    }
}

/// `true` for a status the client retries: `429` and `502`-`504`.
const fn is_retryable_response(status: u16) -> bool {
    status == 429 || is_retryable_status(status)
}

/// The server's wait hint for a retryable status: `Retry-After` on `429`
/// (1 s when absent) and on `503`.
fn retry_hint(status: u16, headers: &HeaderMap) -> Option<Duration> {
    match status {
        429 => Some(parse_retry_after(headers).unwrap_or(Duration::from_secs(1))),
        503 => parse_retry_after(headers),
        _ => None,
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

/// Set while this process is replaying a failure capsule, blocking every
/// outbound request (see [`block_outbound_for_replay`]).
static OUTBOUND_BLOCKED: AtomicBool = AtomicBool::new(false);

/// Fail every outbound HTTP request from now on.
///
/// Called by the capsule replay one-shot
/// ([`AppBuilder::run`](crate::app::AppBuilder::run) under
/// `AUTUMN_REPLAY_CAPSULE`), which rebuilds the real application — including
/// its real `reqwest` client — and must not let it reach anything the capsule
/// did not record. There is deliberately no way to unset it: a process that has
/// started replaying a capsule never goes back to serving traffic.
pub(crate) fn block_outbound_for_replay() {
    OUTBOUND_BLOCKED.store(true, Ordering::SeqCst);
}

/// Whether outbound HTTP is blocked for capsule replay.
#[must_use]
pub(crate) fn outbound_blocked_for_replay() -> bool {
    OUTBOUND_BLOCKED.load(Ordering::SeqCst)
}

// ── Failure-capsule seam (#1634) ─────────────────────────────────────────────
//
// The whole seam is behind the `reporting` feature, which is what gates the
// `capsule` module. `OutboundRecorder` has a no-op twin below so `send` reads
// the same on either build.

/// The headers the *caller* set, in the order they were set.
///
/// One definition for both halves of the seam: the recorder stores this, and
/// replay compares against it. Headers the client adds later — the injected
/// W3C trace context, and any default the underlying `reqwest::Client` was
/// built with — are deliberately outside it, so a client-side default can
/// never be read as the handler changing its request.
#[cfg(feature = "reporting")]
fn caller_headers(builder: &RequestBuilder) -> Vec<(String, String)> {
    builder
        .extra_headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// Serve one outbound call from a capsule's effect tape.
///
/// Which [`HttpErrorKind`] a failure is, so replay can rebuild the variant and
/// not merely quote the text.
#[cfg(feature = "reporting")]
const fn http_error_kind(error: &ClientError) -> crate::capsule::schema::HttpErrorKind {
    use crate::capsule::schema::HttpErrorKind as Kind;
    match error {
        ClientError::Json(_) => Kind::Json,
        ClientError::NoMock(..) => Kind::NoMock,
        ClientError::CircuitBreakerOpen => Kind::CircuitBreakerOpen,
        ClientError::ThrottledLocally { .. } => Kind::ThrottledLocally,
        ClientError::SsrfBlocked(_) => Kind::SsrfBlocked,
        ClientError::TooManyRedirects(_) => Kind::TooManyRedirects,
        ClientError::RedirectRejected(_) => Kind::RedirectRejected,
        ClientError::InvalidUrl(_) => Kind::InvalidUrl,
        ClientError::FaultInjected(_) => Kind::FaultInjected,
        // Everything else — a `reqwest` transport error, and the replay-only
        // variants a recorded run cannot have produced — keeps its text alone.
        _ => Kind::Transport,
    }
}

/// Rebuild the error a recorded call produced, as the handler observed it.
///
/// Variants whose payload survives in the recording are rebuilt exactly, so
/// code that branches on them takes the branch it took in production. The rest
/// come back as [`ClientError::ReplayedRequestFailure`], whose `Display` is the
/// recorded text verbatim — the closest an unreconstructible foreign error can
/// be brought.
#[cfg(feature = "reporting")]
fn rebuild_client_error(
    kind: Option<crate::capsule::schema::HttpErrorKind>,
    text: String,
) -> ClientError {
    use crate::capsule::schema::HttpErrorKind as Kind;
    match kind {
        Some(Kind::CircuitBreakerOpen) => ClientError::CircuitBreakerOpen,
        Some(Kind::ThrottledLocally) => text
            .strip_prefix("outbound request to ")
            .and_then(|rest| rest.split_once(" throttled locally"))
            .map_or_else(
                || ClientError::ReplayedRequestFailure(text.clone()),
                |(host, _)| ClientError::ThrottledLocally {
                    host: host.to_owned(),
                },
            ),
        Some(Kind::SsrfBlocked) => {
            ClientError::SsrfBlocked(strip_prefix_payload(&text, "SSRF policy blocked address: "))
        }
        Some(Kind::RedirectRejected) => {
            ClientError::RedirectRejected(strip_prefix_payload(&text, "redirect rejected: "))
        }
        Some(Kind::InvalidUrl) => {
            ClientError::InvalidUrl(strip_prefix_payload(&text, "invalid or unresolvable URL: "))
        }
        Some(Kind::FaultInjected) => ClientError::FaultInjected(text),
        Some(Kind::TooManyRedirects) => text
            .trim_end_matches(')')
            .rsplit_once("(max ")
            .and_then(|(_, hops)| hops.parse().ok())
            .map_or_else(
                || ClientError::ReplayedRequestFailure(text.clone()),
                ClientError::TooManyRedirects,
            ),
        // `NoMock` carries the method and URL, which the effect already holds;
        // it is a test-harness error a recorded production run cannot produce,
        // so it is not worth rebuilding structurally.
        Some(Kind::Json | Kind::NoMock | Kind::Transport) | None => {
            ClientError::ReplayedRequestFailure(text)
        }
    }
}

/// The payload of a single-field error, recovered from its recorded `Display`.
///
/// The prefixes are this module's own `#[error(...)]` strings, so they cannot
/// drift without this file changing; a recording that does not carry one is
/// handed back whole rather than silently truncated.
#[cfg(feature = "reporting")]
fn strip_prefix_payload(text: &str, prefix: &str) -> String {
    text.strip_prefix(prefix).unwrap_or(text).to_owned()
}

/// A recorded transport failure is reproduced as one, not as a status: a
/// handler whose bug is "we do not handle a connection reset" must meet the
/// reset again.
#[cfg(feature = "reporting")]
fn replayed_response(
    tape: &crate::capsule::effects::ReplayEffects,
    request: &crate::capsule::effects::OutboundRequest<'_>,
) -> Result<Response, ClientError> {
    let Some(recorded) = tape.next_http(request) else {
        // `next_http` already logged the divergence; the call fails closed.
        return Err(ClientError::BlockedDuringReplay(
            request.method.to_owned(),
            request.url.to_owned(),
        ));
    };
    if let Some(error) = recorded.error {
        return Err(rebuild_client_error(recorded.error_kind, error));
    }
    let mut headers = HeaderMap::new();
    for (name, value) in &recorded.response_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    Ok(Response {
        status: reqwest::StatusCode::from_u16(recorded.status)
            .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
        headers,
        body: Bytes::from(crate::capsule::effects::body_bytes(&recorded.response_body)),
        // The recorded final location when redirects moved it, so a handler
        // inspecting `Response::url()` sees what it saw live.
        url: recorded
            .final_url
            .as_deref()
            .or(Some(request.url))
            .and_then(|url| url::Url::parse(url).ok()),
    })
}

/// Tees one outbound exchange into the in-flight request's capsule.
///
/// Armed before the send so the request half is captured even when the call
/// never produces a response, and disarmed to a no-op whenever no capture
/// scope is active — which is every request in an application that has not
/// turned `[failure_capture]` on, so the cost on the ordinary path is one
/// task-local probe.
#[cfg(feature = "reporting")]
struct OutboundRecorder {
    scope: Option<Arc<crate::capsule::CaptureScope>>,
    /// Tape position, taken when the call *starts*. Two calls a handler
    /// `join!`s therefore land in initiation order rather than completion
    /// order — which is the order replay consumes them in.
    slot: Option<usize>,
    /// Cached from the scope's settings so `finish` need not re-read them.
    max_body_bytes: usize,
    method: String,
    url: String,
    request_headers: Vec<(String, String)>,
    request_body: crate::capsule::CapsuleBody,
}

#[cfg(feature = "reporting")]
impl OutboundRecorder {
    /// Snapshot the request half, if a capsule is being recorded.
    ///
    /// Records the headers the *caller* set. Headers the client adds later —
    /// the injected W3C trace context, and any default the underlying
    /// `reqwest::Client` was built with — are not on the tape, because replay
    /// matches on method and URL and does not need them; the recorded request
    /// half is a debugging aid, not the match key.
    fn arm(builder: &RequestBuilder) -> Self {
        let scope = crate::capsule::current_scope();
        let Some(scope) = scope else {
            return Self {
                scope: None,
                slot: None,
                max_body_bytes: 0,
                method: String::new(),
                url: String::new(),
                request_headers: Vec::new(),
                request_body: crate::capsule::CapsuleBody::Absent,
            };
        };
        let max_body_bytes = scope.settings().max_body_bytes;
        let slot = scope.reserve_http();
        let request_headers = caller_headers(builder);
        Self {
            scope: Some(scope),
            slot,
            max_body_bytes,
            method: builder.method.to_string(),
            url: builder.url.clone(),
            request_headers,
            request_body: builder
                .body
                .as_ref()
                .map_or(crate::capsule::CapsuleBody::Absent, |body| {
                    encode_body(body, max_body_bytes)
                }),
        }
    }

    /// Complete the reserved slot with the finished exchange.
    fn finish(self, result: &Result<Response, ClientError>) {
        let (Some(scope), Some(slot)) = (self.scope, self.slot) else {
            return;
        };
        let (status, response_headers, response_body, error, final_url) = match result {
            Ok(response) => (
                response.status.as_u16(),
                response
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        // A header value is bytes, not text. Replacing an
                        // opaque non-UTF-8 value with an empty string would
                        // hand replay a header the peer never sent, and code
                        // reading it through `as_bytes()` would branch on
                        // nothing; the lossy form at least preserves its shape
                        // and length rather than deleting it.
                        (
                            name.as_str().to_owned(),
                            value.to_str().map_or_else(
                                |_| String::from_utf8_lossy(value.as_bytes()).into_owned(),
                                ToOwned::to_owned,
                            ),
                        )
                    })
                    .collect(),
                encode_body(&response.body, self.max_body_bytes),
                None,
                response
                    .url()
                    .map(reqwest::Url::as_str)
                    .filter(|final_url| *final_url != self.url)
                    .map(ToOwned::to_owned),
            ),
            Err(error) => (
                0,
                Vec::new(),
                crate::capsule::CapsuleBody::Absent,
                Some((error.to_string(), http_error_kind(error))),
                None,
            ),
        };
        let (error, error_kind) = match error {
            Some((text, kind)) => (Some(text), Some(kind)),
            None => (None, None),
        };
        scope.fill_http(
            slot,
            crate::capsule::HttpEffect {
                method: self.method,
                url: self.url,
                request_headers: self.request_headers,
                request_body: self.request_body,
                status,
                response_headers,
                final_url,
                response_body,
                error,
                error_kind,
            },
        );
    }
}

/// Record a body as capsule text when it is UTF-8, base64 otherwise.
///
/// Text is what makes a capsule diffable and what lets redaction parse a JSON
/// payload by key; base64 is the honest fallback for a protobuf or an image.
#[cfg(feature = "reporting")]
fn encode_body(bytes: &Bytes, max_body_bytes: usize) -> crate::capsule::CapsuleBody {
    if bytes.is_empty() {
        return crate::capsule::CapsuleBody::Absent;
    }
    // Checked *before* the copy, not after: a 50 MB download must not be
    // duplicated into a `String` (or grown by a third into base64) only to be
    // thrown away by the capsule's body cap a moment later.
    if bytes.len() > max_body_bytes {
        return crate::capsule::CapsuleBody::Skipped {
            declared_len: Some(bytes.len()),
        };
    }
    std::str::from_utf8(bytes).map_or_else(
        |_| {
            use base64::Engine as _;
            crate::capsule::CapsuleBody::Base64(
                base64::engine::general_purpose::STANDARD.encode(bytes),
            )
        },
        |text| crate::capsule::CapsuleBody::Text(text.to_owned()),
    )
}

/// No capsule support compiled in: the recorder is an empty value the
/// optimizer removes.
#[cfg(not(feature = "reporting"))]
struct OutboundRecorder;

#[cfg(not(feature = "reporting"))]
impl OutboundRecorder {
    const fn arm(_builder: &RequestBuilder) -> Self {
        Self
    }

    const fn finish(self, _result: &Result<Response, ClientError>) {}
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
    /// When present (a sim with a `SimNet`), calls go through the simulated
    /// network instead of the real one.
    sim_net: Option<Arc<crate::sim::SimNet>>,
    /// Retry budgets and deadline header settings (issue #3058).
    retry: RetrySettings,
    /// Source of retry jitter. `from_state` uses the app's entropy, so a sim
    /// seed replays the same delays.
    entropy: Arc<dyn crate::entropy::Entropy>,
    /// Client-side adaptive throttle (issue #3068). `None` when off.
    throttle: Option<Arc<crate::admission::AdaptiveThrottle>>,
}

/// The app-wide client-side throttle, so that every `Client::from_state`
/// shares one set of per-host counts (issue #3068). `config` is the setting
/// it was built from.
#[derive(Clone)]
pub(crate) struct SharedThrottle {
    throttle: Arc<crate::admission::AdaptiveThrottle>,
    config: crate::config::AdaptiveThrottleConfig,
}

/// Put the app-wide throttle in `state` when `[http.client.adaptive_throttle]`
/// is on.
pub(crate) fn install_shared_throttle(
    state: &crate::AppState,
    config: &crate::config::HttpClientConfig,
) {
    let _ = shared_throttle(state, config);
}

/// Serializes the replacement of the shared throttle.
static THROTTLE_REPLACE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The app-wide throttle for the effective `config`, or `None` when it is
/// off. A `state_initializer` can replace the config after boot. If the
/// shared throttle was built from other settings, or there is none, a new
/// one replaces it, so that all clients share counts for the settings in
/// force.
fn shared_throttle(
    state: &crate::AppState,
    config: &crate::config::HttpClientConfig,
) -> Option<Arc<crate::admission::AdaptiveThrottle>> {
    let settings = config.adaptive_throttle;
    if !settings.enabled {
        return None;
    }
    let current = || {
        state
            .extension::<SharedThrottle>()
            .filter(|shared| shared.config == settings)
            .map(|shared| Arc::clone(&shared.throttle))
    };
    if let Some(throttle) = current() {
        return Some(throttle);
    }
    // Replace under a lock and check again, so that concurrent first calls
    // after a config change share one throttle. Only this slow path locks.
    let _guard = THROTTLE_REPLACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(throttle) = current() {
        return Some(throttle);
    }
    let throttle = throttle_from_config(config)?;
    state.insert_extension(SharedThrottle {
        throttle: Arc::clone(&throttle),
        config: settings,
    });
    Some(throttle)
}

/// A new throttle for `config`, or `None` when it is off.
fn throttle_from_config(
    config: &crate::config::HttpClientConfig,
) -> Option<Arc<crate::admission::AdaptiveThrottle>> {
    let t = config.adaptive_throttle;
    t.enabled.then(|| {
        Arc::new(crate::admission::AdaptiveThrottle::new(
            t.k,
            Duration::from_secs(t.window_secs),
        ))
    })
}

/// The entropy a client without app state uses.
fn os_entropy() -> Arc<dyn crate::entropy::Entropy> {
    Arc::new(crate::entropy::OsEntropy)
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
            .redirect(pooled_redirect_policy())
            .build()
            .expect("failed to build reqwest client");
        Self {
            inner,
            alias: None,
            base_url: None,
            base_urls: HashMap::new(),
            retry_policy: RetryPolicy {
                request_timeout: Some(timeout),
                ..RetryPolicy::default()
            },
            mock: None,
            resilience_config: None,
            sim_net: None,
            retry: RetrySettings::standalone(),
            entropy: os_entropy(),
            throttle: None,
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
            .redirect(pooled_redirect_policy())
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
                max_backoff: Duration::from_millis(config.max_backoff_ms),
            },
            mock: None,
            resilience_config: None,
            sim_net: None,
            // `from_config` and `from_state` set the budgets.
            retry: RetrySettings {
                budgets: None,
                send_deadline_header: config.send_deadline_header,
            },
            entropy: os_entropy(),
            throttle: throttle_from_config(config),
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
            sim_net: None,
            // `from_state` sets the budgets.
            retry: RetrySettings {
                budgets: None,
                send_deadline_header: true,
            },
            entropy: os_entropy(),
            throttle: None,
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
        let mut client = Self::from_config_with_inner(Self::build_inner(config), config);
        client.retry.budgets = config
            .retry_budget
            .enabled
            .then(|| Arc::new(RetryBudgets::new(&config.retry_budget)));
        client
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

        let budget_config = config
            .as_ref()
            .map_or_else(RetryBudgetConfig::default, |cfg| {
                cfg.client.retry_budget.clone()
            });
        // One throttle for the whole app, so per-host counts add up.
        let throttle = config
            .as_ref()
            .and_then(|cfg| shared_throttle(state, &cfg.client));
        let mut client = match (config, shared) {
            (Some(cfg), Some(inner)) => Self::from_config_with_inner(inner, &cfg.client),
            (Some(cfg), None) => Self::from_config(&cfg.client),
            (None, Some(inner)) => Self::with_inner(inner),
            (None, None) => Self::new(),
        };

        // The extractor builds a new client for each request, so the budgets
        // live in the app state. Thus all requests of one app share them.
        client.retry.budgets = shared_retry_budgets(state, &budget_config);

        client.resilience_config = autumn_config.map(|c| Arc::new(c.resilience.clone()));

        if let Some(ext) = state.extension::<HttpMockRegistryExt>() {
            client = client.with_mock(ext.0.clone());
        }
        client.sim_net = state.extension::<crate::sim::SimNet>();
        client.throttle = throttle;
        // Retry jitter and the automatic key are not made again on capsule
        // replay, so they must not go on the capsule's random tape.
        let entropy = state.entropy_arc();
        client.entropy = entropy.unrecorded().unwrap_or(entropy);

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
            sim_net: self.sim_net.clone(),
            retry: self.retry.clone(),
            entropy: self.entropy.clone(),
            throttle: self.throttle.clone(),
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
            sim_net: self.sim_net.clone(),
            retry: self.retry.clone(),
            entropy: self.entropy.clone(),
            throttle: self.throttle.clone(),
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
            discard_response_body: false,
            breaker_scoped: false,
            sim_net: self.sim_net.clone(),
            forward_request_id: true,
            retry: self.retry.clone(),
            entropy: self.entropy.clone(),
            throttle: self.throttle.clone(),
            criticality: None,
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
    /// request if *any* address is blocked. The connection is then pinned to the
    /// full set of validated addresses so reqwest cannot re-resolve the host
    /// (closing the DNS-rebinding / TOCTOU window) yet can still fall back across
    /// them in order if the first is unreachable. Redirects are followed manually up to
    /// `SSRF_SAFE_MAX_REDIRECTS` hops, re-running resolve→validate→pin on each
    /// hop; a hop that downgrades the scheme from `https` to `http` is rejected
    /// as defence-in-depth.
    ///
    /// The redirect count and per-hop validation honour a chained builder
    /// override (the built-in resolve→validate→pin and scheme-downgrade guards
    /// always apply):
    ///
    /// - by default, up to `SSRF_SAFE_MAX_REDIRECTS` hops are followed;
    /// - a chained [`no_redirect`](RequestBuilder::no_redirect) returns the
    ///   initial `3xx` verbatim — the initial URL is still resolved, validated
    ///   and pinned, but no redirect is followed;
    /// - a chained [`follow_redirects(max, validator)`](RequestBuilder::follow_redirects)
    ///   caps following at `max` hops (so `max == 0` turns the first `3xx` into
    ///   [`ClientError::TooManyRedirects`]) and additionally runs the caller's
    ///   `validator(&next)` on every hop, on top of the built-in guards.
    ///
    /// Like the test-mock path, this custom send path bypasses the process-wide
    /// circuit-breaker registry to avoid entangling per-URL SSRF fetches with
    /// the shared per-host breaker state.
    ///
    /// **Env proxies are bypassed.** Each pinned per-hop client is built with
    /// `.no_proxy()`, so `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` are ignored
    /// and the socket connects directly to the validated/pinned address. This is
    /// required for the pin to hold: reqwest checks proxy interception before the
    /// connector where the `resolve()` override applies, so a configured proxy
    /// would otherwise receive the request and re-resolve the host — reopening
    /// the DNS-rebinding / SSRF window this API closes.
    ///
    /// **Cannot be combined with [`pin_to`](RequestBuilder::pin_to).** This path
    /// performs its own per-hop resolve→validate→pin and never reads the
    /// `pin_to` address, so an explicit pin would be silently ignored. Chaining
    /// the two is therefore rejected at send time with
    /// [`ClientError::PinNotAllowedWithSsrfSafe`]. Use `pin_to` alone for a
    /// caller-chosen fixed address, or `get_ssrf_safe` alone for guarded
    /// automatic per-hop pinning.
    #[must_use]
    pub fn get_ssrf_safe(&self, url: impl Into<String>) -> RequestBuilder {
        self.build_request(Method::GET, url.into()).ssrf_safe()
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

tokio::task_local! {
    /// The redirect targets the pooled client followed during one send, in
    /// order. An error does not say which host failed after a redirect, and
    /// every host on the way gets its refill, so the plain path reads them
    /// here.
    static FOLLOWED: std::cell::RefCell<Vec<String>>;
    /// `true` while the plain path follows redirects itself, so the pooled
    /// client returns each redirect (see `pooled_redirect_policy`).
    static MANUAL_REDIRECTS: bool;
}

/// Send `req` on the pooled client. Also return the redirect targets the
/// HTTP stack followed, in order.
async fn send_tracking_redirects(
    req: reqwest::RequestBuilder,
) -> (Result<reqwest::Response, reqwest::Error>, Vec<String>) {
    FOLLOWED
        .scope(std::cell::RefCell::new(Vec::new()), async {
            let sent = req.send().await;
            (sent, FOLLOWED.with(std::cell::RefCell::take))
        })
        .await
}

/// Set `Referer` for a redirect from `previous` to `next`, as reqwest does:
/// the previous URL without credentials or fragment, and none on an
/// `https` to `http` hop.
fn set_referer(headers: &mut HeaderMap, next: &str, previous: &str) {
    let (Ok(next), Ok(mut referer)) = (url::Url::parse(next), url::Url::parse(previous)) else {
        return;
    };
    if next.scheme() == "http" && referer.scheme() == "https" {
        headers.remove(reqwest::header::REFERER);
        return;
    }
    let _ = referer.set_username("");
    let _ = referer.set_password(None);
    referer.set_fragment(None);
    if let Ok(value) = HeaderValue::from_str(referer.as_str()) {
        headers.insert(reqwest::header::REFERER, value);
    }
}

/// The hop limit of the plain send path, as in reqwest's default policy.
const PLAIN_MAX_REDIRECTS: usize = 10;

/// The redirect policy of the pooled client: reqwest's default of
/// [`PLAIN_MAX_REDIRECTS`] hops, but no automatic hop while a request
/// deadline is set. The plain send path then follows the redirect itself, so
/// each hop sends the time left at that hop in [`DEADLINE_HEADER`] (issue
/// #3058).
fn pooled_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if Deadline::current().is_some() || MANUAL_REDIRECTS.try_with(|m| *m).unwrap_or(false) {
            attempt.stop()
        } else if attempt.previous().len() > PLAIN_MAX_REDIRECTS {
            // `previous` holds the first URL too, as in reqwest's own limit.
            attempt.error("too many redirects")
        } else {
            let _ = FOLLOWED.try_with(|hops| hops.borrow_mut().push(attempt.url().to_string()));
            attempt.follow()
        }
    })
}

/// The client of one hop of [`RequestBuilder::follow_loop`].
#[derive(Clone, Copy)]
enum HopClient<'a> {
    /// The pooled client of the plain path.
    Pooled(&'a reqwest::Client),
    /// A new client per hop, with this timeout.
    OneShot(Duration),
}

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
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-request switches; each has its own builder method"
)]
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
    /// When `true`, the response body is dropped unread. See
    /// [`RequestBuilder::discard_response_body`].
    discard_response_body: bool,
    /// When `true`, `send_recorded` keeps circuit-breaker accounting even on
    /// the custom send path (`needs_custom_path()`). See
    /// [`RequestBuilder::breaker_scoped`].
    breaker_scoped: bool,
    /// The simulated network, when the client came from a sim app state.
    sim_net: Option<Arc<crate::sim::SimNet>>,
    /// When `true` (the default), `send` adds the current request's id as
    /// `x-request-id`. See [`RequestBuilder::without_request_id`].
    forward_request_id: bool,
    /// Retry budgets and deadline header settings (issue #3058).
    retry: RetrySettings,
    /// Source of retry jitter and of the automatic `Idempotency-Key`.
    entropy: Arc<dyn crate::entropy::Entropy>,
    /// Client-side adaptive throttle (issue #3068).
    throttle: Option<Arc<crate::admission::AdaptiveThrottle>>,
    /// The criticality to send. `None` sends the inbound request's.
    criticality: Option<crate::admission::Criticality>,
}

impl RequestBuilder {
    /// Do not send the current request's id as `x-request-id`.
    ///
    /// By default, a request sent during an inbound request carries that
    /// request's id (issue #3064). Use this for a host that must not see it.
    #[must_use]
    pub const fn without_request_id(mut self) -> Self {
        self.forward_request_id = false;
        self
    }

    /// Send this criticality in the `X-Autumn-Criticality` header (issue
    /// #3068). Without this call, the client sends the criticality of the
    /// inbound request that it serves, if any.
    #[must_use]
    pub const fn criticality(mut self, criticality: crate::admission::Criticality) -> Self {
        self.criticality = Some(criticality);
        self
    }

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

    /// Set a raw byte body, without assuming any content type.
    ///
    /// Use this for binary payloads — [`text_body`](Self::text_body) takes a
    /// `String` and so cannot carry bytes that are not valid UTF-8. Set the
    /// content type yourself with [`header`](Self::header). The Web Push
    /// transport uses this for RFC 8291-encrypted bodies.
    #[must_use]
    pub fn bytes_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Override the maximum retry count for this request.
    ///
    /// This does not enable retries for `POST` and `PATCH`. Use
    /// [`retry_non_idempotent`](Self::retry_non_idempotent) for that.
    #[must_use]
    pub const fn retries(mut self, max: u32) -> Self {
        self.retry_policy.max_retries = max;
        self
    }

    /// Retry non-idempotent methods (`POST`, `PATCH`) too.
    ///
    /// A retried `POST` can run two times on the server. So for a
    /// non-idempotent method, the client sends an `Idempotency-Key` header
    /// with a random value, the same on every attempt. A key you set with
    /// [`header`](Self::header) is kept.
    #[must_use]
    pub const fn retry_non_idempotent(mut self) -> Self {
        self.retry_policy.retry_idempotent_only = false;
        self
    }

    /// Override the cap on the jittered backoff between attempts.
    #[must_use]
    pub const fn max_backoff(mut self, max: Duration) -> Self {
        self.retry_policy.max_backoff = max;
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

    /// Return the status and headers without reading the response body.
    ///
    /// Every other path collects the whole body into memory with no ceiling,
    /// which is the right default for an API call whose payload the caller
    /// wants. It is the wrong default when the *remote host* is untrusted and
    /// the caller needs only the status: a server that streams indefinitely
    /// then costs one unbounded allocation per request until the timeout
    /// fires.
    ///
    /// Web Push is exactly that shape — the endpoint URL is chosen by the
    /// client, and the transport only ever reads the status code — so it sets
    /// this. [`Response::bytes`] then returns empty; dropping the underlying
    /// response closes the connection without draining it.
    #[must_use]
    pub const fn discard_response_body(mut self) -> Self {
        self.discard_response_body = true;
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
    ///
    /// **TOCTOU / rebinding limitation.** The `validator` receives the redirect
    /// target as a *string* (not a resolved IP), and the subsequent connection
    /// re-resolves that host via normal DNS. So a validator that inspects IP
    /// literals sees only literal hosts, has a connect-time TOCTOU window
    /// against a hostname that re-resolves between check and connect, and
    /// provides no address pinning. When you need pinned, rebind-safe following
    /// that validates every resolved IP and pins the connection per hop, use
    /// [`Client::get_ssrf_safe`] instead.
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
    /// **Requires a domain (hostname) URL host.** The pin is enforced via a DNS
    /// `resolve` override, but reqwest/hyper treat an IP-literal URL host as
    /// already-resolved and never consult the resolver — so the override is
    /// skipped and the socket would connect to the literal in the URL, not the
    /// pinned address. A `pin_to` request whose URL host is an IP literal
    /// (IPv4 or IPv6) is therefore **rejected at send time** with
    /// [`ClientError::PinRequiresDomainHost`]. Put the desired IP directly in the
    /// URL (no pin needed), or use a domain host.
    ///
    /// **Pinning applies to the initial connection only.** Redirects are **not**
    /// followed under `pin_to`: a `3xx` is returned to the caller verbatim (as if
    /// [`no_redirect`](Self::no_redirect) were set) rather than being
    /// auto-followed to a possibly-different host that would be re-resolved via
    /// normal DNS, silently escaping the pin. If you need pinned, rebind-safe
    /// per-hop following, use [`Client::get_ssrf_safe`] instead.
    ///
    /// **Cannot be combined with [`follow_redirects`](Self::follow_redirects).**
    /// Because the pin only covers the first hop while later hops would re-resolve
    /// via DNS, chaining `pin_to` with `follow_redirects` (in either order) is
    /// rejected at send time with [`ClientError::IncompatiblePinRedirect`] rather
    /// than silently following a redirect off the pinned address. Use
    /// [`Client::get_ssrf_safe`] for pinned, per-hop-revalidated redirect
    /// following, `pin_to` alone (which returns the `3xx` unfollowed), or
    /// `follow_redirects` without `pin_to`.
    ///
    /// Implemented via a one-shot `reqwest::ClientBuilder::resolve(host, addr)`
    /// scoped to this request. Note that reqwest ignores the **port** in the
    /// resolve override and connects to the port from the request URL, so
    /// `addr.port()` is only honoured when it matches the URL's port (which it
    /// does for addresses obtained by resolving that same URL). Routes the
    /// request through the custom one-shot-client send path, which bypasses the
    /// process-wide circuit breaker.
    ///
    /// **Env proxies are bypassed.** The pinned client is built with
    /// `.no_proxy()`, so `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` are ignored
    /// and the socket connects directly to `addr`. This is required for the pin
    /// to hold: reqwest checks proxy interception before the connector where the
    /// `resolve()` override applies, so a configured proxy would otherwise
    /// receive the request and re-resolve the host — defeating the pin.
    #[must_use]
    pub const fn pin_to(mut self, addr: SocketAddr) -> Self {
        self.pin_addr = Some(addr);
        self
    }

    /// Route this request through the SSRF-safe resolve→validate→pin send path
    /// documented on [`Client::get_ssrf_safe`], for any HTTP method.
    ///
    /// [`Client::get_ssrf_safe`] only builds `GET` requests, but the guarantee
    /// it describes — reject a resolved address on the built-in SSRF deny-list,
    /// pin the connection to the validated set, re-validate on every redirect
    /// hop — is implemented by [`Self::send`] purely from this flag and is not
    /// GET-specific. Any outbound call whose destination is not a value the
    /// app itself chose — a webhook subscriber's `target_url`, a user-supplied
    /// callback, an OAuth discovery endpoint — needs this on **every** verb it
    /// uses, not only reads. Chain it after [`Client::post`], [`Client::put`],
    /// etc.:
    ///
    /// ```rust,ignore
    /// client.post(target_url).ssrf_safe().json(&payload).send().await?;
    /// ```
    ///
    /// Same incompatibility with [`pin_to`](Self::pin_to) as `get_ssrf_safe`:
    /// this path performs its own per-hop resolve→validate→pin, so chaining an
    /// explicit pin is rejected at send time with
    /// [`ClientError::PinNotAllowedWithSsrfSafe`].
    #[must_use]
    pub const fn ssrf_safe(mut self) -> Self {
        self.ssrf_safe = true;
        self
    }

    /// Keep circuit-breaker accounting even when this request also needs the
    /// custom send path (`ssrf_safe()`, `pin_to()`, `no_redirect()`,
    /// `follow_redirects()`).
    ///
    /// That custom path otherwise bypasses `send_recorded`'s breaker block
    /// entirely — right for its usual case, a one-off fetch of an arbitrary
    /// caller-chosen URL, where per-host breaker state is mostly noise. A
    /// caller instead making *repeated* calls to a small, durable set of
    /// hosts — outbound webhook delivery is the motivating case (#2480 code
    /// review) — still wants the breaker: without it, a down receiver never
    /// fails fast, and every queued delivery pays a full
    /// DNS-resolve-and-connect timeout instead of the open-breaker
    /// short-circuit every other outbound call gets.
    ///
    /// This flag is read from *inside* `send_recorded`, not layered on
    /// externally, which is what makes it free to get right on every other
    /// axis `send` already handles correctly: a mocked client's `self.mock.is_some()`
    /// check runs first regardless (mock behavior is unaffected); a capsule
    /// replay's `current_tape()` check happens even earlier, in `send` itself,
    /// before `send_recorded` is ever reached (an open-breaker attempt is
    /// therefore never gated on live state during replay, and — because it
    /// stays behind `send`'s own capture tee — an open-breaker attempt made
    /// *while capturing* is recorded into the capsule exactly like any other
    /// outcome, so a later replay of that exact run reproduces
    /// `CircuitBreakerOpen` instead of diverging).
    ///
    /// Breaker keying always uses this request's *resolved* URL (`self.url`
    /// — already expanded past any `[http.client.base_urls]` alias by
    /// `Client::build_request`), so an alias-targeted destination keys the
    /// same breaker bucket a literal-URL request to the same host would.
    #[must_use]
    pub(crate) const fn breaker_scoped(mut self) -> Self {
        self.breaker_scoped = true;
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

        // Replay serves outbound calls from the capsule's effect tape (#1634) and never
        // dials the peer. A call the tape cannot answer is a divergence, already logged by
        // `next_http`, and fails closed here. The unconditional block below stays as the
        // backstop for a replay process whose current task carries no tape — app boot, a
        // state initializer, work the handler spawned — because a capsule records no
        // response for any of those either. The check is here rather than on the client
        // because a handler can build one any way it likes (`Client::new()`, `from_state`,
        // a stored one) and every path must be closed.
        #[cfg(feature = "reporting")]
        if let Some(tape) = crate::capsule::effects::current_tape() {
            let method = self.method.to_string();
            // Derived exactly as `OutboundRecorder::arm` derives the recorded
            // half, so the comparison is like with like. No cap on the body:
            // the capsule's own cap already applied at record time, and a
            // recording that hit it is refused before it ever reaches replay.
            let headers = caller_headers(&self);
            let body = self
                .body
                .as_ref()
                .map_or(crate::capsule::CapsuleBody::Absent, |body| {
                    encode_body(body, usize::MAX)
                });
            return replayed_response(
                &tape,
                &crate::capsule::effects::OutboundRequest {
                    method: &method,
                    url: &self.url,
                    headers: &headers,
                    body: &body,
                },
            );
        }
        if outbound_blocked_for_replay() {
            return Err(ClientError::BlockedDuringReplay(
                self.method.to_string(),
                self.url.clone(),
            ));
        }

        // Capture: the exchange is recorded into the in-flight request's
        // capsule scope, when there is one. Recording wraps every send path
        // below (mock, custom, breaker-guarded) so a capsule cannot miss an
        // exchange because the caller happened to use `no_redirect` or a
        // pinned address.
        let recorder = OutboundRecorder::arm(&self);
        let result = self.send_recorded().await;
        recorder.finish(&result);
        result
    }

    /// [`send`](Self::send), minus the replay gate and the capture tee.
    async fn send_recorded(mut self) -> Result<Response, ClientError> {
        // Staging fault injection (#3071). Inert outside a fault scope.
        crate::fault_injection::inject(crate::fault_injection::FaultTarget::Http)
            .await
            .map_err(|fault| ClientError::FaultInjected(fault.to_string()))?;
        // After the capture tee armed, so a capsule records only the caller's
        // own headers and replay compares like with like.
        if self.forward_request_id {
            add_current_request_id(&mut self.extra_headers);
        }
        // After the capture tee, so a capsule records the caller's headers
        // only and replays without a random key.
        self.ensure_idempotency_key();
        self.ensure_criticality_header();

        // A sim network serves every send path, so nothing reaches the real
        // network. Like mocks, it bypasses the process-global breaker.
        if let Some(net) = self.sim_net.clone() {
            return self.send_sim(&net).await;
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
        // per-host breaker registry — unless the caller opted in via
        // `breaker_scoped()` (repeated calls to a small, durable host set; see
        // its doc comment).
        if self.needs_custom_path() {
            if self.breaker_scoped {
                return self.send_custom_breaker_guarded().await;
            }
            return self.send_custom(false).await;
        }

        // ── Resilience / Circuit Breaker ──────────────────────────────────
        let breaker = breaker_for_url(self.resilience_config.as_ref(), &self.url);

        // Check if circuit breaker is open
        let Ok(guard) = breaker.admit() else {
            return Err(ClientError::CircuitBreakerOpen);
        };

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
            // Neither says anything about the host. The caller ran out of
            // time (issue #3058): a cancelled call, which counts only past the
            // slow-call threshold. A local throttle reject (#3068): dropping
            // the guard records nothing for a fast call and frees a half-open
            // slot.
            Err(ClientError::DeadlineExceeded | ClientError::ThrottledLocally { .. }) => {
                drop(guard);
            }
            Err(_) => {
                guard.failure();
            }
        }
        res
    }

    /// The custom send path (`send_custom`), with breaker accounting exactly
    /// like the plain-path breaker block above: `admit` gate,
    /// `CircuitBreakerGuard` covering the call (its `Drop` releases a
    /// half-open slot if this future is cancelled or panics before finishing),
    /// `< 500` success threshold. Only reached when `breaker_scoped()` was
    /// set — see its doc comment for why this has to live here rather than in
    /// an external wrapper around `send()`.
    async fn send_custom_breaker_guarded(self) -> Result<Response, ClientError> {
        let breaker = breaker_for_url(self.resilience_config.as_ref(), &self.url);
        let Ok(guard) = breaker.admit() else {
            return Err(ClientError::CircuitBreakerOpen);
        };
        // Like the breaker, the throttle keeps per-host state, so on the
        // custom path it covers only `breaker_scoped` calls: a small, durable
        // host set. Unscoped custom calls go to user-supplied URLs, and one
        // entry per host would grow without bound. The ticket is taken after
        // the breaker admits, so a fail-fast `CircuitBreakerOpen` call never
        // reaches the throttle. `send_one` retries inside, so the throttle
        // counts the call once. A local reject leaves the breaker neutral.
        let ticket = match self.throttle_attempt(None) {
            Ok(ticket) => ticket,
            Err(error) => {
                drop(guard);
                return Err(error);
            }
        };
        // Mirrors the
        // plain-path breaker block's own `is_half_open` (passed to
        // `send_inner` to force a single attempt): a half-open probe is a
        // budgeted, limited trial (`half_open_trial_count`), and letting
        // `send_one`'s own retry loop turn one trial into several real
        // network attempts spends that budget on one logical delivery
        // instead of testing recovery with independent probes (#2480
        // review, round 10).
        let is_half_open = breaker.state() == crate::circuit_breaker::CircuitState::HalfOpen;
        let res = self.send_custom(is_half_open).await;
        record_call(ticket, &res);
        match &res {
            Ok(resp) if resp.status().as_u16() < 500 => guard.success(),
            // The caller ran out of time, not the upstream (issue #3058): a
            // cancelled call, which counts only past the slow-call threshold.
            Err(ClientError::DeadlineExceeded) => drop(guard),
            _ => guard.failure(),
        }
        res
    }

    async fn send_inner(self, suppress_retries: bool) -> Result<Response, ClientError> {
        // ── Mock short-circuit ──────────────────────────────────────────────
        if let Some(ref mock) = self.mock {
            return self.mock_response(mock);
        }

        // ── Real network request with retries ───────────────────────────────
        let start = crate::time::ambient_instant();
        let max_attempts = self.max_attempts(suppress_retries);
        let mut gate = self.retry_gate(url_host(&self.url).as_deref());
        // A caller's own deadline header also needs a new value per attempt
        // and per hop, so the client follows redirects itself then too.
        let caller_header = self
            .extra_headers
            .get(DEADLINE_HEADER)
            .and_then(crate::deadline::parse_header)
            .is_some();
        if gate.deadline.is_some() || caller_header {
            return self.follow_pooled(suppress_retries, &gate).await;
        }
        let mut last_retry = None;
        let mut delay = Duration::ZERO;
        // Hosts refilled for this request: the first one already was.
        let mut refilled: std::collections::HashSet<String> =
            url_host(&self.url).into_iter().collect();

        for attempt in 0..max_attempts {
            if attempt > 0 {
                tokio::time::sleep(delay).await;
            }
            // A throttled attempt ends the call, retry or not.
            let ticket = self.begin_attempt(&gate, None)?;
            let last = attempt + 1 == max_attempts;
            let (span, sent, followed) =
                self.send_plain_attempt(&gate, attempt, &mut refilled).await;
            match sent {
                Ok(resp) => {
                    let status = resp.status();
                    let headers = resp.headers().clone();
                    let url_used = resp.url().clone();

                    // 429 and 502-504: retry while attempts, time and budget
                    // remain. The HTTP stack may have followed a redirect, so
                    // charge the host that answered.
                    if is_retryable_response(status.as_u16()) && !last {
                        gate.rekey(url_used.as_str());
                        let wait = self.retry_policy.retry_delay(
                            &*self.entropy,
                            attempt,
                            retry_hint(status.as_u16(), &headers),
                        );
                        let kind = retry_kind(status.as_u16());
                        if gate.allow(kind, wait) {
                            if let Some(ticket) = ticket {
                                ticket.record(throttle_accepts(status.as_u16()));
                            }
                            delay = wait;
                            last_retry = Some(kind);
                            continue;
                        }
                    }

                    let body = if self.discard_response_body {
                        // Dropped unread — see `discard_response_body`.
                        Ok(Bytes::new())
                    } else {
                        read_body_in_span(resp, &span, &gate).await
                    };
                    // Count the attempt only now: a body that fails to arrive
                    // is a transport error, not an accept.
                    if let Some(ticket) = ticket {
                        ticket.record(body.is_ok() && throttle_accepts(status.as_u16()));
                    }
                    let body = body?;
                    // Refund only after the body arrived.
                    gate.finish(last_retry, status.as_u16());
                    let elapsed = crate::time::ambient_instant().saturating_duration_since(start);
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
                // The request deadline stopped the attempt.
                Err(e) if e.is_timeout() && gate.expired() => {
                    return Err(ClientError::DeadlineExceeded);
                }
                // Only retry transient connect/timeout errors; non-transient errors
                // (e.g. malformed URL) fail immediately.
                Err(e) if (e.is_connect() || e.is_timeout()) && !last => {
                    if let Some(ticket) = ticket {
                        ticket.record(false);
                    }
                    // The HTTP stack may have followed a redirect: charge the
                    // host that failed.
                    // The host that failed: the last redirect target, or the
                    // request's own host.
                    gate.rekey(followed.last().map_or(self.url.as_str(), String::as_str));
                    let wait = self.retry_policy.retry_delay(&*self.entropy, attempt, None);
                    if !gate.allow(RetryKind::Transient, wait) {
                        return Err(ClientError::Request(e.without_url()));
                    }
                    delay = wait;
                    last_retry = Some(RetryKind::Transient);
                }
                Err(e) => {
                    if let Some(ticket) = ticket {
                        ticket.record(false);
                    }
                    return Err(ClientError::Request(e.without_url()));
                }
            }
        }

        // The retry loop always returns inside the last attempt; this is unreachable.
        unreachable!("retry loop exited without returning a result — this is a bug")
    }

    /// The deadline and retry budget for one send to `host`.
    fn retry_gate(&self, host: Option<&str>) -> RetryGate {
        RetryGate::start(
            self.retry.budgets.clone(),
            host,
            self.retry.send_deadline_header,
        )
    }

    /// The plain path under a request deadline. The pooled client does not
    /// follow a redirect then (see `pooled_redirect_policy`), so this follows
    /// it, and each hop sends the time left at that hop.
    async fn follow_pooled(
        self,
        suppress_retries: bool,
        gate: &RetryGate,
    ) -> Result<Response, ClientError> {
        // `send_one` asks the throttle (issue #3068) for each attempt of
        // each hop, as the plain path does for each attempt.
        let client = self.client.clone();
        MANUAL_REDIRECTS
            .scope(
                true,
                self.follow_loop(
                    PLAIN_MAX_REDIRECTS,
                    Arc::new(|_: &str| true),
                    HopClient::Pooled(&client),
                    suppress_retries,
                    gate,
                ),
            )
            .await
    }

    /// Add an `Idempotency-Key` header when this request can retry a
    /// non-idempotent method and the caller set no key. Every attempt sends
    /// the same key, so the server can drop a duplicate.
    fn ensure_idempotency_key(&mut self) {
        let retries_unsafe_method = !is_idempotent_method(&self.method)
            && !self.retry_policy.retry_idempotent_only
            && self.retry_policy.max_retries > 0;
        if !retries_unsafe_method || self.extra_headers.contains_key(IDEMPOTENCY_KEY) {
            return;
        }
        let key = self.entropy.uuid_v4().to_string();
        if let Ok(value) = HeaderValue::from_str(&key) {
            self.extra_headers
                .insert(HeaderName::from_static(IDEMPOTENCY_KEY), value);
        }
    }

    /// Add the `X-Autumn-Criticality` header (issue #3068): the value set by
    /// [`Self::criticality`], else the inbound request's when it is not
    /// `default` (a missing header means `default`, so the client does not
    /// send it to every host). A header that the caller set wins.
    fn ensure_criticality_header(&mut self) {
        if self
            .extra_headers
            .contains_key(crate::admission::CRITICALITY_HEADER)
        {
            return;
        }
        let inbound = crate::admission::current_criticality()
            .filter(|c| *c != crate::admission::Criticality::Default);
        if let Some(c) = self.criticality.or(inbound) {
            self.extra_headers.insert(
                HeaderName::from_static(crate::admission::CRITICALITY_HEADER),
                HeaderValue::from_static(c.as_str()),
            );
        }
    }

    /// Start one attempt: stop at the deadline, ask the throttle, then let
    /// the gate record the attempt. The throttle is asked before
    /// [`RetryGate::check`], so a throttled attempt refills no budget.
    fn begin_attempt(
        &self,
        gate: &RetryGate,
        host: Option<&str>,
    ) -> Result<Option<ThrottleTicket>, ClientError> {
        if gate.expired() {
            return Err(ClientError::DeadlineExceeded);
        }
        let ticket = self.throttle_attempt(host)?;
        gate.check()?;
        Ok(ticket)
    }

    /// Send one plain-path attempt in its own CLIENT span (issue #3064). The
    /// trace context is injected inside the span, so the next service's
    /// parent is this attempt. Returns the span, so the caller reads the body
    /// in it, the send result and the redirect targets followed. Each host
    /// that served a redirect gets its refill, even when the send or the body
    /// fails later.
    async fn send_plain_attempt(
        &self,
        gate: &RetryGate,
        attempt: u32,
        refilled: &mut std::collections::HashSet<String>,
    ) -> (
        tracing::Span,
        Result<reqwest::Response, reqwest::Error>,
        Vec<String>,
    ) {
        let span = client_attempt_span(&self.method, &self.url, attempt);
        let req = self.plain_attempt(gate, &span);
        let (sent, followed) =
            tracing::Instrument::instrument(send_tracking_redirects(req), span.clone()).await;
        record_send_outcome(&span, &sent);
        gate.record_destinations(&followed, refilled);
        (span, sent, followed)
    }

    /// The request of one plain-path attempt: the attempt timeout under a
    /// deadline, the trace context, then the caller's headers, which may
    /// override or extend the trace headers.
    fn plain_attempt(&self, gate: &RetryGate, span: &tracing::Span) -> reqwest::RequestBuilder {
        let timeout = gate.attempt_timeout(self.retry_policy.request_timeout);
        let mut req = self.client.request(self.method.clone(), &self.url);
        if gate.deadline.is_some()
            && let Some(timeout) = timeout
        {
            req = req.timeout(timeout);
        }
        req = span.in_scope(|| inject_trace_context(req, &self.extra_headers));
        req = with_caller_headers(req, gate, timeout, &self.extra_headers);
        if let Some(body) = &self.body {
            req = req.body(body.clone());
        }
        req
    }

    /// Ask the throttle for one attempt (issue #3068). `host` is the throttle
    /// key; `None` takes it from the URL. Returns `Ok(None)` without a
    /// throttle, and [`ClientError::ThrottledLocally`] when it rejects.
    fn throttle_attempt(&self, host: Option<&str>) -> Result<Option<ThrottleTicket>, ClientError> {
        let Some(throttle) = &self.throttle else {
            return Ok(None);
        };
        let Some(host) = host.map(str::to_owned).or_else(|| throttle_host(&self.url)) else {
            return Ok(None);
        };
        admit_throttle(throttle, host, &*self.entropy).map(Some)
    }

    /// How many attempts the retry policy allows for this request.
    const fn max_attempts(&self, suppress_retries: bool) -> u32 {
        if suppress_retries {
            1
        } else if is_idempotent_method(&self.method) || !self.retry_policy.retry_idempotent_only {
            self.retry_policy.max_retries.saturating_add(1)
        } else {
            1
        }
    }

    /// The canned response for this request from `mock`, or
    /// [`ClientError::NoMock`].
    fn mock_response(&self, mock: &MockRegistry) -> Result<Response, ClientError> {
        let Some(mock_resp) = mock.find_match(&self.method, &self.url, self.alias.as_deref())
        else {
            // A mock registry is present but nothing matched — treat as a test
            // failure rather than falling through to the network.
            return Err(ClientError::NoMock(
                self.method.to_string(),
                self.url.clone(),
            ));
        };
        let status =
            reqwest::StatusCode::from_u16(mock_resp.status).unwrap_or(reqwest::StatusCode::OK);
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

        Ok(Response {
            status,
            headers: HeaderMap::new(),
            body: Bytes::from(body_bytes),
            url: None,
        })
    }

    /// Send through the simulated network (issue #2967), with the attempts,
    /// jittered backoff, `Retry-After` on 429 and 503, and per-attempt
    /// timeout of the real retry loop.
    async fn send_sim(self, net: &crate::sim::SimNet) -> Result<Response, ClientError> {
        use tracing::Instrument as _;

        let url = self.sim_url()?;
        let host = url
            .host_str()
            .ok_or_else(|| ClientError::InvalidUrl(format!("{}: no host", self.url)))?
            .to_owned();
        let max_attempts = self.max_attempts(false);
        let gate = self.retry_gate(url_host(url.as_str()).as_deref());
        let mut last_retry = None;
        let mut delay = Duration::ZERO;
        for attempt in 0..max_attempts {
            if attempt > 0 {
                tokio::time::sleep(delay).await;
            }
            let ticket = self.begin_attempt(&gate, Some(&host))?;
            let last = attempt + 1 == max_attempts;
            // One CLIENT span per attempt, as on the real send paths. The host
            // router sees this span in its `traceparent`.
            let span = client_attempt_span(&self.method, url.as_str(), attempt);
            let timeout = gate.attempt_timeout(self.retry_policy.request_timeout);
            let deadline_header = gate.header(timeout, &self.extra_headers);
            let exchange = self
                .sim_attempt(net, &host, &url, timeout, deadline_header)
                .instrument(span.clone());
            // Under a deadline, check it before each poll of the attempt, as
            // the real paths' request timeout does: `tokio::time::timeout`
            // polls the attempt first, so an answer ready at the deadline
            // would still be used.
            let exchange = async {
                match gate.deadline {
                    Some(deadline) => crate::deadline::Bounded::until(deadline, exchange)
                        .await
                        .unwrap_or_else(|crate::deadline::DeadlineExceeded| {
                            Err(SimAttemptError::Transient(format!(
                                "request to {host} reached the request deadline"
                            )))
                        }),
                    None => exchange.await,
                }
            };
            let outcome = match timeout {
                Some(limit) => {
                    tokio::time::timeout(limit, exchange)
                        .await
                        .unwrap_or_else(|_elapsed| {
                            Err(SimAttemptError::Transient(format!(
                                "request to {host} timed out after {limit:?}"
                            )))
                        })
                }
                None => exchange.await,
            };
            match &outcome {
                Ok(response) => record_attempt_status(&span, response.status.as_u16()),
                Err(SimAttemptError::Transient(_)) => record_attempt_error(&span, "network"),
                Err(SimAttemptError::Fatal(_)) => record_attempt_error(&span, "request"),
            }
            drop(span);
            // A transient failure at the caller's deadline says nothing about
            // the host: its ticket is dropped, which counts as an accept.
            let deadline_stop =
                matches!(outcome, Err(SimAttemptError::Transient(_))) && gate.expired();
            if let Some(ticket) = ticket
                && !deadline_stop
            {
                ticket.record(matches!(&outcome, Ok(r) if throttle_accepts(r.status.as_u16())));
            }
            let response = match outcome {
                Ok(response) => response,
                // A drop or a timeout is transient, like a real connect or
                // timeout error, so it is retried.
                Err(SimAttemptError::Transient(_)) if gate.expired() => {
                    return Err(ClientError::DeadlineExceeded);
                }
                Err(SimAttemptError::Transient(message)) if !last => {
                    let wait = self.retry_policy.retry_delay(&*self.entropy, attempt, None);
                    if !gate.allow(RetryKind::Transient, wait) {
                        return Err(ClientError::SimNetwork(message));
                    }
                    delay = wait;
                    last_retry = Some(RetryKind::Transient);
                    continue;
                }
                Err(SimAttemptError::Transient(message)) => {
                    return Err(ClientError::SimNetwork(message));
                }
                Err(SimAttemptError::Fatal(error)) => return Err(error),
            };
            let status = response.status.as_u16();
            if is_retryable_response(status) && !last {
                let wait = self.retry_policy.retry_delay(
                    &*self.entropy,
                    attempt,
                    retry_hint(status, &response.headers),
                );
                let kind = retry_kind(status);
                if gate.allow(kind, wait) {
                    delay = wait;
                    last_retry = Some(kind);
                    continue;
                }
            }
            gate.finish(last_retry, status);
            return Ok(response);
        }
        unreachable!("the sim retry loop returns on its last attempt")
    }

    /// The absolute URL a sim call goes to. A relative URL on a named client
    /// (`client.named("payments").get("/charge")`) goes to the host named by
    /// the alias, as a named http mock would match it.
    fn sim_url(&self) -> Result<reqwest::Url, ClientError> {
        match (reqwest::Url::parse(&self.url), self.alias.as_deref()) {
            (Ok(url), _) => Ok(url),
            (Err(url::ParseError::RelativeUrlWithoutBase), Some(alias)) => {
                let path = self.url.trim_start_matches('/');
                reqwest::Url::parse(&format!("http://{alias}/{path}"))
                    .map_err(|error| ClientError::InvalidUrl(format!("{}: {error}", self.url)))
            }
            (Err(error), _) => Err(ClientError::InvalidUrl(format!("{}: {error}", self.url))),
        }
    }

    /// One attempt through the simulated network: the network, then the host
    /// router or the http mocks.
    async fn sim_attempt(
        &self,
        net: &crate::sim::SimNet,
        host: &str,
        url: &reqwest::Url,
        timeout: Option<Duration>,
        deadline_header: Option<HeaderValue>,
    ) -> Result<Response, SimAttemptError> {
        net.transmit(host, timeout)
            .await
            .map_err(|fault| SimAttemptError::Transient(format!("request to {host} {fault}")))?;
        match (net.service(host), self.mock.as_ref()) {
            (Some(router), _) => serve_sim_host(router, self, url.clone(), deadline_header)
                .await
                .map_err(SimAttemptError::Fatal),
            (None, Some(mock)) => self.mock_response(mock).map_err(SimAttemptError::Fatal),
            (None, None) => Err(SimAttemptError::Fatal(ClientError::SimNetwork(format!(
                "no sim host named {host}"
            )))),
        }
    }

    /// `true` when any security-hardening option requires the custom send path.
    const fn needs_custom_path(&self) -> bool {
        self.ssrf_safe
            || self.pin_addr.is_some()
            || !matches!(self.redirect_mode, RedirectMode::Default)
    }

    /// Dispatch to the appropriate custom send path. Consumes `self`.
    ///
    /// `is_half_open`: `true` when this call is a circuit-breaker half-open
    /// probe (only ever passed by [`Self::send_custom_breaker_guarded`]);
    /// forces every `send_one` this dispatch reaches down to a single
    /// attempt, exactly like the plain-path breaker block's own
    /// `send_inner(is_half_open)` already does — see
    /// `send_custom_breaker_guarded`'s doc comment.
    async fn send_custom(self, is_half_open: bool) -> Result<Response, ClientError> {
        // Reject the incompatible `get_ssrf_safe` + `pin_to` combination up
        // front — deterministically, before any network I/O. `get_ssrf_safe`
        // routes through `send_ssrf_safe`, which runs its OWN per-hop
        // resolve→validate→pin and never reads `self.pin_addr`; a caller's
        // explicit `pin_to(addr)` would therefore be silently ignored. Fail
        // loudly instead so the mismatch is caught rather than masked. Use
        // `pin_to` alone for a caller-chosen fixed address, or `get_ssrf_safe`
        // alone for guarded automatic per-hop pinning.
        if self.ssrf_safe && self.pin_addr.is_some() {
            return Err(ClientError::PinNotAllowedWithSsrfSafe(
                "get_ssrf_safe cannot be combined with pin_to: the SSRF-safe path \
                 performs its own per-hop resolve/validate/pin and never reads the \
                 pin_to address, so an explicit pin would be silently ignored. Use \
                 pin_to alone for a caller-chosen address, or get_ssrf_safe alone \
                 for guarded automatic per-hop pinning.",
            ));
        }

        // Reject the incompatible `pin_to` + `follow_redirects` combination up
        // front — deterministically, before issuing any request. A single pinned
        // `SocketAddr` only applies to hop 0 of `follow_loop`; later hops resolve
        // via normal DNS, so following a cross-host `3xx` would silently escape
        // the pin and defeat its purpose. `get_ssrf_safe` never sets `pin_addr`,
        // so its per-hop resolve→validate→pin path is unaffected by this guard.
        if self.pin_addr.is_some() && matches!(self.redirect_mode, RedirectMode::Follow { .. }) {
            return Err(ClientError::IncompatiblePinRedirect(
                "pin_to cannot be combined with follow_redirects: the pin only \
                 covers the first hop and later redirect hops re-resolve via DNS, \
                 escaping the pin. Use get_ssrf_safe for pinned, per-hop-revalidated \
                 redirect following; pin_to alone (which returns the 3xx unfollowed); \
                 or follow_redirects without pin_to.",
            ));
        }

        // Reject `pin_to` on an IP-literal URL host up front, deterministically, before
        // any network I/O. reqwest and hyper treat an IP-literal host as already resolved
        // and do not consult the DNS resolver, so the `resolve_to_addrs` override
        // `build_oneshot_client` installs — which is what enforces the pin — is skipped
        // and the socket connects to the literal in the URL rather than the pinned
        // address, silently violating `pin_to`'s documented guarantee. `get_ssrf_safe`
        // never sets `pin_addr`, and validates and connects to the same literal, so it
        // stays safe; this guard targets only the explicit `pin_to` primitive.
        if self.pin_addr.is_some() && url_host_is_ip_literal(&self.url)? {
            return Err(ClientError::PinRequiresDomainHost(
                "pin_to cannot be honored for an IP-literal URL host because the \
                 HTTP stack connects to the literal directly and skips the pinned \
                 address; put the desired IP directly in the URL, or use a domain host.",
            ));
        }

        // One gate for the whole send, across redirect hops. The request
        // deadline also makes the one-shot client timeout shorter.
        let gate = self.retry_gate(url_host(&self.url).as_deref());
        // Only the deadline here: the first-attempt refill waits for
        // `send_one`, after DNS and address checks that may stop the call.
        if gate.expired() {
            return Err(ClientError::DeadlineExceeded);
        }
        let timeout = gate
            .attempt_timeout(self.retry_policy.request_timeout)
            .unwrap_or_else(|| Duration::from_secs(30));

        if self.ssrf_safe {
            return self.send_ssrf_safe(timeout, is_half_open, &gate).await;
        }

        // Extract the follow parameters (ending the borrow) before moving `self`.
        let follow = match &self.redirect_mode {
            RedirectMode::Follow { max, validator } => Some((*max, validator.clone())),
            RedirectMode::None | RedirectMode::Default => None,
        };
        if let Some((max, validator)) = follow {
            return self
                .follow_loop(
                    max,
                    validator,
                    HopClient::OneShot(timeout),
                    is_half_open,
                    &gate,
                )
                .await;
        }

        // Only `RedirectMode::None` (explicit `no_redirect`) and the pin-only
        // `RedirectMode::Default` reach here (`Follow` and `ssrf_safe` returned
        // above). Both use `Policy::none()`: a 3xx is returned to the caller
        // verbatim rather than being auto-followed. Critically, for the
        // pin-only path this stops reqwest from silently following a cross-host
        // redirect and re-resolving the new host via normal DNS — which would
        // defeat the pin. Callers who want to follow redirects while staying
        // pinned/rebind-safe use `get_ssrf_safe` (or `follow_redirects`).
        let policy = reqwest::redirect::Policy::none();
        let resolve = self.pin_resolve()?;
        let client = build_oneshot_client(resolve, policy, timeout)?;
        send_one(
            &client,
            &self.method,
            &self.url,
            &self.extra_headers,
            self.body.as_ref(),
            &self.retry_policy,
            &*self.entropy,
            self.discard_response_body,
            None,
            is_half_open,
            &gate,
            false,
            None,
            None,
        )
        .await
    }

    /// Compute the `(host, addr)` resolve override for a pinned request, if any.
    fn pin_resolve(&self) -> Result<Option<(String, Vec<SocketAddr>)>, ClientError> {
        match self.pin_addr {
            // Single-address pin routed through the same set-based path as the
            // multi-address SSRF-safe pin (a one-element slice).
            Some(addr) => Ok(Some((host_of(&self.url)?, vec![addr]))),
            None => Ok(None),
        }
    }

    /// Manual redirect-following loop with per-hop validation (Feature #1238).
    async fn follow_loop(
        self,
        max: usize,
        validator: RedirectValidator,
        hop_client: HopClient<'_>,
        is_half_open: bool,
        gate: &RetryGate,
    ) -> Result<Response, ClientError> {
        let original =
            url::Url::parse(&self.url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        let mut current = self.url.clone();
        // Threaded across hops so cross-origin header stripping (Fix A) and
        // RFC method/body rewriting (Fix B) accumulate correctly.
        let mut method = self.method.clone();
        let mut headers = self.extra_headers.clone();
        let mut body = self.body.clone();
        // On the pooled path the whole chain shares one retry count, as when
        // reqwest follows the redirects. It comes from the original method:
        // a `POST` that a 303 turns into a `GET` gets no retries the `POST`
        // did not have.
        let chain_retries = matches!(hop_client, HopClient::Pooled(_))
            .then(|| ChainRetries::new(self.max_attempts(is_half_open).saturating_sub(1)));
        // Hosts refilled for this chain: the first one already was.
        let mut refilled: std::collections::HashSet<String> =
            url_host(&self.url).into_iter().collect();
        for hop in 0.. {
            // Pin only applies to the first hop's original target.
            let resolve = if hop == 0 {
                match self.pin_addr {
                    Some(addr) => Some((host_of(&current)?, vec![addr])),
                    None => None,
                }
            } else {
                None
            };
            // On any post-origin hop, drop credential-bearing headers if the
            // current target is cross-origin (stays stripped once stripped).
            if hop > 0 {
                strip_sensitive_headers_if_cross_origin(&mut headers, &original, &current)?;
            }
            let client = match hop_client {
                HopClient::Pooled(client) => client.clone(),
                HopClient::OneShot(timeout) => {
                    build_oneshot_client(resolve, reqwest::redirect::Policy::none(), timeout)?
                }
            };
            // Each hop uses the retry budget of its own host.
            let hop_gate;
            let hop_gate = if hop == 0 {
                gate
            } else {
                hop_gate = gate.for_hop(&current, &mut refilled);
                &hop_gate
            };
            let resp = send_one(
                &client,
                &method,
                &current,
                &headers,
                body.as_ref(),
                &self.retry_policy,
                &*self.entropy,
                self.discard_response_body,
                None,
                is_half_open,
                hop_gate,
                true,
                chain_retries.as_ref(),
                // The custom path counts a whole call instead (see
                // `send_custom_breaker_guarded`).
                matches!(hop_client, HopClient::Pooled(_))
                    .then_some(self.throttle.as_ref())
                    .flatten(),
            )
            .await?;

            let next = match redirect_target(&resp, &current) {
                Ok(Some(next)) => next,
                Ok(None) => return Ok(resp),
                // reqwest returns a redirect with a bad `Location` as is.
                Err(ClientError::InvalidUrl(_)) if matches!(hop_client, HopClient::Pooled(_)) => {
                    return Ok(resp);
                }
                Err(error) => return Err(error),
            };
            if hop >= max {
                return Err(ClientError::TooManyRedirects(max));
            }
            if !validator(&next) {
                return Err(ClientError::RedirectRejected(next));
            }
            // RFC 7231/7538 method+body rewriting before the next hop.
            rewrite_after_redirect(resp.status(), &mut method, &mut body, &mut headers);
            // The pooled client stands in for reqwest's own redirect
            // handling, which sets `Referer`.
            if matches!(hop_client, HopClient::Pooled(_)) {
                set_referer(&mut headers, &next, &current);
            }
            current = next;
        }
        unreachable!("redirect loop is bounded by `max` and always returns")
    }

    /// Derive the SSRF-safe redirect plan `(follow, max)` from the builder's
    /// [`RedirectMode`], so a chained `no_redirect()` / `follow_redirects(..)`
    /// overrides the default hop cap on the SSRF-safe path:
    ///
    /// - [`RedirectMode::Default`] → `(true, SSRF_SAFE_MAX_REDIRECTS)`.
    /// - [`RedirectMode::None`] (`no_redirect()`) → `(false, 0)` — the initial
    ///   `3xx` is returned verbatim (the `max` is unused).
    /// - [`RedirectMode::Follow { max, .. }`] (`follow_redirects(max, ..)`) →
    ///   `(true, max)`. The caller's per-hop validator is pulled from
    ///   `self.redirect_mode` separately inside the send loop.
    const fn ssrf_redirect_plan(&self) -> (bool, usize) {
        match &self.redirect_mode {
            RedirectMode::Default => (true, SSRF_SAFE_MAX_REDIRECTS),
            RedirectMode::None => (false, 0),
            RedirectMode::Follow { max, .. } => (true, *max),
        }
    }

    /// Composed SSRF-safe send path (Features #1238 + #1239). Resolves and
    /// validates every hop, pins the connection, and rejects scheme downgrades.
    ///
    /// The follow/hop-cap behaviour comes from [`ssrf_redirect_plan`](Self::ssrf_redirect_plan),
    /// so a chained `no_redirect()` / `follow_redirects(max, ..)` overrides the
    /// default cap while every per-hop safety step (resolve→validate→pin,
    /// https→http downgrade block, sensitive-header stripping, method/body
    /// rewrite) still applies.
    /// Runs the whole resolve→validate→pin→redirect operation against one
    /// deadline `timeout` from now, rather than handing every phase of every
    /// hop a fresh `timeout`-length budget.
    ///
    /// Without this, `timeout` only bounded each hop's own connect/response
    /// wait — the DNS lookup in [`resolve_and_validate`] had no timeout of
    /// its own, and every redirect hop got a full fresh `timeout` regardless
    /// of how long earlier hops already took. A subscriber-controlled
    /// destination (the motivating case: outbound webhook delivery, #2480
    /// code review) with stalled DNS or a slow multi-hop redirect chain
    /// could therefore occupy a job worker for `timeout × (hops + 1)` plus
    /// unbounded DNS wait, rather than the single `timeout` every other
    /// outbound call is bounded by.
    ///
    /// The budget shrinks across two seams per hop — the DNS lookup, then
    /// the connect/response reqwest performs — rather than being enforced by
    /// one coarse outer `tokio::time::timeout` wrapping the whole call: a
    /// coarse wrap would race the *same* duration against both this
    /// operation's start and each hop's own reqwest-level timeout, and since
    /// the outer clock always starts first it would almost always fire
    /// first, silently reclassifying an ordinary single-hop stall from
    /// `ClientError::Request` (a `reqwest::Error` callers can query with
    /// `.is_timeout()`) into `ClientError::InvalidUrl` (#2480 review, round
    /// 7). Shrinking the *reqwest* timeout instead means a connect/response
    /// stall in the common (no-redirect) case still times out inside
    /// reqwest itself and keeps that exact, already-documented error shape;
    /// only a stall in the DNS lookup — which had no error shape of its own
    /// to preserve, because it had no timeout at all before this fix —
    /// surfaces as `InvalidUrl`, consistent with the DNS-failure case
    /// immediately below reusing the same variant.
    async fn send_ssrf_safe(
        self,
        timeout: Duration,
        is_half_open: bool,
        gate: &RetryGate,
    ) -> Result<Response, ClientError> {
        let deadline = crate::time::ambient_instant() + timeout;
        let (follow, max) = self.ssrf_redirect_plan();
        let original =
            url::Url::parse(&self.url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        let mut current = self.url.clone();
        // Threaded across hops so cross-origin header stripping (Fix A) and
        // RFC method/body rewriting (Fix B) accumulate correctly.
        let mut method = self.method.clone();
        let mut headers = self.extra_headers.clone();
        let mut body = self.body.clone();
        // Hosts refilled for this chain: the first one already was.
        let mut refilled: std::collections::HashSet<String> =
            url_host(&self.url).into_iter().collect();
        for hop in 0.. {
            let remaining_for_lookup = deadline_remaining_or_timeout(deadline, &current)
                .map_err(|error| gate.classify(error))?;
            // Resolve host → ALL validated addresses (rejects if ANY resolved IP
            // is blocked), then pin the full set so reqwest cannot re-resolve but
            // can still fall back across the validated addresses in order.
            // Wrapped in the *remaining* budget, not the full per-request
            // `timeout`: resolve_and_validate's DNS lookup previously had no
            // timeout of its own at all.
            let addrs = tokio::time::timeout(remaining_for_lookup, resolve_and_validate(&current))
                .await
                .map_err(|_| gate.classify(ssrf_safe_deadline_error(&current)))??;
            let host = host_of(&current)?;
            // Re-measured after the lookup, so a slow DNS response shrinks
            // what's left for the connect/response phase below rather than
            // that phase getting a fresh full `timeout` regardless.
            let remaining_for_connect = deadline_remaining_or_timeout(deadline, &current)
                .map_err(|error| gate.classify(error))?;
            let client = build_oneshot_client(
                Some((host, addrs)),
                reqwest::redirect::Policy::none(),
                remaining_for_connect,
            )?;
            // On any post-origin hop, drop credential-bearing headers if the
            // current target is cross-origin (stays stripped once stripped).
            if hop > 0 {
                strip_sensitive_headers_if_cross_origin(&mut headers, &original, &current)?;
            }
            // Each hop uses the retry budget of its own host.
            let hop_gate;
            let hop_gate = if hop == 0 {
                gate
            } else {
                hop_gate = gate.for_hop(&current, &mut refilled);
                &hop_gate
            };
            let resp = send_one(
                &client,
                &method,
                &current,
                &headers,
                body.as_ref(),
                &self.retry_policy,
                &*self.entropy,
                self.discard_response_body,
                Some(deadline),
                is_half_open,
                hop_gate,
                false,
                None,
                None,
            )
            .await?;

            // Honour a chained `no_redirect()`: return the response verbatim
            // BEFORE parsing the `Location` header. The initial URL was still
            // resolved / validated / pinned above. Parsing `Location` here (via
            // `redirect_target`) would let an untrusted server force an
            // `InvalidUrl` error out of a `no_redirect()` fetch by returning a
            // followable 3xx with a malformed `Location`, violating the
            // documented "return the initial 3xx verbatim" contract.
            if !follow {
                return Ok(resp);
            }
            let Some(next) = redirect_target(&resp, &current)? else {
                return Ok(resp);
            };
            if hop >= max {
                return Err(ClientError::TooManyRedirects(max));
            }
            // Defence-in-depth: reject an https→http downgrade on redirect.
            if scheme_is_https(&current)? && !scheme_is_https(&next)? {
                return Err(ClientError::RedirectRejected(format!(
                    "https→http scheme downgrade on redirect to {next}"
                )));
            }
            // Caller-supplied per-hop validator, present only in the
            // `follow_redirects` override. It runs in ADDITION to the built-in
            // resolve/validate/pin already applied at the top of the loop.
            if let RedirectMode::Follow { validator, .. } = &self.redirect_mode
                && !validator(&next)
            {
                return Err(ClientError::RedirectRejected(next));
            }
            // RFC 7231/7538 method+body rewriting before the next hop.
            rewrite_after_redirect(resp.status(), &mut method, &mut body, &mut headers);
            current = next;
            // The next loop iteration re-resolves + re-validates `current`
            // before connecting, so a redirect to a blocked address is rejected
            // there with `SsrfBlocked`.
        }
        unreachable!("redirect loop is bounded by the SSRF-safe redirect plan")
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

/// Time left until `deadline`, or the deadline error if it has already
/// passed before this hop's next phase (DNS lookup or connect) even began.
/// Used by [`RequestBuilder::send_ssrf_safe`] to shrink the budget handed to
/// each successive phase rather than resetting it every hop.
fn deadline_remaining_or_timeout(
    deadline: std::time::Instant,
    current: &str,
) -> Result<Duration, ClientError> {
    let now = crate::time::ambient_instant();
    if now >= deadline {
        return Err(ssrf_safe_deadline_error(current));
    }
    Ok(deadline - now)
}

/// The error [`RequestBuilder::send_ssrf_safe`] returns when its overall
/// deadline is exhausted — reusing [`ClientError::InvalidUrl`] rather than a
/// new variant (see that call site's doc comment for why), consistent with
/// the adjacent DNS-lookup-failure case in [`resolve_and_validate`] already
/// using the same variant for the same phase.
fn ssrf_safe_deadline_error(current: &str) -> ClientError {
    ClientError::InvalidUrl(format!(
        "SSRF-safe resolve/redirect operation exceeded its deadline resolving {current}"
    ))
}

/// Resolve (or create) the circuit breaker for `url`'s host, honouring a
/// per-client `resilience_config` override exactly as [`RequestBuilder::send_recorded`]'s
/// own breaker path does. Shared by both the plain-path breaker block and
/// [`RequestBuilder::send_custom_breaker_guarded`] so the two cannot drift on
/// how a host name is derived from the URL.
fn breaker_for_url(
    resilience_config: Option<&Arc<crate::config::ResilienceConfig>>,
    url: &str,
) -> crate::circuit_breaker::CircuitBreaker {
    let host = url::Url::parse(url).ok().map_or_else(
        || "unknown".to_owned(),
        |u| {
            let h = u.host_str().unwrap_or("unknown");
            u.port()
                .map_or_else(|| h.to_owned(), |port| format!("{h}:{port}"))
        },
    );

    resilience_config.map_or_else(
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
    )
}

// ── Deadline and retry budget (issue #3058) ──────────────────────────────────

/// Why a retry of a response with `status` is necessary.
const fn retry_kind(status: u16) -> RetryKind {
    if status == 429 {
        RetryKind::Throttling
    } else {
        RetryKind::Transient
    }
}

/// The retry budget key of `url`: its host and its effective port. `None` for
/// a relative URL.
fn url_host(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(
        parsed
            .port_or_known_default()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")),
    )
}

/// Retry budgets and deadline header settings of a [`Client`].
#[derive(Clone)]
struct RetrySettings {
    /// Retry budgets, one for each host. `None` when the budget is off.
    budgets: Option<Arc<RetryBudgets>>,
    /// Send [`DEADLINE_HEADER`] when a request deadline is set.
    send_deadline_header: bool,
}

impl RetrySettings {
    /// Settings for a client made outside an app: its own default budgets.
    fn standalone() -> Self {
        Self {
            budgets: Some(Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()))),
            send_deadline_header: true,
        }
    }
}

/// The shortest time left for which a retry starts. A shorter attempt cannot
/// get an answer, and tokio timers round up to whole milliseconds.
const MIN_ATTEMPT: Duration = Duration::from_millis(10);

/// The app's retry budgets, stored in `AppState`. `config` is the setting
/// they were built from.
#[derive(Clone)]
pub(crate) struct SharedRetryBudgets {
    budgets: Arc<RetryBudgets>,
    config: RetryBudgetConfig,
}

/// Serializes the replacement of the shared retry budgets.
static BUDGETS_REPLACE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The app-wide retry budgets for the effective `config`, or `None` when
/// they are off. A `state_initializer` may replace the config after boot, so
/// budgets built from other settings are replaced, as the shared throttle is.
fn shared_retry_budgets(
    state: &crate::AppState,
    config: &RetryBudgetConfig,
) -> Option<Arc<RetryBudgets>> {
    if !config.enabled {
        return None;
    }
    let current = || {
        state
            .extension::<SharedRetryBudgets>()
            .filter(|shared| shared.config == *config)
            .map(|shared| Arc::clone(&shared.budgets))
    };
    if let Some(budgets) = current() {
        return Some(budgets);
    }
    // Replace under a lock and check again, so that concurrent first calls
    // after a config change share one set. Only this slow path locks.
    let _guard = BUDGETS_REPLACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(budgets) = current() {
        return Some(budgets);
    }
    let budgets = Arc::new(RetryBudgets::new(config));
    state.insert_extension(SharedRetryBudgets {
        budgets: Arc::clone(&budgets),
        config: config.clone(),
    });
    Some(budgets)
}

/// The request deadline and the retry budget for one send.
///
/// All three retry loops (plain, sim, custom) ask it the same questions.
struct RetryGate {
    deadline: Option<Deadline>,
    /// All budgets of the client, to pick the budget of a redirect hop.
    budgets: Option<Arc<RetryBudgets>>,
    /// The budget of the current host.
    budget: Option<Arc<RetryBudget>>,
    /// The key of the current host's budget.
    host: Option<String>,
    send_header: bool,
    /// The first-attempt refill of `budget`, done by the first
    /// [`check`](Self::check) that finds time left, so a request that never
    /// starts an attempt refills nothing.
    refill_pending: AtomicBool,
    /// The tokens [`allow`](Self::allow) took for a retry that has not
    /// started. The next [`check`](Self::check) that lets the retry run keeps
    /// them; a send cancelled in the backoff gives them back on drop.
    pending_retry: std::sync::Mutex<Option<(Arc<RetryBudget>, RetryKind)>>,
    /// The caller's own [`DEADLINE_HEADER`] value as a deadline, fixed at the
    /// first attempt. The value is relative, so a retry or a later hop sends
    /// what is left of it, not the whole value again.
    caller_deadline: std::sync::OnceLock<Option<Deadline>>,
}

impl Drop for RetryGate {
    fn drop(&mut self) {
        let pending = self
            .pending_retry
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((budget, kind)) = pending {
            budget.release(kind);
        }
    }
}

impl RetryGate {
    /// Read the current deadline. The first attempt is recorded in the
    /// budget of `host` when it starts.
    fn start(budgets: Option<Arc<RetryBudgets>>, host: Option<&str>, send_header: bool) -> Self {
        Self::with_deadline(Deadline::current(), budgets, host, send_header)
    }

    fn with_deadline(
        deadline: Option<Deadline>,
        budgets: Option<Arc<RetryBudgets>>,
        host: Option<&str>,
        send_header: bool,
    ) -> Self {
        let budget = budgets
            .as_deref()
            .zip(host)
            .map(|(budgets, host)| budgets.for_host(host));
        Self {
            deadline,
            budgets,
            budget,
            host: host.map(str::to_owned),
            send_header,
            refill_pending: AtomicBool::new(true),
            pending_retry: std::sync::Mutex::new(None),
            caller_deadline: std::sync::OnceLock::new(),
        }
    }

    /// Move to the budget of `url`'s host when it is not the current host,
    /// for example after the HTTP stack followed a redirect.
    ///
    /// The plain path refills a redirect host with
    /// [`record_destinations`](Self::record_destinations), so this does not.
    fn rekey(&mut self, url: &str) {
        let host = url_host(url);
        if host.is_some() && host != self.host {
            self.budget = self
                .budgets
                .as_deref()
                .zip(host.as_deref())
                .map(|(budgets, host)| budgets.for_host(host));
            self.host = host;
        }
    }

    /// Record a first attempt in the budget of `url`'s host when it is not
    /// the current host: the HTTP stack followed a redirect and the answer is
    /// final. The gate keeps its budget, so a refund still goes to the budget
    /// that paid for the retry.
    fn record_destination(&self, url: &str) {
        let host = url_host(url);
        // No deadline check: the HTTP stack only lists a host it sent to.
        if host.is_none() || host == self.host {
            return;
        }
        if let Some((budgets, host)) = self.budgets.as_deref().zip(host) {
            budgets.for_host(&host).record_request();
        }
    }

    /// [`record_destination`](Self::record_destination) for each host of a
    /// redirect chain that is not in `seen`, then add it to `seen`.
    fn record_destinations(&self, urls: &[String], seen: &mut std::collections::HashSet<String>) {
        for url in urls {
            if url_host(url).is_some_and(|host| seen.insert(host)) {
                self.record_destination(url);
            }
        }
    }

    /// The gate of a redirect hop to `url`: the same deadline, and the
    /// budget of the hop's host.
    ///
    /// The hop's host gets its first-attempt refill only when it is not in
    /// `refilled`, so a host that a chain visits again is credited once.
    fn for_hop(&self, url: &str, refilled: &mut std::collections::HashSet<String>) -> Self {
        let mut hop = Self {
            deadline: self.deadline,
            budgets: self.budgets.clone(),
            budget: self.budget.clone(),
            host: self.host.clone(),
            send_header: self.send_header,
            refill_pending: AtomicBool::new(false),
            pending_retry: std::sync::Mutex::new(None),
            caller_deadline: self.caller_deadline.clone(),
        };
        hop.rekey(url);
        if url_host(url).is_some_and(|host| refilled.insert(host)) {
            hop.refill_pending = AtomicBool::new(true);
        }
        hop
    }

    /// The error of a failed body read: [`ClientError::DeadlineExceeded`] for
    /// a timeout the request deadline caused, else [`ClientError::Request`].
    fn body_error(&self, error: reqwest::Error) -> ClientError {
        if error.is_timeout() && self.expired() {
            ClientError::DeadlineExceeded
        } else {
            ClientError::Request(error.without_url())
        }
    }

    /// `error`, or [`ClientError::DeadlineExceeded`] when the request deadline
    /// has passed and so is the cause.
    fn classify(&self, error: ClientError) -> ClientError {
        if self.expired() {
            ClientError::DeadlineExceeded
        } else {
            error
        }
    }

    /// An error when no time is left to start an attempt.
    ///
    /// The first check with time left does the first-attempt refill.
    fn check(&self) -> Result<(), ClientError> {
        if self.expired() {
            return Err(ClientError::DeadlineExceeded);
        }
        // The retry starts, so it keeps its tokens.
        self.pending_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if self.refill_pending.swap(false, Ordering::Relaxed)
            && let Some(budget) = &self.budget
        {
            budget.record_request();
        }
        Ok(())
    }

    /// The timeout of the next attempt: `per_try`, or the time left if that is
    /// shorter.
    fn attempt_timeout(&self, per_try: Option<Duration>) -> Option<Duration> {
        match (per_try, self.deadline) {
            (Some(limit), Some(deadline)) => Some(deadline.clamp(limit)),
            (None, Some(deadline)) => Some(deadline.remaining()),
            (limit, None) => limit,
        }
    }

    /// `true` when the request deadline has passed.
    fn expired(&self) -> bool {
        self.deadline.is_some_and(Deadline::is_expired)
    }

    /// `true` when a retry of `kind` can start after `wait`. It must have at
    /// least [`MIN_ATTEMPT`] left after the wait, and it takes tokens from the
    /// budget. The tokens come back if the retry never starts.
    fn allow(&self, kind: RetryKind, wait: Duration) -> bool {
        if self
            .deadline
            .is_some_and(|deadline| deadline.remaining() <= wait + MIN_ATTEMPT)
        {
            return false;
        }
        let Some(budget) = &self.budget else {
            return true;
        };
        if !budget.try_acquire(kind) {
            return false;
        }
        *self
            .pending_retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Arc::clone(budget), kind));
        true
    }

    /// Give back the tokens of the last retry when the final `status` is a
    /// success (below 400).
    fn finish(&self, last_retry: Option<RetryKind>, status: u16) {
        if let (Some(budget), Some(kind)) = (&self.budget, last_retry)
            && status < 400
        {
            budget.release(kind);
        }
    }

    /// The [`DEADLINE_HEADER`] value for an attempt with `timeout`. `None`
    /// with no deadline and no caller value the client can read.
    ///
    /// A value the caller set is kept when it is shorter, so the header never
    /// says more than the time left. When this returns a value, the caller's
    /// own copy of the header is not sent.
    fn header(&self, timeout: Option<Duration>, caller: &HeaderMap) -> Option<HeaderValue> {
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        // The time left of the request deadline, when one is set.
        let left = self.deadline.and(timeout).map(millis);
        let Some(value) = caller.get(DEADLINE_HEADER) else {
            return left.filter(|_| self.send_header).map(HeaderValue::from);
        };
        // The first attempt sends the caller's value as set; later ones send
        // what is left of it, with or without a request deadline.
        let mut first = None;
        let caller_deadline = self.caller_deadline.get_or_init(|| {
            first = crate::deadline::parse_header(value);
            first.map(Deadline::after)
        });
        let theirs = first.map(millis).or_else(|| {
            caller_deadline
                .as_ref()
                .map(|deadline| millis(deadline.remaining()))
        });
        match (left, theirs) {
            (Some(left), Some(theirs)) => Some(HeaderValue::from(left.min(theirs))),
            (Some(value), None) | (None, Some(value)) => Some(HeaderValue::from(value)),
            // No deadline, and a value the client cannot read: sent as set.
            (None, None) => None,
        }
    }
}

/// Add the [`DEADLINE_HEADER`] for an attempt with `timeout`, then the
/// caller's headers. The caller's own deadline header is left out when the
/// gate sends one, which is never longer than the caller's.
fn with_caller_headers(
    mut req: reqwest::RequestBuilder,
    gate: &RetryGate,
    timeout: Option<Duration>,
    caller: &HeaderMap,
) -> reqwest::RequestBuilder {
    let deadline_value = gate.header(timeout, caller);
    let sends_deadline = deadline_value.is_some();
    if let Some(value) = deadline_value {
        req = req.header(DEADLINE_HEADER, value);
    }
    for (name, value) in caller {
        if sends_deadline && name == DEADLINE_HEADER {
            continue;
        }
        req = req.header(name.clone(), value.clone());
    }
    req
}

// ── Custom send-path helpers (redirect / pin / SSRF-safe) ─────────────────────

/// Why one simulated attempt failed.
enum SimAttemptError {
    /// A drop, a partition or a timeout: retried while attempts remain.
    Transient(String),
    /// Not retried.
    Fatal(ClientError),
}

/// Serve one request from a simulated host's router (issue #2967).
async fn serve_sim_host(
    router: axum::Router,
    request: &RequestBuilder,
    url: reqwest::Url,
    deadline_header: Option<HeaderValue>,
) -> Result<Response, ClientError> {
    let target = url.query().map_or_else(
        || url.path().to_owned(),
        |query| format!("{}?{query}", url.path()),
    );
    let mut builder = axum::http::Request::builder()
        .method(request.method.clone())
        .uri(target);
    if !request.extra_headers.contains_key(reqwest::header::HOST) {
        // Host and port as the URL writes them, so an IPv6 host keeps its
        // brackets (`[::1]:8080`).
        let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
        builder = builder.header(reqwest::header::HOST, authority);
    }
    // Trace context first, as on the real send path. A caller header wins.
    for (name, value) in trace_headers_to_send(trace_context_headers(), &request.extra_headers) {
        builder = builder.header(name, value);
    }
    let sends_deadline = deadline_header.is_some();
    if let Some(value) = deadline_header {
        builder = builder.header(DEADLINE_HEADER, value);
    }
    // The real client sends a `Content-Length` for a known-size body. A
    // caller header of the same name wins.
    if let Some(body) = &request.body
        && !request
            .extra_headers
            .contains_key(reqwest::header::CONTENT_LENGTH)
    {
        builder = builder.header(reqwest::header::CONTENT_LENGTH, body.len());
    }
    for (name, value) in &request.extra_headers {
        if sends_deadline && name == DEADLINE_HEADER {
            continue;
        }
        builder = builder.header(name, value);
    }
    let body = request.body.clone().unwrap_or_default();
    let http_request = builder
        .body(axum::body::Body::from(body))
        .map_err(|error| ClientError::SimNetwork(error.to_string()))?;
    // The host is another process: it must not see the caller's task-local
    // deadline. Only the deadline header goes to it.
    let response = crate::deadline::unscoped(tower::ServiceExt::oneshot(router, http_request))
        .await
        .unwrap_or_else(|never| match never {});
    let status = response.status();
    let headers = response.headers().clone();
    let body = if request.discard_response_body {
        Bytes::new()
    } else {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(|error| ClientError::SimNetwork(error.to_string()))?
    };
    Ok(Response {
        status,
        headers,
        body,
        url: Some(url),
    })
}

/// Build a one-shot `reqwest::Client` for the custom send path, with the given
/// redirect policy, per-request timeout, and optional DNS `resolve` override.
///
/// **Proxy bypass on pinned clients.** When a `resolve` override is present the
/// client is built with `.no_proxy()` so the request connects DIRECTLY to the
/// validated/pinned address. reqwest evaluates proxy interception (from
/// `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`) BEFORE the connector where the
/// `resolve()` override applies, so a configured env proxy would otherwise send
/// the request to the proxy — which re-resolves the target host, reopening the
/// exact DNS-rebinding / SSRF window that pinning closes. Non-pinned callers
/// (`resolve == None`) keep reqwest's default proxy behaviour untouched.
fn build_oneshot_client(
    resolve: Option<(String, Vec<SocketAddr>)>,
    policy: reqwest::redirect::Policy,
    timeout: Duration,
) -> Result<reqwest::Client, ClientError> {
    let mut builder = reqwest::ClientBuilder::new()
        .timeout(timeout)
        .redirect(policy);
    if let Some((host, addrs)) = resolve
        && !addrs.is_empty()
    {
        // Pin to the FULL validated set: reqwest tries the addresses in order and
        // falls back on connection failure, so an unreachable first address no
        // longer dooms the request. Every pinned address was already validated,
        // so the TOCTOU/SSRF guarantee is preserved. A pinned request must never
        // route through a re-resolving proxy.
        builder = builder.no_proxy().resolve_to_addrs(&host, &addrs);
    }
    builder.build().map_err(ClientError::Request)
}

/// The retries a redirect chain shares (see `follow_pooled`).
struct ChainRetries {
    /// Retries the chain may still make.
    left: AtomicU32,
    /// Retries already made, so a later hop's backoff goes on from them.
    used: AtomicU32,
    /// When the attempt that answered with the redirect being followed runs
    /// out of its `request_timeout`. As with reqwest's own redirects, one
    /// timeout covers all hops of an attempt.
    attempt_end: Mutex<Option<Instant>>,
}

impl ChainRetries {
    const fn new(retries: u32) -> Self {
        Self {
            left: AtomicU32::new(retries),
            used: AtomicU32::new(0),
            attempt_end: Mutex::new(None),
        }
    }

    /// Take the end of the attempt that led to this hop.
    fn take_attempt_end(&self) -> Option<Instant> {
        self.attempt_end
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Carry `end` to the next hop's first attempt.
    fn set_attempt_end(&self, end: Option<Instant>) {
        *self
            .attempt_end
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = end;
    }
}

/// One attempt that the throttle let through. [`Self::record`] counts the
/// outcome. A ticket dropped without an outcome (the caller cancelled the
/// call) counts as an accept: the host did not reject it.
struct ThrottleTicket {
    throttle: Arc<crate::admission::AdaptiveThrottle>,
    host: String,
    recorded: bool,
}

impl ThrottleTicket {
    fn record(mut self, accepted: bool) {
        self.recorded = true;
        self.throttle
            .record(&self.host, crate::time::ambient_instant(), accepted);
    }
}

impl Drop for ThrottleTicket {
    fn drop(&mut self) {
        if !self.recorded {
            self.throttle
                .record(&self.host, crate::time::ambient_instant(), true);
        }
    }
}

/// Ask `throttle` for one attempt to `host` (issue #3068).
/// [`ClientError::ThrottledLocally`] when it rejects.
fn admit_throttle(
    throttle: &Arc<crate::admission::AdaptiveThrottle>,
    host: String,
    entropy: &dyn crate::entropy::Entropy,
) -> Result<ThrottleTicket, ClientError> {
    let now = crate::time::ambient_instant();
    if throttle.admit(&host, now, || entropy.next_u64()) {
        Ok(ThrottleTicket {
            throttle: Arc::clone(throttle),
            host,
            recorded: false,
        })
    } else {
        tracing::debug!(host = %host, "outbound request throttled locally");
        Err(ClientError::ThrottledLocally { host })
    }
}

/// Count a whole call that took one throttle `ticket`. A call that the
/// caller's deadline stopped says nothing about the host (issue #3058): its
/// ticket is dropped, which counts as an accept.
fn record_call(ticket: Option<ThrottleTicket>, res: &Result<Response, ClientError>) {
    if let Some(ticket) = ticket
        && !matches!(res, Err(ClientError::DeadlineExceeded))
    {
        ticket.record(matches!(res, Ok(r) if throttle_accepts(r.status.as_u16())));
    }
}

/// `false` for the statuses a host uses to reject for overload.
const fn throttle_accepts(status: u16) -> bool {
    !matches!(status, 429 | 503)
}

/// The throttle key of `url`: `host`, or `host:port` with an explicit port.
fn throttle_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(
        parsed
            .port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")),
    )
}

/// Send a single request through `client` (no manual redirect following — the
/// client's redirect policy governs that) with the same transient-error and
/// 429/5xx retry behaviour as the shared path, and collect the [`Response`].
///
/// `deadline`, when set, bounds the *retry loop* — checked before each
/// backoff sleep, so a retry that would start after the deadline is skipped
/// in favour of failing the call immediately with a deadline error, rather
/// than a stale response/error from an attempt that already ran. This exists
/// for
/// [`RequestBuilder::send_ssrf_safe`] (#2480 review, round 8): passing a
/// shrunk per-hop `timeout` into `client`'s own reqwest-level timeout, as
/// every other caller here already does, bounds one attempt's connect/
/// response wait, but `client` is reused across every retry `send_one`
/// itself performs — each gets that same per-attempt timeout again, and the
/// backoff/`Retry-After` sleeps between attempts are outside it entirely, so
/// a retried, timing-out SSRF-safe hop could still run well past the overall
/// deadline the caller computed. Checked only before sleeping (never wrapped
/// around an in-flight attempt), so it cannot race an attempt's own
/// reqwest-level timeout the way an outer `tokio::time::timeout` would (see
/// round 7's `ClientError::Request`/`.is_timeout()` regression) — the first
/// attempt's error shape is always preserved unchanged. Other callers pass
/// `None`, so their behavior is exactly as before this parameter existed.
///
/// Two more `deadline` seams round 9 found `send_ssrf_safe`'s own deadline
/// fix had missed: `client`'s own reqwest-level timeout is fixed at
/// build-hop-start, so a retry within the same hop still got the *original*
/// per-attempt budget rather than what's actually left; and the
/// `Retry-After` wait was not checked against the deadline. Both are fixed
/// here. Every attempt sends with an explicit per-request `.timeout()`
/// recomputed from `deadline`. This overrides `client`'s own timeout only
/// when `deadline` is set, so callers that pass `None` see no change. If a
/// `Retry-After` wait (on a 429 or 503, issue #3054) reaches the deadline,
/// the loop does not sleep. It returns that response as final, as it does
/// when `max_attempts` runs out.
///
/// `suppress_retries`, when `true`, forces a single attempt regardless of
/// `retry_policy` — the same thing [`RequestBuilder::send_inner`]'s own
/// `suppress_retries` parameter does for the plain (non-custom) breaker path
/// during a circuit-breaker half-open probe (#2480 review, round 10): a
/// probe is a budgeted, limited trial, and letting this retry loop turn one
/// trial into several real network attempts spends that budget on one
/// logical delivery rather than testing recovery with independent probes.
#[allow(
    clippy::too_many_arguments,
    reason = "one seam shared by three call sites (plain custom path, follow_loop, send_ssrf_safe); \
              splitting the request-shape fields into their own struct would still leave the \
              retry/discard/deadline/half-open knobs alongside it, for no reduction in what a \
              caller reasons about"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one retry loop; the deadline and budget checks (#3058) sit at each decision"
)]
async fn send_one(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    extra_headers: &HeaderMap,
    body: Option<&Bytes>,
    retry_policy: &RetryPolicy,
    entropy: &dyn crate::entropy::Entropy,
    discard_response_body: bool,
    deadline: Option<Instant>,
    suppress_retries: bool,
    gate: &RetryGate,
    skip_redirect_body: bool,
    chain: Option<&ChainRetries>,
    throttle: Option<&Arc<crate::admission::AdaptiveThrottle>>,
) -> Result<Response, ClientError> {
    let start = crate::time::ambient_instant();
    let mut last_retry = None;
    let mut max_attempts = if suppress_retries {
        1
    } else if is_idempotent_method(method) || !retry_policy.retry_idempotent_only {
        retry_policy.max_retries.saturating_add(1)
    } else {
        1
    };
    // A redirect chain shares one retry count (see `follow_pooled`), and
    // its backoff goes on from the retries earlier hops made.
    let retries_before = chain.map_or(0, |chain| chain.used.load(Ordering::Relaxed));
    if let Some(chain) = chain {
        max_attempts = max_attempts.min(chain.left.load(Ordering::Relaxed).saturating_add(1));
    }
    let mut last_transient_err: Option<reqwest::Error> = None;

    // A prior attempt's own connect/timeout error may be why the deadline is
    // already gone — surface that error (preserving
    // `ClientError::Request(e).is_timeout()` for callers) instead of masking
    // it as an unrelated `InvalidUrl`.
    let deadline_exceeded_err = |last_transient_err: &mut Option<reqwest::Error>| {
        last_transient_err.take().map_or_else(
            || ssrf_safe_deadline_error(url),
            |e| ClientError::Request(e.without_url()),
        )
    };

    // `true` when a wait of `wait` leaves less than `MIN_ATTEMPT` before the
    // hop deadline, so no retry can run after it. `RetryGate::allow` keeps
    // the same margin before the request deadline.
    let reaches_hop_deadline = |wait: Duration| {
        deadline.is_some_and(|d| {
            crate::time::ambient_instant()
                .checked_add(wait.saturating_add(MIN_ATTEMPT))
                .is_none_or(|resume| resume >= d)
        })
    };

    let mut delay = Duration::ZERO;
    for attempt in 0..max_attempts {
        let last = attempt + 1 == max_attempts;
        if attempt > 0 {
            if let Some(chain) = chain {
                chain.left.fetch_sub(1, Ordering::Relaxed);
                chain.used.fetch_add(1, Ordering::Relaxed);
            }
            if deadline.is_some_and(|d| crate::time::ambient_instant() >= d) {
                return Err(gate.classify(deadline_exceeded_err(&mut last_transient_err)));
            }
            let mut sleep_for = delay;
            if let Some(d) = deadline {
                sleep_for =
                    sleep_for.min(d.saturating_duration_since(crate::time::ambient_instant()));
            }
            tokio::time::sleep(sleep_for).await;
            if deadline.is_some_and(|d| crate::time::ambient_instant() >= d) {
                return Err(gate.classify(deadline_exceeded_err(&mut last_transient_err)));
            }
        }

        // With `throttle`, each attempt asks it (issue #3068), as on the plain
        // path: a throttled retry ends the call. It asks before
        // `RetryGate::check`, so a throttled attempt refills no budget.
        if gate.expired() {
            return Err(ClientError::DeadlineExceeded);
        }
        let ticket = match throttle.zip(throttle_host(url)) {
            Some((throttle, host)) => Some(admit_throttle(throttle, host, entropy)?),
            None => None,
        };
        gate.check()?;
        let mut req = client.request(method.clone(), url);
        // Recomputed fresh every attempt (not just retries) rather than
        // relying solely on `client`'s own timeout, which was fixed when the
        // caller built it at hop-start: without this override, a retry deep
        // into a hop's budget would still get the full original per-attempt
        // timeout rather than what's actually left before `deadline`.
        let attempt_start = crate::time::ambient_instant();
        let hop_timeout = deadline.map(|d| d.saturating_duration_since(attempt_start));
        // On a chain the client follows itself, the first attempt of a later
        // hop goes on with the attempt that answered with the redirect, and
        // keeps what is left of its `request_timeout`. A retry is a new
        // attempt, with a new timeout.
        let carried_end = if attempt == 0 {
            chain.and_then(ChainRetries::take_attempt_end)
        } else {
            None
        };
        let per_try = match (retry_policy.request_timeout, carried_end) {
            (Some(_), Some(end)) => Some(end.saturating_duration_since(attempt_start)),
            (limit, _) => limit,
        };
        let attempt_end = carried_end.or_else(|| {
            retry_policy
                .request_timeout
                .and_then(|limit| attempt_start.checked_add(limit))
        });
        // The request deadline (issue #3058) can make it shorter again.
        let attempt_timeout = if gate.deadline.is_some() {
            gate.attempt_timeout(hop_timeout.or(per_try))
        } else if carried_end.is_some() {
            // A later hop of a chain without a deadline.
            hop_timeout.or(per_try)
        } else {
            hop_timeout
        };
        if let Some(timeout) = attempt_timeout {
            req = req.timeout(timeout);
        }
        let span = client_attempt_span(method, url, attempt);
        req = span.in_scope(|| inject_trace_context(req, extra_headers));
        req = with_caller_headers(req, gate, attempt_timeout, extra_headers);
        if let Some(body) = body {
            req = req.body(body.clone());
        }

        match send_in_span(req, &span).await {
            Ok(resp) => {
                let status = resp.status();
                let headers = resp.headers().clone();
                let url_used = resp.url().clone();

                if is_retryable_response(status.as_u16())
                    && !last
                    && deadline.is_none_or(|d| crate::time::ambient_instant() < d)
                {
                    let hint = retry_hint(status.as_u16(), &headers);
                    let next = retry_policy.retry_delay(
                        entropy,
                        retries_before.saturating_add(attempt),
                        hint,
                    );
                    // A wait (hinted or not) that reaches the deadline cannot
                    // retry in time, so this response is the final outcome.
                    let kind = retry_kind(status.as_u16());
                    if !reaches_hop_deadline(next) && gate.allow(kind, next) {
                        if let Some(ticket) = ticket {
                            ticket.record(throttle_accepts(status.as_u16()));
                        }
                        delay = next;
                        last_retry = Some(kind);
                        continue;
                    }
                }
                // A redirect the caller follows: its body is not needed, and
                // may be large or never end.
                let followed = skip_redirect_body
                    && matches!(redirect_location(status, &headers, url), Ok(Some(_)));
                if followed && let Some(chain) = chain {
                    chain.set_attempt_end(attempt_end);
                }
                let body = if discard_response_body || followed {
                    // Dropped unread — see `RequestBuilder::discard_response_body`.
                    Ok(Bytes::new())
                } else {
                    read_body_in_span(resp, &span, gate).await
                };
                // Count the attempt only now: a body that fails to arrive is
                // a transport error, not an accept. A body the caller's
                // deadline stopped drops the ticket, which counts as an accept.
                if let Some(ticket) = ticket
                    && !matches!(body, Err(ClientError::DeadlineExceeded))
                {
                    ticket.record(body.is_ok() && throttle_accepts(status.as_u16()));
                }
                let body = body?;
                // Refund only after the body arrived.
                gate.finish(last_retry, status.as_u16());
                log_request(
                    method.as_str(),
                    &url_used,
                    status.as_u16(),
                    crate::time::ambient_instant().saturating_duration_since(start),
                    extra_headers,
                );
                return Ok(Response {
                    status,
                    headers,
                    body,
                    url: Some(url_used),
                });
            }
            // The request deadline stopped the attempt.
            Err(e) if e.is_timeout() && gate.expired() => {
                return Err(ClientError::DeadlineExceeded);
            }
            Err(e) if (e.is_connect() || e.is_timeout()) && !last => {
                if let Some(ticket) = ticket {
                    ticket.record(false);
                }
                let wait =
                    retry_policy.retry_delay(entropy, retries_before.saturating_add(attempt), None);
                // No retry fits in the hop, so none is charged.
                if reaches_hop_deadline(wait) || !gate.allow(RetryKind::Transient, wait) {
                    return Err(ClientError::Request(e.without_url()));
                }
                delay = wait;
                last_retry = Some(RetryKind::Transient);
                last_transient_err = Some(e);
            }
            Err(e) => {
                if let Some(ticket) = ticket {
                    ticket.record(false);
                }
                return Err(ClientError::Request(e.without_url()));
            }
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

/// `true` when the URL's host is an IP literal (IPv4 or IPv6) rather than a
/// domain name. Uses the `url` crate's parsed [`url::Host`] so bracketed IPv6
/// literals and decimal/octal/hex IPv4 encodings are classified correctly.
fn url_host_is_ip_literal(url: &str) -> Result<bool, ClientError> {
    let parsed = url::Url::parse(url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    match parsed.host() {
        Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => Ok(true),
        Some(url::Host::Domain(_)) => Ok(false),
        None => Err(ClientError::InvalidUrl(format!("URL has no host: {url}"))),
    }
}

/// `true` when the URL's scheme is `https` (case-insensitive).
fn scheme_is_https(url: &str) -> Result<bool, ClientError> {
    let parsed = url::Url::parse(url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    Ok(parsed.scheme().eq_ignore_ascii_case("https"))
}

/// If `resp` is a followable redirect (status `301`, `302`, `303`, `307`, or
/// `308`) carrying a `Location` header, resolve it to an absolute URL (joining
/// relative locations against `base`). Returns `Ok(None)` when the response is
/// not a followable redirect: any status outside that set (including non-3xx and
/// the non-followable 3xx `300`/`304`/`305`/`306`), or a followable status
/// missing its `Location` header.
fn redirect_target(resp: &Response, base: &str) -> Result<Option<String>, ClientError> {
    redirect_location(resp.status(), resp.headers(), base)
}

/// [`redirect_target`] from a response's status and headers, before its body
/// is read.
fn redirect_location(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    base: &str,
) -> Result<Option<String>, ClientError> {
    // Only the statuses reqwest itself follows are treated as redirects. A
    // response like `304 Not Modified` (or 300/305/306) can legitimately carry
    // a `Location` header without being a followable redirect, so matching the
    // entire 300–399 range via `is_redirection()` would wrongly issue an extra
    // request instead of returning the response to the caller.
    match status {
        reqwest::StatusCode::MOVED_PERMANENTLY
        | reqwest::StatusCode::FOUND
        | reqwest::StatusCode::SEE_OTHER
        | reqwest::StatusCode::TEMPORARY_REDIRECT
        | reqwest::StatusCode::PERMANENT_REDIRECT => {}
        _ => return Ok(None),
    }
    let Some(location) = headers
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

/// Strip credential-bearing headers when a redirect hop crosses origins.
///
/// If `current`'s origin (scheme + host + port, per [`url::Url::origin`])
/// differs from the `original` request URL's origin, the `Authorization`,
/// `Cookie`, `Cookie2`, `Proxy-Authorization` and `WWW-Authenticate` headers
/// are removed from `headers` so they are never forwarded to a cross-origin
/// target (credential leak).
/// Because the caller threads a single mutable `headers` map across hops, once
/// these headers are stripped on any hop they stay stripped for the remainder
/// of the chain — the safe, conservative behaviour.
fn strip_sensitive_headers_if_cross_origin(
    headers: &mut HeaderMap,
    original: &url::Url,
    current: &str,
) -> Result<(), ClientError> {
    let current_url =
        url::Url::parse(current).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    if current_url.origin() != original.origin() {
        // The set reqwest's own redirect handling removes.
        headers.remove(reqwest::header::AUTHORIZATION);
        headers.remove(reqwest::header::COOKIE);
        headers.remove("cookie2");
        headers.remove(reqwest::header::PROXY_AUTHORIZATION);
        headers.remove(reqwest::header::WWW_AUTHENTICATE);
    }
    Ok(())
}

/// Apply RFC 7231 §6.4 / RFC 7538 method-and-body rewriting after receiving a
/// redirect `status`, before issuing the next hop. Mutates `method` and `body`
/// in place.
///
/// - **303 See Other**: switch to `GET` (a `HEAD` stays `HEAD`) and drop the body.
/// - **301 Moved Permanently / 302 Found**: a `POST` becomes a bodyless `GET`;
///   every other method (and its body) is preserved — matching prevailing
///   browser behaviour.
/// - **307 Temporary Redirect / 308 Permanent Redirect**: preserve the method
///   **and** the body verbatim (the RFC-correct behaviour — the body must NOT
///   be dropped).
/// - Any other redirect status: leave method and body untouched.
///
/// Whenever the body is dropped (the POST→GET / 303→GET rewrites), the payload
/// (entity) headers threaded across hops are also removed from `headers` so the
/// bodyless follow-up hop does not carry a misleading `Content-Type` /
/// `Content-Length` / `Transfer-Encoding` / `Content-Encoding` /
/// `Content-Language` — matching reqwest's redirect layer. On 307/308 the body
/// is preserved, so those headers are left intact.
fn rewrite_after_redirect(
    status: reqwest::StatusCode,
    method: &mut Method,
    body: &mut Option<Bytes>,
    headers: &mut HeaderMap,
) {
    match status.as_u16() {
        303 => {
            if *method != Method::HEAD {
                *method = Method::GET;
            }
            *body = None;
            strip_payload_headers(headers);
        }
        301 | 302 if *method == Method::POST => {
            *method = Method::GET;
            *body = None;
            strip_payload_headers(headers);
        }
        _ => {}
    }
}

/// Remove payload (entity) headers from the threaded per-hop header map. Called
/// when a redirect rewrite drops the request body so a bodyless GET does not
/// keep carrying the original payload's `Content-Type` etc.
fn strip_payload_headers(headers: &mut HeaderMap) {
    use reqwest::header::{
        CONTENT_ENCODING, CONTENT_LANGUAGE, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING,
    };
    headers.remove(CONTENT_TYPE);
    headers.remove(CONTENT_LENGTH);
    headers.remove(TRANSFER_ENCODING);
    headers.remove(CONTENT_ENCODING);
    headers.remove(CONTENT_LANGUAGE);
}

/// Validate a set of resolved socket addresses against the built-in SSRF
/// deny-list, failing closed.
///
/// Returns `Err(SsrfBlocked)` (naming the first blocked address) if **any**
/// address's IP is blocked ([`is_blocked_ip`]); otherwise returns the whole set
/// unchanged, preserving order. Factored out of [`resolve_and_validate`] as a
/// pure, synchronous helper so the validation policy is unit-testable without
/// real multi-record DNS.
fn validate_resolved_addrs(addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, ClientError> {
    for addr in &addrs {
        if is_blocked_ip(addr.ip()) {
            return Err(ClientError::SsrfBlocked(addr.ip().to_string()));
        }
    }
    Ok(addrs)
}

/// Resolve `url`'s host to **all** validated [`SocketAddr`]s, rejecting with
/// [`ClientError::SsrfBlocked`] if the host is (or resolves to) any blocked IP.
///
/// IP-literal hosts (including decimal/octal/hex encodings, which the `url`
/// crate normalises to an `Ipv4Addr` at parse time) are validated directly with
/// no DNS lookup. Domain hosts are resolved **once** via `tokio::net::lookup_host`
/// (keeping the TOCTOU window closed) and rejected if **any** resolved address
/// is blocked. Since the whole DNS response is rejected when any IP is blocked,
/// it is safe to return the full validated set (order preserved) so a caller can
/// pin all of them and let reqwest try them in order — an unreachable first
/// address no longer dooms the request.
async fn resolve_and_validate(url: &str) -> Result<Vec<SocketAddr>, ClientError> {
    let parsed = url::Url::parse(url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
    // Explicit scheme allowlist (defence-in-depth): only http/https may be
    // resolved and connected on the safe path. Reject ftp://, gopher://,
    // file://, etc. here — before any DNS lookup or connection — rather than
    // relying on reqwest to reject them after the fact.
    let scheme = parsed.scheme();
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(ClientError::InvalidUrl(format!(
            "unsupported URL scheme `{scheme}` (only http/https are allowed): {url}"
        )));
    }
    let port = parsed.port_or_known_default().ok_or_else(|| {
        ClientError::InvalidUrl(format!("URL has no port and unknown scheme: {url}"))
    })?;
    let host = parsed
        .host()
        .ok_or_else(|| ClientError::InvalidUrl(format!("URL has no host: {url}")))?;

    match host {
        url::Host::Ipv4(v4) => validate_resolved_addrs(vec![SocketAddr::new(IpAddr::V4(v4), port)]),
        url::Host::Ipv6(v6) => validate_resolved_addrs(vec![SocketAddr::new(IpAddr::V6(v6), port)]),
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
            // Reject if ANY resolved address is blocked (fail closed); otherwise
            // return the whole validated set (order preserved from the lookup).
            validate_resolved_addrs(addrs)
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
    let now = crate::time::ambient_now();
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

/// Open the CLIENT span for one outbound attempt (issue #3064).
///
/// Field names follow the OpenTelemetry HTTP client conventions. The span
/// holds the URL path only, not the query: a query can hold secrets.
fn client_attempt_span(method: &Method, url: &str, attempt: u32) -> tracing::Span {
    let span = tracing::info_span!(
        "http.client.request",
        otel.name = %method,
        otel.kind = "client",
        http.request.method = %method,
        server.address = tracing::field::Empty,
        url.path = tracing::field::Empty,
        http.request.resend_count = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    // The conventions leave `resend_count` unset on the first attempt.
    if attempt > 0 {
        span.record("http.request.resend_count", attempt);
    }
    // Parse the URL only when a subscriber listens.
    if !span.is_disabled()
        && let Ok(parsed) = reqwest::Url::parse(url)
    {
        span.record("server.address", parsed.host_str().unwrap_or_default());
        span.record("url.path", parsed.path());
    }
    span
}

/// Record the response status on an attempt span. A 4xx or 5xx status sets
/// the span status to `ERROR`, as the HTTP client conventions require.
fn record_attempt_status(span: &tracing::Span, status: u16) {
    span.record("http.response.status_code", status);
    if status >= 400 {
        span.record("otel.status_code", "ERROR");
    }
}

/// Record a failed attempt: the error class and the `ERROR` span status.
fn record_attempt_error(span: &tracing::Span, kind: &'static str) {
    span.record("error.type", kind);
    span.record("otel.status_code", "ERROR");
}

/// Read the response body inside the attempt `span`, so the span covers the
/// body transfer. A read failure sets `error.type = "body"`, and `gate`
/// classifies it (see [`RetryGate::body_error`]).
async fn read_body_in_span(
    response: reqwest::Response,
    span: &tracing::Span,
    gate: &RetryGate,
) -> Result<Bytes, ClientError> {
    use tracing::Instrument as _;

    let body = response.bytes().instrument(span.clone()).await;
    if body.is_err() {
        record_attempt_error(span, "body");
    }
    body.map_err(|e| gate.body_error(e))
}

/// Send `request` inside `span`, then record the status code or the error
/// class on the span.
///
/// The caller keeps `span` open through the body read, and ends it before a
/// `Retry-After` sleep: the sleep is not part of the attempt.
async fn send_in_span(
    request: reqwest::RequestBuilder,
    span: &tracing::Span,
) -> Result<reqwest::Response, reqwest::Error> {
    use tracing::Instrument as _;

    let sent = request.send().instrument(span.clone()).await;
    record_send_outcome(span, &sent);
    sent
}

/// Record the status code or the error class of one send on its attempt
/// `span`.
fn record_send_outcome(span: &tracing::Span, sent: &Result<reqwest::Response, reqwest::Error>) {
    match sent {
        Ok(response) => record_attempt_status(span, response.status().as_u16()),
        Err(error) => {
            let kind = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else {
                "request"
            };
            record_attempt_error(span, kind);
        }
    }
}

/// Inject the W3C trace-context headers of the active span, except the ones
/// that [`trace_headers_to_send`] drops for `caller_headers`.
fn inject_trace_context(
    builder: reqwest::RequestBuilder,
    caller_headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    let mut builder = builder;
    for (name, value) in trace_headers_to_send(trace_context_headers(), caller_headers) {
        builder = builder.header(name, value);
    }
    builder
}

/// The injected trace headers to send. Skip a name that `caller_headers`
/// holds: `RequestBuilder::header` appends, so if not, the request sends both
/// values.
///
/// `traceparent` and `tracestate` are one W3C context. When the caller sets
/// either one, skip both, so the request never mixes two contexts.
fn trace_headers_to_send(
    injected: Vec<(String, HeaderValue)>,
    caller_headers: &HeaderMap,
) -> Vec<(String, HeaderValue)> {
    const W3C_PAIR: [&str; 2] = ["traceparent", "tracestate"];
    let caller_has_context = W3C_PAIR
        .iter()
        .any(|name| caller_headers.contains_key(*name));
    injected
        .into_iter()
        .filter(|(name, _)| {
            let paired = caller_has_context && W3C_PAIR.contains(&name.as_str());
            !paired && !caller_headers.contains_key(name.as_str())
        })
        .collect()
}

/// Add the current request's id as `x-request-id` (issue #3064). Do nothing
/// when the caller set the header, or when there is no current request.
fn add_current_request_id(headers: &mut HeaderMap) {
    static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
    if headers.contains_key(&X_REQUEST_ID) {
        return;
    }
    if let Some(value) =
        crate::log::context::current_request_id().and_then(|id| HeaderValue::from_str(&id).ok())
    {
        headers.insert(X_REQUEST_ID.clone(), value);
    }
}

/// The W3C trace-context headers for the active span. Empty when the
/// `telemetry-otlp` feature is disabled or no span has a valid context.
#[allow(clippy::missing_const_for_fn)]
fn trace_context_headers() -> Vec<(String, HeaderValue)> {
    #[cfg(not(feature = "telemetry-otlp"))]
    {
        Vec::new()
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
        map.into_iter()
            .filter_map(|(name, value)| Some((name, HeaderValue::from_str(&value).ok()?)))
            .collect()
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

    /// Issue #3068: a capsule replays a local throttle reject as the same
    /// variant, so a handler that matches it takes the same branch.
    #[cfg(feature = "reporting")]
    #[test]
    fn throttled_locally_round_trips_through_a_capsule() {
        let err = ClientError::ThrottledLocally {
            host: "api.example.com:8443".to_owned(),
        };
        let kind = http_error_kind(&err);
        assert_eq!(
            kind,
            crate::capsule::schema::HttpErrorKind::ThrottledLocally
        );
        match rebuild_client_error(Some(kind), err.to_string()) {
            ClientError::ThrottledLocally { host } => assert_eq!(host, "api.example.com:8443"),
            other => panic!("rebuilt as {other:?}"),
        }
    }

    /// Issue #3071: a capsule replays an injected fault as the same variant.
    #[cfg(feature = "reporting")]
    #[test]
    fn fault_injected_round_trips_through_a_capsule() {
        let err = ClientError::FaultInjected("fault injection: injected http error".to_owned());
        let kind = http_error_kind(&err);
        assert_eq!(kind, crate::capsule::schema::HttpErrorKind::FaultInjected);
        match rebuild_client_error(Some(kind), err.to_string()) {
            ClientError::FaultInjected(text) => {
                assert_eq!(text, "fault injection: injected http error");
            }
            other => panic!("rebuilt as {other:?}"),
        }
    }

    /// Regression (#3068 review): a local throttle reject says nothing about
    /// the host, so it must not count as a circuit-breaker failure.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn throttled_calls_do_not_open_the_circuit_breaker() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        // 429 is a reject for the throttle, but a success for the breaker.
        let app = axum::Router::new().route(
            "/x",
            axum::routing::get(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut config = HttpClientConfig::default();
        config.adaptive_throttle.enabled = true;
        let client = Client::from_config(&config);
        let url = format!("http://{addr}/x");
        let mut throttled = 0;
        for _ in 0..200 {
            match client.get(&url).no_retry().send().await {
                Err(ClientError::ThrottledLocally { .. }) => throttled += 1,
                Ok(r) => assert_eq!(r.status().as_u16(), 429),
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(
            throttled > 50,
            "the throttle must reject locally: {throttled}"
        );
        let breaker = crate::circuit_breaker::global_registry().get_or_create(
            &addr.to_string(),
            crate::circuit_breaker::CircuitBreakerPolicy::default(),
        );
        assert_eq!(
            breaker.state(),
            crate::circuit_breaker::CircuitState::Closed,
            "local rejects must not open the breaker"
        );
        crate::circuit_breaker::global_registry().clear();
    }

    /// Regression (#3183 review): a `state_initializer` that replaces the
    /// config after boot gets one shared throttle for the new settings.
    #[test]
    fn from_state_shares_a_throttle_built_from_the_effective_config() {
        let state = crate::AppState::for_test();
        let boot = HttpClientConfig::default();
        install_shared_throttle(&state, &boot);
        assert!(state.extension::<SharedThrottle>().is_none(), "off at boot");
        assert!(Client::from_state(&state).throttle.is_none());

        let mut replaced = crate::config::HttpConfig::default();
        replaced.client.adaptive_throttle.enabled = true;
        state.insert_extension(replaced.clone());
        let a = Client::from_state(&state)
            .throttle
            .expect("on after replace");
        let b = Client::from_state(&state)
            .throttle
            .expect("on after replace");
        assert!(Arc::ptr_eq(&a, &b), "clients share one throttle");

        replaced.client.adaptive_throttle.k = 3.0;
        state.insert_extension(replaced.clone());
        let c = Client::from_state(&state).throttle.expect("still on");
        assert!(!Arc::ptr_eq(&a, &c), "new settings build a new throttle");

        replaced.client.adaptive_throttle.enabled = false;
        state.insert_extension(replaced);
        assert!(Client::from_state(&state).throttle.is_none(), "off again");
    }

    /// A `state_initializer` that replaces the config after boot gets retry
    /// budgets built from the new settings, shared by all clients.
    #[test]
    fn from_state_rebuilds_the_retry_budgets_when_their_config_changes() {
        let state = crate::AppState::for_test();
        let mut config = crate::config::HttpConfig::default();
        state.insert_extension(config.clone());
        let a = Client::from_state(&state).retry.budgets.expect("on");
        let b = Client::from_state(&state).retry.budgets.expect("on");
        assert!(Arc::ptr_eq(&a, &b), "clients share one set of budgets");

        config.client.retry_budget.capacity = 50;
        state.insert_extension(config.clone());
        let c = Client::from_state(&state).retry.budgets.expect("still on");
        assert!(!Arc::ptr_eq(&a, &c), "new settings build new budgets");
        assert!((c.for_host("h:80").available() - 50.0).abs() < f64::EPSILON);

        config.client.retry_budget.enabled = false;
        state.insert_extension(config);
        assert!(Client::from_state(&state).retry.budgets.is_none(), "off");
    }

    /// Regression (#3183 review): concurrent first calls after a config
    /// change share one throttle.
    #[test]
    fn concurrent_first_calls_share_one_throttle() {
        let state = crate::AppState::for_test();
        let mut config = crate::config::HttpConfig::default();
        config.client.adaptive_throttle.enabled = true;
        state.insert_extension(config);
        let throttles: Vec<_> = std::thread::scope(|scope| {
            // Spawn all threads before the first join, so the calls overlap.
            let mut handles = Vec::with_capacity(8);
            for _ in 0..8 {
                handles.push(scope.spawn(|| Client::from_state(&state).throttle.expect("on")));
            }
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });
        assert!(
            throttles.iter().all(|t| Arc::ptr_eq(t, &throttles[0])),
            "every client must share the one throttle"
        );
    }

    /// Regression (#3183 review): a response whose body fails to arrive is
    /// a transport error for the throttle, not an accept.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_truncated_body_is_not_an_accept() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        // A server that sends a 200 head with a 100-byte length, 3 bytes of
        // body, then closes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0_u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nabc")
                    .await;
                drop(socket);
            }
        });

        let mut config = HttpClientConfig::default();
        config.adaptive_throttle.enabled = true;
        let client = Client::from_config(&config);
        let url = format!("http://{addr}/x");
        for _ in 0..20 {
            let res = client.get(&url).no_retry().send().await;
            assert!(
                matches!(
                    res,
                    Err(ClientError::Request(_) | ClientError::ThrottledLocally { .. })
                ),
                "a truncated body is an error: {res:?}"
            );
        }
        let throttle = client.throttle.as_ref().expect("on");
        assert!(
            throttle.reject_probability(&addr.to_string(), crate::time::ambient_instant()) > 0.5,
            "truncated bodies must count as rejects"
        );
        crate::circuit_breaker::global_registry().clear();
    }

    #[test]
    fn throttle_host_keeps_an_explicit_port() {
        assert_eq!(
            throttle_host("https://a.example/x").as_deref(),
            Some("a.example")
        );
        assert_eq!(
            throttle_host("http://a.example:8080/x").as_deref(),
            Some("a.example:8080")
        );
        assert_eq!(throttle_host("not a url"), None);
        assert!(throttle_accepts(200) && throttle_accepts(500));
        assert!(!throttle_accepts(429) && !throttle_accepts(503));
    }
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
            max_backoff_ms: 20_000,
            base_urls: std::collections::HashMap::new(),
            ..HttpClientConfig::default()
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
            max_backoff_ms: 20_000,
            base_urls,
            ..HttpClientConfig::default()
        };
        let client = Client::from_config(&config);
        let stripe = client.named("stripe");
        assert_eq!(stripe.base_url.as_deref(), Some("https://api.stripe.com"));
        assert_eq!(stripe.alias.as_deref(), Some("stripe"));

        // Unknown alias falls back to client-level base_url (None in this case).
        let other = client.named("sendgrid");
        assert!(other.base_url.is_none());
    }

    // PR #2480 review, round 4 (redesign): `breaker_scoped()` moved breaker accounting for
    // the custom send path from an external wrapper — `BreakerGuardedCall`, now removed —
    // to a flag `send_recorded` itself reads. That is what makes the mock bypass, the
    // capsule-replay bypass, and correct capsule recording of an open-breaker attempt all
    // free: they were already correctly ordered around `send_recorded` before this flag
    // existed, for every other caller.
    //
    // This test locks in the one thing the external wrapper got wrong (P1, round 1):
    // keying the breaker by the alias a caller passed to `post`/`get` rather than the
    // expanded destination. `self.url` is set once, by `Client::build_request`, before any
    // `breaker_scoped()` or `ssrf_safe()` flag is read, so there is no separate "pass the
    // right URL" step left to get wrong.
    #[test]
    fn breaker_scoped_keys_by_resolved_url_not_alias() {
        // breaker_for_url really does get_or_create against the shared global
        // registry, so this needs the same isolation as every other test that
        // touches it directly — otherwise a concurrently-running test's "no
        // breaker entry for this host" assertion can observe the entry this
        // test creates (and never cleans up otherwise).
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut base_urls = std::collections::HashMap::new();
        base_urls.insert(
            "hook-service".to_owned(),
            "http://mock-receiver/base".to_owned(),
        );
        let config = HttpClientConfig {
            timeout_secs: 30,
            max_retries: 3,
            max_retry_after_secs: 10,
            max_backoff_ms: 20_000,
            base_urls,
            ..HttpClientConfig::default()
        };
        let client = Client::from_config(&config);

        let req = client
            .named("hook-service")
            .post("hook-service")
            .ssrf_safe()
            .breaker_scoped();
        assert_eq!(req.url, "http://mock-receiver/base/hook-service");
        assert!(req.breaker_scoped);

        // The bare alias would neither parse as a URL nor name the real
        // destination host — exactly the bug the P1 finding caught.
        assert!(url::Url::parse("hook-service").is_err());
        let breaker = super::breaker_for_url(None, &req.url);
        assert_eq!(breaker.name(), "mock-receiver");

        crate::circuit_breaker::global_registry().clear();
    }

    // PR #2480 review, round 4: `breaker_scoped()` on the custom send path
    // must trip and fail fast exactly like the plain-path breaker already
    // does (`test_http_client_circuit_breaker_integration`), and must key on
    // the same host a non-custom-path request to the same URL would.
    //
    // No listener needed: `send_ssrf_safe` refuses a loopback destination
    // (`SsrfBlocked`) before ever dialing, and that refusal is exactly the
    // kind of failure the breaker must count — a subscriber pointing
    // `target_url` at a blocked destination repeatedly must trip the breaker
    // on those refusals just as it would on real 5xxs, not bypass accounting
    // entirely.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn breaker_scoped_custom_path_trips_and_fails_fast() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let mut rc = crate::config::ResilienceConfig::default();
        rc.circuit_breaker.defaults.failure_ratio_threshold = Some(0.5);
        rc.circuit_breaker.defaults.minimum_sample_count = Some(3);
        rc.circuit_breaker.defaults.open_duration_secs = Some(10);
        let client = Client {
            resilience_config: Some(Arc::new(rc)),
            ..Client::new()
        };

        let url = "http://127.0.0.1:1/blocked";

        for _ in 0..3 {
            let res = client.post(url).ssrf_safe().breaker_scoped().send().await;
            assert!(
                matches!(res, Err(ClientError::SsrfBlocked(_))),
                "expected SsrfBlocked, got {res:?}"
            );
        }

        // The breaker for 127.0.0.1 should now be OPEN — the next attempt
        // must fail fast with CircuitBreakerOpen rather than SsrfBlocked,
        // proving it never re-entered send_custom (no re-resolution, no
        // reqwest client built).
        let res = client.post(url).ssrf_safe().breaker_scoped().send().await;
        assert!(matches!(res, Err(ClientError::CircuitBreakerOpen)));

        crate::circuit_breaker::global_registry().clear();
    }

    /// Regression (#3183 review): a fail-fast `CircuitBreakerOpen` call on
    /// the breaker-scoped custom path never reaches the host, so the
    /// throttle must not count it.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn open_breaker_calls_are_not_charged_to_the_throttle() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let mut rc = crate::config::ResilienceConfig::default();
        rc.circuit_breaker.defaults.failure_ratio_threshold = Some(0.5);
        rc.circuit_breaker.defaults.minimum_sample_count = Some(3);
        rc.circuit_breaker.defaults.open_duration_secs = Some(10);
        let rc = Arc::new(rc);
        let url = "http://127.0.0.1:1/blocked";

        // Trip the breaker with a client that has no throttle.
        let plain = Client {
            resilience_config: Some(Arc::clone(&rc)),
            ..Client::new()
        };
        for _ in 0..3 {
            let _ = plain.post(url).ssrf_safe().breaker_scoped().send().await;
        }

        let throttle = Arc::new(crate::admission::AdaptiveThrottle::new(
            2.0,
            Duration::from_secs(120),
        ));
        let throttled = Client {
            resilience_config: Some(rc),
            throttle: Some(Arc::clone(&throttle)),
            ..Client::new()
        };
        for _ in 0..50 {
            let res = throttled
                .post(url)
                .ssrf_safe()
                .breaker_scoped()
                .send()
                .await;
            assert!(
                matches!(res, Err(ClientError::CircuitBreakerOpen)),
                "an open breaker fails fast, not through the throttle: {res:?}"
            );
        }
        assert!(
            throttle.reject_probability("127.0.0.1:1", crate::time::ambient_instant())
                < f64::EPSILON,
            "fail-fast calls must not be counted"
        );
        crate::circuit_breaker::global_registry().clear();
    }

    // PR #2480 review, round 4: a client with a mock registry attached must
    // never touch the real breaker even with `breaker_scoped()` set —
    // `send_recorded` checks `self.mock.is_some()` before it ever reads
    // `breaker_scoped`, so a deliberately-mocked failure in one test cannot
    // open the shared global breaker for a host name reused by an unrelated
    // later test.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn breaker_scoped_bypassed_for_mocked_client() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let registry = Arc::new(MockRegistry::new());
        let mock = MockSetupBuilder {
            registry: registry.clone(),
            alias: "http://mock-receiver/hook".to_owned(),
            method: None,
            path: None,
        }
        .post("/hook")
        .respond_with(500, serde_json::json!({ "error": "down" }));
        let client = Client::new().with_mock(registry);

        // More attempts than minimum_sample_count would need to trip a real
        // breaker at default thresholds — every one must still reach the
        // mock rather than short-circuiting on CircuitBreakerOpen.
        for _ in 0..12 {
            let res = client
                .named("http://mock-receiver/hook")
                .post("http://mock-receiver/hook")
                .ssrf_safe()
                .breaker_scoped()
                .send()
                .await;
            let res = res.expect("a mocked client must never see CircuitBreakerOpen");
            assert_eq!(res.status().as_u16(), 500);
        }
        mock.expect_called(12);

        // No breaker entry should exist for the mocked host at all.
        assert!(
            crate::circuit_breaker::global_registry()
                .all_breakers()
                .iter()
                .all(|b| b.name() != "mock-receiver"),
            "a mocked send must never create a real breaker entry"
        );

        crate::circuit_breaker::global_registry().clear();
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

    // TEST 32 (issue #3054): retries() sets only the count. POST retries need
    // retry_non_idempotent().
    #[test]
    fn retries_does_not_clear_idempotent_only_flag() {
        let client = Client::new();
        let builder = client.post("https://example.com").retries(2);
        assert_eq!(builder.retry_policy.max_retries, 2);
        assert!(builder.retry_policy.retry_idempotent_only);
        assert_eq!(builder.max_attempts(false), 1, "POST must not retry");

        let builder = builder.retry_non_idempotent();
        assert!(!builder.retry_policy.retry_idempotent_only);
        assert_eq!(builder.max_attempts(false), 3);
    }

    #[test]
    fn idempotency_key_is_added_only_for_opted_in_unsafe_retries() {
        let client = Client::new();
        let key_of = |builder: &RequestBuilder| {
            builder
                .extra_headers
                .get(IDEMPOTENCY_KEY)
                .map(|value| value.to_str().unwrap().to_owned())
        };

        let mut post = client.post("https://example.com").retry_non_idempotent();
        post.ensure_idempotency_key();
        let first = key_of(&post).expect("an opted-in POST gets a key");
        post.ensure_idempotency_key();
        assert_eq!(key_of(&post), Some(first), "the key does not change");

        let mut caller = client
            .post("https://example.com")
            .header("Idempotency-Key", "mine")
            .retry_non_idempotent();
        caller.ensure_idempotency_key();
        assert_eq!(key_of(&caller).as_deref(), Some("mine"));

        for mut builder in [
            client.post("https://example.com").retries(3),
            client
                .post("https://example.com")
                .retry_non_idempotent()
                .no_retry(),
            client.get("https://example.com").retry_non_idempotent(),
        ] {
            builder.ensure_idempotency_key();
            assert_eq!(key_of(&builder), None);
        }
    }

    #[test]
    fn retry_delay_is_jittered_and_capped() {
        let entropy = crate::entropy::SeededEntropy::new(9);
        let policy = RetryPolicy {
            max_backoff: Duration::from_secs(1),
            ..RetryPolicy::default()
        };
        for attempt in 0..40 {
            let delay = policy.retry_delay(&entropy, attempt, None);
            let ceiling = Duration::from_millis(crate::backoff::ceiling_ms(100, 1_000, attempt));
            assert!(delay <= ceiling, "attempt {attempt}: {delay:?}");
        }
    }

    #[test]
    fn retry_delay_clamps_a_hint_to_the_backoff_window() {
        let entropy = crate::entropy::SeededEntropy::new(10);
        let policy = RetryPolicy::default();
        // First retry: the backoff is in [0, 100 ms], added to the hint.
        let wait = policy.retry_delay(&entropy, 0, Some(Duration::from_secs(2)));
        assert!(wait >= Duration::from_secs(2) && wait <= Duration::from_millis(2_100));
        // The 5 s slack bounds a long hint.
        let wait = policy.retry_delay(&entropy, 0, Some(Duration::from_secs(3_600)));
        assert!(wait >= Duration::from_secs(5) && wait <= Duration::from_millis(5_100));
    }

    #[test]
    fn retry_hint_reads_retry_after_on_429_and_503_only() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("2"));
        assert_eq!(retry_hint(429, &headers), Some(Duration::from_secs(2)));
        assert_eq!(retry_hint(503, &headers), Some(Duration::from_secs(2)));
        assert_eq!(retry_hint(502, &headers), None);
        let empty = HeaderMap::new();
        assert_eq!(retry_hint(429, &empty), Some(Duration::from_secs(1)));
        assert_eq!(retry_hint(503, &empty), None);
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

    /// The sim `Host` header keeps IPv6 brackets and the port (issue #2967).
    #[test]
    fn sim_host_header_keeps_ipv6_brackets() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let echo = axum::Router::new().route(
            "/status",
            axum::routing::get(|headers: HeaderMap| async move {
                headers
                    .get(reqwest::header::HOST)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned()
            }),
        );
        runtime.block_on(async {
            for (url, host) in [
                ("http://[::1]:8080/status", "[::1]:8080"),
                ("http://[::1]/status", "[::1]"),
                ("http://payments:8443/status", "payments:8443"),
            ] {
                let request = Client::new().get(url);
                let parsed = reqwest::Url::parse(url).unwrap();
                let seen = serve_sim_host(echo.clone(), &request, parsed, None)
                    .await
                    .unwrap()
                    .text();
                assert_eq!(seen, host, "{url}");
            }
        });
    }

    /// A sim request with a body carries its `Content-Length`, as the real
    /// client's does, and a caller value wins (issue #2967).
    #[test]
    fn sim_host_receives_content_length() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let echo = axum::Router::new().fallback(|headers: HeaderMap| async move {
            headers
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("none")
                .to_owned()
        });
        let url = "http://payments/charge";
        let parsed = reqwest::Url::parse(url).unwrap();
        runtime.block_on(async {
            for (request, expected) in [
                (Client::new().post(url).text_body("hello"), "5"),
                (
                    Client::new().post(url).json(&serde_json::json!({"a": 1})),
                    "7",
                ),
                (Client::new().post(url).bytes_body(Bytes::new()), "0"),
                (Client::new().get(url), "none"),
            ] {
                let seen = serve_sim_host(echo.clone(), &request, parsed.clone(), None)
                    .await
                    .unwrap()
                    .text();
                assert_eq!(seen, expected);
            }
        });
    }

    /// A sim host sees the active span's `traceparent`, as a real upstream
    /// does, and a caller header of the same name wins (issue #2967).
    #[cfg(feature = "telemetry-otlp")]
    #[test]
    fn sim_host_receives_trace_context() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::propagation::TraceContextPropagator;
        use opentelemetry_sdk::trace::SdkTracerProvider;
        use tracing_subscriber::prelude::*;

        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        let provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let echo = || {
            axum::Router::new().route(
                "/echo",
                axum::routing::get(|headers: HeaderMap| async move {
                    headers
                        .get("traceparent")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned()
                }),
            )
        };
        let url = reqwest::Url::parse("http://payments/echo").unwrap();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("sim_trace_test");
            let _guard = span.enter();
            runtime.block_on(async {
                let request = Client::new().get("http://payments/echo");
                let traceparent = serve_sim_host(echo(), &request, url.clone(), None)
                    .await
                    .unwrap()
                    .text();
                assert!(traceparent.starts_with("00-"), "{traceparent}");

                let request = Client::new()
                    .get("http://payments/echo")
                    .header("traceparent", "caller-value");
                let response = serve_sim_host(echo(), &request, url.clone(), None)
                    .await
                    .unwrap();
                assert_eq!(response.text(), "caller-value");
            });
        });
    }

    // TEST 34: the request id is added only inside a request.
    #[test]
    fn no_request_id_outside_a_request() {
        let mut headers = HeaderMap::new();
        add_current_request_id(&mut headers);
        assert!(headers.is_empty());
    }

    #[tokio::test]
    async fn current_request_id_is_added() {
        let ctx = crate::log::context::LogContext::new(Some("rid-1".to_owned()));
        let mut headers = HeaderMap::new();
        crate::log::context::scope(ctx, async { add_current_request_id(&mut headers) }).await;
        assert_eq!(headers.get("x-request-id").unwrap(), "rid-1");
    }

    #[tokio::test]
    async fn caller_request_id_is_kept() {
        let ctx = crate::log::context::LogContext::new(Some("rid-1".to_owned()));
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("mine"));
        crate::log::context::scope(ctx, async { add_current_request_id(&mut headers) }).await;
        assert_eq!(headers.get_all("x-request-id").iter().count(), 1);
        assert_eq!(headers.get("x-request-id").unwrap(), "mine");
    }

    /// `traceparent` and `tracestate` are one W3C context. A caller value for
    /// either one drops both injected headers, so the request never mixes two
    /// contexts.
    #[test]
    fn caller_trace_header_drops_the_whole_injected_pair() {
        let injected = || {
            vec![
                ("traceparent".to_owned(), HeaderValue::from_static("ours-p")),
                ("tracestate".to_owned(), HeaderValue::from_static("ours-s")),
                ("baggage".to_owned(), HeaderValue::from_static("ours-b")),
            ]
        };
        let names = |caller: &[&'static str]| {
            let mut headers = HeaderMap::new();
            for name in caller {
                headers.insert(*name, HeaderValue::from_static("mine"));
            }
            let mut sent: Vec<String> = trace_headers_to_send(injected(), &headers)
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            sent.sort();
            sent
        };
        assert_eq!(names(&[]), ["baggage", "traceparent", "tracestate"]);
        assert_eq!(names(&["traceparent"]), ["baggage"]);
        assert_eq!(names(&["tracestate"]), ["baggage"]);
        assert_eq!(names(&["baggage"]), ["traceparent", "tracestate"]);
    }

    #[test]
    fn caller_trace_header_suppresses_the_injected_one() {
        let mut caller = HeaderMap::new();
        caller.insert("traceparent", HeaderValue::from_static("mine"));
        let request =
            inject_trace_context(reqwest::Client::new().get("https://example.com"), &caller)
                .build()
                .unwrap();
        assert!(request.headers().get("traceparent").is_none());
    }

    // A request built with `without_request_id` sends no id.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn without_request_id_sends_no_id() {
        use axum::{Router, http::HeaderMap as AxumHeaders, routing::get};

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let app = Router::new().route(
            "/echo",
            get(|headers: AxumHeaders| async move {
                headers
                    .get("x-request-id")
                    .map_or_else(|| "none".to_owned(), |v| v.to_str().unwrap().to_owned())
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let ctx = crate::log::context::LogContext::new(Some("rid-1".to_owned()));
        let url = format!("http://{addr}/echo");
        let (forwarded, opted_out) = crate::log::context::scope(ctx, async {
            let forwarded = Client::new().get(&url).send().await.unwrap().text();
            let opted_out = Client::new()
                .get(&url)
                .without_request_id()
                .send()
                .await
                .unwrap()
                .text();
            (forwarded, opted_out)
        })
        .await;
        assert_eq!(forwarded, "rid-1");
        assert_eq!(opted_out, "none");

        crate::circuit_breaker::global_registry().clear();
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

    /// Records each `http.client.request` span and the fields set on it.
    #[derive(Clone, Default)]
    struct ClientSpanCapture {
        spans: std::sync::Arc<
            std::sync::Mutex<
                std::collections::HashMap<u64, std::collections::BTreeMap<String, String>>,
            >,
        >,
    }

    struct SpanFieldVisitor<'a>(&'a mut std::collections::BTreeMap<String, String>);

    impl tracing::field::Visit for SpanFieldVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(
                field.name().to_owned(),
                format!("{value:?}").trim_matches('"').to_owned(),
            );
        }
    }

    impl<S> tracing_subscriber::Layer<S> for ClientSpanCapture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() != "http.client.request" {
                return;
            }
            let mut fields = std::collections::BTreeMap::new();
            attrs.record(&mut SpanFieldVisitor(&mut fields));
            self.spans.lock().unwrap().insert(id.into_u64(), fields);
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if let Some(fields) = self.spans.lock().unwrap().get_mut(&id.into_u64()) {
                values.record(&mut SpanFieldVisitor(fields));
            }
        }
    }

    /// Records how long each `http.client.request` span stays open.
    #[derive(Clone, Default)]
    struct ClientSpanDurations {
        opened: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u64, Instant>>>,
        closed: std::sync::Arc<std::sync::Mutex<Vec<Duration>>>,
    }

    impl<S> tracing_subscriber::Layer<S> for ClientSpanDurations
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() == "http.client.request" {
                self.opened
                    .lock()
                    .unwrap()
                    .insert(id.into_u64(), Instant::now());
            }
        }

        fn on_close(&self, id: tracing::span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            let start = self.opened.lock().unwrap().remove(&id.into_u64());
            if let Some(start) = start {
                self.closed.lock().unwrap().push(start.elapsed());
            }
        }
    }

    /// Install both span capture layers on this thread, warm up the callsite,
    /// and return them.
    fn capture_client_spans() -> (
        ClientSpanCapture,
        ClientSpanDurations,
        tracing::subscriber::DefaultGuard,
    ) {
        use tracing_subscriber::layer::SubscriberExt as _;

        let fields = ClientSpanCapture::default();
        let durations = ClientSpanDurations::default();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(fields.clone())
                .with(durations.clone()),
        );
        drop(client_attempt_span(&Method::GET, "http://warm.up/", 0));
        tracing::callsite::rebuild_interest_cache();
        fields.spans.lock().unwrap().clear();
        durations.closed.lock().unwrap().clear();
        (fields, durations, guard)
    }

    // The attempt span stays open while the body streams, and records a body
    // read failure.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn attempt_span_covers_the_response_body() {
        use axum::{Router, body::Body, routing::get};
        use futures::StreamExt as _;

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        let app = Router::new()
            .route(
                "/slow",
                get(|| async {
                    let stream = futures::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok::<_, std::io::Error>(Bytes::from_static(b"late"))
                    });
                    Body::from_stream(stream)
                }),
            )
            .route(
                "/broken",
                get(|| async {
                    // Headers and one chunk go out, then the body fails.
                    let stream = futures::stream::iter([
                        Ok(Bytes::from_static(b"partial")),
                        Err(std::io::Error::other("cut")),
                    ])
                    .then(|item| async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        item
                    });
                    Body::from_stream(stream)
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        for custom_path in [false, true] {
            let (fields, durations, _guard) = capture_client_spans();
            let mut slow = Client::new().get(format!("http://{addr}/slow"));
            let mut broken = Client::new().get(format!("http://{addr}/broken"));
            if custom_path {
                slow = slow.no_redirect();
                broken = broken.no_redirect();
            }
            assert_eq!(slow.send().await.unwrap().text(), "late");
            let closed = durations.closed.lock().unwrap().clone();
            assert_eq!(closed.len(), 1, "custom={custom_path}: {closed:?}");
            assert!(
                closed[0] >= Duration::from_millis(250),
                "custom={custom_path}: the span ended before the body: {closed:?}"
            );

            fields.spans.lock().unwrap().clear();
            assert!(broken.send().await.is_err());
            let spans: Vec<_> = fields.spans.lock().unwrap().values().cloned().collect();
            assert_eq!(spans.len(), 1, "custom={custom_path}: {spans:?}");
            assert_eq!(
                spans[0].get("error.type").map(String::as_str),
                Some("body"),
                "custom={custom_path}: {spans:?}"
            );
            assert_eq!(
                spans[0].get("otel.status_code").map(String::as_str),
                Some("ERROR"),
                "custom={custom_path}: {spans:?}"
            );

            // A 4xx response is an error span too.
            fields.spans.lock().unwrap().clear();
            let mut missing = Client::new().get(format!("http://{addr}/missing"));
            if custom_path {
                missing = missing.no_redirect();
            }
            assert_eq!(missing.send().await.unwrap().status().as_u16(), 404);
            let spans: Vec<_> = fields.spans.lock().unwrap().values().cloned().collect();
            assert_eq!(spans.len(), 1, "custom={custom_path}: {spans:?}");
            assert_eq!(
                spans[0].get("otel.status_code").map(String::as_str),
                Some("ERROR"),
                "custom={custom_path}: {spans:?}"
            );
        }

        crate::circuit_breaker::global_registry().clear();
    }

    // A sim attempt opens a CLIENT span too, one per attempt.
    #[tokio::test]
    async fn sim_attempts_open_client_spans() {
        use axum::{Router, http::StatusCode, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (fields, _durations, _guard) = capture_client_spans();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let payments = Router::new().route(
            "/charge",
            get(move || {
                let calls = std::sync::Arc::clone(&calls);
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    }
                }
            }),
        );
        let mut client = Client::new();
        client.sim_net = Some(Arc::new(
            crate::sim::SimNet::new().host("payments", payments),
        ));

        let response = client.get("http://payments/charge").send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);

        let mut attempts: Vec<_> = fields.spans.lock().unwrap().values().cloned().collect();
        attempts.sort_by_key(|f| f.get("http.request.resend_count").cloned());
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(
            attempts[0].get("server.address").map(String::as_str),
            Some("payments")
        );
        assert_eq!(
            attempts[0]
                .get("http.response.status_code")
                .map(String::as_str),
            Some("503")
        );
        assert_eq!(
            attempts[1]
                .get("http.response.status_code")
                .map(String::as_str),
            Some("200")
        );
        // A 5xx attempt is an error span; a 2xx attempt has no status.
        assert_eq!(
            attempts[0].get("otel.status_code").map(String::as_str),
            Some("ERROR"),
            "{attempts:?}"
        );
        assert_eq!(attempts[1].get("otel.status_code"), None, "{attempts:?}");
    }

    // A 429 attempt span ends before the `Retry-After` sleep, so its duration
    // is the HTTP exchange only, on the plain and the custom send paths.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn attempt_span_ends_before_the_retry_after_sleep() {
        use axum::{Router, http::StatusCode, response::IntoResponse as _, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tracing_subscriber::layer::SubscriberExt as _;

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        for custom_path in [false, true] {
            let durations = ClientSpanDurations::default();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(durations.clone()),
            );
            drop(client_attempt_span(&Method::GET, "http://warm.up/", 0));
            tracing::callsite::rebuild_interest_cache();
            durations.closed.lock().unwrap().clear();

            // The first call answers 429 with `Retry-After: 1`, the next 200.
            let calls = std::sync::Arc::new(AtomicUsize::new(0));
            let app = Router::new().route(
                "/limited",
                get(move || {
                    let calls = std::sync::Arc::clone(&calls);
                    async move {
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")]).into_response()
                        } else {
                            StatusCode::OK.into_response()
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let mut request = Client::new().get(format!("http://{addr}/limited"));
            if custom_path {
                request = request.no_redirect();
            }
            assert_eq!(request.send().await.unwrap().status().as_u16(), 200);

            let closed = durations.closed.lock().unwrap().clone();
            assert_eq!(closed.len(), 2, "custom={custom_path}: {closed:?}");
            assert!(
                closed[0] < Duration::from_millis(500),
                "custom={custom_path}: the 429 attempt span held the 1s sleep: {closed:?}"
            );
        }

        crate::circuit_breaker::global_registry().clear();
    }

    // One CLIENT span per outbound attempt (issue #3064), on the plain and
    // the custom send paths.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn each_attempt_opens_one_client_span() {
        use axum::{Router, http::StatusCode, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tracing_subscriber::layer::SubscriberExt as _;

        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        for custom_path in [false, true] {
            let capture = ClientSpanCapture::default();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(capture.clone()),
            );
            // Another test can register this callsite on a thread with no
            // subscriber, and cache it as "never". Register it here first,
            // rebuild the cache, then discard the warm-up span.
            drop(client_attempt_span(&Method::GET, "http://warm.up/", 0));
            tracing::callsite::rebuild_interest_cache();
            capture.spans.lock().unwrap().clear();

            // The first call answers 503, the next 200: two attempts.
            let calls = std::sync::Arc::new(AtomicUsize::new(0));
            let app = Router::new().route(
                "/flaky",
                get(move || {
                    let calls = std::sync::Arc::clone(&calls);
                    async move {
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            StatusCode::OK
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let mut request = Client::new().get(format!("http://{addr}/flaky"));
            if custom_path {
                request = request.no_redirect();
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status().as_u16(), 200);

            let mut attempts: Vec<_> = capture.spans.lock().unwrap().values().cloned().collect();
            attempts.sort_by_key(|f| f.get("http.request.resend_count").cloned());
            assert_eq!(attempts.len(), 2, "custom={custom_path}: {attempts:?}");
            for (n, fields) in attempts.iter().enumerate() {
                assert_eq!(fields.get("otel.kind").map(String::as_str), Some("client"));
                assert_eq!(
                    fields.get("http.request.method").map(String::as_str),
                    Some("GET")
                );
                assert_eq!(
                    fields.get("server.address").map(String::as_str),
                    Some("127.0.0.1")
                );
                let resend = fields.get("http.request.resend_count");
                assert_eq!(resend, (n > 0).then(|| n.to_string()).as_ref());
            }
            assert_eq!(
                attempts[0]
                    .get("http.response.status_code")
                    .map(String::as_str),
                Some("503")
            );
            assert_eq!(
                attempts[1]
                    .get("http.response.status_code")
                    .map(String::as_str),
                Some("200")
            );
        }

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

        // retries(1): 2 total attempts, a 0-100 ms jittered sleep between them.
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

    /// Serve `503` + `Retry-After: 1` once, then `200`. Returns the address
    /// and the hit counter.
    async fn serve_down_once_with_retry_after() -> (SocketAddr, Arc<AtomicUsize>) {
        use axum::{Router, response::IntoResponse as _, routing::get};
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/down",
            get(move || {
                let counter = counter.clone();
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            [("retry-after", "1")],
                        )
                            .into_response()
                    } else {
                        axum::http::StatusCode::OK.into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr, hits)
    }

    /// Issue #3054 on the real network path (`send_inner`) and the custom
    /// path (`send_one`, via `no_redirect`): a 503 with `Retry-After: 1`
    /// waits at least 1 s.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn real_paths_honour_retry_after_on_503() {
        let _lock = crate::circuit_breaker::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::circuit_breaker::global_registry().clear();

        for custom_path in [false, true] {
            let (addr, hits) = serve_down_once_with_retry_after().await;
            let mut request = Client::new()
                .get(format!("http://127.0.0.1:{}/down", addr.port()))
                .retries(1);
            if custom_path {
                request = request.no_redirect();
            }
            let start = std::time::Instant::now();
            let response = request.send().await.unwrap();
            let waited = start.elapsed();
            assert_eq!(response.status().as_u16(), 200, "custom_path={custom_path}");
            assert_eq!(hits.load(Ordering::SeqCst), 2);
            assert!(
                waited >= Duration::from_secs(1),
                "custom_path={custom_path}: waited only {waited:?}"
            );
        }

        crate::circuit_breaker::global_registry().clear();
    }

    #[test]
    fn from_config_reads_max_backoff_ms() {
        let config = HttpClientConfig {
            max_backoff_ms: 1_500,
            ..Default::default()
        };
        assert_eq!(
            Client::from_config(&config).retry_policy.max_backoff,
            Duration::from_millis(1_500)
        );
        assert_eq!(RetryPolicy::default().max_backoff, Duration::from_secs(20));
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

    /// Capsule replay is offline by construction (issue #1598, AC4): a capsule
    /// records the request, the clock and the database, and nothing about the
    /// services the handler called. The block is process-wide and one-way, so
    /// this checks the predicate's default and the message an operator sees —
    /// the wiring itself is pinned by `app::tests::replay_reaches_nothing_outside_the_capsule`,
    /// which cannot be expressed here without blocking the whole test binary.
    #[test]
    fn outbound_is_open_until_a_replay_blocks_it() {
        assert!(
            !outbound_blocked_for_replay(),
            "a serving process must never start out blocked"
        );
        let error = ClientError::BlockedDuringReplay(
            "GET".to_owned(),
            "https://payments.example/charge".to_owned(),
        )
        .to_string();
        assert!(
            error.contains("blocked during replay") && error.contains("not recorded"),
            "the error must say why the call did not happen, got {error}"
        );
        assert!(
            error.contains("GET") && error.contains("https://payments.example/charge"),
            "and which call it was, got {error}"
        );
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
            "192.88.99.1",     // 6to4 anycast relay
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
            "fec0::1",                // deprecated site-local
            "::ffff:169.254.169.254", // IPv4-mapped metadata
            "::ffff:127.0.0.1",       // IPv4-mapped loopback
            // Remaining IANA special-purpose prefixes (Globally Reachable = False).
            "100::1",          // Discard-Only 100::/64 (RFC 6666)
            "100::dead:beef",  // Discard-Only 100::/64 (RFC 6666)
            "2001:2::1",       // Benchmarking 2001:2::/48 (RFC 5180)
            "2001:10::1",      // ORCHID 2001:10::/28 (RFC 4843)
            "2001:20::1",      // ORCHIDv2 2001:20::/28 (RFC 7343)
            "2001:20:abcd::1", // ORCHIDv2 within /28 (RFC 7343)
            "2001:2f::1",      // ORCHIDv2 top of /28 (s[1]=0x002f, RFC 7343)
            "3fff::1",         // Documentation 3fff::/20 (RFC 9637)
            "3fff:0fff::1",    // Documentation top of /20 (s[1]=0x0fff, RFC 9637)
            "5f00::1",         // SRv6 SIDs 5f00::/16 (RFC 9602)
            "5f00:1234::1",    // SRv6 SIDs within /16 (RFC 9602)
            "2620:4f:8000::1", // Direct Delegation AS112 (RFC 7534)
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

        // Negative tests: real public addresses adjacent to the newly-blocked
        // special-purpose prefixes must stay allowed (no over-blocking).
        let public_addrs = [
            "2001:4860:4860::8888", // Google DNS
            "2606:4700:4700::1111", // Cloudflare DNS
            "2400:cb00:2048::1",    // public 2400 (Cloudflare)
            "2620:0:2d0:200::7",    // public 2620 NOT in AS112 2620:4f:8000::/48
            "2001:2:1::1",          // just outside benchmarking /48 (s[2]=1, not ORCHID)
            "3fff:abcd::1",         // outside documentation /20 (s[1]=0xabcd > 0x0fff)
            "4000::1",              // outside documentation 3fff::/20
            "5e00::1",              // outside SRv6 5f00::/16
            "6000::1",              // outside SRv6 5f00::/16
        ];
        for s in public_addrs {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_public_ip(ip), "{s} should be public");
            assert!(!is_blocked_ip(ip), "{s} should not be blocked");
        }
    }

    // TEST 46b: SSRF policy blocks private / metadata IPv4 tunnelled inside an
    // IPv6 literal via NAT64 (64:ff9b::/96) and 6to4 (2002::/16), while leaving
    // a genuinely-public embedded IPv4 (and native public v6) public.
    #[test]
    fn ssrf_policy_blocks_tunnelled_ipv4() {
        let blocked = [
            "64:ff9b::a9fe:a9fe", // NAT64 → 169.254.169.254 (cloud metadata)
            "64:ff9b::7f00:1",    // NAT64 → 127.0.0.1 (loopback)
            // RFC 8215 local-use NAT64 `64:ff9b:1::/48` is denied outright.
            "64:ff9b:1::a9fe:a9fe", // local-use NAT64 → 169.254.169.254
            "64:ff9b:1::7f00:1",    // local-use NAT64 → 127.0.0.1
            "64:ff9b:1::808:808",   // local-use NAT64 embedding public 8.8.8.8: still blocked
            "2002:a9fe:a9fe::",     // 6to4  → 169.254.169.254
            "2002:7f00:1::",        // 6to4  → 127.0.0.1
            "2002:0a00:0001::",     // 6to4  → 10.0.0.1
            // SIIT IPv4-translated `::ffff:0:0:0/96` (RFC 6052): segment[4] ==
            // 0xffff, segment[5] == 0, IPv4 in the last 32 bits. Distinct from
            // IPv4-mapped `::ffff:0:0/96`, so must be decoded and re-checked.
            "::ffff:0:169.254.169.254", // SIIT → 169.254.169.254 (cloud metadata)
            "::ffff:0:127.0.0.1",       // SIIT → 127.0.0.1 (loopback)
        ];
        for s in blocked {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_blocked_ip(ip), "{s} should be blocked");
            assert!(!is_public_ip(ip), "{s} should not be public");
        }

        // 192.88.99.0/24 (6to4 anycast relay) is blocked as a plain IPv4 literal.
        let anycast: IpAddr = "192.88.99.1".parse().unwrap();
        assert!(is_blocked_ip(anycast), "192.88.99.1 should be blocked");

        // A 6to4 address embedding a genuinely-public IPv4 (8.8.8.8) stays
        // public (the embedded v4 is public, so it falls through to the native
        // v6 checks, which do not match 2002::/16). So does a native public v6.
        // A SIIT IPv4-translated address embedding a genuinely-public IPv4
        // (8.8.8.8) stays public — the decoded v4 is public, so it falls
        // through to the native v6 checks, which do not match it.
        for s in ["2002:0808:0808::", "2606:4700::1111", "::ffff:0:8.8.8.8"] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_public_ip(ip), "{s} should be public");
            assert!(!is_blocked_ip(ip), "{s} should not be blocked");
        }
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

    fn redirect_307(location: String) -> axum::response::Response {
        axum::response::Response::builder()
            .status(307)
            .header("location", location)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    // Build a response with an arbitrary status that carries a `Location`
    // header — used to prove non-followable 3xx statuses (304/300/…) are NOT
    // treated as redirects even when they advertise a `Location`.
    fn response_with_location(status: u16, location: String) -> axum::response::Response {
        axum::response::Response::builder()
            .status(status)
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

    // TEST 48b: the non-ssrf `no_redirect()` path returns the 3xx verbatim even
    // when `Location` is a MALFORMED URL. That path uses reqwest's
    // `Policy::none()` and never parses `Location`, so a bad target cannot turn
    // a `no_redirect()` fetch into an error. This guards the same "don't parse
    // Location when not following" contract that `send_ssrf_safe` now enforces
    // by returning the 3xx BEFORE calling `redirect_target`.
    //
    // NOTE: the SSRF-safe equivalent — `get_ssrf_safe(url).no_redirect()`
    // against a listener returning a malformed `Location` — cannot be exercised
    // end-to-end in-sandbox: the SSRF guard denies loopback (127.0.0.1), so the
    // request is rejected during resolve→validate before any response is
    // received. This testable-layer variant documents/guards the shared
    // contract instead.
    #[tokio::test]
    async fn no_redirect_returns_3xx_with_malformed_location() {
        use axum::{Router, routing::get};
        let addr = spawn(Router::new().route(
            "/start",
            // `ht!tp://\bad` is not a parseable absolute URL (invalid scheme),
            // but it is a valid HTTP header value, so the server can emit it.
            get(|| async { redirect_302("ht!tp://\\bad".to_owned()) }),
        ))
        .await;

        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/start", addr.port()))
            .no_redirect()
            .send()
            .await
            .expect("no_redirect() must return the 3xx even with a malformed Location");

        assert_eq!(resp.status().as_u16(), 302);
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("ht!tp://\\bad")
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
        // Public literal with an explicit port → Ok, single-element validated
        // set, port preserved.
        let ok = resolve_and_validate("http://8.8.8.8:8080/path")
            .await
            .unwrap();
        assert_eq!(
            ok,
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 8080)]
        );

        // https default port is inferred.
        let ok_https = resolve_and_validate("https://1.1.1.1/").await.unwrap();
        assert_eq!(ok_https.len(), 1);
        assert_eq!(ok_https[0].port(), 443);

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

    // TEST 56b: validate_resolved_addrs — the pure validation core shared by the
    // resolve→validate step. A set of public addrs returns ALL of them in order;
    // a set mixing a public and a blocked addr is rejected with SsrfBlocked; an
    // all-public IPv6+IPv4 mix returns everything in order. This exercises the
    // multi-record path (pin to ALL validated addresses) without needing real
    // multi-record DNS.
    #[test]
    fn validate_resolved_addrs_returns_all_public_rejects_any_blocked() {
        let v4 = |a, b, c, d, p| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), p);

        // Two public addrs → Ok with BOTH returned, order preserved.
        let two = vec![v4(1, 1, 1, 1, 443), v4(8, 8, 8, 8, 443)];
        assert_eq!(validate_resolved_addrs(two.clone()).unwrap(), two);

        // Public + blocked (10.0.0.1, RFC1918) → Err(SsrfBlocked).
        let mixed = vec![v4(1, 1, 1, 1, 443), v4(10, 0, 0, 1, 443)];
        assert!(
            matches!(
                validate_resolved_addrs(mixed),
                Err(ClientError::SsrfBlocked(_))
            ),
            "a set containing a blocked address must be rejected"
        );

        // All-public IPv6 (2606:4700:4700::1111, Cloudflare) + IPv4 mix → Ok,
        // all preserved in order.
        let v6 = SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
            443,
        );
        let mix = vec![v6, v4(8, 8, 8, 8, 443)];
        assert_eq!(validate_resolved_addrs(mix.clone()).unwrap(), mix);
    }

    // TEST 57: pin_to alone does NOT auto-follow a cross-host redirect. reqwest
    // would otherwise re-resolve the new host via normal DNS, silently escaping
    // the pin. The 302 must be returned verbatim and the onward target must
    // never be connected to.
    #[tokio::test]
    async fn pin_to_does_not_follow_redirect_unpinned() {
        use axum::{Router, routing::get};
        use std::sync::atomic::AtomicBool;

        // The redirect target that must never be reached.
        let touched = Arc::new(AtomicBool::new(false));
        let touched2 = touched.clone();
        let onward = spawn(Router::new().route(
            "/onward",
            get(move || {
                let t = touched2.clone();
                async move {
                    t.store(true, Ordering::SeqCst);
                    "REACHED"
                }
            }),
        ))
        .await;
        let onward_port = onward.port();

        let start =
            spawn(Router::new().route(
                "/start",
                get(move || async move {
                    redirect_302(format!("http://127.0.0.1:{onward_port}/onward"))
                }),
            ))
            .await;
        let start_port = start.port();

        // Use a DOMAIN host (`pinned.invalid`) so the pin's resolve override is
        // actually consulted; pinning an IP-literal host is now rejected by the
        // PinRequiresDomainHost guard. The non-resolvable domain reaches the
        // loopback listener only via the pin.
        let resp = Client::new()
            .get(format!("http://pinned.invalid:{start_port}/start"))
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), start_port))
            .send()
            .await
            .expect("pinned request should return the 302 unfollowed");

        assert_eq!(
            resp.status().as_u16(),
            302,
            "pin_to must return the redirect unfollowed"
        );
        assert!(
            !touched.load(Ordering::SeqCst),
            "pin_to must not silently follow the redirect onward"
        );
    }

    // TEST 58: get_ssrf_safe rejects a non-http(s) scheme up front with
    // InvalidUrl, before any DNS resolution or connection.
    #[tokio::test]
    async fn get_ssrf_safe_rejects_non_http_scheme() {
        for raw in ["ftp://public.example/resource", "gopher://public.example/"] {
            let result = Client::new().get_ssrf_safe(raw).send().await;
            assert!(
                matches!(result, Err(ClientError::InvalidUrl(_))),
                "{raw} should be rejected with InvalidUrl, got {result:?}"
            );
        }
    }

    // TEST 59: a cross-origin redirect (different port ⇒ different origin) must
    // NOT forward credential-bearing request headers to the new origin. This is
    // the manual-redirect-loop version of reqwest's built-in strip-on-cross-host
    // behaviour, closing a credential-leak hole (Fix A).
    #[tokio::test]
    async fn follow_redirects_strips_sensitive_headers_cross_origin() {
        use axum::{Router, routing::get};

        let seen: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        // Listener B records the headers it received (different port = different
        // origin from A).
        let b_addr = spawn(Router::new().route(
            "/dst",
            get(move |headers: HeaderMap| {
                let slot = seen2.clone();
                async move {
                    *slot.lock().unwrap() = Some(headers);
                    "ok"
                }
            }),
        ))
        .await;
        let b_port = b_addr.port();

        // Listener A 302-redirects onto B.
        let a_addr = spawn(Router::new().route(
            "/",
            get(move || async move { redirect_302(format!("http://127.0.0.1:{b_port}/dst")) }),
        ))
        .await;

        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/", a_addr.port()))
            .header("authorization", "secret")
            .header("cookie", "session=abc")
            .header("proxy-authorization", "Basic zzz")
            .follow_redirects(3, |_| true)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        let headers = seen
            .lock()
            .unwrap()
            .clone()
            .expect("listener B must have been reached");
        assert!(
            headers.get("authorization").is_none(),
            "authorization must be stripped on a cross-origin redirect"
        );
        assert!(
            headers.get("cookie").is_none(),
            "cookie must be stripped on a cross-origin redirect"
        );
        assert!(
            headers.get("proxy-authorization").is_none(),
            "proxy-authorization must be stripped on a cross-origin redirect"
        );
    }

    // TEST 60: a SAME-origin redirect (relative `Location`, same host:port) must
    // keep credential-bearing headers — stripping only applies across origins.
    #[tokio::test]
    async fn follow_redirects_keeps_sensitive_headers_same_origin() {
        use axum::{Router, routing::get};

        let seen: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        let addr = spawn(
            Router::new()
                .route("/", get(|| async { redirect_302("/next".to_owned()) }))
                .route(
                    "/next",
                    get(move |headers: HeaderMap| {
                        let slot = seen2.clone();
                        async move {
                            *slot.lock().unwrap() = Some(headers);
                            "ok"
                        }
                    }),
                ),
        )
        .await;

        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/", addr.port()))
            .header("authorization", "secret")
            .follow_redirects(3, |_| true)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        let headers = seen
            .lock()
            .unwrap()
            .clone()
            .expect("/next must have been reached");
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("secret"),
            "authorization must be preserved on a same-origin redirect"
        );
    }

    // TEST 61: RFC 7231 §6.4.3 — a 302 in response to a POST rewrites the next
    // hop to a bodyless GET (Fix B).
    #[tokio::test]
    async fn follow_redirects_302_post_becomes_get() {
        use axum::{
            Router,
            routing::{any, post},
        };

        let seen: Arc<Mutex<Option<(String, HeaderMap, Bytes)>>> = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        // B records the method + headers + body it actually received, for any verb.
        let b_addr = spawn(Router::new().route(
            "/dst",
            any(move |method: Method, headers: HeaderMap, body: Bytes| {
                let slot = seen2.clone();
                async move {
                    *slot.lock().unwrap() = Some((method.to_string(), headers, body));
                    "ok"
                }
            }),
        ))
        .await;
        let b_port = b_addr.port();

        let a_addr = spawn(Router::new().route(
            "/",
            post(move || async move { redirect_302(format!("http://127.0.0.1:{b_port}/dst")) }),
        ))
        .await;

        // `.json(..)` sets a request body AND `Content-Type: application/json`.
        // On the 302 POST→GET rewrite the body is dropped, and the payload
        // headers must be dropped with it (Fix B) — otherwise the followed GET
        // would carry a misleading `Content-Type` for a body it no longer has.
        let resp = Client::new()
            .post(format!("http://127.0.0.1:{}/", a_addr.port()))
            .json(&serde_json::json!({"payload": true}))
            .follow_redirects(3, |_| true)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        let (method, headers, body) = seen
            .lock()
            .unwrap()
            .clone()
            .expect("listener B must have been reached");
        assert_eq!(method, "GET", "302 must rewrite POST → GET");
        assert!(body.is_empty(), "302 POST→GET must drop the request body");
        assert!(
            !headers.contains_key(reqwest::header::CONTENT_TYPE),
            "302 POST→GET must drop the Content-Type payload header"
        );
        assert!(
            !headers.contains_key(reqwest::header::CONTENT_LENGTH),
            "302 POST→GET must drop the Content-Length payload header"
        );
    }

    // TEST 62: RFC 7231 §6.4.7 — a 307 preserves BOTH the method and the body
    // across the redirect (the review bot's drop-body-every-hop snippet was
    // wrong here) (Fix B).
    #[tokio::test]
    async fn follow_redirects_307_preserves_method_and_body() {
        use axum::{
            Router,
            routing::{any, post},
        };

        let seen: Arc<Mutex<Option<(String, Bytes)>>> = Arc::new(Mutex::new(None));
        let seen2 = seen.clone();
        let b_addr = spawn(Router::new().route(
            "/dst",
            any(move |method: Method, body: Bytes| {
                let slot = seen2.clone();
                async move {
                    *slot.lock().unwrap() = Some((method.to_string(), body));
                    "ok"
                }
            }),
        ))
        .await;
        let b_port = b_addr.port();

        let a_addr = spawn(Router::new().route(
            "/",
            post(move || async move { redirect_307(format!("http://127.0.0.1:{b_port}/dst")) }),
        ))
        .await;

        let resp = Client::new()
            .post(format!("http://127.0.0.1:{}/", a_addr.port()))
            .text_body("payload")
            .follow_redirects(3, |_| true)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 200);
        let (method, body) = seen
            .lock()
            .unwrap()
            .clone()
            .expect("listener B must have been reached");
        assert_eq!(method, "POST", "307 must preserve the POST method");
        assert_eq!(
            &body[..],
            b"payload",
            "307 must preserve the request body verbatim"
        );
    }

    // TEST 63: get_ssrf_safe derives its redirect follow/cap from the chained
    // builder mode via ssrf_redirect_plan, so `no_redirect()` /
    // `follow_redirects(max, ..)` override the SSRF-safe default hop cap.
    //
    // NOTE: the network-level follow behaviour of get_ssrf_safe cannot be
    // exercised in-sandbox — the only reachable address here is loopback, which
    // the SSRF guard blocks before connecting — so this deterministic unit test
    // on the factored `ssrf_redirect_plan` helper stands in for it.
    #[test]
    fn ssrf_redirect_plan_honours_chained_override() {
        let client = Client::new();

        // Default get_ssrf_safe → follow up to the SSRF-safe hop cap.
        let default = client.get_ssrf_safe("https://example.com/");
        assert_eq!(
            default.ssrf_redirect_plan(),
            (true, SSRF_SAFE_MAX_REDIRECTS),
            "default SSRF-safe path follows up to SSRF_SAFE_MAX_REDIRECTS"
        );

        // no_redirect() → do NOT follow.
        let none = client.get_ssrf_safe("https://example.com/").no_redirect();
        let (follow, _max) = none.ssrf_redirect_plan();
        assert!(
            !follow,
            "no_redirect() must disable following on the safe path"
        );

        // follow_redirects(3, ..) → follow up to the caller's max.
        let follow3 = client
            .get_ssrf_safe("https://example.com/")
            .follow_redirects(3, |_| true);
        assert_eq!(
            follow3.ssrf_redirect_plan(),
            (true, 3),
            "follow_redirects(3, ..) must cap the safe path at 3 hops"
        );
    }

    // TEST 64: pin_to + follow_redirects is rejected at send time with
    // IncompatiblePinRedirect — deterministically, without touching the network.
    // A single pinned SocketAddr only covers hop 0; later redirect hops re-resolve
    // via normal DNS, so following would silently escape the pin.
    #[tokio::test]
    async fn pin_then_follow_redirects_is_rejected() {
        let result = Client::new()
            .get("http://example.com/")
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
            .follow_redirects(2, |_| true)
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::IncompatiblePinRedirect(_))),
            "expected IncompatiblePinRedirect, got {result:?}"
        );
    }

    // TEST 65: the rejection is order-independent — chaining follow_redirects
    // before pin_to produces the same IncompatiblePinRedirect error.
    #[tokio::test]
    async fn follow_redirects_then_pin_is_rejected() {
        let result = Client::new()
            .get("http://example.com/")
            .follow_redirects(2, |_| true)
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::IncompatiblePinRedirect(_))),
            "expected IncompatiblePinRedirect, got {result:?}"
        );
    }

    // TEST 66: pin_to combined with no_redirect is NOT affected by the guard —
    // the request proceeds and returns the 3xx verbatim (RedirectMode::None).
    #[tokio::test]
    async fn pin_with_no_redirect_is_allowed() {
        use axum::{Router, routing::get};

        let addr = spawn(Router::new().route(
            "/start",
            get(|| async { redirect_302("http://127.0.0.1:1/onward".to_owned()) }),
        ))
        .await;
        let port = addr.port();

        // Use a DOMAIN host so the pin's resolve override is consulted; pinning
        // an IP-literal host is now rejected by the PinRequiresDomainHost guard.
        let resp = Client::new()
            .get(format!("http://pinned.invalid:{port}/start"))
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .no_redirect()
            .send()
            .await
            .expect("pin_to + no_redirect must return the 3xx unfollowed, not error");

        assert_eq!(
            resp.status().as_u16(),
            302,
            "pin_to + no_redirect returns the redirect verbatim"
        );
    }

    // TEST 67: pin_to on an IPv4-literal URL host is rejected at send time with
    // PinRequiresDomainHost — deterministically, without touching the network.
    // reqwest/hyper treat an IP-literal host as already-resolved and skip the
    // resolve override that installs the pin, so the socket would connect to the
    // literal in the URL (198.51.100.1), NOT the pinned 127.0.0.1 — silently
    // bypassing the pin. The guard rejects it instead.
    #[tokio::test]
    async fn pin_to_ipv4_literal_host_is_rejected() {
        let result = Client::new()
            .get("http://198.51.100.1:8080/")
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::PinRequiresDomainHost(_))),
            "expected PinRequiresDomainHost, got {result:?}"
        );
    }

    // TEST 68: the same rejection applies to an IPv6-literal URL host.
    #[tokio::test]
    async fn pin_to_ipv6_literal_host_is_rejected() {
        let result = Client::new()
            .get("http://[2606:4700::1111]/")
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::PinRequiresDomainHost(_))),
            "expected PinRequiresDomainHost, got {result:?}"
        );
    }

    // TEST 68b: get_ssrf_safe combined with pin_to is rejected at send time with
    // PinNotAllowedWithSsrfSafe — deterministically, without touching the
    // network. The SSRF-safe path runs its own per-hop resolve/validate/pin and
    // never reads the pin_to address, so an explicit pin would be silently
    // ignored; the guard fails loudly instead.
    #[tokio::test]
    async fn get_ssrf_safe_with_pin_to_is_rejected() {
        let result = Client::new()
            .get_ssrf_safe("http://example.com/")
            .pin_to(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080))
            .send()
            .await;

        assert!(
            matches!(result, Err(ClientError::PinNotAllowedWithSsrfSafe(_))),
            "expected PinNotAllowedWithSsrfSafe, got {result:?}"
        );
    }

    // TEST 69: a `304 Not Modified` that happens to carry a `Location` header is
    // NOT a followable redirect. reqwest only follows 301/302/303/307/308, so
    // `redirect_target` must return the 304 to the caller verbatim rather than
    // issuing a second request against the `Location` target.
    #[tokio::test]
    async fn follow_redirects_does_not_follow_304_with_location() {
        use axum::{Router, routing::get};
        use std::sync::atomic::AtomicBool;

        // Listener B must never be reached.
        let touched = Arc::new(AtomicBool::new(false));
        let touched2 = touched.clone();
        let b_addr = spawn(Router::new().route(
            "/dst",
            get(move || {
                let t = touched2.clone();
                async move {
                    t.store(true, Ordering::SeqCst);
                    "SHOULD-NOT-BE-HIT"
                }
            }),
        ))
        .await;
        let b_port = b_addr.port();

        // Listener A returns 304 + Location pointing at B.
        let a_addr = spawn(Router::new().route(
            "/start",
            get(move || async move {
                response_with_location(304, format!("http://127.0.0.1:{b_port}/dst"))
            }),
        ))
        .await;

        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/start", a_addr.port()))
            .follow_redirects(5, |_| true)
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            304,
            "a 304 with a Location header must be returned verbatim, not followed"
        );
        assert!(
            !touched.load(Ordering::SeqCst),
            "the 304 Location target must never be requested"
        );
    }

    // TEST 70: a `300 Multiple Choices` that carries a `Location` header is
    // likewise not a followable redirect (only 301/302/303/307/308 are), so the
    // 300 is returned to the caller and the `Location` target is never hit.
    #[tokio::test]
    async fn follow_redirects_does_not_follow_300_with_location() {
        use axum::{Router, routing::get};
        use std::sync::atomic::AtomicBool;

        let touched = Arc::new(AtomicBool::new(false));
        let touched2 = touched.clone();
        let b_addr = spawn(Router::new().route(
            "/dst",
            get(move || {
                let t = touched2.clone();
                async move {
                    t.store(true, Ordering::SeqCst);
                    "SHOULD-NOT-BE-HIT"
                }
            }),
        ))
        .await;
        let b_port = b_addr.port();

        let a_addr = spawn(Router::new().route(
            "/start",
            get(move || async move {
                response_with_location(300, format!("http://127.0.0.1:{b_port}/dst"))
            }),
        ))
        .await;

        let resp = Client::new()
            .get(format!("http://127.0.0.1:{}/start", a_addr.port()))
            .follow_redirects(5, |_| true)
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status().as_u16(),
            300,
            "a 300 with a Location header must be returned verbatim, not followed"
        );
        assert!(
            !touched.load(Ordering::SeqCst),
            "the 300 Location target must never be requested"
        );
    }

    // ── Issue #3058: deadline and retry budget on the real send paths ────────
    #[allow(clippy::large_futures, reason = "test futures, awaited once")]
    mod deadline_and_budget {
        use super::super::*;
        use std::sync::atomic::AtomicU32;

        /// A server that counts hits on `/x`. It answers with `status` and
        /// `headers`, or never when `status` is `None`.
        async fn counting(
            status: Option<u16>,
            headers: &'static [(&'static str, &'static str)],
        ) -> (String, Arc<AtomicU32>) {
            let hits = Arc::new(AtomicU32::new(0));
            let counter = Arc::clone(&hits);
            let app = axum::Router::new().route(
                "/x",
                axum::routing::get(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async move {
                        let Some(code) = status else {
                            return std::future::pending().await;
                        };
                        let mut response = axum::response::Response::builder().status(code);
                        for (name, value) in headers {
                            response = response.header(*name, *value);
                        }
                        response.body(axum::body::Body::empty()).unwrap()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            (format!("http://127.0.0.1:{}/x", addr.port()), hits)
        }

        /// A client whose budget allows one transient retry and never refills.
        fn one_retry_client() -> Client {
            let mut config = crate::config::HttpClientConfig::default();
            config.retry_budget.capacity = 14;
            config.retry_budget.retry_ratio = 0.0;
            Client::from_config(&config)
        }

        /// Run `send` under a deadline `after` from now, with a 5 s safety
        /// limit.
        async fn with_deadline(
            after: Duration,
            send: impl std::future::Future<Output = Result<Response, ClientError>>,
        ) -> Result<Response, ClientError> {
            tokio::time::timeout(Duration::from_secs(5), Deadline::after(after).scope(send))
                .await
                .expect("the deadline must stop the call")
        }

        fn lock() -> std::sync::MutexGuard<'static, ()> {
            let guard = crate::circuit_breaker::TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::circuit_breaker::global_registry().clear();
            guard
        }

        /// A server where `/r` waits `delay`, then redirects to `/t`. Both
        /// record the deadline header they get.
        async fn redirecting(delay: Duration) -> (String, Arc<Mutex<Vec<(String, u64)>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let record = |path: &'static str, seen: Arc<Mutex<Vec<(String, u64)>>>| {
                move |headers: HeaderMap| {
                    let millis = headers
                        .get(DEADLINE_HEADER)
                        .and_then(|value| value.to_str().ok()?.parse().ok())
                        .unwrap_or(u64::MAX);
                    seen.lock().unwrap().push((path.to_owned(), millis));
                }
            };
            let at_r = record("/r", Arc::clone(&seen));
            let at_t = record("/t", Arc::clone(&seen));
            let app = axum::Router::new()
                .route(
                    "/r",
                    axum::routing::get(move |headers: HeaderMap| async move {
                        at_r(headers);
                        tokio::time::sleep(delay).await;
                        axum::response::Redirect::temporary("/t")
                    }),
                )
                .route(
                    "/t",
                    axum::routing::get(move |headers: HeaderMap| async move {
                        at_t(headers);
                        "done"
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            (format!("http://127.0.0.1:{}/r", addr.port()), seen)
        }

        #[tokio::test]
        async fn plain_path_sends_a_fresh_deadline_header_after_a_redirect() {
            let (url, seen) = redirecting(Duration::from_millis(500)).await;
            let response = with_deadline(Duration::from_secs(3), Client::new().get(&url).send())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let seen = seen.lock().unwrap().clone();
            let [(first, at_origin), (second, at_target)] = seen.as_slice() else {
                panic!("two hops: {seen:?}");
            };
            assert_eq!((first.as_str(), second.as_str()), ("/r", "/t"));
            assert!(*at_origin <= 3_000, "{seen:?}");
            assert!(
                *at_target + 400 <= *at_origin,
                "the target gets the time left after the redirect: {seen:?}"
            );
        }

        #[tokio::test]
        async fn a_redirected_connect_error_charges_the_destination_budget() {
            // A port with nothing listening on it.
            let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let dead_port = closed.local_addr().unwrap().port();
            drop(closed);
            let target = format!("http://127.0.0.1:{dead_port}/t");
            let app = axum::Router::new().route(
                "/r",
                axum::routing::get(move || {
                    let target = target.clone();
                    async move { axum::response::Redirect::temporary(&target) }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin_port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let client = Client::from_config(&crate::config::HttpClientConfig::default());
            let budgets = client.retry.budgets.clone().unwrap();
            let result = client
                .get(format!("http://127.0.0.1:{origin_port}/r"))
                .retries(1)
                .send()
                .await;
            assert!(result.is_err(), "{result:?}");
            let full = f64::from(RetryBudgetConfig::default().capacity);
            let origin = budgets.for_host(&format!("127.0.0.1:{origin_port}"));
            let destination = budgets.for_host(&format!("127.0.0.1:{dead_port}"));
            assert!((origin.available() - full).abs() < f64::EPSILON, "origin");
            assert!(destination.available() < full, "the destination paid");
        }

        #[tokio::test]
        async fn a_redirect_under_a_deadline_sets_referer() {
            let seen = Arc::new(Mutex::new(None));
            let record = Arc::clone(&seen);
            let app = axum::Router::new()
                .route(
                    "/r",
                    axum::routing::get(|| async { axum::response::Redirect::temporary("/t") }),
                )
                .route(
                    "/t",
                    axum::routing::get(move |headers: HeaderMap| async move {
                        *record.lock().unwrap() = headers
                            .get(reqwest::header::REFERER)
                            .map(|value| value.to_str().unwrap().to_owned());
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let origin = format!("http://127.0.0.1:{port}/r");
            let response = with_deadline(Duration::from_secs(3), Client::new().get(&origin).send())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            assert_eq!(seen.lock().unwrap().as_deref(), Some(origin.as_str()));
        }

        #[test]
        fn referer_drops_credentials_and_is_not_sent_on_a_downgrade() {
            let mut headers = HeaderMap::new();
            set_referer(&mut headers, "https://b/x", "https://u:p@a/y#frag");
            assert_eq!(headers[reqwest::header::REFERER], "https://a/y");
            set_referer(&mut headers, "http://c/z", "https://b/x");
            assert!(!headers.contains_key(reqwest::header::REFERER));
        }

        #[test]
        fn every_host_of_a_redirect_chain_is_refilled() {
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let middle = budgets.for_host("b:443");
            let last = budgets.for_host("c:443");
            while middle.try_acquire(RetryKind::Transient) {}
            while last.try_acquire(RetryKind::Transient) {}
            let (middle_before, last_before) = (middle.available(), last.available());
            let gate =
                RetryGate::with_deadline(None, Some(Arc::clone(&budgets)), Some("a:443"), true);
            let mut seen = std::collections::HashSet::new();
            gate.record_destinations(
                &[
                    "https://b/1".to_owned(),
                    "https://c/2".to_owned(),
                    "https://c/3".to_owned(),
                ],
                &mut seen,
            );
            assert!(middle.available() > middle_before, "the middle host");
            let one_refill = middle.available() - middle_before;
            assert!(
                (last.available() - last_before - one_refill).abs() < f64::EPSILON,
                "one refill per host"
            );
        }

        /// A server whose `/r` answers 302 with `location`, and whose `/t`
        /// answers 200. With `endless`, the 302 body never ends.
        async fn redirect_server(location: &'static str, endless: bool) -> String {
            let app = axum::Router::new()
                .route(
                    "/r",
                    axum::routing::get(move || async move {
                        let body = if endless {
                            axum::body::Body::from_stream(futures::stream::pending::<
                                Result<Bytes, std::io::Error>,
                            >())
                        } else {
                            axum::body::Body::empty()
                        };
                        axum::response::Response::builder()
                            .status(302)
                            .header("location", location)
                            .body(body)
                            .unwrap()
                    }),
                )
                .route("/t", axum::routing::get(|| async { "done" }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://127.0.0.1:{port}/r")
        }

        #[tokio::test]
        async fn a_redirect_under_a_deadline_does_not_read_the_redirect_body() {
            let url = redirect_server("/t", true).await;
            let response = with_deadline(Duration::from_secs(3), Client::new().get(&url).send())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            assert_eq!(response.text(), "done");
        }

        #[tokio::test]
        async fn a_bad_location_under_a_deadline_returns_the_redirect() {
            let url = redirect_server("http://[::1", false).await;
            let response = with_deadline(Duration::from_secs(3), Client::new().get(&url).send())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 302);
            let plain = Client::new().get(&url).send().await.unwrap();
            assert_eq!(
                plain.status().as_u16(),
                302,
                "the same as without a deadline"
            );
        }

        #[tokio::test]
        async fn a_redirect_chain_under_a_deadline_shares_one_retry_count() {
            use axum::response::IntoResponse;
            let origin_hits = Arc::new(AtomicU32::new(0));
            let target_hits = Arc::new(AtomicU32::new(0));
            let (origin, target) = (Arc::clone(&origin_hits), Arc::clone(&target_hits));
            // Each hop fails once, then works.
            let app = axum::Router::new()
                .route(
                    "/r",
                    axum::routing::get(move || async move {
                        if origin.fetch_add(1, Ordering::SeqCst) == 0 {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                        } else {
                            axum::response::Redirect::temporary("/t").into_response()
                        }
                    }),
                )
                .route(
                    "/t",
                    axum::routing::get(move || async move {
                        if target.fetch_add(1, Ordering::SeqCst) == 0 {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                        } else {
                            "done".into_response()
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = format!("http://127.0.0.1:{port}/r");
            let response = with_deadline(
                Duration::from_secs(3),
                Client::new().get(&url).retries(1).send(),
            )
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 503, "the one retry is spent");
            assert_eq!(origin_hits.load(Ordering::SeqCst), 2);
            assert_eq!(target_hits.load(Ordering::SeqCst), 1);
        }

        /// Entropy that always draws one value.
        #[derive(Debug)]
        struct FixedDraw(u64);

        impl crate::entropy::Entropy for FixedDraw {
            fn next_u64(&self) -> u64 {
                self.0
            }
            fn fill_bytes(&self, dest: &mut [u8]) {
                dest.fill(0);
            }
        }

        #[tokio::test]
        async fn a_cross_origin_redirect_under_a_deadline_strips_what_reqwest_strips() {
            let seen: Arc<std::sync::Mutex<Option<HeaderMap>>> =
                Arc::new(std::sync::Mutex::new(None));
            let slot = Arc::clone(&seen);
            let target = super::spawn(axum::Router::new().route(
                "/dst",
                axum::routing::get(move |headers: HeaderMap| {
                    let slot = Arc::clone(&slot);
                    async move {
                        *slot.lock().unwrap() = Some(headers);
                        "ok"
                    }
                }),
            ))
            .await;
            let target_port = target.port();
            let origin = super::spawn(axum::Router::new().route(
                "/",
                axum::routing::get(move || async move {
                    super::redirect_302(format!("http://127.0.0.1:{target_port}/dst"))
                }),
            ))
            .await;
            let response = with_deadline(
                Duration::from_secs(3),
                Client::new()
                    .get(format!("http://127.0.0.1:{}/", origin.port()))
                    .header("authorization", "secret")
                    .header("cookie", "a=b")
                    .header("cookie2", "c=d")
                    .header("proxy-authorization", "Basic zzz")
                    .header("www-authenticate", "Basic realm=x")
                    .send(),
            )
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let headers = seen.lock().unwrap().clone().expect("target reached");
            for name in [
                "authorization",
                "cookie",
                "cookie2",
                "proxy-authorization",
                "www-authenticate",
            ] {
                assert!(headers.get(name).is_none(), "{name} reached the target");
            }
        }

        #[tokio::test]
        async fn a_retry_cancelled_during_its_backoff_gives_its_tokens_back() {
            let (url, hits) = counting(Some(502), &[]).await;
            let host = url_host(&url).unwrap();
            // The first retry waits 100 % 101 = 100 ms.
            let mut client = Client::new();
            client.entropy = Arc::new(FixedDraw(100));
            let budget = client.retry.budgets.clone().unwrap().for_host(&host);
            let cancelled = tokio::time::timeout(
                Duration::from_millis(50),
                client.get(&url).retries(3).send(),
            )
            .await;
            assert!(cancelled.is_err(), "cancelled in the backoff");
            assert_eq!(hits.load(Ordering::SeqCst), 1);
            let full = f64::from(RetryBudgetConfig::default().capacity);
            assert!(
                (budget.available() - full).abs() < f64::EPSILON,
                "no retry ran, so none is paid for: {}",
                budget.available()
            );
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn a_deadline_stop_on_the_custom_path_is_not_a_throttle_reject() {
            let _lock = crate::circuit_breaker::TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::circuit_breaker::global_registry().clear();
            let (url, _hits) = counting(None, &[]).await;
            let throttle = Arc::new(crate::admission::AdaptiveThrottle::new(
                2.0,
                Duration::from_secs(120),
            ));
            let client = Client {
                resilience_config: Some(Arc::new(crate::config::ResilienceConfig::default())),
                throttle: Some(Arc::clone(&throttle)),
                ..Client::new()
            };
            for _ in 0..10 {
                let result = with_deadline(
                    Duration::from_millis(30),
                    client.get(&url).no_redirect().breaker_scoped().send(),
                )
                .await;
                assert!(
                    matches!(result, Err(ClientError::DeadlineExceeded)),
                    "{result:?}"
                );
            }
            let host = throttle_host(&url).unwrap();
            assert!(
                throttle.reject_probability(&host, crate::time::ambient_instant()) < f64::EPSILON,
                "the caller's deadline is not the host's reject"
            );
            crate::circuit_breaker::global_registry().clear();
        }

        #[tokio::test]
        async fn the_throttle_applies_under_a_deadline() {
            let (url, _hits) = counting(Some(503), &[]).await;
            let throttle = Arc::new(crate::admission::AdaptiveThrottle::new(
                2.0,
                Duration::from_secs(120),
            ));
            let client = Client {
                throttle: Some(Arc::clone(&throttle)),
                ..Client::new()
            };
            let mut throttled = 0;
            for _ in 0..100 {
                let result =
                    with_deadline(Duration::from_secs(5), client.get(&url).no_retry().send()).await;
                if matches!(result, Err(ClientError::ThrottledLocally { .. })) {
                    throttled += 1;
                }
            }
            assert!(
                throttled > 20,
                "the deadline path must use the throttle: {throttled}"
            );
        }

        #[tokio::test]
        async fn each_retry_under_a_deadline_is_a_throttle_attempt() {
            use axum::response::IntoResponse;
            // Each call gets 503, 503, then 200.
            let hits = Arc::new(AtomicU32::new(0));
            let counter = Arc::clone(&hits);
            let upstream = super::spawn(axum::Router::new().route(
                "/x",
                axum::routing::get(move || {
                    let status = if counter.fetch_add(1, Ordering::SeqCst) % 3 < 2 {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        axum::http::StatusCode::OK
                    };
                    async move { status.into_response() }
                }),
            ))
            .await;
            let url = format!("http://127.0.0.1:{}/x", upstream.port());
            let throttle = Arc::new(crate::admission::AdaptiveThrottle::new(
                2.0,
                Duration::from_secs(120),
            ));
            // The highest draw: the throttle never rejects, so every call
            // runs its three attempts.
            let mut client = Client {
                throttle: Some(Arc::clone(&throttle)),
                ..Client::new()
            };
            client.entropy = Arc::new(FixedDraw(u64::MAX));
            for _ in 0..10 {
                let response =
                    with_deadline(Duration::from_secs(4), client.get(&url).retries(2).send())
                        .await
                        .unwrap();
                assert_eq!(response.status().as_u16(), 200);
            }
            assert_eq!(hits.load(Ordering::SeqCst), 30);
            // 30 attempts, 10 accepts: below 1/K, as on the path without a
            // deadline. One count per call would hide the 503s.
            let host = throttle_host(&url).unwrap();
            assert!(
                throttle.reject_probability(&host, crate::time::ambient_instant()) > 0.3,
                "each attempt counts"
            );
        }

        #[tokio::test]
        async fn a_retry_sends_what_is_left_of_the_callers_deadline_header() {
            use axum::response::IntoResponse;
            // The first attempt takes 60 ms and fails; the retry works.
            let seen: Arc<std::sync::Mutex<Vec<u64>>> = Arc::default();
            let slot = Arc::clone(&seen);
            let upstream = super::spawn(axum::Router::new().route(
                "/x",
                axum::routing::get(move |headers: HeaderMap| {
                    let slot = Arc::clone(&slot);
                    async move {
                        let value = headers
                            .get(DEADLINE_HEADER)
                            .and_then(|v| v.to_str().ok()?.parse().ok())
                            .unwrap_or(u64::MAX);
                        let first = {
                            let mut seen = slot.lock().unwrap();
                            seen.push(value);
                            seen.len() == 1
                        };
                        if first {
                            tokio::time::sleep(Duration::from_millis(60)).await;
                            axum::http::StatusCode::BAD_GATEWAY.into_response()
                        } else {
                            axum::http::StatusCode::OK.into_response()
                        }
                    }
                }),
            ))
            .await;
            // A zero draw: the retry starts at once.
            let mut client = Client::new();
            client.entropy = Arc::new(FixedDraw(0));
            let response = with_deadline(
                Duration::from_secs(4),
                client
                    .get(format!("http://127.0.0.1:{}/x", upstream.port()))
                    .header(DEADLINE_HEADER, "200")
                    .retries(1)
                    .send(),
            )
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 2);
            assert!(seen[0] <= 200, "{seen:?}");
            // 60 ms of the caller's 200 ms are gone.
            assert!(
                seen[1] <= 140,
                "the retry re-extended the deadline: {seen:?}"
            );
        }

        #[tokio::test]
        async fn a_redirect_chain_under_a_deadline_keeps_the_client_timeout() {
            use axum::response::IntoResponse;
            // Each hop takes 200 ms: 400 ms in all, over the 300 ms timeout.
            let app = axum::Router::new()
                .route(
                    "/a",
                    axum::routing::get(|| async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        axum::response::Redirect::temporary("/b").into_response()
                    }),
                )
                .route(
                    "/b",
                    axum::routing::get(|| async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        "ok"
                    }),
                );
            let upstream = super::spawn(app).await;
            let mut client = Client::new();
            client.retry_policy.request_timeout = Some(Duration::from_millis(300));
            let result = with_deadline(
                Duration::from_secs(4),
                client
                    .get(format!("http://127.0.0.1:{}/a", upstream.port()))
                    .no_retry()
                    .send(),
            )
            .await;
            // Without a deadline, reqwest's one timeout covers the redirects.
            assert!(
                matches!(&result, Err(ClientError::Request(e)) if e.is_timeout()),
                "the chain must not get a new timeout per hop: {result:?}"
            );
        }

        #[tokio::test]
        async fn without_a_deadline_a_retry_sends_what_is_left_of_the_callers_header() {
            use axum::response::IntoResponse;
            // The first attempt takes 60 ms and fails; the retry works.
            let seen: Arc<std::sync::Mutex<Vec<u64>>> = Arc::default();
            let slot = Arc::clone(&seen);
            let upstream = super::spawn(axum::Router::new().route(
                "/x",
                axum::routing::get(move |headers: HeaderMap| {
                    let slot = Arc::clone(&slot);
                    async move {
                        let first = {
                            let mut seen = slot.lock().unwrap();
                            seen.push(header_millis(&headers));
                            seen.len() == 1
                        };
                        if first {
                            tokio::time::sleep(Duration::from_millis(60)).await;
                            axum::http::StatusCode::BAD_GATEWAY.into_response()
                        } else {
                            axum::http::StatusCode::OK.into_response()
                        }
                    }
                }),
            ))
            .await;
            let mut client = Client::new();
            client.entropy = Arc::new(FixedDraw(0));
            // No task deadline: only the caller's header.
            let response = client
                .get(format!("http://127.0.0.1:{}/x", upstream.port()))
                .header(DEADLINE_HEADER, "200")
                .retries(1)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen[0], 200, "{seen:?}");
            assert!(seen[1] <= 140, "the retry re-extended the header: {seen:?}");
        }

        #[tokio::test]
        async fn without_a_deadline_a_redirect_hop_sends_what_is_left_of_the_callers_header() {
            use axum::response::IntoResponse;
            let seen: Arc<std::sync::Mutex<Vec<u64>>> = Arc::default();
            let (at_a, at_b) = (Arc::clone(&seen), Arc::clone(&seen));
            let app = axum::Router::new()
                .route(
                    "/a",
                    axum::routing::get(move |headers: HeaderMap| async move {
                        at_a.lock().unwrap().push(header_millis(&headers));
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        axum::response::Redirect::temporary("/b").into_response()
                    }),
                )
                .route(
                    "/b",
                    axum::routing::get(move |headers: HeaderMap| async move {
                        at_b.lock().unwrap().push(header_millis(&headers));
                        "ok"
                    }),
                );
            let upstream = super::spawn(app).await;
            let response = Client::new()
                .get(format!("http://127.0.0.1:{}/a", upstream.port()))
                .header(DEADLINE_HEADER, "200")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 2, "{seen:?}");
            assert_eq!(seen[0], 200, "{seen:?}");
            assert!(seen[1] <= 140, "the hop re-extended the header: {seen:?}");
        }

        /// The [`DEADLINE_HEADER`] value of a request, or `u64::MAX`.
        fn header_millis(headers: &HeaderMap) -> u64 {
            headers
                .get(DEADLINE_HEADER)
                .and_then(|v| v.to_str().ok()?.parse().ok())
                .unwrap_or(u64::MAX)
        }

        #[tokio::test]
        async fn a_redirect_hop_continues_the_chain_backoff() {
            use axum::response::IntoResponse;
            let origin_hits = Arc::new(AtomicU32::new(0));
            let target_hits = Arc::new(AtomicU32::new(0));
            let (origin, target) = (Arc::clone(&origin_hits), Arc::clone(&target_hits));
            // Each hop fails once, then works.
            let app = axum::Router::new()
                .route(
                    "/r",
                    axum::routing::get(move || async move {
                        if origin.fetch_add(1, Ordering::SeqCst) == 0 {
                            axum::http::StatusCode::BAD_GATEWAY.into_response()
                        } else {
                            axum::response::Redirect::temporary("/t").into_response()
                        }
                    }),
                )
                .route(
                    "/t",
                    axum::routing::get(move || async move {
                        if target.fetch_add(1, Ordering::SeqCst) == 0 {
                            axum::http::StatusCode::BAD_GATEWAY.into_response()
                        } else {
                            "done".into_response()
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = format!("http://127.0.0.1:{port}/r");
            // 20_099 = 101 × 199: the first retry waits 20_099 % 101 = 0 ms,
            // the second 20_099 % 201 = 200 ms.
            let mut client = Client::new();
            client.entropy = Arc::new(FixedDraw(20_099));
            let start = std::time::Instant::now();
            let response =
                with_deadline(Duration::from_secs(5), client.get(&url).retries(2).send())
                    .await
                    .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            assert_eq!(origin_hits.load(Ordering::SeqCst), 2);
            assert_eq!(target_hits.load(Ordering::SeqCst), 2);
            assert!(
                start.elapsed() >= Duration::from_millis(200),
                "the hop's retry is the chain's second: {:?}",
                start.elapsed()
            );
        }

        #[tokio::test]
        async fn a_post_redirected_to_get_under_a_deadline_gets_no_retry() {
            use axum::response::IntoResponse;
            let target_hits = Arc::new(AtomicU32::new(0));
            let target = Arc::clone(&target_hits);
            let app = axum::Router::new()
                .route(
                    "/r",
                    axum::routing::post(|| async { axum::response::Redirect::to("/t") }),
                )
                .route(
                    "/t",
                    axum::routing::get(move || async move {
                        if target.fetch_add(1, Ordering::SeqCst) == 0 {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                        } else {
                            "done".into_response()
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = format!("http://127.0.0.1:{port}/r");
            let response = with_deadline(
                Duration::from_secs(3),
                Client::new().post(&url).retries(3).send(),
            )
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 503, "as without a deadline");
            assert_eq!(target_hits.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn a_backoff_past_the_hop_deadline_returns_the_response() {
            let (url, _hits) = counting(Some(502), &[]).await;
            let policy = RetryPolicy {
                max_retries: 20,
                ..RetryPolicy::default()
            };
            let gate = RetryGate::with_deadline(None, None, None, true);
            let result = send_one(
                &reqwest::Client::new(),
                &Method::GET,
                &url,
                &HeaderMap::new(),
                None,
                &policy,
                &crate::entropy::SeededEntropy::new(1),
                false,
                Some(crate::time::ambient_instant() + Duration::from_millis(300)),
                false,
                &gate,
                false,
                None,
                None,
            )
            .await;
            assert_eq!(
                result.map(|response| response.status().as_u16()).ok(),
                Some(502),
                "the last response, not a deadline error"
            );
        }

        #[tokio::test]
        async fn a_transport_retry_past_the_hop_deadline_is_not_charged() {
            let app = axum::Router::new().route(
                "/hang",
                axum::routing::get(|| async {
                    std::future::pending::<()>().await;
                    ""
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let host = format!("127.0.0.1:{port}");
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let gate =
                RetryGate::with_deadline(None, Some(Arc::clone(&budgets)), Some(&host), true);
            // The attempt uses up the hop: no retry can follow it.
            let result = send_one(
                &reqwest::Client::new(),
                &Method::GET,
                &format!("http://{host}/hang"),
                &HeaderMap::new(),
                None,
                &RetryPolicy::default(),
                &crate::entropy::SeededEntropy::new(1),
                false,
                Some(crate::time::ambient_instant() + Duration::from_millis(200)),
                false,
                &gate,
                false,
                None,
                None,
            )
            .await;
            assert!(result.is_err(), "{result:?}");
            let full = f64::from(RetryBudgetConfig::default().capacity);
            let available = budgets.for_host(&host).available();
            assert!(
                (full - available).abs() < f64::EPSILON,
                "no tokens for a retry that cannot run: {available}"
            );
        }

        #[tokio::test]
        async fn a_retry_needs_a_minimum_attempt_before_the_hop_deadline() {
            let (url, hits) = counting(Some(502), &[]).await;
            let gate = RetryGate::with_deadline(None, None, None, true);
            // The 100 ms backoff ends before the hop deadline, but leaves less
            // than `MIN_ATTEMPT` for the retry.
            let result = send_one(
                &reqwest::Client::new(),
                &Method::GET,
                &url,
                &HeaderMap::new(),
                None,
                &RetryPolicy::default(),
                &FixedDraw(100),
                false,
                Some(crate::time::ambient_instant() + Duration::from_millis(105)),
                false,
                &gate,
                false,
                None,
                None,
            )
            .await;
            assert_eq!(
                result.map(|response| response.status().as_u16()).ok(),
                Some(502),
                "the response, not a retry that cannot run"
            );
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn an_origin_failure_after_a_redirect_charges_the_origin() {
            // The first request redirects to a dead port; later ones hang.
            let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let dead_port = closed.local_addr().unwrap().port();
            drop(closed);
            let target = format!("http://127.0.0.1:{dead_port}/t");
            let hits = Arc::new(AtomicU32::new(0));
            let counter = Arc::clone(&hits);
            let app = axum::Router::new().route(
                "/r",
                axum::routing::get(move || {
                    let target = target.clone();
                    let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
                    async move {
                        if !first {
                            std::future::pending::<()>().await;
                        }
                        axum::response::Redirect::temporary(&target)
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin_port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let client = Client::with_timeout(Duration::from_millis(300));
            let budgets = client.retry.budgets.clone().unwrap();
            let result = client
                .get(format!("http://127.0.0.1:{origin_port}/r"))
                .retries(2)
                .send()
                .await;
            assert!(result.is_err(), "{result:?}");
            let full = f64::from(RetryBudgetConfig::default().capacity);
            let cost = f64::from(RetryBudgetConfig::default().transient_cost);
            let origin = budgets.for_host(&format!("127.0.0.1:{origin_port}"));
            let dead = budgets.for_host(&format!("127.0.0.1:{dead_port}"));
            assert!(
                (full - dead.available() - cost).abs() < f64::EPSILON,
                "the dead target pays its one retry: {}",
                dead.available()
            );
            assert!(
                (full - origin.available() - cost).abs() < f64::EPSILON,
                "the origin pays the retry after its own timeout: {}",
                origin.available()
            );
        }

        #[tokio::test]
        async fn a_redirect_host_is_refilled_when_the_final_body_fails() {
            // The target's body fails after the head.
            let target_app = axum::Router::new().route(
                "/t",
                axum::routing::get(|| async {
                    axum::body::Body::from_stream(futures::stream::once(async {
                        Err::<Bytes, _>(std::io::Error::other("broken body"))
                    }))
                }),
            );
            let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target_port = target_listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(target_listener, target_app).await.unwrap() });
            let target = format!("http://127.0.0.1:{target_port}/t");
            let origin_app = axum::Router::new().route(
                "/r",
                axum::routing::get(move || {
                    let target = target.clone();
                    async move { axum::response::Redirect::temporary(&target) }
                }),
            );
            let origin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin_port = origin_listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(origin_listener, origin_app).await.unwrap() });

            let client = Client::new();
            let budgets = client.retry.budgets.clone().unwrap();
            let drained = budgets.for_host(&format!("127.0.0.1:{target_port}"));
            while drained.try_acquire(RetryKind::Transient) {}
            let before = drained.available();
            let result = client
                .get(format!("http://127.0.0.1:{origin_port}/r"))
                .send()
                .await;
            assert!(result.is_err(), "{result:?}");
            assert!(drained.available() > before, "the target got its refill");
        }

        #[tokio::test]
        async fn ten_redirects_are_followed_with_and_without_a_deadline() {
            // `/r/{n}` redirects to `/r/{n - 1}`; `/r/0` answers.
            let app = axum::Router::new().route(
                "/r/{n}",
                axum::routing::get(
                    |axum::extract::Path(n): axum::extract::Path<u32>| async move {
                        use axum::response::IntoResponse;
                        if n == 0 {
                            "done".into_response()
                        } else {
                            axum::response::Redirect::temporary(&format!("/r/{}", n - 1))
                                .into_response()
                        }
                    },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let ten = format!("http://127.0.0.1:{port}/r/10");
            let eleven = format!("http://127.0.0.1:{port}/r/11");

            let plain = Client::new().get(&ten).send().await.unwrap();
            assert_eq!(plain.status().as_u16(), 200, "as reqwest's own limit");
            let timed = with_deadline(Duration::from_secs(3), Client::new().get(&ten).send())
                .await
                .unwrap();
            assert_eq!(timed.status().as_u16(), 200);

            assert!(Client::new().get(&eleven).send().await.is_err());
            assert!(
                with_deadline(Duration::from_secs(3), Client::new().get(&eleven).send())
                    .await
                    .is_err()
            );
        }

        #[tokio::test]
        async fn a_caller_deadline_header_is_capped_at_the_time_left() {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let record = Arc::clone(&seen);
            let app = axum::Router::new().route(
                "/x",
                axum::routing::get(move |headers: HeaderMap| async move {
                    let values: Vec<String> = headers
                        .get_all(DEADLINE_HEADER)
                        .iter()
                        .map(|value| value.to_str().unwrap().to_owned())
                        .collect();
                    record.lock().unwrap().push(values);
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = format!("http://127.0.0.1:{port}/x");

            // A larger caller value is cut to the time left.
            let send = Client::new()
                .get(&url)
                .header(DEADLINE_HEADER, "5000")
                .send();
            with_deadline(Duration::from_secs(1), send).await.unwrap();
            // A smaller caller value is kept.
            let send = Client::new()
                .get(&url)
                .header(DEADLINE_HEADER, "200")
                .send();
            with_deadline(Duration::from_secs(3), send).await.unwrap();
            // With no deadline, the caller's value goes as is.
            Client::new()
                .get(&url)
                .header(DEADLINE_HEADER, "5000")
                .send()
                .await
                .unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 3);
            assert_eq!(seen[0].len(), 1, "one header: {seen:?}");
            assert!(seen[0][0].parse::<u64>().unwrap() <= 1_000, "{seen:?}");
            assert_eq!(seen[1], vec!["200".to_owned()]);
            assert_eq!(seen[2], vec!["5000".to_owned()]);
        }

        #[tokio::test]
        async fn plain_path_follows_a_redirect_without_a_deadline() {
            let (url, seen) = redirecting(Duration::ZERO).await;
            let response = Client::new().get(&url).send().await.unwrap();
            assert_eq!(response.status().as_u16(), 200);
            assert_eq!(seen.lock().unwrap().len(), 2);
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn plain_path_stops_a_hanging_call_at_the_deadline() {
            let _lock = lock();
            let (url, hits) = counting(None, &[]).await;
            let result =
                with_deadline(Duration::from_millis(300), Client::new().get(&url).send()).await;
            assert!(
                matches!(result, Err(ClientError::DeadlineExceeded)),
                "{result:?}"
            );
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "no retry after the deadline"
            );
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn plain_path_does_not_start_a_retry_that_cannot_fit() {
            let _lock = lock();
            let (url, hits) = counting(Some(503), &[]).await;
            // 50 retries with jittered backoff need far more than 700 ms; the
            // deadline stops them, and the last 503 comes back.
            let response = with_deadline(
                Duration::from_millis(700),
                Client::new().get(&url).retries(50).send(),
            )
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 503);
            let made = hits.load(Ordering::SeqCst);
            assert!(
                (2..51).contains(&made),
                "retries stop at the deadline: {made}"
            );
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn plain_path_skips_a_retry_after_wait_past_the_deadline() {
            let _lock = lock();
            let (url, hits) = counting(Some(429), &[("retry-after", "10")]).await;
            let response = with_deadline(Duration::from_secs(2), Client::new().get(&url).send())
                .await
                .unwrap();
            assert_eq!(
                response.status().as_u16(),
                429,
                "the 429 comes back at once"
            );
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn plain_path_obeys_the_retry_budget() {
            let _lock = lock();
            let (url, hits) = counting(Some(503), &[]).await;
            let client = one_retry_client();
            let first = client.get(&url).send().await.unwrap();
            assert_eq!(first.status().as_u16(), 503);
            assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry from the budget");
            let second = client.get(&url).send().await.unwrap();
            assert_eq!(second.status().as_u16(), 503);
            assert_eq!(
                hits.load(Ordering::SeqCst),
                3,
                "the empty budget blocks retries"
            );
        }

        #[tokio::test]
        async fn custom_path_obeys_the_deadline_and_the_budget() {
            let (url, hits) = counting(Some(503), &[]).await;
            let response = with_deadline(
                Duration::from_millis(700),
                Client::new().get(&url).retries(50).no_redirect().send(),
            )
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 503);
            let made = hits.load(Ordering::SeqCst);
            assert!(
                (2..51).contains(&made),
                "retries stop at the deadline: {made}"
            );

            let client = one_retry_client();
            client.get(&url).no_redirect().send().await.unwrap();
            assert_eq!(
                hits.load(Ordering::SeqCst),
                made + 2,
                "one retry from the budget"
            );
            client.get(&url).no_redirect().send().await.unwrap();
            assert_eq!(
                hits.load(Ordering::SeqCst),
                made + 3,
                "the empty budget blocks retries"
            );
        }

        #[tokio::test]
        async fn custom_path_reports_a_deadline_timeout_as_deadline_exceeded() {
            let (url, hits) = counting(None, &[]).await;
            let result = with_deadline(
                Duration::from_millis(300),
                Client::new().get(&url).no_redirect().send(),
            )
            .await;
            assert!(
                matches!(result, Err(ClientError::DeadlineExceeded)),
                "{result:?}"
            );
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "no retry after the deadline"
            );
        }

        /// A server whose response head arrives at once and whose body never
        /// ends.
        async fn stalled_body() -> String {
            let app = axum::Router::new().route(
                "/x",
                axum::routing::get(|| async {
                    let stream = futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
                    axum::response::Response::new(axum::body::Body::from_stream(stream))
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://127.0.0.1:{}/x", addr.port())
        }

        #[tokio::test]
        #[allow(clippy::await_holding_lock)]
        async fn a_body_stalled_past_the_deadline_is_deadline_exceeded() {
            let _lock = lock();
            let url = stalled_body().await;
            let plain =
                with_deadline(Duration::from_millis(300), Client::new().get(&url).send()).await;
            assert!(
                matches!(plain, Err(ClientError::DeadlineExceeded)),
                "{plain:?}"
            );
            let custom = with_deadline(
                Duration::from_millis(300),
                Client::new().get(&url).no_redirect().send(),
            )
            .await;
            assert!(
                matches!(custom, Err(ClientError::DeadlineExceeded)),
                "{custom:?}"
            );
        }

        #[tokio::test]
        async fn ssrf_safe_path_fails_at_once_when_the_deadline_has_passed() {
            let result = Deadline::at(tokio::time::Instant::now())
                .scope(Client::new().get_ssrf_safe("https://example.com/").send())
                .await;
            assert!(
                matches!(result, Err(ClientError::DeadlineExceeded)),
                "{result:?}"
            );
        }

        #[test]
        fn a_redirect_hop_uses_the_budget_of_its_own_host() {
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let origin = RetryGate::with_deadline(
                None,
                Some(Arc::clone(&budgets)),
                Some("origin:443"),
                true,
            );
            let hop = origin.for_hop("https://target/x", &mut std::collections::HashSet::new());
            assert!(Arc::ptr_eq(
                hop.budget.as_ref().unwrap(),
                &budgets.for_host("target:443")
            ));
            assert!(!Arc::ptr_eq(
                hop.budget.as_ref().unwrap(),
                origin.budget.as_ref().unwrap()
            ));
        }

        #[test]
        fn a_redirect_chain_refills_each_host_once() {
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let origin_budget = budgets.for_host("a:443");
            let target_budget = budgets.for_host("b:443");
            while origin_budget.try_acquire(RetryKind::Transient) {}
            while target_budget.try_acquire(RetryKind::Transient) {}
            let origin =
                RetryGate::with_deadline(None, Some(Arc::clone(&budgets)), Some("a:443"), true);
            let mut refilled = std::collections::HashSet::from(["a:443".to_owned()]);
            // a -> b -> a -> b
            origin.check().unwrap();
            let after_first = origin_budget.available();
            let before_b = target_budget.available();
            origin
                .for_hop("https://b/1", &mut refilled)
                .check()
                .unwrap();
            let after_b = target_budget.available();
            assert!(after_b > before_b, "the first visit to b refills it");
            origin
                .for_hop("https://a/2", &mut refilled)
                .check()
                .unwrap();
            origin
                .for_hop("https://b/3", &mut refilled)
                .check()
                .unwrap();
            assert!(
                (origin_budget.available() - after_first).abs() < f64::EPSILON,
                "the origin is not credited again"
            );
            assert!(
                (target_budget.available() - after_b).abs() < f64::EPSILON,
                "the target is credited once"
            );
        }

        #[test]
        fn rekey_moves_to_the_budget_of_the_answering_host() {
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let mut gate = RetryGate::with_deadline(
                None,
                Some(Arc::clone(&budgets)),
                Some("origin:443"),
                true,
            );
            let origin = Arc::clone(gate.budget.as_ref().unwrap());
            gate.rekey("https://origin/x");
            assert!(
                Arc::ptr_eq(gate.budget.as_ref().unwrap(), &origin),
                "same host"
            );
            gate.rekey("https://target/x");
            assert!(Arc::ptr_eq(
                gate.budget.as_ref().unwrap(),
                &budgets.for_host("target:443")
            ));
        }

        #[test]
        fn a_redirected_answer_refills_the_destination_budget() {
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let target = budgets.for_host("target:443");
            while target.try_acquire(RetryKind::Transient) {}
            let gate = RetryGate::with_deadline(
                None,
                Some(Arc::clone(&budgets)),
                Some("origin:443"),
                true,
            );
            let origin = Arc::clone(gate.budget.as_ref().unwrap());
            let (before, origin_before) = (target.available(), origin.available());

            gate.record_destination("https://origin/x");
            assert!(
                (target.available() - before).abs() < f64::EPSILON,
                "same host"
            );

            gate.record_destination("https://target/x");
            assert!(target.available() > before, "the destination is refilled");
            assert!(
                (origin.available() - origin_before).abs() < f64::EPSILON,
                "the origin budget is unchanged"
            );
            assert!(
                Arc::ptr_eq(gate.budget.as_ref().unwrap(), &origin),
                "no rekey"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn an_expired_deadline_reclassifies_a_hop_error() {
            let gate = Deadline::at(tokio::time::Instant::now())
                .scope(async { RetryGate::start(None, None, true) })
                .await;
            let error = gate.classify(ClientError::InvalidUrl("dns stalled".into()));
            assert!(matches!(error, ClientError::DeadlineExceeded));
            let open = RetryGate::start(None, None, true);
            let error = open.classify(ClientError::InvalidUrl("dns".into()));
            assert!(matches!(error, ClientError::InvalidUrl(_)));
        }

        #[tokio::test]
        async fn a_rejected_ssrf_safe_host_is_not_refilled() {
            let client = Client::new();
            let budget = client
                .retry
                .budgets
                .clone()
                .unwrap()
                .for_host("127.0.0.1:80");
            while budget.try_acquire(RetryKind::Transient) {}
            let empty = budget.available();
            // Loopback is refused before any HTTP attempt.
            let result = client.get_ssrf_safe("http://127.0.0.1/").send().await;
            assert!(result.is_err(), "{result:?}");
            assert!(
                (budget.available() - empty).abs() < f64::EPSILON,
                "no refill without an attempt: {}",
                budget.available()
            );
        }

        #[tokio::test(start_paused = true)]
        async fn a_followed_redirect_host_is_refilled_after_the_deadline() {
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let target = budgets.for_host("t:80");
            while target.try_acquire(RetryKind::Transient) {}
            let empty = target.available();
            let gate = RetryGate::with_deadline(
                Some(Deadline::after(Duration::from_millis(1))),
                Some(budgets),
                Some("o:80"),
                true,
            );
            // The redirect was followed, then the deadline passed.
            tokio::time::advance(Duration::from_millis(5)).await;
            let mut seen = std::collections::HashSet::from(["o:80".to_owned()]);
            gate.record_destinations(&["http://t/".to_owned()], &mut seen);
            assert!(
                target.available() > empty,
                "the target's attempt started, so it is refilled"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn a_request_past_its_deadline_does_not_refill_the_budget() {
            let budgets = Arc::new(RetryBudgets::new(&RetryBudgetConfig::default()));
            let budget = budgets.for_host("h:80");
            while budget.try_acquire(RetryKind::Transient) {}
            let empty = budget.available();
            let late = RetryGate::with_deadline(
                Some(Deadline::at(tokio::time::Instant::now())),
                Some(Arc::clone(&budgets)),
                Some("h:80"),
                true,
            );
            assert!(late.check().is_err());
            assert!(
                (budget.available() - empty).abs() < f64::EPSILON,
                "no refill"
            );

            // Live when built, expired before the first attempt.
            let short = RetryGate::with_deadline(
                Some(Deadline::after(Duration::from_millis(1))),
                Some(Arc::clone(&budgets)),
                Some("h:80"),
                true,
            );
            tokio::time::advance(Duration::from_millis(5)).await;
            assert!(short.check().is_err());
            assert!(
                (budget.available() - empty).abs() < f64::EPSILON,
                "no refill without an attempt"
            );

            let live = RetryGate::with_deadline(None, Some(budgets), Some("h:80"), true);
            assert!(
                (budget.available() - empty).abs() < f64::EPSILON,
                "not when built"
            );
            live.check().unwrap();
            let once = budget.available();
            assert!(once > empty, "a live request refills at its first attempt");
            live.check().unwrap();
            assert!((budget.available() - once).abs() < f64::EPSILON, "once");
        }

        #[test]
        fn budget_keys_keep_the_port() {
            assert_eq!(url_host("http://svc:8001/a").as_deref(), Some("svc:8001"));
            assert_eq!(url_host("https://svc/a").as_deref(), Some("svc:443"));
            assert_eq!(url_host("http://svc/a").as_deref(), Some("svc:80"));
            assert_eq!(url_host("/relative"), None);
        }

        #[test]
        fn a_retry_ending_in_a_client_or_server_error_keeps_its_tokens_spent() {
            let budget = Arc::new(RetryBudget::new(&RetryBudgetConfig::default()));
            let gate = RetryGate {
                deadline: None,
                budgets: None,
                budget: Some(Arc::clone(&budget)),
                host: None,
                send_header: true,
                refill_pending: AtomicBool::new(false),
                pending_retry: std::sync::Mutex::new(None),
                caller_deadline: std::sync::OnceLock::new(),
            };
            assert!(gate.allow(RetryKind::Transient, Duration::ZERO));
            // The retry starts.
            gate.check().unwrap();
            gate.finish(Some(RetryKind::Transient), 500);
            assert!((budget.available() - 486.0).abs() < f64::EPSILON);
            gate.finish(Some(RetryKind::Transient), 200);
            assert!((budget.available() - 500.0).abs() < f64::EPSILON);
        }
    }
}
