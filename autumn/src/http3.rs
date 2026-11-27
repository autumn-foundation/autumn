//! HTTP/3 over QUIC, beside the `[server.tls]` listener.
//!
//! [`Http3Listener`] binds a UDP `quinn` endpoint and serves a
//! [`tower::Service`] over `h3`. The app serves the same service as the TCP
//! listener, so `TrustedProxiesLayer` and `MethodOverrideLayer` apply over h3
//! too. [`AltSvcLayer`] adds the `Alt-Svc` header to TCP responses, so
//! browsers know to try h3.
//!
//! Adapted from `axum-h3` 0.0.6 (MIT). Changes:
//!
//! - Generic over a `tower::Service`, not an `axum::Router`.
//! - Each request carries `ConnectInfo<TlsConnectInfo>`, then
//!   [`ClientIdentityLayer`](crate::tls::client_auth::ClientIdentityLayer)
//!   stamps `ConnectInfo<SocketAddr>`. Without the peer address,
//!   trusted-proxy resolution would trust every peer.
//! - A request without `Host` gets one from `:authority`.
//! - Hop-by-hop response headers, which HTTP/3 forbids, are removed.
//! - Shutdown sends GOAWAY and lets in-flight requests finish.
//!
//! Extended CONNECT, WebTransport, QUIC datagrams and 0-RTT stay off.
//! WebSockets therefore stay on HTTP/1.1 and HTTP/2.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::http::{HeaderValue, Request, Response, header};
use bytes::{Buf as _, Bytes};
use http_body_util::BodyExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Http3Config;
use crate::http_server::HttpLimits;
use crate::tls::TlsConnectInfo;

/// After the last response of a drained connection, wait this long for the
/// client to close. A server close drops stream data that is not sent yet.
const DRAIN_LINGER: Duration = Duration::from_secs(2);

/// After all connections end, wait this long for the endpoint to go idle.
const ENDPOINT_IDLE_GRACE: Duration = Duration::from_secs(2);

/// Default handshake limit, the `[server.tls]` default.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Headers that HTTP/3 forbids in a response (RFC 9114, section 4.2).
const CONNECTION_HEADERS: [header::HeaderName; 5] = [
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::HeaderName::from_static("keep-alive"),
    header::HeaderName::from_static("proxy-connection"),
];

/// Resolved settings for [`Http3Listener`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Http3Settings {
    /// Close a connection after this much idle time.
    pub max_idle_timeout: Duration,
    /// Requests in flight on one connection.
    pub max_concurrent_bidi_streams: u32,
    /// Largest request head, in bytes. `None`: no limit from the server.
    pub max_field_section_size: Option<u64>,
    /// Open QUIC connections. `None`: no limit. The TCP listener has its own
    /// limit of the same size.
    pub max_connections: Option<usize>,
    /// Time limit for the QUIC handshake.
    pub handshake_timeout: Duration,
}

impl Default for Http3Settings {
    fn default() -> Self {
        Self::from_config(&Http3Config::default(), &HttpLimits::default())
    }
}

impl Http3Settings {
    /// Settings from `[server.http3]` and the `[server.http]` limits.
    #[must_use]
    pub fn from_config(config: &Http3Config, limits: &HttpLimits) -> Self {
        Self {
            max_idle_timeout: Duration::from_secs(config.max_idle_timeout_secs.max(1)),
            max_concurrent_bidi_streams: config.max_concurrent_bidi_streams.max(1),
            max_field_section_size: limits.max_header_bytes.map(|v| v as u64),
            max_connections: limits.max_connections,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    /// Set the handshake time limit. `0` becomes one second.
    #[must_use]
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout.max(Duration::from_secs(1));
        self
    }
}

/// The HTTP/3 listener could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Http3Error {
    /// The rustls config has no cipher suite that QUIC can use.
    #[error("the TLS config cannot serve QUIC: {0}")]
    Crypto(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    /// The idle timeout is too large for QUIC.
    #[error("[server.http3] max_idle_timeout_secs is too large: {0}")]
    IdleTimeout(#[from] quinn::VarIntBoundsExceeded),
    /// The UDP socket could not bind.
    #[error("failed to bind the HTTP/3 UDP socket on {addr}: {source}")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// The cause.
        source: io::Error,
    },
}

/// A bound HTTP/3 (QUIC) endpoint. Serve it with [`serve`](Self::serve).
#[derive(Debug)]
pub struct Http3Listener {
    endpoint: quinn::Endpoint,
    settings: Http3Settings,
}

impl Http3Listener {
    /// Bind a UDP endpoint on `addr` that serves `tls`.
    ///
    /// Build `tls` with
    /// [`ServerTlsParts::quic_config`](crate::tls::ServerTlsParts::quic_config),
    /// so that QUIC shares the TCP listener's certificate.
    ///
    /// # Errors
    ///
    /// Returns [`Http3Error`] when the TLS config cannot serve QUIC, the idle
    /// timeout is out of range, or the socket cannot bind.
    pub fn bind(
        addr: SocketAddr,
        tls: Arc<rustls::ServerConfig>,
        settings: Http3Settings,
    ) -> Result<Self, Http3Error> {
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport
            .max_concurrent_bidi_streams(settings.max_concurrent_bidi_streams.into())
            .max_idle_timeout(Some(quinn::IdleTimeout::try_from(
                settings.max_idle_timeout,
            )?))
            // No QUIC datagrams.
            .datagram_receive_buffer_size(None);
        server.transport_config(Arc::new(transport));
        let endpoint = quinn::Endpoint::server(server, addr)
            .map_err(|source| Http3Error::Bind { addr, source })?;
        Ok(Self { endpoint, settings })
    }

    /// The bound UDP address.
    ///
    /// # Errors
    ///
    /// Returns the socket error.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Serve `service` until `shutdown` is cancelled.
    ///
    /// On cancel: stop accepting, send GOAWAY on each connection, let
    /// in-flight requests finish, then close the endpoint.
    ///
    /// # Errors
    ///
    /// None at this time. The `io::Result` matches the TCP serve loop.
    pub async fn serve<S, B>(self, service: S, shutdown: CancellationToken) -> io::Result<()>
    where
        S: tower::Service<Request<axum::body::Body>, Response = Response<B>, Error = Infallible>
            + Clone
            + Send
            + 'static,
        S::Future: Send + 'static,
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<axum::BoxError>,
    {
        let Self { endpoint, settings } = self;
        // Stamps `ConnectInfo<SocketAddr>` from the `TlsConnectInfo` set below.
        let service = tower::Layer::layer(&crate::tls::client_auth::ClientIdentityLayer, service);
        let slots = settings
            .max_connections
            .map(|max| Arc::new(Semaphore::new(max)));
        let mut connections = JoinSet::new();
        loop {
            let incoming = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => continue,
                incoming = endpoint.accept() => match incoming {
                    Some(incoming) => incoming,
                    None => break,
                },
            };
            let slot = match &slots {
                Some(slots) => {
                    if let Ok(slot) = Arc::clone(slots).try_acquire_owned() {
                        Some(slot)
                    } else {
                        tracing::debug!(
                            peer = %incoming.remote_address(),
                            "HTTP/3 connection refused: max_connections reached"
                        );
                        incoming.refuse();
                        continue;
                    }
                }
                None => None,
            };
            connections.spawn(serve_connection(
                incoming,
                service.clone(),
                shutdown.clone(),
                settings.clone(),
                slot,
            ));
        }
        // Refuse new connections. Established ones drain below.
        endpoint.set_server_config(None);
        while connections.join_next().await.is_some() {}
        endpoint.close(0u32.into(), b"server shutdown");
        let _ = tokio::time::timeout(ENDPOINT_IDLE_GRACE, endpoint.wait_idle()).await;
        Ok(())
    }
}

/// What the connection loop does next.
enum Next<R> {
    Drain,
    Request(R),
    Closed,
    Reaped,
}

async fn serve_connection<S, B>(
    incoming: quinn::Incoming,
    service: S,
    shutdown: CancellationToken,
    settings: Http3Settings,
    _slot: Option<OwnedSemaphorePermit>,
) where
    S: tower::Service<Request<axum::body::Body>, Response = Response<B>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<axum::BoxError>,
{
    let peer = incoming.remote_address();
    let conn = match tokio::time::timeout(settings.handshake_timeout, incoming).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => {
            tracing::debug!(peer = %peer, error = %e, "HTTP/3 handshake failed");
            return;
        }
        Err(_) => {
            tracing::debug!(peer = %peer, "HTTP/3 handshake timed out");
            return;
        }
    };
    let chain = conn.peer_identity().and_then(|any| {
        any.downcast::<Vec<rustls_pki_types::CertificateDer<'static>>>()
            .ok()
    });
    let info = TlsConnectInfo::from_peer_chain(peer, chain.as_deref().map(Vec::as_slice));

    let mut builder = h3::server::builder();
    if let Some(max) = settings.max_field_section_size {
        builder.max_field_section_size(max);
    }
    let quic = conn.clone();
    let mut h3_conn: h3::server::Connection<h3_quinn::Connection, Bytes> =
        match builder.build(h3_quinn::Connection::new(conn)).await {
            Ok(h3_conn) => h3_conn,
            Err(e) => {
                tracing::debug!(peer = %peer, error = %e, "HTTP/3 connection setup failed");
                return;
            }
        };

    let mut requests = JoinSet::new();
    let mut draining = false;
    loop {
        if draining && requests.is_empty() {
            break;
        }
        let next = tokio::select! {
            biased;
            () = shutdown.cancelled(), if !draining => Next::Drain,
            Some(_) = requests.join_next(), if !requests.is_empty() => Next::Reaped,
            accepted = h3_conn.accept() => match accepted {
                Ok(Some(resolver)) => Next::Request(resolver),
                Ok(None) => Next::Closed,
                Err(e) => {
                    if !e.is_h3_no_error() {
                        tracing::debug!(peer = %peer, error = %e, "HTTP/3 connection ended");
                    }
                    Next::Closed
                }
            },
        };
        match next {
            Next::Drain => {
                draining = true;
                // GOAWAY: the client opens no new requests here.
                if let Err(e) = h3_conn.shutdown(0).await {
                    tracing::debug!(peer = %peer, error = %e, "HTTP/3 GOAWAY failed");
                    break;
                }
            }
            Next::Request(resolver) => {
                requests.spawn(serve_request(resolver, service.clone(), info.clone()));
            }
            Next::Reaped => {}
            Next::Closed => break,
        }
    }
    while requests.join_next().await.is_some() {}
    if draining {
        let _ = tokio::time::timeout(DRAIN_LINGER, quic.closed()).await;
    }
}

async fn serve_request<S, B>(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    service: S,
    info: TlsConnectInfo,
) where
    S: tower::Service<Request<axum::body::Body>, Response = Response<B>, Error = Infallible>
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<axum::BoxError>,
{
    let (request, stream) = match resolver.resolve_request().await {
        Ok(resolved) => resolved,
        Err(e) => {
            tracing::debug!(peer = %info.peer, error = %e, "HTTP/3 request head rejected");
            return;
        }
    };
    let (mut send, recv) = stream.split();
    let (mut parts, ()) = request.into_parts();
    add_host_from_authority(&mut parts);
    parts.extensions.insert(axum::extract::ConnectInfo(info));
    let request = Request::from_parts(parts, axum::body::Body::new(RecvBody::new(recv)));

    let Ok(response) = tower::ServiceExt::oneshot(service, request).await;
    let (mut head, body) = response.into_parts();
    for name in &CONNECTION_HEADERS {
        head.headers.remove(name);
    }
    if send
        .send_response(Response::from_parts(head, ()))
        .await
        .is_err()
    {
        return;
    }
    let mut body = axum::body::Body::new(body);
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => {
                tracing::debug!(error = %e, "HTTP/3 response body failed");
                send.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
                return;
            }
        };
        let sent = match frame.into_data() {
            Ok(data) if data.is_empty() => Ok(()),
            Ok(data) => send.send_data(data).await,
            Err(frame) => match frame.into_trailers() {
                Ok(trailers) => send.send_trailers(trailers).await,
                Err(_) => Ok(()),
            },
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = send.finish().await;
}

/// Give the request a `Host` header from `:authority` when it has none.
/// Origin checks and tenant routing read `Host`.
fn add_host_from_authority(parts: &mut axum::http::request::Parts) {
    if parts.headers.contains_key(header::HOST) {
        return;
    }
    if let Some(authority) = parts.uri.authority()
        && let Ok(value) = HeaderValue::from_str(authority.as_str())
    {
        parts.headers.insert(header::HOST, value);
    }
}

/// The request body, read from the h3 receive stream.
struct RecvBody {
    stream: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>,
    state: RecvState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecvState {
    Data,
    Trailers,
    Done,
}

impl RecvBody {
    const fn new(stream: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>) -> Self {
        Self {
            stream,
            state: RecvState::Data,
        }
    }
}

impl http_body::Body for RecvBody {
    type Data = Bytes;
    type Error = h3::error::StreamError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        if this.state == RecvState::Data {
            match ready!(this.stream.poll_recv_data(cx)) {
                Ok(Some(mut chunk)) => {
                    let len = chunk.remaining();
                    return Poll::Ready(Some(Ok(http_body::Frame::data(chunk.copy_to_bytes(len)))));
                }
                Ok(None) => this.state = RecvState::Trailers,
                Err(e) => {
                    this.state = RecvState::Done;
                    return Poll::Ready(Some(Err(e)));
                }
            }
        }
        if this.state == RecvState::Trailers {
            let trailers = ready!(this.stream.poll_recv_trailers(cx));
            this.state = RecvState::Done;
            return Poll::Ready(match trailers {
                Ok(Some(trailers)) => Some(Ok(http_body::Frame::trailers(trailers))),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            });
        }
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        self.state == RecvState::Done
    }
}

/// Adds `Alt-Svc` to each response that has none, so clients learn that h3
/// is available. A header that the app sets stays as it is.
#[derive(Debug, Clone)]
pub struct AltSvcLayer {
    value: HeaderValue,
}

impl AltSvcLayer {
    /// A layer that sends `value`, for example `h3=":443"; ma=86400`.
    #[must_use]
    pub const fn new(value: HeaderValue) -> Self {
        Self { value }
    }

    /// The layer for `config`, advertising h3 on `udp_port`.
    ///
    /// # Errors
    ///
    /// None in practice: the value is ASCII digits and punctuation.
    pub fn for_port(
        config: &Http3Config,
        udp_port: u16,
    ) -> Result<Self, axum::http::header::InvalidHeaderValue> {
        HeaderValue::from_str(&config.alt_svc_value(udp_port)).map(Self::new)
    }
}

impl<S> tower::Layer<S> for AltSvcLayer {
    type Service = AltSvc<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AltSvc {
            inner,
            value: self.value.clone(),
        }
    }
}

/// The [`AltSvcLayer`] service.
#[derive(Debug, Clone)]
pub struct AltSvc<S> {
    inner: S,
    value: HeaderValue,
}

impl<S, Req, B> tower::Service<Req> for AltSvc<S>
where
    S: tower::Service<Req, Response = Response<B>>,
    S::Future: Send + 'static,
{
    type Response = Response<B>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let future = self.inner.call(req);
        let value = self.value.clone();
        Box::pin(async move {
            let mut response = future.await?;
            response
                .headers_mut()
                .entry(header::ALT_SVC)
                .or_insert(value);
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_comes_from_the_authority_when_absent() {
        let (mut parts, ()) = Request::get("https://example.com:8443/x")
            .body(())
            .unwrap()
            .into_parts();
        add_host_from_authority(&mut parts);
        assert_eq!(parts.headers[header::HOST], "example.com:8443");
    }

    #[test]
    fn an_existing_host_header_is_kept() {
        let (mut parts, ()) = Request::get("https://example.com/x")
            .header(header::HOST, "other.example")
            .body(())
            .unwrap()
            .into_parts();
        add_host_from_authority(&mut parts);
        assert_eq!(parts.headers[header::HOST], "other.example");
    }

    #[test]
    fn default_settings_match_the_config_defaults() {
        let settings = Http3Settings::default();
        assert_eq!(settings.max_idle_timeout, Duration::from_secs(30));
        assert_eq!(settings.max_concurrent_bidi_streams, 100);
        assert_eq!(settings.max_field_section_size, None);
        assert_eq!(settings.max_connections, None);
        assert_eq!(settings.handshake_timeout, DEFAULT_HANDSHAKE_TIMEOUT);
    }

    #[test]
    fn a_zero_handshake_timeout_becomes_one_second() {
        let settings = Http3Settings::default().with_handshake_timeout(Duration::ZERO);
        assert_eq!(settings.handshake_timeout, Duration::from_secs(1));
    }

    #[test]
    fn alt_svc_layer_for_port_uses_the_config_value() {
        let config = Http3Config {
            enabled: true,
            ..Http3Config::default()
        };
        let layer = AltSvcLayer::for_port(&config, 8443).unwrap();
        assert_eq!(layer.value, "h3=\":8443\"; ma=86400");
    }

    #[tokio::test]
    async fn bind_rejects_a_tls_config_with_no_quic_initial_suite() {
        // QUIC protects its first packets with TLS13_AES_128_GCM_SHA256.
        let provider = Arc::new(rustls::crypto::CryptoProvider {
            cipher_suites: vec![rustls::crypto::ring::cipher_suite::TLS13_AES_256_GCM_SHA384],
            ..rustls::crypto::ring::default_provider()
        });
        let key = crate::tls::certified_key_from_pem(
            include_bytes!("../tests/fixtures/tls/localhost.cert.pem"),
            include_bytes!("../tests/fixtures/tls/localhost.key.pem"),
            &crate::tls::crypto_provider(),
        )
        .unwrap();
        let tls = crate::tls::build_quic_server_config(
            provider,
            Arc::new(crate::tls::ReloadableCertResolver::new(key)),
            None,
        )
        .expect("rustls accepts a TLS 1.3 suite");
        let err = Http3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            tls,
            Http3Settings::default(),
        )
        .expect_err("no QUIC initial suite");
        assert!(matches!(err, Http3Error::Crypto(_)), "{err}");
    }
}
