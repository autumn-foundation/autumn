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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
        ClientError::SsrfBlocked(_) => Kind::SsrfBlocked,
        ClientError::TooManyRedirects(_) => Kind::TooManyRedirects,
        ClientError::RedirectRejected(_) => Kind::RedirectRejected,
        ClientError::InvalidUrl(_) => Kind::InvalidUrl,
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
        Some(Kind::SsrfBlocked) => {
            ClientError::SsrfBlocked(strip_prefix_payload(&text, "SSRF policy blocked address: "))
        }
        Some(Kind::RedirectRejected) => {
            ClientError::RedirectRejected(strip_prefix_payload(&text, "redirect rejected: "))
        }
        Some(Kind::InvalidUrl) => {
            ClientError::InvalidUrl(strip_prefix_payload(&text, "invalid or unresolvable URL: "))
        }
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
            discard_response_body: false,
            breaker_scoped: false,
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
    /// When `true`, the response body is dropped unread. See
    /// [`RequestBuilder::discard_response_body`].
    discard_response_body: bool,
    /// When `true`, `send_recorded` keeps circuit-breaker accounting even on
    /// the custom send path (`needs_custom_path()`). See
    /// [`RequestBuilder::breaker_scoped`].
    breaker_scoped: bool,
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

        // Replay serves outbound calls from the capsule's effect tape (#1634)
        // and never dials the peer. A call the tape cannot answer is a
        // divergence, already logged by `next_http`, and fails closed here.
        //
        // The unconditional block below stays as the backstop for a replay
        // process whose current task carries no tape — app boot, a state
        // initializer, work the handler spawned — because a capsule records no
        // response for any of those either. The check is here rather than on
        // the client because a handler can build one any way it likes
        // (`Client::new()`, `from_state`, a stored one) and every path must be
        // closed.
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
    async fn send_recorded(self) -> Result<Response, ClientError> {
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
            return self.send_custom().await;
        }

        // ── Resilience / Circuit Breaker ──────────────────────────────────
        let breaker = breaker_for_url(self.resilience_config.as_ref(), &self.url);

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

    /// The custom send path (`send_custom`), with breaker accounting exactly
    /// like the plain-path breaker block above: `before_call` gate,
    /// `CircuitBreakerGuard` covering the call (its `Drop` releases a
    /// half-open slot if this future is cancelled or panics before finishing),
    /// `< 500` success threshold. Only reached when `breaker_scoped()` was
    /// set — see its doc comment for why this has to live here rather than in
    /// an external wrapper around `send()`.
    async fn send_custom_breaker_guarded(self) -> Result<Response, ClientError> {
        let breaker = breaker_for_url(self.resilience_config.as_ref(), &self.url);
        if breaker.before_call().is_err() {
            return Err(ClientError::CircuitBreakerOpen);
        }
        let guard = crate::circuit_breaker::CircuitBreakerGuard::new(breaker);
        let res = self.send_custom().await;
        match &res {
            Ok(resp) if resp.status().as_u16() < 500 => guard.success(),
            _ => guard.failure(),
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

                    let body = if self.discard_response_body {
                        // Dropped unread — see `discard_response_body`.
                        Bytes::new()
                    } else {
                        resp.bytes()
                            .await
                            .map_err(|e| ClientError::Request(e.without_url()))?
                    };
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

        // Reject `pin_to` on an IP-literal URL host up front — deterministically,
        // before any network I/O. reqwest/hyper treat an IP-literal host as
        // already-resolved and do NOT consult the DNS resolver, so the
        // `resolve_to_addrs` override installed by `build_oneshot_client` (which
        // is what enforces the pin) is skipped and the socket connects to the
        // literal in the URL rather than the pinned address — silently violating
        // `pin_to`'s documented guarantee. `get_ssrf_safe` never sets `pin_addr`
        // (and validates+connects to the same literal, so it stays safe), so this
        // guard targets only the explicit `pin_to` primitive.
        if self.pin_addr.is_some() && url_host_is_ip_literal(&self.url)? {
            return Err(ClientError::PinRequiresDomainHost(
                "pin_to cannot be honored for an IP-literal URL host because the \
                 HTTP stack connects to the literal directly and skips the pinned \
                 address; put the desired IP directly in the URL, or use a domain host.",
            ));
        }

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
            self.discard_response_body,
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
        timeout: Duration,
    ) -> Result<Response, ClientError> {
        let original =
            url::Url::parse(&self.url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        let mut current = self.url.clone();
        // Threaded across hops so cross-origin header stripping (Fix A) and
        // RFC method/body rewriting (Fix B) accumulate correctly.
        let mut method = self.method.clone();
        let mut headers = self.extra_headers.clone();
        let mut body = self.body.clone();
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
            let client = build_oneshot_client(resolve, reqwest::redirect::Policy::none(), timeout)?;
            let resp = send_one(
                &client,
                &method,
                &current,
                &headers,
                body.as_ref(),
                &self.retry_policy,
                self.discard_response_body,
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
            // RFC 7231/7538 method+body rewriting before the next hop.
            rewrite_after_redirect(resp.status(), &mut method, &mut body, &mut headers);
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
    async fn send_ssrf_safe(self, timeout: Duration) -> Result<Response, ClientError> {
        let (follow, max) = self.ssrf_redirect_plan();
        let original =
            url::Url::parse(&self.url).map_err(|e| ClientError::InvalidUrl(e.to_string()))?;
        let mut current = self.url.clone();
        // Threaded across hops so cross-origin header stripping (Fix A) and
        // RFC method/body rewriting (Fix B) accumulate correctly.
        let mut method = self.method.clone();
        let mut headers = self.extra_headers.clone();
        let mut body = self.body.clone();
        for hop in 0.. {
            // Resolve host → ALL validated addresses (rejects if ANY resolved IP
            // is blocked), then pin the full set so reqwest cannot re-resolve but
            // can still fall back across the validated addresses in order.
            let addrs = resolve_and_validate(&current).await?;
            let host = host_of(&current)?;
            let client = build_oneshot_client(
                Some((host, addrs)),
                reqwest::redirect::Policy::none(),
                timeout,
            )?;
            // On any post-origin hop, drop credential-bearing headers if the
            // current target is cross-origin (stays stripped once stripped).
            if hop > 0 {
                strip_sensitive_headers_if_cross_origin(&mut headers, &original, &current)?;
            }
            let resp = send_one(
                &client,
                &method,
                &current,
                &headers,
                body.as_ref(),
                &self.retry_policy,
                self.discard_response_body,
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

// ── Custom send-path helpers (redirect / pin / SSRF-safe) ─────────────────────

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
    discard_response_body: bool,
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

                let body = if discard_response_body {
                    // Dropped unread — see `RequestBuilder::discard_response_body`.
                    Bytes::new()
                } else {
                    resp.bytes()
                        .await
                        .map_err(|e| ClientError::Request(e.without_url()))?
                };
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
    // Only the statuses reqwest itself follows are treated as redirects. A
    // response like `304 Not Modified` (or 300/305/306) can legitimately carry
    // a `Location` header without being a followable redirect, so matching the
    // entire 300–399 range via `is_redirection()` would wrongly issue an extra
    // request instead of returning the response to the caller.
    match resp.status() {
        reqwest::StatusCode::MOVED_PERMANENTLY
        | reqwest::StatusCode::FOUND
        | reqwest::StatusCode::SEE_OTHER
        | reqwest::StatusCode::TEMPORARY_REDIRECT
        | reqwest::StatusCode::PERMANENT_REDIRECT => {}
        _ => return Ok(None),
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

/// Strip credential-bearing headers when a redirect hop crosses origins.
///
/// If `current`'s origin (scheme + host + port, per [`url::Url::origin`])
/// differs from the `original` request URL's origin, the `Authorization`,
/// `Cookie`, and `Proxy-Authorization` headers are removed from `headers` so
/// they are never forwarded to a cross-origin target (credential leak).
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
        headers.remove(reqwest::header::AUTHORIZATION);
        headers.remove(reqwest::header::COOKIE);
        headers.remove(reqwest::header::PROXY_AUTHORIZATION);
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

    // PR #2480 review, round 4 (redesign): `breaker_scoped()` moved breaker
    // accounting for the custom send path from an external wrapper
    // (`BreakerGuardedCall`, now removed) to a flag `send_recorded` itself
    // reads — which is what makes the mock bypass, the capsule-replay bypass,
    // and correct capsule *recording* of an open-breaker attempt all free:
    // they were already correctly ordered around `send_recorded` before this
    // flag existed, for every other caller.
    //
    // This test locks in the one thing the external wrapper got wrong (P1,
    // round 1): keying the breaker by the alias a caller passed to
    // `post`/`get`/etc. rather than the expanded destination. `self.url` is
    // set once, by `Client::build_request`, before any `breaker_scoped()` /
    // `ssrf_safe()` flag is even read — so there is no separate "pass the
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
            base_urls,
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
}
