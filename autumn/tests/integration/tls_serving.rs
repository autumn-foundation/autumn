//! End-to-end coverage for the direct-HTTPS (native TLS) listener (issue
//! #1603).
//!
//! These tests exercise the REAL serving path: the public
//! [`autumn_web::tls::TlsListener`] (the same `axum::serve::Listener` the app
//! binds when `[server.tls]` is set) served through `axum::serve` with the
//! same connect-info and graceful-shutdown wiring `app.rs` uses. A rustls
//! client that trusts the test certificate then drives it over TLS.
//!
//! Scope: the *listener* — the handshake, the accept loop under hostile
//! clients, and connect-info. The rest of the app surface under TLS (probes,
//! the request timeout, SSE, `wss://`, draining an in-flight request, and
//! certificate renewal) lives in `tls_app_surface.rs`.
//!
//! The client here speaks rustls synchronously over a `std::net::TcpStream` on
//! a blocking thread, deliberately: several of these tests need a client that
//! holds a raw socket open, and a blocking client makes "this request did not
//! wait behind that stalled handshake" a direct assertion.

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use rustls_pki_types::ServerName;

use super::tls_support::{CertFixture, RecordingVerifier, serve_tls_router};

/// A running HTTPS test server plus the knobs to talk to and stop it.
struct TestServer {
    addr: SocketAddr,
    shutdown: tokio_util::sync::CancellationToken,
    handle: tokio::task::JoinHandle<std::io::Result<()>>,
    /// Holds the certificate tempdir open for the server's lifetime.
    _fixture: CertFixture,
}

/// Boot the real `TlsListener` on an ephemeral port, serving a minimal router
/// with the same connect-info + graceful-shutdown wiring as `app.rs`. Uses a
/// generous handshake timeout that never fires in the well-behaved-client tests.
async fn serve_tls() -> TestServer {
    serve_tls_with_handshake_timeout(Duration::from_secs(10)).await
}

/// Like [`serve_tls`] but with a caller-chosen per-handshake timeout, so a test
/// can prove a stalled handshake is shed rather than parking the accept loop.
async fn serve_tls_with_handshake_timeout(handshake_timeout: Duration) -> TestServer {
    let fixture = CertFixture::write();
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/whoami",
            // Prove the peer SocketAddr connect-info still flows through the
            // TLS listener exactly as on the plain-TCP path.
            get(
                |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>| async move {
                    peer.ip().to_string()
                },
            ),
        );

    let (server, _resolver) = serve_tls_router(router, &fixture, handshake_timeout).await;

    TestServer {
        addr: server.addr,
        shutdown: server.shutdown,
        handle: server.handle,
        _fixture: fixture,
    }
}

/// Perform a blocking HTTPS GET over rustls, trusting the test certificate, and
/// return the raw response (status line + headers + body). Runs synchronously,
/// so callers wrap it in `spawn_blocking`.
fn https_get(addr: SocketAddr, path: &str) -> std::io::Result<String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(RecordingVerifier::default()))
        .with_no_client_auth();

    // SNI/verification uses the certificate's `localhost` SAN even though we
    // dial 127.0.0.1.
    let server_name = ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let sock = std::net::TcpStream::connect(addr)?;
    let mut tls = rustls::StreamOwned::new(conn, sock);

    write!(
        tls,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    tls.flush()?;
    let mut resp = String::new();
    // A clean stream close after `Connection: close` surfaces as EOF; ignore a
    // benign "close_notify not received" which some servers omit.
    match tls.read_to_string(&mut resp) {
        Ok(_) => {}
        Err(e) if !resp.is_empty() => {
            let _ = e;
        }
        Err(e) => return Err(e),
    }
    Ok(resp)
}

#[tokio::test]
async fn serves_https_and_returns_200() {
    let server = serve_tls().await;
    let addr = server.addr;

    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/health"))
        .await
        .unwrap()
        .expect("HTTPS GET /health");

    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "expected 200 over TLS, got: {resp:?}"
    );
    assert!(resp.contains("ok"), "expected body over TLS, got: {resp:?}");

    server.shutdown.cancel();
    let _ = server.handle.await;
}

#[tokio::test]
async fn peer_socketaddr_connect_info_flows_over_tls() {
    let server = serve_tls().await;
    let addr = server.addr;

    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/whoami"))
        .await
        .unwrap()
        .expect("HTTPS GET /whoami");

    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");
    // The handler echoed the peer IP resolved from ConnectInfo<SocketAddr>.
    assert!(
        resp.contains("127.0.0.1"),
        "peer SocketAddr should flow through the TLS listener, got: {resp:?}"
    );

    server.shutdown.cancel();
    let _ = server.handle.await;
}

#[tokio::test]
async fn graceful_shutdown_completes_under_tls() {
    let server = serve_tls().await;
    let addr = server.addr;

    // One successful request first, so the listener is definitely serving.
    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/health"))
        .await
        .unwrap()
        .expect("HTTPS GET before shutdown");
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");

    // Trigger graceful shutdown; the serve task must join cleanly and promptly.
    server.shutdown.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(5), server.handle).await;
    let result = joined.expect("serve task should stop within the timeout");
    assert!(
        matches!(result, Ok(Ok(()))),
        "graceful shutdown under TLS should return Ok, got: {result:?}"
    );
}

#[tokio::test]
async fn bad_handshake_does_not_kill_the_listener() {
    let server = serve_tls().await;
    let addr = server.addr;

    // A plaintext client that never speaks TLS: connect, send junk, close. The
    // handshake fails inside the accept loop, which must skip it rather than
    // die.
    tokio::task::spawn_blocking(move || {
        if let Ok(mut sock) = std::net::TcpStream::connect(addr) {
            let _ = sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
            let _ = sock.flush();
        }
    })
    .await
    .unwrap();

    // The listener must still serve a subsequent, well-formed HTTPS request.
    let resp = tokio::task::spawn_blocking(move || https_get(addr, "/health"))
        .await
        .unwrap()
        .expect("HTTPS GET after a bad handshake");
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "listener should survive a failed handshake, got: {resp:?}"
    );

    server.shutdown.cancel();
    let _ = server.handle.await;
}

#[tokio::test]
async fn hung_handshake_does_not_wedge_the_listener() {
    // A short handshake timeout so the stalled connection is shed quickly.
    let server = serve_tls_with_handshake_timeout(Duration::from_secs(1)).await;
    let addr = server.addr;

    // Open a raw TCP connection and send NOTHING — no ClientHello, ever. Before
    // the handshake-timeout fix this parks `acceptor.accept(...)` inside the
    // accept loop forever, denying every other client (single-connection DoS).
    // Hold the socket open for the duration of the good request below.
    let hung = std::net::TcpStream::connect(addr).expect("open raw TCP connection");

    // Immediately, while the hung socket is still connected and silent, a
    // well-behaved client must still get served. Give it a generous but bounded
    // timeout: before the fix this hangs; after, it returns 200 promptly.
    let good = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || https_get(addr, "/health")),
    )
    .await
    .expect("good request must not hang behind the stalled handshake")
    .unwrap()
    .expect("HTTPS GET /health while a handshake is stalled");

    assert!(
        good.starts_with("HTTP/1.1 200"),
        "listener must keep serving while a handshake is stalled, got: {good:?}"
    );

    // Keep the hung socket alive until after the good request completed, so the
    // test truly overlapped a stalled handshake with a real one.
    drop(hung);

    server.shutdown.cancel();
    let _ = server.handle.await;
}

#[tokio::test]
async fn concurrent_handshakes_do_not_serialize_accept() {
    // Number of stalled connections opened before the good request.
    const STALLED: usize = 5;

    // A short per-handshake timeout so each stalled connection would, IF the
    // accept loop were serialized, hold it up for this long.
    let handshake_timeout = Duration::from_secs(2);
    let server = serve_tls_with_handshake_timeout(handshake_timeout).await;
    let addr = server.addr;

    // Open several raw TCP connections that connect and send NOTHING — no
    // ClientHello, ever. With an accept loop that ran the handshake inline,
    // each of these would park acceptance for the full `handshake_timeout`, so
    // a good request queued behind them would wait up to
    // `STALLED * handshake_timeout` (here 5 * 2s = 10s). With handshakes driven
    // concurrently off the accept loop, the good request is served promptly.
    let mut hung = Vec::with_capacity(STALLED);
    for _ in 0..STALLED {
        hung.push(std::net::TcpStream::connect(addr).expect("open raw TCP connection"));
    }
    // Give the acceptor a beat to pull the stalled connections off the queue,
    // so the good connection below is genuinely contending with in-flight
    // stalled handshakes.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The good request must complete well under `STALLED * handshake_timeout`
    // (10s). A 5s bound is generous against CI slowness yet still proves the
    // good request was NOT serialized behind the five stalled handshakes.
    let start = std::time::Instant::now();
    let good = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || https_get(addr, "/health")),
    )
    .await
    .expect("good request must not serialize behind stalled handshakes")
    .unwrap()
    .expect("HTTPS GET /health while handshakes are stalled");
    let elapsed = start.elapsed();

    assert!(
        good.starts_with("HTTP/1.1 200"),
        "listener must serve a real client while handshakes stall, got: {good:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "good request took {elapsed:?}; expected well under STALLED*timeout (10s), \
         proving handshakes are not serialized in the accept loop"
    );

    // Keep the stalled sockets alive until the good request completed, so the
    // test truly overlapped stalled handshakes with a real one.
    drop(hung);

    server.shutdown.cancel();
    let _ = server.handle.await;
}
