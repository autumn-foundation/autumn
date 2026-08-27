//! The seam between the framework's push encoder and the network.
//!
//! Everything above this module is deterministic: given a message and a
//! subscription it produces one fully-formed [`PushRequest`] — endpoint,
//! headers (including the VAPID `Authorization`), and the RFC 8291-encrypted
//! body. A [`PushTransport`] is the only thing that touches the network.
//!
//! Splitting it out this way is what makes Web Push testable without a browser
//! or a live push service: [`RecordingPushTransport`] captures every request
//! and lets a test choose the status each endpoint answers with, so a test can
//! assert the exact bytes and headers that would have gone out, and drive the
//! `410 Gone` pruning path deterministically.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use super::PushError;

/// One fully-formed, encrypted push request, ready to be `POST`ed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    /// The push service URL to `POST` to.
    pub endpoint: String,
    /// Request headers, with lowercase names.
    pub headers: Vec<(String, String)>,
    /// The RFC 8291 `aes128gcm` body.
    pub body: Vec<u8>,
}

impl PushRequest {
    /// The value of `name` (compared case-insensitively), if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Future returned by [`PushTransport::deliver`], resolving to the push
/// service's HTTP status code.
pub type PushTransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<u16, PushError>> + Send + 'a>>;

/// Delivers an encrypted push request to a push service.
///
/// Implement this to route push traffic through your own HTTP stack, a queue,
/// or a fake. A transport reports the push service's **status code**; deciding
/// what a status means (notably that `404`/`410` prune the subscription) is
/// the framework's job, not the transport's, so every transport behaves
/// identically. Return [`PushError::Transport`] only when no status was
/// obtained at all.
pub trait PushTransport: Send + Sync + 'static {
    /// Deliver `request`, resolving to the push service's status code.
    fn deliver<'a>(&'a self, request: &'a PushRequest) -> PushTransportFuture<'a>;
}

impl<T: PushTransport + ?Sized> PushTransport for Arc<T> {
    fn deliver<'a>(&'a self, request: &'a PushRequest) -> PushTransportFuture<'a> {
        (**self).deliver(request)
    }
}

// ── Recording transport ─────────────────────────────────────────────────────

/// A [`PushTransport`] that records every request instead of sending it.
///
/// Shipped (not test-only) so applications can assert their own push
/// behaviour the same way the framework asserts its own.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::push::{PushMessage, RecordingPushTransport};
///
/// let transport = RecordingPushTransport::new()
///     // Drive the stale-subscription path deterministically.
///     .responding_with("https://push.example.com/dead", 410);
///
/// // … build a `WebPush` over `transport.clone()` and send …
///
/// let sent = transport.requests();
/// assert_eq!(sent[0].endpoint, "https://push.example.com/dead");
/// ```
///
/// Cloning shares the recorded requests, so a clone handed to the service
/// still observes what the original sees.
#[derive(Debug, Clone)]
pub struct RecordingPushTransport {
    requests: Arc<Mutex<Vec<PushRequest>>>,
    /// Per-endpoint status overrides, in insertion order.
    statuses: Arc<Mutex<Vec<(String, u16)>>>,
}

impl Default for RecordingPushTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingPushTransport {
    /// A transport that records everything and answers `201 Created` — what a
    /// push service returns when it has accepted a message.
    #[must_use]
    pub fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            statuses: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Answer `status` for requests to `endpoint`.
    ///
    /// Chain it to script a specific endpoint's behaviour — most usefully
    /// `410` (the push service reporting the subscription is gone) so the
    /// pruning path can be tested without waiting for a real expiry.
    ///
    /// # Panics
    ///
    /// If another thread panicked while holding this transport's lock.
    #[must_use]
    pub fn responding_with(self, endpoint: &str, status: u16) -> Self {
        self.statuses
            .lock()
            .expect("recording transport mutex poisoned")
            .push((endpoint.to_owned(), status));
        self
    }

    /// Every request delivered so far, in dispatch order.
    ///
    /// # Panics
    ///
    /// If another thread panicked while holding this transport's lock.
    #[must_use]
    pub fn requests(&self) -> Vec<PushRequest> {
        self.requests
            .lock()
            .expect("recording transport mutex poisoned")
            .clone()
    }
}

impl PushTransport for RecordingPushTransport {
    fn deliver<'a>(&'a self, request: &'a PushRequest) -> PushTransportFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("recording transport mutex poisoned")
                .push(request.clone());
            let status = self
                .statuses
                .lock()
                .expect("recording transport mutex poisoned")
                .iter()
                .find(|(endpoint, _)| *endpoint == request.endpoint)
                .map_or(201, |(_, status)| *status);
            Ok(status)
        })
    }
}

// ── HTTP transport ──────────────────────────────────────────────────────────

/// The default [`PushTransport`]: a real `POST` to the push service.
///
/// Uses the framework's outbound [`Client`](crate::http_client::Client) for
/// tracing context propagation and timeouts, and adds the SSRF protection a
/// push endpoint specifically needs.
///
/// # Why this does its own address validation
///
/// The endpoint is a URL the *client* chose, and this is the code that
/// connects to it. [`BrowserSubscription::decode`](super::store::BrowserSubscription::decode)
/// already refuses IP-literal and loopback endpoints at the subscribe
/// boundary, but that check cannot see where a *hostname* resolves — an
/// attacker who controls a DNS record can point `push.attacker.example` at
/// `169.254.169.254` and pass it. `Client`'s own address deny-list only runs
/// on the `get_ssrf_safe` path, which is GET-only, so this transport applies
/// the equivalent to its `POST`:
///
/// 1. resolve the host and refuse the request unless **every** resolved
///    address passes [`is_blocked_ip`](crate::http_client::is_blocked_ip);
/// 2. pin the connection to the validated address, so the socket cannot be
///    re-pointed between the check and the connect (DNS rebinding); and
/// 3. refuse to follow redirects, so a real-looking public endpoint cannot
///    `307` the body onto an internal address — reqwest's default policy
///    would replay the POST with no re-validation of the new hop.
#[cfg(feature = "http-client")]
#[derive(Clone)]
pub struct HttpPushTransport {
    client: crate::http_client::Client,
}

#[cfg(feature = "http-client")]
impl std::fmt::Debug for HttpPushTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpPushTransport").finish_non_exhaustive()
    }
}

#[cfg(feature = "http-client")]
impl HttpPushTransport {
    /// Build a transport over an existing outbound client.
    #[must_use]
    pub const fn new(client: crate::http_client::Client) -> Self {
        Self { client }
    }
}

#[cfg(feature = "http-client")]
impl PushTransport for HttpPushTransport {
    fn deliver<'a>(&'a self, request: &'a PushRequest) -> PushTransportFuture<'a> {
        Box::pin(async move {
            // Every address the host resolves to, all of them already checked
            // against the SSRF deny-list. Keeping the whole set matters: a
            // dual-stack push service on a host with no working IPv6 route —
            // or a CDN whose first answer is momentarily unreachable — would
            // otherwise fail every push despite the endpoint being perfectly
            // reachable on another address.
            let addrs = resolve_and_validate_endpoint(&request.endpoint).await?;

            let mut last_error = None;
            for addr in addrs {
                let mut builder = self
                    .client
                    .post(&request.endpoint)
                    // See the type docs: pin the checked address and refuse to
                    // follow a redirect, so neither DNS rebinding nor a `307`
                    // can steer this POST at an internal host.
                    .pin_to(addr)
                    .no_redirect();
                for (name, value) in &request.headers {
                    builder = builder.header(name, value);
                }
                // A `410 Gone` is a normal, expected answer that must reach
                // the pruning logic, so a status code is never an error here
                // — and never a reason to try another address either. Only a
                // genuine transport failure falls through to the next one.
                match builder.bytes_body(request.body.clone()).send().await {
                    Ok(response) => return Ok(response.status().as_u16()),
                    Err(e) => last_error = Some(e.to_string()),
                }
            }
            Err(PushError::Transport(last_error.unwrap_or_else(|| {
                "the push endpoint host resolved to no usable address".to_owned()
            })))
        })
    }
}

/// Resolve `endpoint`'s host and return **every** address that is safe to
/// connect to, in the resolver's order.
///
/// Every resolved address must pass the outbound client's SSRF deny-list: a
/// host that resolves to *any* blocked address is refused outright rather than
/// filtered down to its public addresses, since a resolver returning both is
/// exactly the shape a rebinding attack produces.
///
/// All of them are returned, not just the first, so the caller can fall back
/// to the next when one is unreachable — see [`HttpPushTransport`].
#[cfg(feature = "http-client")]
async fn resolve_and_validate_endpoint(
    endpoint: &str,
) -> Result<Vec<std::net::SocketAddr>, PushError> {
    let parsed = url::Url::parse(endpoint)
        .map_err(|e| PushError::InvalidEndpoint(format!("{endpoint}: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| PushError::InvalidEndpoint(format!("{endpoint}: no host")))?
        .to_owned();
    // `port_or_known_default` gives 443 for https, the only scheme
    // `validate_endpoint` lets through.
    let port = parsed.port_or_known_default().unwrap_or(443);

    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| PushError::Transport(format!("could not resolve the push endpoint: {e}")))?
        .collect();

    if let Some(blocked) = addrs
        .iter()
        .find(|addr| crate::http_client::is_blocked_ip(addr.ip()))
    {
        return Err(PushError::InvalidEndpoint(format!(
            "the push endpoint host resolves to a blocked address ({}); refusing to connect",
            blocked.ip()
        )));
    }
    if addrs.is_empty() {
        return Err(PushError::Transport(
            "the push endpoint host resolved to no addresses".to_owned(),
        ));
    }
    Ok(addrs)
}

/// A transport for a build with no HTTP client compiled in.
///
/// Reports the failure instead of pretending to deliver: a push that cannot
/// possibly go out must never look like it did.
#[cfg(not(feature = "http-client"))]
#[derive(Debug, Clone, Copy)]
pub struct UnavailablePushTransport;

#[cfg(not(feature = "http-client"))]
impl PushTransport for UnavailablePushTransport {
    fn deliver<'a>(&'a self, _request: &'a PushRequest) -> PushTransportFuture<'a> {
        Box::pin(async {
            Err(PushError::Transport(
                "no outbound HTTP client is available: autumn-web was built without the \
                 `http-client` feature. Enable it, or register your own transport."
                    .to_owned(),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(endpoint: &str) -> PushRequest {
        PushRequest {
            endpoint: endpoint.to_owned(),
            headers: vec![
                ("authorization".to_owned(), "vapid t=x, k=y".to_owned()),
                ("content-encoding".to_owned(), "aes128gcm".to_owned()),
            ],
            body: vec![1, 2, 3],
        }
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        // Header names are case-insensitive on the wire, and a test asserting
        // on `Authorization` must not fail because the sender wrote it lower.
        let request = request("https://push.example.com/a");
        assert_eq!(request.header("Authorization"), Some("vapid t=x, k=y"));
        assert_eq!(request.header("AUTHORIZATION"), Some("vapid t=x, k=y"));
        assert_eq!(request.header("missing"), None);
    }

    #[tokio::test]
    async fn recording_transport_defaults_to_created() {
        // `201 Created` is what a push service returns on acceptance; a
        // default that fell outside 2xx would make every unscripted test look
        // like a failed delivery.
        let transport = RecordingPushTransport::new();
        assert_eq!(
            transport
                .deliver(&request("https://push.example.com/a"))
                .await
                .expect("deliver"),
            201
        );
    }

    #[tokio::test]
    async fn recording_transport_applies_a_scripted_status_by_endpoint() {
        let transport = RecordingPushTransport::new()
            .responding_with("https://push.example.com/dead", 410)
            .responding_with("https://push.example.com/busy", 429);

        for (endpoint, expected) in [
            ("https://push.example.com/dead", 410),
            ("https://push.example.com/busy", 429),
            ("https://push.example.com/other", 201),
        ] {
            assert_eq!(
                transport
                    .deliver(&request(endpoint))
                    .await
                    .expect("deliver"),
                expected,
                "{endpoint}"
            );
        }
    }

    #[tokio::test]
    async fn recording_transport_uses_the_first_matching_script() {
        // Deterministic precedence: a test that scripts an endpoint twice must
        // not get whichever entry the iteration order happened to reach.
        let transport = RecordingPushTransport::new()
            .responding_with("https://push.example.com/a", 410)
            .responding_with("https://push.example.com/a", 500);
        assert_eq!(
            transport
                .deliver(&request("https://push.example.com/a"))
                .await
                .expect("deliver"),
            410
        );
    }

    #[tokio::test]
    async fn recording_transport_records_in_dispatch_order_and_shares_across_clones() {
        let transport = RecordingPushTransport::new();
        let clone = transport.clone();
        for endpoint in ["https://push.example.com/1", "https://push.example.com/2"] {
            clone.deliver(&request(endpoint)).await.expect("deliver");
        }
        let recorded = transport.requests();
        assert_eq!(recorded.len(), 2, "a clone must share the recording");
        assert_eq!(recorded[0].endpoint, "https://push.example.com/1");
        assert_eq!(recorded[1].endpoint, "https://push.example.com/2");
        assert_eq!(recorded[0].body, vec![1, 2, 3]);
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn the_http_transport_refuses_an_endpoint_resolving_to_a_blocked_address() {
        // `localhost` is a hostname, so the subscribe-time IP-literal rule
        // cannot catch it — this is the layer that must.
        let err = resolve_and_validate_endpoint("https://localhost/push")
            .await
            .expect_err("a loopback-resolving host is refused");
        assert!(matches!(err, PushError::InvalidEndpoint(_)), "{err:?}");
        assert!(
            err.to_string().contains("blocked address"),
            "the error must say why: {err}"
        );
    }

    #[cfg(feature = "http-client")]
    #[tokio::test]
    async fn the_http_transport_reports_an_unresolvable_host_as_a_transport_error() {
        // `.invalid` is reserved by RFC 2606 and never resolves. A DNS failure
        // is transient, not a reason to prune the subscription, so it must be
        // a Transport error rather than an InvalidEndpoint one.
        let err = resolve_and_validate_endpoint("https://nothing.invalid/push")
            .await
            .expect_err("an unresolvable host fails");
        assert!(matches!(err, PushError::Transport(_)), "{err:?}");
    }

    #[cfg(not(feature = "http-client"))]
    #[tokio::test]
    async fn the_unavailable_transport_errors_rather_than_pretending_to_deliver() {
        let err = UnavailablePushTransport
            .deliver(&request("https://push.example.com/a"))
            .await
            .expect_err("a push that cannot go out must never look like it did");
        assert!(matches!(err, PushError::Transport(_)), "{err:?}");
    }
}
