//! ALPN negotiation on the in-process TLS listener (issue #2321).
//!
//! Drives the real `TlsListener` with real h2 and HTTP/1.1 clients. If the
//! server stops advertising `h2`, these tests fail.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use autumn_web::config::AutumnConfig;
use autumn_web::sse::{Event, Sse};
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use futures::stream::Stream;
use rustls_pki_types::ServerName;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::tls_support::{
    CertFixture, RECORDING_HANDSHAKE_TIMEOUT, RecordingVerifier, TestServer, serve_tls_router,
};

type Tls = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

const H2: &[u8] = b"h2";
const H1: &[u8] = b"http/1.1";
const SSE_GAP: Duration = Duration::from_millis(1500);

#[get("/fast")]
async fn fast() -> &'static str {
    "quick"
}

#[get("/slow")]
async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_secs(3)).await;
    "done"
}

static DRAIN_ENTERED: AtomicBool = AtomicBool::new(false);

#[get("/drain")]
async fn drain() -> &'static str {
    DRAIN_ENTERED.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(500)).await;
    "drained"
}

/// `tick-0` now, `tick-1` after [`SSE_GAP`].
#[get("/stream")]
async fn stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = futures::stream::unfold(0u8, |i| async move {
        if i >= 2 {
            return None;
        }
        if i > 0 {
            tokio::time::sleep(SSE_GAP).await;
        }
        Some((Ok(Event::default().data(format!("tick-{i}"))), i + 1))
    });
    Sse::new(events)
}

async fn serve(request_timeout_ms: Option<u64>) -> (TestServer, CertFixture) {
    let fixture = CertFixture::write();
    let mut config = AutumnConfig::default();
    config.server.timeouts.request_timeout_ms = request_timeout_ms;
    let router = TestApp::new()
        .routes(routes![fast, slow, drain, stream])
        .config(config)
        .build()
        .into_router();
    let (server, _reloader) = serve_tls_router(router, &fixture, RECORDING_HANDSHAKE_TIMEOUT).await;
    (server, fixture)
}

/// TLS client that offers exactly `offered` as its ALPN list.
async fn connect(server: &TestServer, offered: &[&[u8]]) -> Tls {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(RecordingVerifier::default()))
        .with_no_client_auth();
    config.alpn_protocols = offered.iter().map(|p| p.to_vec()).collect();
    let tcp = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .expect("TLS handshake")
}

fn negotiated(stream: &Tls) -> Option<Vec<u8>> {
    stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
}

/// Open an h2 connection and return its request handle.
async fn h2_open(stream: Tls) -> h2::client::SendRequest<bytes::Bytes> {
    let (send, conn) = h2::client::handshake(stream).await.expect("h2 handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    send
}

/// Send `GET path` and return the response head and body stream.
async fn h2_send(
    send: &mut h2::client::SendRequest<bytes::Bytes>,
    path: &str,
) -> (u16, h2::RecvStream) {
    let request = http::Request::get(format!("https://localhost{path}"))
        .body(())
        .unwrap();
    let (response, _) = send
        .clone()
        .ready()
        .await
        .expect("h2 ready")
        .send_request(request, true)
        .expect("send request");
    let response = response.await.expect("h2 response");
    (response.status().as_u16(), response.into_body())
}

async fn h2_body(mut body: h2::RecvStream) -> String {
    let mut out = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.expect("h2 data");
        let _ = body.flow_control().release_capacity(chunk.len());
        out.extend_from_slice(&chunk);
    }
    String::from_utf8(out).unwrap()
}

/// Plain `Connection: close` GET over an already-open TLS stream.
async fn h1_get(mut stream: Tls, path: &str) -> String {
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    // A close without `close_notify` is fine here; keep what was read.
    let _ = stream.read_to_end(&mut raw).await;
    String::from_utf8_lossy(&raw).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_style_client_negotiates_h2_and_is_served() {
    let (server, _fixture) = serve(None).await;
    let stream = connect(&server, &[H2, H1]).await;
    assert_eq!(negotiated(&stream).as_deref(), Some(H2), "ALPN must pick h2");

    let mut send = h2_open(stream).await;
    let (status, body) = h2_send(&mut send, "/fast").await;
    assert_eq!(status, 200);
    assert_eq!(h2_body(body).await, "quick");
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http11_only_client_still_works() {
    let (server, _fixture) = serve(None).await;
    let stream = connect(&server, &[H1]).await;
    assert_eq!(negotiated(&stream).as_deref(), Some(H1));
    let raw = h1_get(stream, "/fast").await;
    assert!(raw.starts_with("HTTP/1.1 200"), "got: {raw:?}");
    assert!(raw.ends_with("quick"), "got: {raw:?}");
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn alpn_less_client_still_works() {
    let (server, _fixture) = serve(None).await;
    let stream = connect(&server, &[]).await;
    assert_eq!(negotiated(&stream), None);
    let raw = h1_get(stream, "/fast").await;
    assert!(raw.starts_with("HTTP/1.1 200"), "got: {raw:?}");
    server.shutdown().await;
}

// axum's `serve` enables RFC 8441 extended CONNECT, so browsers open
// `wss://` as an h2 stream. It must echo, not break.
#[cfg(feature = "ws")]
mod wss {
    use super::*;
    use autumn_web::ws::{Message, WebSocket, WsHandler};
    use autumn_web::{routes, ws};

    #[ws("/echo")]
    async fn echo() -> impl WsHandler {
        |mut socket: WebSocket| async move {
            while let Some(Ok(Message::Text(text))) = socket.recv().await {
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_echoes_over_h2_extended_connect() {
        let fixture = CertFixture::write();
        let router = TestApp::new().routes(routes![echo]).build().into_router();
        let (server, _reloader) =
            serve_tls_router(router, &fixture, RECORDING_HANDSHAKE_TIMEOUT).await;
        let mut send = h2_open(connect(&server, &[H2, H1]).await).await;
        let request = http::Request::connect("https://localhost/echo")
            .extension(h2::ext::Protocol::from_static("websocket"))
            .header("sec-websocket-version", "13")
            .body(())
            .unwrap();
        let (response, mut tx) = send.send_request(request, false).expect("CONNECT");
        let response = response.await.expect("CONNECT response");
        assert_eq!(response.status(), 200, "wss over h2 must be accepted");
        let mut rx = response.into_body();

        // Masked client text frame "ping" (zero mask key).
        tx.send_data(bytes::Bytes::from_static(b"\x81\x84\0\0\0\0ping"), false)
            .expect("send frame");
        let echoed = tokio::time::timeout(Duration::from_secs(5), rx.data())
            .await
            .expect("echo timed out")
            .expect("stream ended before the echo")
            .expect("h2 data");
        assert_eq!(&echoed[..], b"\x81\x04ping");
        server.shutdown.cancel();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn request_timeout_applies_over_h2() {
    let (server, _fixture) = serve(Some(400)).await;
    let mut send = h2_open(connect(&server, &[H2, H1]).await).await;
    let (status, _) = h2_send(&mut send, "/slow").await;
    assert_eq!(status, 503);
    let (status, body) = h2_send(&mut send, "/fast").await;
    assert_eq!(status, 200);
    assert_eq!(h2_body(body).await, "quick");
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_streams_incrementally_over_h2() {
    // Deadline is shorter than the SSE gap: a stream must be exempt.
    let (server, _fixture) = serve(Some(400)).await;
    let mut send = h2_open(connect(&server, &[H2, H1]).await).await;
    let (status, mut body) = h2_send(&mut send, "/stream").await;
    assert_eq!(status, 200);

    let started = Instant::now();
    let mut seen = String::new();
    while !seen.contains("tick-0") {
        let chunk = tokio::time::timeout(Duration::from_secs(10), body.data())
            .await
            .expect("no first event in 10s")
            .expect("stream ended early")
            .expect("h2 data");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(
        !seen.contains("tick-1") && started.elapsed() < SSE_GAP,
        "first event must arrive before the second is produced: {seen:?}"
    );
    while !seen.contains("tick-1") {
        let chunk = tokio::time::timeout(Duration::from_secs(10), body.data())
            .await
            .expect("no second event in 10s")
            .expect("stream ended before tick-1")
            .expect("h2 data");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_shutdown_drains_an_in_flight_h2_request() {
    let (server, _fixture) = serve(None).await;
    let mut send = h2_open(connect(&server, &[H2, H1]).await).await;
    let inflight = tokio::spawn(async move {
        let (status, body) = h2_send(&mut send, "/drain").await;
        (status, h2_body(body).await)
    });

    let waited = Instant::now();
    while !DRAIN_ENTERED.load(Ordering::SeqCst) {
        assert!(waited.elapsed() < Duration::from_secs(20), "handler never entered");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    server.shutdown.cancel();

    let (status, body) = tokio::time::timeout(Duration::from_secs(20), inflight)
        .await
        .expect("in-flight h2 request never finished")
        .expect("task panicked");
    assert_eq!((status, body.as_str()), (200, "drained"));
    let joined = tokio::time::timeout(Duration::from_secs(20), server.handle)
        .await
        .expect("serve task should stop after draining")
        .expect("serve task panicked");
    assert!(joined.is_ok(), "shutdown should return Ok, got {joined:?}");
}
