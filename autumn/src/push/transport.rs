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
/// Uses the framework's outbound [`Client`](crate::http_client::Client), so
/// push traffic inherits its tracing context propagation, timeouts, and — the
/// reason it matters here — its SSRF address policy. The subscribe boundary
/// already refuses IP-literal and loopback endpoints
/// ([`BrowserSubscription::decode`](super::store::BrowserSubscription::decode));
/// this covers the remaining case of a *hostname* that resolves to a private
/// address.
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
            let mut builder = self.client.post(&request.endpoint);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            // A `410 Gone` is a normal, expected answer that must reach the
            // pruning logic, so status codes are never turned into errors
            // here — only a genuine transport failure is.
            match builder.bytes_body(request.body.clone()).send().await {
                Ok(response) => Ok(response.status().as_u16()),
                Err(e) => Err(PushError::Transport(e.to_string())),
            }
        })
    }
}
