//! Outbound HTTP for a sandboxed plugin, bounded by a declared host list
//! (issue #1632).
//!
//! The manifest names hostnames; anything else is denied and recorded with the
//! plugin's name and the destination it wanted. That list is compared by **exact
//! equality**, and this module exists mostly to make sure it stays that way.
//!
//! # Every looser comparison is a bypass
//!
//! | Written as | Accepts |
//! | --- | --- |
//! | `url.contains("api.example.com")` | `https://attacker.test/?x=api.example.com` |
//! | `url.starts_with("https://api.example.com")` | `https://api.example.com.attacker.test/` |
//! | `host.ends_with("api.example.com")` | `evil-api.example.com` |
//! | `host == granted`, host parsed loosely | `https://api.example.com@attacker.test/` |
//!
//! The last one is the interesting case: `api.example.com@attacker.test` is a
//! host of `attacker.test` and userinfo of `api.example.com`, and a parser that
//! takes everything before the first `/` after the scheme reads it the other way
//! round. So [`host_of`] refuses userinfo outright rather than parsing past it —
//! a plugin has no use for it, and refusing is a rule with no edge cases.
//!
//! # Redirects are the allow-list's real escape hatch
//!
//! Checking the URL the guest wrote bounds *the first hop*. A granted
//! `api.example.com` that answers `302 Location: https://attacker.test/collect`
//! sends the request — body and all — somewhere the manifest never named, and a
//! host that only inspected the outgoing URL would record one allowed call to
//! the granted host and nothing else. Most HTTP clients follow redirects by
//! default, so the natural wiring is the vulnerable one.
//!
//! Two things close it, and it needs both:
//!
//! * every [`OutboundRequest`] carries [`allowed_hosts`](OutboundRequest::allowed_hosts)
//!   and [`follow_redirects: false`](OutboundRequest::follow_redirects), so an
//!   implementation has what it needs to be correct without re-deriving the
//!   grant; and
//! * every [`OutboundResponse`] must report the
//!   [`final_url`](OutboundResponse::final_url) it actually fetched, and the
//!   host **re-checks that against the grant** before a byte reaches the guest.
//!
//! The second is what makes the first more than a comment: an implementation
//! that follows a redirect anyway must either report where it ended up — and be
//! denied — or lie, and a backend that lies is host-side code the operator
//! wrote, which is the same trust boundary as the database driver.
//!
//! IP-range (SSRF) guarding for the app-level client is #1627's, not this
//! module's; the allow-list here is a *name* allow-list and says so.

use std::sync::Arc;

use super::{CallResult, CallValue, CapabilityCall, CapabilityRuntime, DenialReason};

/// The most headers a guest may set on one outbound request.
pub const MAX_OUTBOUND_HEADERS: usize = 16;

/// The most bytes the *response* headers of one outbound call may carry.
///
/// The allow-list says which headers come back; it says nothing about how big
/// they are, and every one of them is chosen by an upstream rather than by the
/// plugin or the host. Comfortably under the reply-queue ceiling, so a response
/// that is otherwise legal cannot be made to fail the request by its headers
/// alone.
pub const MAX_RESPONSE_HEADER_BYTES: usize = 8 * 1024;

/// Request headers a sandboxed plugin may set on an outbound call.
///
/// An allow-list, like every other header list in this subsystem. The ones that
/// are absent are absent on purpose: `Host` re-points the request past the
/// allow-list at the connection layer, `Cookie` and `Authorization` would carry
/// credentials the sandbox never gave the plugin in the first place, and
/// `Forwarded`/`X-Forwarded-*` let a plugin forge the provenance of a call the
/// host is making on its behalf.
pub const ALLOWED_OUTBOUND_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "content-type",
    "idempotency-key",
    "if-match",
    "if-none-match",
    "user-agent",
];

/// Response headers that come back to the guest.
pub const ALLOWED_OUTBOUND_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "etag",
    "last-modified",
    "location",
    "retry-after",
];

/// Methods a plugin may use outbound.
const ALLOWED_OUTBOUND_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// One outbound call, already checked against the grant list.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OutboundRequest {
    /// The plugin making the call, for the upstream's logs and for ours.
    pub plugin: String,
    /// Upper-case HTTP method.
    pub method: String,
    /// Absolute URL whose host appears in `[grants].hosts`.
    pub url: String,
    /// Allow-listed request headers.
    pub headers: Vec<(String, String)>,
    /// Request body.
    pub body: String,
    /// The largest response body this call may return, from the
    /// `outbound_response_bytes` quota.
    ///
    /// Carried into the request rather than only checked on the way out, so an
    /// implementation can bound its *read* instead of buffering a hostile
    /// upstream's gigabyte and then being told it was too big. The host checks
    /// the answer against it either way — a backend an embedder wrote is not
    /// where this bound may be missing.
    pub max_response_bytes: usize,
    /// The most response headers this call may return, and the most bytes they
    /// may carry between them.
    ///
    /// Carried for the same reason `max_response_bytes` is, and it was missing
    /// for the same reason it would have been easy to leave missing: the
    /// allow-list says *which* headers come back and nothing about how many or
    /// how large, and every one of them is the upstream's choice. Bounding only
    /// the vector the host rebuilds afterwards bounds the reply and not the
    /// allocation — by then the backend has already materialised whatever the
    /// upstream sent. An implementation MUST stop reading headers once either
    /// ceiling is reached. The host re-applies both to what comes back, because
    /// a backend an embedder wrote is not where this bound may be missing.
    pub max_response_headers: usize,
    /// See [`max_response_headers`](Self::max_response_headers).
    pub max_response_header_bytes: usize,
    /// Every hostname this plugin was granted.
    ///
    /// The host has already checked `url` against it. It is carried anyway so an
    /// implementation that must make a per-hop decision — a redirect policy, a
    /// connection-level check — can do so without being handed the manifest.
    pub allowed_hosts: Vec<String>,
    /// Always `false`, and the field exists to say so where an implementation
    /// will see it.
    ///
    /// Following a redirect is how the allow-list is escaped; see the module
    /// header. A client that defaults to following must be configured off for
    /// this call.
    pub follow_redirects: bool,
    /// How long this call may take before the implementation must give up.
    ///
    /// Fuel bounds the guest's instructions and cannot bound a socket that
    /// never answers. The call runs on a blocking worker holding the plugin's
    /// concurrency permit, so an implementation that waits forever shuts the
    /// plugin's prefix and degrades the shared blocking pool — this is the
    /// ceiling that stops it, from the `outbound_timeout_ms` quota.
    pub timeout: std::time::Duration,
}

/// What an upstream said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundResponse {
    /// Status code.
    pub status: u16,
    /// Allow-listed response headers.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: String,
    /// The URL the bytes actually came from.
    ///
    /// Required, not optional, and re-checked by the host against the grant
    /// before the guest sees anything. For an implementation that honours
    /// [`follow_redirects: false`](OutboundRequest::follow_redirects) this is
    /// always the request's own URL and reporting it costs a clone; for one that
    /// followed a redirect it is the only thing standing between a granted host
    /// and an ungranted one.
    pub final_url: String,
}

impl OutboundResponse {
    /// A response that came from the URL it was asked for.
    ///
    /// The shape an implementation that does not follow redirects always wants,
    /// so honouring the contract is the shorter thing to write.
    #[must_use]
    pub fn from_url(url: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            final_url: url.into(),
        }
    }
}

/// Something that can make an outbound call on a plugin's behalf.
///
/// Synchronous, because the sandbox interpreter is: `SandboxHost::run` is
/// already dispatched to a blocking worker, and an implementation that bridges
/// to the async framework client does so there rather than making every caller
/// of this trait async.
pub trait OutboundHttp: Send + Sync + 'static {
    /// Perform the call.
    ///
    /// An implementation carries two obligations the sandbox cannot enforce from
    /// the outside, and both are load-bearing:
    ///
    /// 1. **Do not follow redirects.** [`OutboundRequest::follow_redirects`] is
    ///    always `false`; a 3xx must come back as a 3xx. Following one sends the
    ///    request to a host the manifest never granted. Most clients follow by
    ///    default — `reqwest` follows up to ten hops unless told otherwise — so
    ///    this is a thing to configure, not a thing to assume.
    /// 2. **Report [`final_url`](OutboundResponse::final_url) honestly.** The
    ///    host re-checks it against the grant and denies the call if it is not a
    ///    granted host, which is what turns obligation 1 from a comment into a
    ///    check.
    ///
    /// It must also respect [`OutboundRequest::timeout`] — this runs on a
    /// blocking worker holding the plugin's concurrency permit, and fuel cannot
    /// bound a socket that never answers — and must not read past
    /// [`OutboundRequest::max_response_bytes`], which the host checks on the way
    /// out but which an implementation should bound on the way in rather than
    /// buffering a hostile upstream's gigabyte first.
    ///
    /// # Errors
    ///
    /// One line for the guest and the audit ledger. It must not carry anything
    /// the plugin was not already entitled to know.
    fn fetch(&self, request: OutboundRequest) -> Result<OutboundResponse, String>;
}

/// The host of an absolute `http`/`https` URL, lower-cased, or `None`.
///
/// Refuses anything an allow-list comparison could not be trusted against:
/// a relative URL, a scheme other than `http`/`https`, userinfo, an empty host,
/// an IPv6 literal (an address is not a name, so a *name* allow-list has
/// nothing to say about one), and a host that is not a plain DNS name.
///
/// A port is allowed and stripped: `api.example.com:8443` is the granted host on
/// a different port, and the grant is about *where the bytes go*, which the name
/// decides.
#[must_use]
pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    // Everything up to the first `/`, `?` or `#` is the authority.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    // Userinfo is refused rather than stripped: `https://api.example.com@evil`
    // is a URL whose host is `evil`, and every allow-list bypass in the header
    // of this module is a variation on reading it the other way round. A plugin
    // has no use for userinfo, so refusing is a rule with no edge cases.
    if authority.contains('@') {
        return None;
    }
    // IPv6 literals carry colons, which the port split below would mangle. They
    // are refused outright: a literal address is not a name, so a name
    // allow-list can neither grant nor deny one honestly.
    if authority.contains('[') || authority.contains(']') {
        return None;
    }
    let mut parts = authority.split(':');
    let host = parts.next()?;
    if let Some(port) = parts.next() {
        // Exactly one colon, and digits after it. `a:b:c` is not a host.
        if parts.next().is_some() || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let host = host.to_ascii_lowercase();
    // The same shape the grant list is validated against, so a URL can never
    // name something no grant could have named — including a trailing dot,
    // which resolves identically and would compare unequal.
    super::super::grants::is_grantable_host(&host).then_some(host)
}

/// The authority a URL reached for, however malformed, for the audit ledger.
///
/// [`host_of`] answers "may this be called", and its `None` is the whole point
/// of the allow-list. This answers the different question the operator surface
/// asks — "what did it reach for" — so it parses as far as it can and hands back
/// what it found rather than a placeholder. It is never used for a grant
/// decision, and the caller bounds and escapes it like any other guest string.
#[must_use]
pub fn attempted_authority(url: &str) -> String {
    let rest = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .trim_start_matches('/');
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Userinfo is the case that matters: `api.example.com@attacker.test` must
    // record `attacker.test`, which is where a browser or a client would go.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        url.to_owned()
    } else {
        authority.to_ascii_lowercase()
    }
}

/// Answer one `http-fetch`. Capability, scope and quota are already checked.
#[allow(
    clippy::too_many_lines,
    reason = "the request is assembled, sent and re-checked in one pass; splitting it would put \
              the redirect re-check somewhere the assembly could be changed without it"
)]
pub(super) fn perform(
    runtime: &CapabilityRuntime,
    call: &CapabilityCall,
    host: &str,
) -> CallResult {
    let id = call.id();
    let CapabilityCall::HttpFetch {
        method,
        url,
        headers,
        body,
        ..
    } = call
    else {
        return CallResult::denied(id, DenialReason::Malformed, "not an outbound call");
    };
    let Some(client) = runtime.services.http.clone() else {
        return CallResult::denied(
            id,
            DenialReason::Unavailable,
            "this host has no outbound HTTP backend wired for sandboxed plugins",
        );
    };

    let method = method.to_ascii_uppercase();
    if !ALLOWED_OUTBOUND_METHODS.contains(&method.as_str()) {
        return CallResult::denied(
            id,
            DenialReason::Malformed,
            format!(
                "method {method:?} is not one of {allowed}",
                method = super::super::manifest::rejected(&method),
                allowed = ALLOWED_OUTBOUND_METHODS.join(", ")
            ),
        );
    }
    if headers.len() > MAX_OUTBOUND_HEADERS {
        return CallResult::denied(
            id,
            DenialReason::Malformed,
            format!("at most {MAX_OUTBOUND_HEADERS} outbound request headers"),
        );
    }
    let mut allowed = Vec::with_capacity(headers.len().min(MAX_OUTBOUND_HEADERS));
    for (name, value) in headers {
        if !ALLOWED_OUTBOUND_REQUEST_HEADERS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(name))
        {
            return CallResult::denied(
                id,
                DenialReason::NotInGrant,
                format!(
                    "outbound request header {name:?} is not one a sandboxed plugin may set",
                    name = super::super::manifest::rejected(name)
                ),
            );
        }
        // A CR or LF in a value splits one request into two at the wire, which
        // is a way to reach a path — or a host — the allow-list never saw.
        if value.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
            return CallResult::denied(
                id,
                DenialReason::Malformed,
                "an outbound header value may not carry CR or LF",
            );
        }
        allowed.push((name.to_ascii_lowercase(), value.clone()));
    }

    let ceiling = runtime.quotas().outbound_response_bytes as usize;
    let request = OutboundRequest {
        plugin: runtime.plugin.clone(),
        method,
        url: url.clone(),
        headers: allowed,
        body: body.clone(),
        max_response_bytes: ceiling,
        max_response_headers: MAX_OUTBOUND_HEADERS,
        max_response_header_bytes: MAX_RESPONSE_HEADER_BYTES,
        allowed_hosts: runtime
            .grants
            .list_for(super::super::manifest::SandboxCapability::HttpOutbound)
            .unwrap_or_default()
            .to_vec(),
        follow_redirects: false,
        timeout: std::time::Duration::from_millis(u64::from(runtime.quotas().outbound_timeout_ms)),
    };
    match client.fetch(request) {
        Ok(response) => {
            // Where the bytes actually came from, re-checked against the grant.
            // An implementation that followed a redirect must say so here, and
            // this is where saying so is refused — see the module header on why
            // checking only the outgoing URL bounds the first hop and nothing
            // else.
            let landed = host_of(&response.final_url);
            if landed.as_deref() != Some(host) {
                return CallResult::denied(
                    id,
                    DenialReason::NotInGrant,
                    format!(
                        "the call to {host} returned bytes from {landed}, which \
                         `[grants].hosts` does not name; a redirect is not followed on a \
                         plugin's behalf",
                        landed = super::super::manifest::rejected(
                            landed.as_deref().unwrap_or(&response.final_url)
                        )
                    ),
                );
            }
            if response.body.len() > ceiling {
                // `ResponseTooLarge` rather than `QuotaExceeded`: the request
                // *left* and the upstream answered, so the audit surface must
                // count this as a host that was called. A quota denial means
                // nothing reached the network.
                return CallResult::denied(
                    id,
                    DenialReason::ResponseTooLarge,
                    format!(
                        "{host} answered {found} bytes, over the {ceiling}-byte \
                         `outbound_response_bytes` quota",
                        found = response.body.len()
                    ),
                );
            }
            // Headers are bounded here, not only by the allow-list. The
            // list says *which* headers pass; it says nothing about how many
            // or how long, and an upstream the plugin was granted can answer
            // with a body inside `outbound_response_bytes` and megabytes of
            // `etag`. The reply is serialized in full before the queue ceiling
            // is checked, so an unbounded collect here is an upstream's way to
            // make the host allocate and then fail the plugin's request.
            let mut headers = Vec::new();
            let mut header_bytes = 0_usize;
            for (name, value) in response.headers {
                if !ALLOWED_OUTBOUND_RESPONSE_HEADERS
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(&name))
                {
                    continue;
                }
                if headers.len() >= MAX_OUTBOUND_HEADERS {
                    break;
                }
                let weight = name.len().saturating_add(value.len());
                if header_bytes.saturating_add(weight) > MAX_RESPONSE_HEADER_BYTES {
                    break;
                }
                header_bytes = header_bytes.saturating_add(weight);
                headers.push((name, value));
            }
            CallResult::Ok {
                id,
                value: CallValue::Http {
                    status: response.status,
                    headers,
                    body: response.body,
                },
            }
        }
        Err(detail) => CallResult::denied(id, DenialReason::BackendError, detail),
    }
}

// ── A recording client ───────────────────────────────────────────────────

/// An [`OutboundHttp`] that answers from a fixed table and records what it was
/// asked.
///
/// The containment property this module claims is about which requests *leave*,
/// so proving it needs somewhere to observe that — and observing it must not
/// require a network, or the adversarial corpus would be a suite nobody runs.
#[derive(Debug, Default)]
pub struct RecordingHttp {
    answers: std::sync::Mutex<Vec<(String, OutboundResponse)>>,
    seen: std::sync::Mutex<Vec<OutboundRequest>>,
}

impl RecordingHttp {
    /// A client that answers nothing.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Answer `url` with `response`.
    ///
    /// The caller sets `final_url`; [`OutboundResponse::from_url`] is the
    /// honest shape, and [`answer_from`](Self::answer_from) the dishonest one.
    pub fn answer(&self, url: impl Into<String>, response: OutboundResponse) {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((url.into(), response));
    }

    /// Every request that actually left.
    #[must_use]
    pub fn seen(&self) -> Vec<OutboundRequest> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl RecordingHttp {
    /// Answer `url` as though the upstream had redirected to `elsewhere`.
    ///
    /// The one thing a test double must be able to do that a well-behaved
    /// client will not: prove that the host re-checks where the bytes came
    /// from rather than trusting the URL it sent.
    pub fn answer_from(&self, url: impl Into<String>, elsewhere: impl Into<String>) {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                url.into(),
                OutboundResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: "redirected".to_owned(),
                    final_url: elsewhere.into(),
                },
            ));
    }
}

impl OutboundHttp for RecordingHttp {
    fn fetch(&self, request: OutboundRequest) -> Result<OutboundResponse, String> {
        let answer = self
            .answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(url, _)| *url == request.url)
            .map(|(_, response)| response.clone());
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        answer.ok_or_else(|| "no upstream answered".to_owned())
    }
}
