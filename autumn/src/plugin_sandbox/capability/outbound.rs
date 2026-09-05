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
//! # What the host still owns
//!
//! Redirects are not followed on the guest's behalf: a 3xx comes back as a 3xx,
//! because a redirect to a host the manifest never named is exactly the escape
//! the allow-list exists to stop, and "check the new host too" is a rule that
//! has to hold for every hop. IP-range (SSRF) guarding for the app-level client
//! is #1627's, not this module's; the allow-list here is a *name* allow-list and
//! says so.

use std::sync::Arc;

use super::{CallResult, CallValue, CapabilityCall, CapabilityRuntime, DenialReason};

/// The most headers a guest may set on one outbound request.
pub const MAX_OUTBOUND_HEADERS: usize = 16;

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

/// Answer one `http-fetch`. Capability, scope and quota are already checked.
pub(super) fn perform(
    runtime: &mut CapabilityRuntime,
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
    };
    match client.fetch(request) {
        Ok(response) => {
            if response.body.len() > ceiling {
                return CallResult::denied(
                    id,
                    DenialReason::QuotaExceeded,
                    format!(
                        "{host} answered {found} bytes, over the {ceiling}-byte \
                         `outbound_response_bytes` quota",
                        found = response.body.len()
                    ),
                );
            }
            CallResult::Ok {
                id,
                value: CallValue::Http {
                    status: response.status,
                    headers: response
                        .headers
                        .into_iter()
                        .filter(|(name, _)| {
                            ALLOWED_OUTBOUND_RESPONSE_HEADERS
                                .iter()
                                .any(|known| known.eq_ignore_ascii_case(name))
                        })
                        .collect(),
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
